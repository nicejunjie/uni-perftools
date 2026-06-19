#ifndef LIBPROF_WRAP_H
#define LIBPROF_WRAP_H

#include <complex.h>
#include <stdbool.h>
#include "libprof.h"

/* Fortran-dialect argument types used in the prototype files. libprof_int_t
 * tracks the build's Fortran INTEGER width (see lp_fint in libprof.h). */
#define libprof_int_t      lp_fint
#define libprof_fcomplex_t float complex
#define libprof_dcomplex_t double complex

/* FFTW opaque/value types (we avoid depending on fftw3.h being installed). */
typedef void *fftw_plan;
typedef void *fftwf_plan;
typedef void *fftwl_plan;
typedef double      fftw_complex[2];
typedef float       fftwf_complex[2];
typedef long double fftwl_complex[2];

/* CBLAS / LAPACKE enums are int-sized; we forward verbatim, so int matches the
 * ABI without depending on cblas.h / lapacke.h being installed. */
typedef int CBLAS_LAYOUT;
typedef int CBLAS_TRANSPOSE;
typedef int CBLAS_UPLO;
typedef int CBLAS_SIDE;
typedef int CBLAS_DIAG;
typedef lp_fint lapack_int;   /* tracks ILP64 width for LAPACKe wrapper ABI */

/* One wrapper body, two symbol-naming schemes selected at compile time:
 *   preload build  -> WRAP(dgemm_)  == dgemm_              (exported, interposes)
 *   frida build    -> WRAP(dgemm_)  == libprof_dbi_dgemm_  (private, replaced in place)
 * The frida .so must NOT export the real name, or it would also interpose. */
#ifdef LIBPROF_BACKEND_FRIDA
#define WRAP(sym) libprof_dbi_##sym
#else
#define WRAP(sym) sym
#endif

#endif
