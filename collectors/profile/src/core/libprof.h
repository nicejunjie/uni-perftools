#ifndef LIBPROF_H
#define LIBPROF_H

#include <stddef.h>
#include <stdint.h>
#include "timer.h"
#include "libprof_slots.h"   /* generated: enum of *_slot ids + LIBPROF_NSLOTS */

/* Fortran INTEGER width. Build with `make ILP64=1` (-DLIBPROF_ILP64) to profile
 * 64-bit-integer BLAS/LAPACK (MKL/NVPL ILP64). Only affects how shape analyzers
 * read dimension arguments; counts/timing are width-independent. */
#ifdef LIBPROF_ILP64
typedef long long lp_fint;
#else
typedef int lp_fint;
#endif

/* ------------------------------------------------------------------ groups */
enum libprof_group {
    LP_BLAS = 0, LP_LAPACK, LP_PBLAS, LP_SCALAPACK,
    LP_CBLAS, LP_LAPACKE, LP_FFTW, LP_MPI, LP_IO, LP_NGROUPS
};

/* ------------------------------------------------------------- metric key */
/* A row aggregates under a key. For the common case the key is just the slot
 * (shape_len == 0, fast path: dense per-slot array). Analyzers may return a
 * shaped key (e.g. "n=1024") which lands in the per-thread hashmap. */
typedef struct {
    uint16_t    slot;
    uint16_t    shape_len;    /* 0 => no shape, use fast path */
    uint64_t    shape_hash;
    const char *shape;        /* interned, NUL-terminated */
} libprof_key_t;

/* Extra per-call semantic metrics produced by an analyzer (off the clock).
 * Currently communication volume in bytes (-> GB/s); flop modeling was dropped
 * because it is only exact for a tiny subset of functions. */
typedef struct { uint64_t bytes; } libprof_delta_t;

/* Accumulator for one key. */
typedef struct {
    uint64_t count;
    double   t_incl;
    double   t_excl;
    uint64_t bytes;
} libprof_metric_t;

/* ----------------------------------------------------------- descriptors */
struct libprof_desc;
/* Analyzer: fills *k (returns 1 if a shaped key was produced, else 0) and *md.
 * args[i] always points AT argument i's value (deref once with the right type),
 * regardless of Fortran-by-pointer vs C-by-value dialect. ret is the function's
 * return value for pointer-returning functions (e.g. fftw_plan), else NULL. */
typedef int (*libprof_analyzer_fn)(libprof_key_t *k, libprof_delta_t *md,
                                   const struct libprof_desc *d, void **args, void *ret);

typedef struct libprof_desc {
    const char         *name;     /* original symbol, e.g. "dgemm_" */
    const char         *wname;    /* DBI wrapper symbol, e.g. "libprof_dbi_dgemm_" */
    const char         *group;    /* "BLAS" */
    uint16_t            group_id; /* enum libprof_group */
    uint16_t            slot;
    libprof_analyzer_fn analyze;  /* NULL for most functions */
    void               *orig;     /* resolved by the active backend */
} libprof_desc_t;

extern libprof_desc_t libprof_desc[LIBPROF_NSLOTS];

/* ------------------------------------------------------- per-thread state */
typedef struct { double t0; double child_time; } libprof_frame_t;

struct libprof_hashmap;  /* tier-2, defined in store.c */

typedef struct libprof_tls {
    libprof_metric_t        by_slot[LIBPROF_NSLOTS]; /* tier-1, no hashing */
    struct libprof_hashmap *shaped;                  /* tier-2, lazily allocated */
    libprof_frame_t        *stack;
    int                     depth;
    int                     cap;
    int                     in_runtime;              /* reentrancy guard */
    int                     tid;
    char                   *arena;                   /* shape-string interning */
    size_t                  arena_off, arena_cap;
    struct libprof_tls     *next;                    /* registry for merge */

    /* sampling profiler: per-thread leaf-PC histogram, preallocated so the
     * signal handler never allocates. Owned by src/sample/sampler.c. */
    uint64_t               *samp_pc;
    uint32_t               *samp_cnt;
    int                     samp_cap, samp_used;
    uint64_t                samp_total, samp_dropped;
    void                   *samp_stack;   /* call-stack table (src/sample/sampler.c) */
    void                   *roof;          /* per-thread roofline histograms (roofline_sampler.c) */
} libprof_tls_t;

