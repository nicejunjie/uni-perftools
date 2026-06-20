/* BLAS shape analyzers: when UPAT_SHAPE=1 they split rows by problem size
 * (e.g. dgemm_[m=400,n=400,k=400]). They carry no flop model - flop counting
 * was dropped as it is only exact for a tiny subset of functions. These are
 * bound only when UPAT_SHAPE is on, so the default hot path is untouched. */
#include "analyzer.h"
#include "config.h"
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

int libprof_make_shape(libprof_key_t *k, const libprof_desc_t *d, const char *fmt, ...)
{
    if (!libprof_cfg.shape) return 0;
    char buf[64];
    va_list ap; va_start(ap, fmt);
    int len = vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);
    if (len < 0) return 0;
    if (len >= (int)sizeof(buf)) len = sizeof(buf) - 1;
    const char *s = libprof_intern(libprof_tls, buf, (uint16_t)len);
    if (!s) return 0;
    k->slot = d->slot;
    k->shape = s;
    k->shape_len = (uint16_t)len;
    k->shape_hash = libprof_fnv1a(s, len);
    return 1;
}

#define I(a, i)  (*(lp_fint *)((a)[i]))  /* Fortran INTEGER (int or int64, ILP64) */
#define IC(a, i) (*(int *)((a)[i]))       /* CBLAS uses plain int dims */

/* Fortran BLAS (all args by pointer). gemm:(ta,tb,m,n,k,..) gemv:(t,m,n,..)
 * syrk:(uplo,trans,n,k,..) trsm:(side,uplo,ta,diag,m,n,..) */
static int an_gemm(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)md;(void)r; return libprof_make_shape(k, d, "m=%ld,n=%ld,k=%ld", (long)I(a,2), (long)I(a,3), (long)I(a,4)); }
static int an_gemv(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)md;(void)r; return libprof_make_shape(k, d, "m=%ld,n=%ld", (long)I(a,1), (long)I(a,2)); }
static int an_syrk(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)md;(void)r; return libprof_make_shape(k, d, "n=%ld,k=%ld", (long)I(a,2), (long)I(a,3)); }
static int an_trsm(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)md;(void)r; return libprof_make_shape(k, d, "m=%ld,n=%ld", (long)I(a,4), (long)I(a,5)); }

/* CBLAS (by value, arrive by address): leading layout/trans shift dimensions;
 * CBLAS dimensions are plain int even in ILP64 builds. */
static int an_cblas_gemm(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)md;(void)r; return libprof_make_shape(k, d, "m=%d,n=%d,k=%d", IC(a,3), IC(a,4), IC(a,5)); }
static int an_cblas_gemv(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)md;(void)r; return libprof_make_shape(k, d, "m=%d,n=%d", IC(a,2), IC(a,3)); }

static void bind(const char *name, libprof_analyzer_fn fn)
{
    for (int i = 0; i < LIBPROF_NSLOTS; i++)
        if (strcmp(libprof_desc[i].name, name) == 0) { libprof_desc[i].analyze = fn; return; }
}

void libprof_register_analyzers(void)
{
    /* per-shape BLAS rows only when requested (keeps the default hot path clean) */
    if (libprof_cfg.shape) {
        static const char types[] = "sdcz";
        char nm[16];
        struct { const char *base; libprof_analyzer_fn fn; } tbl[] = {
            {"gemm_", an_gemm}, {"gemv_", an_gemv}, {"syrk_", an_syrk}, {"trsm_", an_trsm},
        };
        for (size_t t = 0; t < sizeof(types) - 1; t++)
            for (size_t j = 0; j < sizeof(tbl) / sizeof(tbl[0]); j++) {
                snprintf(nm, sizeof(nm), "%c%s", types[t], tbl[j].base);
                bind(nm, tbl[j].fn);
            }
        const char *cg[] = {"cblas_sgemm", "cblas_dgemm", "cblas_cgemm", "cblas_zgemm"};
        for (size_t j = 0; j < 4; j++) bind(cg[j], an_cblas_gemm);
        bind("cblas_sgemv", an_cblas_gemv);
        bind("cblas_dgemv", an_cblas_gemv);
    }

    /* MPI (byte volume) and FFTW (size attribution) are always meaningful. */
    extern void libprof_register_mpi_analyzers(void) __attribute__((weak));
    if (libprof_register_mpi_analyzers) libprof_register_mpi_analyzers();
    extern void libprof_register_fftw_analyzers(void) __attribute__((weak));
    if (libprof_register_fftw_analyzers) libprof_register_fftw_analyzers();
    extern void libprof_register_io_analyzers(void) __attribute__((weak));
    if (libprof_register_io_analyzers) libprof_register_io_analyzers();
}
