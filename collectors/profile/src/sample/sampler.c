/* Statistical sampling profiler. Per-thread POSIX timer -> realtime signal ->
 * handler records the interrupted PC (leaf) and, when UPAT_SAMPLE_STACK>1, the
 * call stack via backtrace(). No malloc/lock on the signal path (all per-thread
 * buffers are preallocated). Symbolization happens in postprocess.
 *
 * CAVEAT (stack mode): backtrace() is NOT async-signal-safe — it can take libgcc's
 * unwinder / glibc's dl_load_lock. If a sample lands while the interrupted thread
 * holds that lock (mid-dlopen, common in plugin/Python-heavy stacks), stack mode
 * can deadlock the thread. The unwinder is warmed up off-signal at init to avoid
 * the first-call malloc, and a fault guard catches SEGV/BUS during unwind, but a
 * lock-held deadlock is not recoverable. Leaf mode (UPAT_SAMPLE_STACK=1) only
 * reads the PC and is safe; prefer it for dlopen-heavy targets. */
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
#include <setjmp.h>
#include <pthread.h>

#define SAMP_SIG   (SIGRTMIN + 6)
#define LEAF_CAP   8192             /* distinct leaf PCs (leaf mode) */
#define MAXD       128              /* max frames captured per sample */

static int            enabled;
/* `active` gates the handler. Accessed via __atomic_* (SEQ_CST): the release
 * store in libprof_sample_stop_all() happens-before the acquire load in the
 * handler, so once a thread observes !active it also sees the stop having
 * occurred. Plain `volatile int` gave ordering against the optimizer but no
 * cross-thread memory barrier. */
static int            active;
static clockid_t      clockid;
static int            depth;        /* frames to record (1 = leaf only) */

/* ---- fault-guarded unwind --------------------------------------------------
 * backtrace() walks .eh_frame in the interrupted thread's context. Frames with
 * missing/bad CFI (hand-written asm, JIT, vendor libs like cuda/cuda-openmpi)
 * make it follow a bad return address into unmapped memory and SIGSEGV/SIGBUS —
 * crashing the *target* (stock `perf record` is immune because the kernel
 * unwinds). We wrap the unwind in sigsetjmp + a temporary SEGV/BUS handler: a
 * fault during unwinding longjmps back and we keep just the leaf PC, instead of
 * killing the program. Faults outside the unwind window chain to the target's
 * own handler so genuine crashes still crash. */
static __thread sigjmp_buf      s_unwind_jmp;
static __thread volatile sig_atomic_t s_unwinding;
static struct sigaction s_old_segv, s_old_bus;
static volatile int     s_chain_saved;          /* captured the app's SEGV/BUS handler once */

static void unwind_guard(int sig, siginfo_t *si, void *ctx)
{
    if (s_unwinding) {
        s_unwinding = 0;
        siglongjmp(s_unwind_jmp, 1);
    }
    /* not a sampler-induced fault → defer to whatever was installed before us */
    struct sigaction *old = (sig == SIGBUS) ? &s_old_bus : &s_old_segv;
    if ((old->sa_flags & SA_SIGINFO) && old->sa_sigaction) {
        old->sa_sigaction(sig, si, ctx);
    } else if (old->sa_handler == SIG_IGN) {
        return;
    } else if (old->sa_handler && old->sa_handler != SIG_DFL) {
        old->sa_handler(sig);
    } else {
        signal(sig, SIG_DFL);
        raise(sig);
    }
}

static __thread timer_t s_timer;
static __thread int     s_armed;

/* Registry of every armed per-thread timer. libprof_sample_stop_all() runs on
 * ONE thread but must disarm EVERY thread's timer before emit reads the shared
 * stack tables — otherwise other threads keep firing SAMP_SIG and mutate their
 * tables (and torn counts) while emit_one walks them. timer_t is a process-wide
 * handle, so timer_delete() from the finalizing thread disarms a timer created
 * on another thread. The list is mutex-guarded; the signal path never touches
 * it. */
