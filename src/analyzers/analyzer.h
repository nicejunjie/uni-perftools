#ifndef LIBPROF_ANALYZER_H
#define LIBPROF_ANALYZER_H

#include "libprof.h"

/* Wire analyzer callbacks into libprof_desc[] entries. Called from
 * libprof_init via a weak hook, so a build without analyzers still links. */
void libprof_register_analyzers(void);

/* Build a shaped key (interned in the calling thread's arena) when
 * SCILIB_SHAPE is enabled. Returns 1 if a key was produced, else 0. */
int libprof_make_shape(libprof_key_t *k, const libprof_desc_t *d, const char *fmt, ...);

#endif
