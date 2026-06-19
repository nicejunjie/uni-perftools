/* Statistical sampling profiler. Per-thread POSIX timer -> realtime signal ->
 * handler records the interrupted leaf PC into a preallocated per-thread map.
 * No malloc/lock on the signal path. Symbolization happens in postprocess. */
#define _GNU_SOURCE
#include "sampler.h"
#include "libprof.h"
#include "config.h"
#include <signal.h>
#include <time.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <ucontext.h>
#include <dlfcn.h>
#include <pthread.h>

#define SAMP_SIG   (SIGRTMIN + 6)   /* avoid clobbering the app's SIGPROF/SIGALRM */
#define SAMP_CAP   8192             /* distinct leaf PCs per thread (power of two) */

static int            enabled;
static volatile int   active;       /* gate the handler off during finalize */
static clockid_t      clockid;

/* per-thread timer handle (the histogram lives in libprof_tls_t) */
static __thread timer_t s_timer;
static __thread int     s_armed;

int libprof_sample_enabled(void) { return enabled; }
int libprof_sample_hz(void)      { return libprof_cfg.sample_hz; }

/* ---- async-signal-safe handler: record the interrupted PC ---------------- */
static void on_sample(int sig, siginfo_t *si, void *ctx)
{
    (void)sig; (void)si;
    if (!active) return;
    libprof_tls_t *t = libprof_tls;
    if (!t || !t->samp_pc) return;

    ucontext_t *uc = (ucontext_t *)ctx;
    uint64_t pc;
#if defined(__x86_64__)
    pc = (uint64_t)uc->uc_mcontext.gregs[REG_RIP];
#elif defined(__aarch64__)
    pc = (uint64_t)uc->uc_mcontext.pc;
#else
    return;
#endif

    t->samp_total++;
    uint64_t mask = (uint64_t)t->samp_cap - 1;
    uint64_t i = (pc >> 2) & mask;               /* PCs are >=4-byte aligned-ish */
    for (int probe = 0; probe < t->samp_cap; probe++) {
        if (t->samp_cnt[i] == 0) {               /* empty slot: claim it */
            t->samp_pc[i] = pc; t->samp_cnt[i] = 1; t->samp_used++; return;
        }
        if (t->samp_pc[i] == pc) { t->samp_cnt[i]++; return; }
        i = (i + 1) & mask;
    }
    t->samp_dropped++;                            /* table full (rare) */
}

void libprof_sample_init(void)
{
    if (!libprof_cfg.sample) return;
    clockid = libprof_cfg.sample_cpu ? CLOCK_THREAD_CPUTIME_ID : CLOCK_MONOTONIC;

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = on_sample;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;        /* restart app syscalls we interrupt */
    sigemptyset(&sa.sa_mask);
    if (sigaction(SAMP_SIG, &sa, NULL) != 0) return;

    enabled = 1;
    active = 1;
    libprof_sample_thread_start();                /* arm the main thread */
}

void libprof_sample_thread_start(void)
{
    if (!enabled || s_armed) return;
    libprof_tls_t *t = libprof_tls ? libprof_tls : libprof_tls_init();
    if (!t->samp_pc) {                            /* preallocate off the signal path */
        t->samp_cap = SAMP_CAP;
        t->samp_pc  = calloc(t->samp_cap, sizeof(uint64_t));
        t->samp_cnt = calloc(t->samp_cap, sizeof(uint32_t));
        if (!t->samp_pc || !t->samp_cnt) { free(t->samp_pc); free(t->samp_cnt);
            t->samp_pc = NULL; return; }
    }

    struct sigevent sev;
    memset(&sev, 0, sizeof(sev));
    sev.sigev_notify = SIGEV_THREAD_ID;
    sev.sigev_signo  = SAMP_SIG;
    int tid = (int)syscall(SYS_gettid);
#ifdef sigev_notify_thread_id
    sev.sigev_notify_thread_id = tid;
#else
    sev._sigev_un._tid = tid;     /* glibc layout when the macro isn't exposed */
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

/* ---- pthread_create interposer: arm each new thread ---------------------- */
typedef void *(*startfn)(void *);
struct tramparg { startfn fn; void *arg; };

static void *tramp(void *p)
{
    struct tramparg a = *(struct tramparg *)p;
    free(p);
    libprof_sample_thread_start();
    void *r = a.fn(a.arg);
    libprof_sample_thread_stop();
    return r;
}

int pthread_create(pthread_t *thread, const pthread_attr_t *attr,
                   void *(*start)(void *), void *arg)
{
    static int (*real)(pthread_t *, const pthread_attr_t *, void *(*)(void *), void *);
    if (!real) real = dlsym(RTLD_NEXT, "pthread_create");
    if (!enabled) return real(thread, attr, start, arg);
    struct tramparg *ta = malloc(sizeof(*ta));
    if (!ta) return real(thread, attr, start, arg);
    ta->fn = start; ta->arg = arg;
    return real(thread, attr, tramp, ta);
}
