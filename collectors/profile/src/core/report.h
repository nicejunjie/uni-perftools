#ifndef LIBPROF_ROWS_H
#define LIBPROF_ROWS_H

#include <stdint.h>

/* One merged row for THIS process (all its threads). Analysis across ranks is
 * done by the postprocess tool, not here. */
typedef struct {
    char     name[96];      /* incl. shape, e.g. "fftw_execute[1024]" */
    char     group[16];
    uint64_t count;
    double   t_incl, t_excl;
    uint64_t bytes;
} libprof_row_t;

/* Merge all per-thread stores of this process into rows (caller frees *out). */
int  libprof_collect_local(libprof_row_t **out);

/* Write this process's raw profile to <prefix>.<rank>.json (no analysis). */
void libprof_write_raw(libprof_row_t *rows, int n, double apptime);

#endif
