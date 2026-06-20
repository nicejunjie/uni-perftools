/* Statistical sampling profiler. Per-thread POSIX timer -> realtime signal ->
 * handler records the interrupted PC (leaf) and, when SCILIB_SAMPLE_STACK>1, the
 * call stack via backtrace(). No malloc/lock on the signal path (all per-thread
 * buffers are preallocated). Symbolization happens in postprocess. */
#define _GNU_SOURCE
#include "sampler.h"
#include "libprof.h"
#include "config.h"
#include <signal.h>
#include <time.h>
#include <string.h>
#include <stdlib.h>
#include <stdio.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <ucontext.h>
#include <execinfo.h>

#define SAMP_SIG   (SIGRTMIN + 6)
#define LEAF_CAP   8192             /* distinct leaf PCs (leaf mode) */
#define MAXD       128              /* max frames captured per sample */

static int            enabled;
static volatile int   active;
static clockid_t      clockid;
static int            depth;        /* frames to record (1 = leaf only) */

static __thread timer_t s_timer;
static __thread int     s_armed;

/* ---- call-stack table (one per thread, via tls->samp_stack) -------------- */
typedef struct {
    uint64_t *fr; int fcap, flen;                 /* frame arena */
    struct sentry { int off; int n; unsigned cnt; uint64_t h; } *e;
    int  ecap, elen;
    int *idx; int icap;                           /* hash -> entry+1 (0 empty) */
    uint64_t total, dropped;
} sstack_t;

int libprof_sample_enabled(void) { return enabled; }
int libprof_sample_hz(void)      { return libprof_cfg.sample_hz; }

static uint64_t hashframes(uint64_t *f, int n)
{
    uint64_t h = 1469598103934665603ULL;
    for (int i = 0; i < n; i++) { h ^= f[i]; h *= 1099511628211ULL; }
    return h;
}

static void stack_record(sstack_t *s, uint64_t *f, int n)
{
    s->total++;
    uint64_t h = hashframes(f, n);
    int mask = s->icap - 1;
    int i = (int)(h & mask);
    while (s->idx[i]) {
        struct sentry *e = &s->e[s->idx[i] - 1];
        if (e->h == h && e->n == n && memcmp(&s->fr[e->off], f, n * sizeof(uint64_t)) == 0) {
            e->cnt++; return;
        }
        i = (i + 1) & mask;
    }
    if (s->elen >= s->ecap || s->flen + n > s->fcap || (s->elen + 1) * 4 >= s->icap * 3) {
        s->dropped++; return;                      /* tables full */
    }
    int off = s->flen;
    memcpy(&s->fr[off], f, n * sizeof(uint64_t));
    s->flen += n;
    s->e[s->elen] = (struct sentry){ off, n, 1, h };
    s->idx[i] = ++s->elen;
}

static void on_sample(int sig, siginfo_t *si, void *ctx)
{
    (void)sig; (void)si;
    if (!active) return;
    libprof_tls_t *t = libprof_tls;
    if (!t) return;

    ucontext_t *uc = (ucontext_t *)ctx;
    uint64_t pc;
#if defined(__x86_64__)
    pc = (uint64_t)uc->uc_mcontext.gregs[REG_RIP];
#elif defined(__aarch64__)
    pc = (uint64_t)uc->uc_mcontext.pc;
#else
    return;
#endif

    if (depth <= 1) {                              /* leaf-only fast path */
        if (!t->samp_pc) return;
        t->samp_total++;
        uint64_t mask = (uint64_t)t->samp_cap - 1, i = (pc >> 2) & mask;
        for (int p = 0; p < t->samp_cap; p++) {
            if (t->samp_cnt[i] == 0) { t->samp_pc[i] = pc; t->samp_cnt[i] = 1; t->samp_used++; return; }
            if (t->samp_pc[i] == pc) { t->samp_cnt[i]++; return; }
            i = (i + 1) & mask;
        }
        t->samp_dropped++;
        return;
    }

    sstack_t *s = t->samp_stack;
    if (!s) return;
    void *bt[MAXD];
    int n = backtrace(bt, depth + 2);              /* +2: handler + sigtrampoline */
    int skip = 2;                                  /* drop on_sample + __restore_rt */
    if (n <= skip) return;
    uint64_t frames[MAXD];
    int m = 0;
    frames[m++] = pc;                              /* exact leaf from ucontext */
    for (int i = skip; i < n && m < depth; i++) frames[m++] = (uint64_t)bt[i];
    stack_record(s, frames, m);
}

void libprof_sample_init(void)
{
    if (!libprof_cfg.sample) return;
    depth = libprof_cfg.sample_stack;
    clockid = libprof_cfg.sample_cpu ? CLOCK_THREAD_CPUTIME_ID : CLOCK_MONOTONIC;

    if (depth > 1) { void *w[4]; backtrace(w, 4); }   /* warm up libgcc unwinder off-signal */

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = on_sample;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SAMP_SIG, &sa, NULL) != 0) return;

    enabled = 1;
    active = 1;
    libprof_sample_thread_start();
}