struct tmrnode { timer_t timer; int live; struct tmrnode *next; };
static struct tmrnode  *tmr_list;
static pthread_mutex_t  tmr_lock = PTHREAD_MUTEX_INITIALIZER;
static __thread struct tmrnode *s_tmrnode;

/* Install our SEGV/BUS unwind guard, capturing the app's prior handler as the
 * chain target the first time. Called OFF the signal path (thread start), not
 * from inside on_sample, to avoid racing the app's own sigaction every sample.
 * Idempotent-safe: if our guard is already installed, the captured chain is
 * left untouched. */
static void install_unwind_guard(void)
{
    struct sigaction g, ps, pb;
    memset(&g, 0, sizeof(g));
    g.sa_sigaction = unwind_guard;
    g.sa_flags = SA_SIGINFO | SA_NODEFER;
    sigemptyset(&g.sa_mask);
    sigaction(SIGSEGV, &g, &ps);
    sigaction(SIGBUS,  &g, &pb);
    if (!s_chain_saved && ps.sa_sigaction != unwind_guard) {
        s_old_segv = ps; s_old_bus = pb; s_chain_saved = 1;
    }
}

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
    if (!__atomic_load_n(&active, __ATOMIC_ACQUIRE)) return;
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

    /* The target's runtime (gfortran -fbacktrace, OpenMPI) may install its own
     * SIGSEGV/SIGBUS handler at init, overriding our unwind guard. Rather than
     * re-arm with sigaction() on every sample (which races the app's handler and
     * is not async-signal-safe to do unconditionally), the guard is installed
     * once per thread off-signal in libprof_sample_thread_start(). We re-arm here
     * ONLY if our guard is no longer the active SEGV handler — i.e. the app
     * replaced it — using sigaction(SIGSEGV, NULL, &cur) to peek without writing.
     * In steady state this is a single read syscall and never overwrites a
     * handler that is still ours. */
    {
        struct sigaction cur;
        if (sigaction(SIGSEGV, NULL, &cur) == 0 && cur.sa_sigaction != unwind_guard)
            install_unwind_guard();
    }

    void *bt[MAXD];
    int n = 0;
    int want = depth + 2;                          /* +2: handler + sigtrampoline */
    if (want > MAXD) want = MAXD;                  /* never overflow bt[] */
    /* guarded: a fault inside backtrace() longjmps back with n unchanged (0),
     * so we still record the leaf rather than crash the target. */
    s_unwinding = 1;
    if (sigsetjmp(s_unwind_jmp, 1) == 0)
        n = backtrace(bt, want);
    s_unwinding = 0;
    int skip = 2;                                  /* drop on_sample + __restore_rt */
    uint64_t frames[MAXD];
    int m = 0;
    frames[m++] = pc;                              /* exact leaf from ucontext (always) */
    for (int i = skip; i < n && m < depth; i++) frames[m++] = (uint64_t)bt[i];
    stack_record(s, frames, m);                    /* leaf-only if the unwind faulted */
}

void libprof_sample_init(void)
{
    if (!libprof_cfg.sample) return;
    depth = libprof_cfg.sample_stack;
    clockid = libprof_cfg.sample_cpu ? CLOCK_THREAD_CPUTIME_ID : CLOCK_MONOTONIC;

    if (depth > 1) {
        void *w[4]; backtrace(w, 4);                  /* warm up libgcc unwinder off-signal */
        /* install the unwind fault guard (SA_NODEFER so a fault re-enters cleanly
         * for the longjmp); chain to any prior handler for non-sampler faults. */
        install_unwind_guard();
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = on_sample;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SAMP_SIG, &sa, NULL) != 0) return;

    enabled = 1;
    __atomic_store_n(&active, 1, __ATOMIC_RELEASE);
    libprof_sample_thread_start();
}