extern __thread libprof_tls_t *libprof_tls;

/* Set at finalize so wrappers stop measuring our own shutdown-time activity
 * (e.g. the I/O we do writing the report file). */
extern volatile int libprof_shutdown;

/* cold-path helpers (in tls.c / store.c) */
libprof_tls_t   *libprof_tls_init(void);
libprof_frame_t *libprof_grow_stack(libprof_tls_t *t);
libprof_metric_t *libprof_shaped_get(libprof_tls_t *t, const libprof_key_t *k);
void             libprof_tls_foreach(void (*fn)(libprof_tls_t *, void *), void *ud);
int              libprof_tls_nthreads(void);
void             libprof_shaped_foreach(libprof_tls_t *t,
                     void (*fn)(const libprof_key_t *, const libprof_metric_t *, void *), void *ud);

/* shape-string interning into a thread's arena (used by analyzers) */
const char      *libprof_intern(libprof_tls_t *t, const char *s, uint16_t len);
uint64_t         libprof_fnv1a(const void *data, size_t len);

/* lifecycle (libprof.c) */
void libprof_init(void);
void libprof_finalize(void);

/* Extra raw-JSON emitters: a module (MPI comm matrix, heap, ...) registers a
 * callback that writes  ,\n  "key": <json>  into the per-rank file at finalize. */
typedef void (*libprof_emitter_fn)(void *file);
void libprof_register_emitter(libprof_emitter_fn fn);
void libprof_emit_extras(void *file);

/* backend hook: resolve the original function for a descriptor (dl: dlsym). */
void *libprof_resolve(libprof_desc_t *d);

/* ------------------------------------------------------------- hot path */
static inline libprof_frame_t *libprof_enter(void)
{
    if (__builtin_expect(libprof_shutdown, 0)) return NULL;
    libprof_tls_t *t = libprof_tls;
    if (__builtin_expect(t == NULL, 0)) t = libprof_tls_init();
    if (__builtin_expect(t->in_runtime, 0)) return NULL;  /* skip measurement */
    libprof_frame_t *f;
    if (__builtin_expect(t->depth >= t->cap, 0)) f = libprof_grow_stack(t);
    else                                         f = &t->stack[t->depth];
    t->depth++;
    f->child_time = 0.0;
    f->t0 = libprof_now();
    return f;
}

/* Mark the calling thread as running profiler-internal code (an analyzer).
 * While set, libprof_enter() returns NULL, so a wrapped function invoked from
 * inside an analyzer runs unmeasured instead of recursing into measurement.
 * Used only on the cold analyzer path; the original-function call stays outside
 * the guard so legitimate nesting (e.g. LAPACK calling BLAS) is still timed. */
static inline void libprof_runtime_begin(void) { if (libprof_tls) libprof_tls->in_runtime = 1; }
static inline void libprof_runtime_end(void)   { if (libprof_tls) libprof_tls->in_runtime = 0; }

static inline void libprof_exit(libprof_desc_t *d, double dt,
                                const libprof_key_t *k, const libprof_delta_t *md)
{
    libprof_tls_t *t = libprof_tls;
    dt -= libprof_overhead;
    if (dt < 0.0) dt = 0.0;
    t->depth--;
    libprof_frame_t *f = &t->stack[t->depth];
    double excl = dt - f->child_time;
    if (excl < 0.0) excl = 0.0;
    if (t->depth > 0) t->stack[t->depth - 1].child_time += dt;

    libprof_metric_t *m;
    if (__builtin_expect(k == NULL || k->shape_len == 0, 1))
        m = &t->by_slot[d->slot];
    else
        m = libprof_shaped_get(t, k);

    m->count++;
    m->t_incl += dt;
    m->t_excl += excl;
    if (md) m->bytes += md->bytes;
}

#endif /* LIBPROF_H */