void libprof_sample_thread_start(void)
{
    if (!enabled || s_armed) return;
    libprof_tls_t *t = libprof_tls ? libprof_tls : libprof_tls_init();

    if (depth <= 1) {
        if (!t->samp_pc) {
            t->samp_cap = LEAF_CAP;
            t->samp_pc  = calloc(t->samp_cap, sizeof(uint64_t));
            t->samp_cnt = calloc(t->samp_cap, sizeof(uint32_t));
            if (!t->samp_pc || !t->samp_cnt) return;
        }
    } else if (!t->samp_stack) {
        sstack_t *s = calloc(1, sizeof(*s));
        s->fcap = 1 << 15; s->fr = calloc(s->fcap, sizeof(uint64_t));
        s->ecap = 1 << 12; s->e = calloc(s->ecap, sizeof(*s->e));
        s->icap = 1 << 13; s->idx = calloc(s->icap, sizeof(int));
        if (!s->fr || !s->e || !s->idx) { free(s->fr); free(s->e); free(s->idx); free(s); return; }
        t->samp_stack = s;
    }

    struct sigevent sev;
    memset(&sev, 0, sizeof(sev));
    sev.sigev_notify = SIGEV_THREAD_ID;
    sev.sigev_signo  = SAMP_SIG;
    int tid = (int)syscall(SYS_gettid);
#ifdef sigev_notify_thread_id
    sev.sigev_notify_thread_id = tid;
#else
    sev._sigev_un._tid = tid;
#endif
    if (timer_create(clockid, &sev, &s_timer) != 0) return;

    long ns = 1000000000L / libprof_cfg.sample_hz;
    struct itimerspec its;
    its.it_interval.tv_sec = ns / 1000000000L;
    its.it_interval.tv_nsec = ns % 1000000000L;
    its.it_value = its.it_interval;
    if (timer_settime(s_timer, 0, &its, NULL) != 0) { timer_delete(s_timer); return; }
    s_armed = 1;
}

void libprof_sample_thread_stop(void)
{
    if (s_armed) { timer_delete(s_timer); s_armed = 0; }
}

void libprof_sample_stop_all(void) { active = 0; libprof_sample_thread_stop(); }

/* ---- emit the whole "sampling" JSON object (called from emit.c) ---------- */
struct ectx { FILE *f; int first; uint64_t total, dropped; };

static void emit_one(libprof_tls_t *t, void *ud)
{
    struct ectx *c = ud;
    if (depth <= 1) {
        c->total += t->samp_total; c->dropped += t->samp_dropped;
        if (!t->samp_pc) return;
        for (int i = 0; i < t->samp_cap; i++)
            if (t->samp_cnt[i]) {
                fprintf(c->f, "%s\n      [%llu,%u]", c->first ? "" : ",",
                        (unsigned long long)t->samp_pc[i], t->samp_cnt[i]);
                c->first = 0;
            }
    } else {
        sstack_t *s = t->samp_stack;
        if (!s) return;
        c->total += s->total; c->dropped += s->dropped;
        for (int i = 0; i < s->elen; i++) {
            struct sentry *e = &s->e[i];
            fprintf(c->f, "%s\n      {\"n\":%u,\"pc\":[", c->first ? "" : ",", e->cnt);
            for (int j = 0; j < e->n; j++)
                fprintf(c->f, "%s%llu", j ? "," : "", (unsigned long long)s->fr[e->off + j]);
            fprintf(c->f, "]}");
            c->first = 0;
        }
    }
}

static void emit_maps(FILE *f)
{
    FILE *m = fopen("/proc/self/maps", "r");
    if (!m) return;
    char line[4096]; int first = 1;
    while (fgets(line, sizeof(line), m)) {
        unsigned long start, end, off; char perms[8], path[4096]; path[0] = 0;
        if (sscanf(line, "%lx-%lx %7s %lx %*s %*s %4095[^\n]", &start, &end, perms, &off, path) < 4)
            continue;
        if (perms[2] != 'x') continue;
        char *p = path; while (*p == ' ') p++;
        if (p[0] != '/') continue;
        fprintf(f, "%s\n      {\"path\":\"%s\",\"start\":%lu,\"end\":%lu,\"off\":%lu}",
                first ? "" : ",", p, start, end, off);
        first = 0;
    }
    fclose(m);
}

void libprof_sample_emit(void *fp)
{
    if (!enabled) return;
    FILE *f = fp;
    fprintf(f, ",\n  \"sampling\": {\n    \"hz\": %d, \"stack\": %d,\n    \"%s\": [",
            libprof_cfg.sample_hz, depth, depth > 1 ? "stacks" : "samples");
    struct ectx c = { f, 1, 0, 0 };
    libprof_tls_foreach(emit_one, &c);
    fprintf(f, "\n    ],\n    \"total\": %llu, \"dropped\": %llu,\n    \"maps\": [",
            (unsigned long long)c.total, (unsigned long long)c.dropped);
    emit_maps(f);
    fprintf(f, "\n    ]\n  }");
}

/* ---- pthread_create interposer: arm each new thread ---------------------- */
#include <dlfcn.h>
#include <pthread.h>
typedef void *(*startfn)(void *);
struct tramparg { startfn fn; void *arg; };

__attribute__((weak)) void libprof_roofline_thread_start(void);
__attribute__((weak)) void libprof_roofline_thread_stop(void);

static void *tramp(void *p)
{
    struct tramparg a = *(struct tramparg *)p;
    free(p);
    libprof_sample_thread_start();
    if (libprof_roofline_thread_start) libprof_roofline_thread_start();
    void *r = a.fn(a.arg);
    if (libprof_roofline_thread_stop) libprof_roofline_thread_stop();
    libprof_sample_thread_stop();
    return r;
}

__attribute__((weak)) int libprof_roofline_enabled(void);

int pthread_create(pthread_t *thread, const pthread_attr_t *attr,
                   void *(*start)(void *), void *arg)
{
    static int (*real)(pthread_t *, const pthread_attr_t *, void *(*)(void *), void *);
    if (!real) real = dlsym(RTLD_NEXT, "pthread_create");
    int roof = libprof_roofline_enabled && libprof_roofline_enabled();
    if (!enabled && !roof) return real(thread, attr, start, arg);
    struct tramparg *ta = malloc(sizeof(*ta));
    if (!ta) return real(thread, attr, start, arg);
    ta->fn = start; ta->arg = arg;
    return real(thread, attr, tramp, ta);
}