void libprof_sample_thread_start(void)
{
    if (!enabled || s_armed) return;
    libprof_tls_t *t = libprof_tls ? libprof_tls : libprof_tls_init();

    /* Re-assert the unwind guard for THIS thread off-signal: the app's runtime
     * (gfortran/OpenMPI) may have replaced our SEGV/BUS handler after our
     * constructor ran. Doing it here (not in the handler) avoids racing the app. */
    if (depth > 1) install_unwind_guard();

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

    /* Register the timer so stop_all() can disarm it from another thread. */
    struct tmrnode *node = calloc(1, sizeof(*node));
    if (node) {
        node->timer = s_timer; node->live = 1;
        pthread_mutex_lock(&tmr_lock);
        node->next = tmr_list; tmr_list = node;
        pthread_mutex_unlock(&tmr_lock);
        s_tmrnode = node;
    }
}

void libprof_sample_thread_stop(void)
{
    if (!s_armed) return;
    if (s_tmrnode) {                        /* mark dead so stop_all won't double-delete */
        pthread_mutex_lock(&tmr_lock);
        if (s_tmrnode->live) { s_tmrnode->live = 0; timer_delete(s_timer); }
        pthread_mutex_unlock(&tmr_lock);
        s_tmrnode = NULL;
    } else {
        timer_delete(s_timer);
    }
    s_armed = 0;
}

void libprof_sample_stop_all(void)
{
    /* Release the stop so the handler's acquire-load observes it (and all writes
     * before it). After this, any thread that takes a sample early-returns. */
    __atomic_store_n(&active, 0, __ATOMIC_RELEASE);

    /* Disarm and delete EVERY registered thread's timer (not just this one), so
     * no thread keeps firing SAMP_SIG and mutating its stack table while emit
     * reads it. A sample in flight on another thread either already saw active=1
     * (harmless, completes) or sees active=0 and returns; once timer_delete
     * returns, no further deliveries occur for that timer. */
    pthread_mutex_lock(&tmr_lock);
    for (struct tmrnode *n = tmr_list; n; n = n->next) {
        if (n->live) { n->live = 0; timer_delete(n->timer); }
    }
    pthread_mutex_unlock(&tmr_lock);
    s_tmrnode = NULL;
    s_armed = 0;
    /* Residual risk (acceptable): a handler on another thread that ALREADY passed
     * the active-load before our release-store can still complete one final record
     * into ITS OWN thread's table. timer_delete() is synchronous (no further
     * deliveries after it returns), and finalize runs as a process destructor when
     * threads are typically already joined (tramp() calls thread_stop on return),
     * so this is a single, self-contained in-flight sample at worst — versus the
     * prior behavior where every other thread's timer stayed fully armed and kept
     * firing throughout emit. There is no portable async-signal-safe way to join
     * an in-flight handler here without a heavier per-thread ack protocol. */
}

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
        /* Emit the path as a properly escaped JSON string. A raw %s breaks the
         * whole prof.*.json (and silently drops the run) if a mapped path contains
         * a '"' or '\\' — rare but legal in a filename. Escape inline, matching
         * emit.c's json_str(). */
        fprintf(f, "%s\n      {\"path\":\"", first ? "" : ",");
        for (char *q = p; *q; q++) {
            if (*q == '"' || *q == '\\') fputc('\\', f);
            fputc(*q, f);
        }
        fprintf(f, "\",\"start\":%lu,\"end\":%lu,\"off\":%lu}", start, end, off);
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
#include <errno.h>
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
    if (!real) return EAGAIN;               /* no real pthread_create: cannot proceed */
    int roof = libprof_roofline_enabled && libprof_roofline_enabled();
    if (!enabled && !roof) return real(thread, attr, start, arg);
    struct tramparg *ta = malloc(sizeof(*ta));
    if (!ta) return real(thread, attr, start, arg);
    ta->fn = start; ta->arg = arg;
    int rc = real(thread, attr, tramp, ta);
    if (rc != 0) free(ta);                  /* failed: tramp never runs, so free its arg */
    return rc;
}
