/* FFTW is plan-based: the cost is in fftw_execute(plan), which carries only an
 * opaque plan pointer. We record each plan's shape at creation in a global
 * registry keyed by the returned plan pointer, then attribute size at
 * execute time. This is the "stateful analyzer" - no special case in the core. */
#include "analyzer.h"
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <stdio.h>
#include <pthread.h>

typedef struct {
    void  *plan;
    char   shape[48];
    int    used;
} pentry_t;

static pentry_t       *reg;
static size_t          reg_cap, reg_len;
static pthread_rwlock_t reg_lock = PTHREAD_RWLOCK_INITIALIZER;

static size_t pmix(void *p) { return ((uintptr_t)p >> 4) * 1099511628211ULL; }

static void reg_put(void *plan, const char *shape)
{
    if (!plan) return;
    pthread_rwlock_wrlock(&reg_lock);
    if ((reg_len + 1) * 4 >= reg_cap * 3) {
        size_t ncap = reg_cap ? reg_cap * 2 : 64;
        pentry_t *ns = calloc(ncap, sizeof(pentry_t));
        for (size_t j = 0; j < reg_cap; j++) {
            if (!reg[j].used) continue;
            size_t i = pmix(reg[j].plan) & (ncap - 1);
            while (ns[i].used) i = (i + 1) & (ncap - 1);
            ns[i] = reg[j];
        }
        free(reg); reg = ns; reg_cap = ncap;
    }
    size_t mask = reg_cap - 1, i = pmix(plan) & mask;
    while (reg[i].used && reg[i].plan != plan) i = (i + 1) & mask;
    if (!reg[i].used) { reg[i].used = 1; reg[i].plan = plan; reg_len++; }
    snprintf(reg[i].shape, sizeof(reg[i].shape), "%s", shape);
    pthread_rwlock_unlock(&reg_lock);
}

static int reg_get(void *plan, char *shape_out, size_t cap)
{
    int found = 0;
    pthread_rwlock_rdlock(&reg_lock);
    if (reg_cap) {
        size_t mask = reg_cap - 1, i = pmix(plan) & mask;
        while (reg[i].used) {
            if (reg[i].plan == plan) {
                snprintf(shape_out, cap, "%s", reg[i].shape);
                found = 1; break;
            }
            i = (i + 1) & mask;
        }
    }
    pthread_rwlock_unlock(&reg_lock);
    return found;
}

static void reg_del(void *plan)
{
    pthread_rwlock_wrlock(&reg_lock);
    if (reg_cap) {
        size_t mask = reg_cap - 1, i = pmix(plan) & mask;
        while (reg[i].used) {
            if (reg[i].plan == plan) { reg[i].used = 0; reg_len--; break; }
            i = (i + 1) & mask;
        }
    }
    pthread_rwlock_unlock(&reg_lock);
}

/* plan creators: dims by value at args[0..rank-1]; ret is the new plan. */
static int an_plan(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d,
                   void **a, void *ret, int rank)
{
    (void)k; (void)md; (void)d;
    char shape[48]; int off = 0;
    for (int i = 0; i < rank; i++) { long dim = *(int *)a[i];
        off += snprintf(shape + off, sizeof(shape) - off, i ? "x%ld" : "%ld", dim); }
    reg_put(ret, shape);
    return 0;   /* the plan-create call itself is just counted */
}
static int an_plan1(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ return an_plan(k, md, d, a, r, 1); }
static int an_plan2(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ return an_plan(k, md, d, a, r, 2); }
static int an_plan3(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ return an_plan(k, md, d, a, r, 3); }

/* execute: args[0] -> plan; attribute transform size from the registry. */
static int an_exec(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *ret)
{
    (void)ret; (void)md;
    void *plan = *(void **)a[0];
    char shape[48];
    if (!reg_get(plan, shape, sizeof(shape))) return 0;
    size_t len = strlen(shape);
    const char *s = libprof_intern(libprof_tls, shape, (uint16_t)len);
    if (!s) return 0;
    k->slot = d->slot; k->shape = s; k->shape_len = (uint16_t)len;
    k->shape_hash = libprof_fnv1a(s, len);
    return 1;
}

static int an_destroy(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *ret)
{ (void)k; (void)md; (void)d; (void)ret; reg_del(*(void **)a[0]); return 0; }

static void bind(const char *name, libprof_analyzer_fn fn)
{
    for (int i = 0; i < LIBPROF_NSLOTS; i++)
        if (strcmp(libprof_desc[i].name, name) == 0) { libprof_desc[i].analyze = fn; return; }
}

void libprof_register_fftw_analyzers(void)
{
    const char *pre[] = {"fftw", "fftwf", "fftwl"};
    char nm[48];
    for (int p = 0; p < 3; p++) {
        /* plan creators: rank from the dft / r2c / c2r dimensionality */
        const char *kinds[] = {"dft", "dft_r2c", "dft_c2r"};
        for (int kk = 0; kk < 3; kk++) {
            snprintf(nm, sizeof(nm), "%s_plan_%s_1d", pre[p], kinds[kk]); bind(nm, an_plan1);
            snprintf(nm, sizeof(nm), "%s_plan_%s_2d", pre[p], kinds[kk]); bind(nm, an_plan2);
            snprintf(nm, sizeof(nm), "%s_plan_%s_3d", pre[p], kinds[kk]); bind(nm, an_plan3);
        }
        /* every execute variant carries the plan in arg 0 -> same lookup */
        snprintf(nm, sizeof(nm), "%s_execute", pre[p]);          bind(nm, an_exec);
        snprintf(nm, sizeof(nm), "%s_execute_dft", pre[p]);      bind(nm, an_exec);
        snprintf(nm, sizeof(nm), "%s_execute_dft_r2c", pre[p]);  bind(nm, an_exec);
        snprintf(nm, sizeof(nm), "%s_execute_dft_c2r", pre[p]);  bind(nm, an_exec);
        snprintf(nm, sizeof(nm), "%s_destroy_plan", pre[p]);     bind(nm, an_destroy);
    }
}
