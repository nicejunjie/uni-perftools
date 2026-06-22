/* Heap high-water tracking (opt-in: UPAT_HEAP=1). Interposes the allocator and
 * records live bytes, peak (high-water), and allocation count. Off by default, so
 * the wrappers are pure passthrough unless enabled. Not compatible with a second
 * preloaded allocator (jemalloc/tcmalloc).
 *
 * The dlsym(RTLD_NEXT) bootstrap can itself call calloc before the real symbol is
 * resolved; we serve those few early allocations from a static buffer. */
#define _GNU_SOURCE
#include "libprof.h"
#include "config.h"
#include <dlfcn.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>

static void *(*r_malloc)(size_t);
static void *(*r_calloc)(size_t, size_t);
static void *(*r_realloc)(void *, size_t);
static void  (*r_free)(void *);
static size_t (*r_usable)(void *);

static char  bootbuf[1 << 16];
static size_t bootoff;
static int   in_boot(void *p) { return (char *)p >= bootbuf && (char *)p < bootbuf + sizeof(bootbuf); }

static unsigned long long live, peak, allocs;

static void resolve(void)
{
    static int done;
    if (done) return;
    done = 1;
    r_malloc  = dlsym(RTLD_NEXT, "malloc");
    r_calloc  = dlsym(RTLD_NEXT, "calloc");
    r_realloc = dlsym(RTLD_NEXT, "realloc");
    r_free    = dlsym(RTLD_NEXT, "free");
    r_usable  = dlsym(RTLD_NEXT, "malloc_usable_size");
}

static inline int on(void) { return libprof_cfg.heap; }

static void add(long delta)
{
    unsigned long long v = __atomic_add_fetch(&live, delta, __ATOMIC_RELAXED);
    __atomic_add_fetch(&allocs, 1, __ATOMIC_RELAXED);
    unsigned long long p = __atomic_load_n(&peak, __ATOMIC_RELAXED);
    while (v > p && !__atomic_compare_exchange_n(&peak, &p, v, 0,
                                                 __ATOMIC_RELAXED, __ATOMIC_RELAXED)) {}
}

void *malloc(size_t n)
{
    if (!r_malloc) resolve();
    void *p = r_malloc(n);
    if (on() && p) add((long)r_usable(p));
    return p;
}

void *calloc(size_t n, size_t s)
{
    if (!r_calloc) {                       /* bootstrap: dlsym may call us */
        if (s != 0 && n > sizeof(bootbuf) / s) return NULL;  /* overflow / too big for bootbuf */
        size_t t = n * s, a = (t + 15) & ~(size_t)15;
        if (bootoff + a > sizeof(bootbuf)) return NULL;
        void *p = bootbuf + bootoff; bootoff += a;
        return p;                          /* static memory is already zeroed */
    }
    void *p = r_calloc(n, s);
    if (on() && p) add((long)r_usable(p));
    return p;
}

void *realloc(void *old, size_t n)
{
    if (!r_realloc) resolve();
    if (in_boot(old)) {                    /* migrate off bootbuf: copy old contents over */
        void *p = malloc(n);
        if (p) {
            size_t avail = (size_t)(bootbuf + sizeof(bootbuf) - (char *)old);
            memcpy(p, old, n < avail ? n : avail);
        }
        return p;
    }
    long before = (on() && old) ? (long)r_usable(old) : 0;
    void *p = r_realloc(old, n);
    if (on() && p) add((long)r_usable(p) - before);
    return p;
}

void free(void *p)
{
    if (!p || in_boot(p)) return;          /* never free the static buffer */
    if (!r_free) resolve();
    if (on()) __atomic_sub_fetch(&live, (long)r_usable(p), __ATOMIC_RELAXED);
    r_free(p);
}

static void emit(void *fp)
{
    fprintf((FILE *)fp, ",\n  \"heap\": {\"peak\": %llu, \"live_at_exit\": %llu, \"allocs\": %llu}",
            peak, live, allocs);
}

void libprof_heap_init(void)
{
    if (!libprof_cfg.heap) return;
    resolve();
    extern void libprof_register_emitter(libprof_emitter_fn);
    libprof_register_emitter(emit);
}
