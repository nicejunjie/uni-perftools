#include "libprof.h"
#include "config.h"
#include "report.h"
#include "util.h"
#include <stdlib.h>

__attribute__((weak)) void libprof_register_analyzers(void);
__attribute__((weak)) void libprof_sample_init(void);
__attribute__((weak)) void libprof_sample_stop_all(void);
__attribute__((weak)) void libprof_roofline_init(void);
__attribute__((weak)) void libprof_roofline_stop_all(void);
__attribute__((weak)) void libprof_heap_init(void);

volatile int libprof_shutdown = 0;
static double apptime;

void libprof_init(void)
{
    libprof_config_parse();
    libprof_timer_calibrate();
    if (libprof_register_analyzers) libprof_register_analyzers();
    if (libprof_sample_init) libprof_sample_init();
    if (libprof_roofline_init) libprof_roofline_init();
    if (libprof_heap_init) libprof_heap_init();
    apptime = -libprof_now();
}

void libprof_finalize(void)
{
    static int done = 0;
    if (done) return;            /* destructor; guard against double finalize */
    done = 1;
    if (libprof_sample_stop_all) libprof_sample_stop_all();  /* quiesce handler */
    if (libprof_roofline_stop_all) libprof_roofline_stop_all();
    libprof_shutdown = 1;        /* stop tracing our own report-writing I/O */
    apptime += libprof_now();

    libprof_row_t *rows = NULL;
    int n = libprof_collect_local(&rows);
    libprof_write_raw(rows, n, apptime);
    free(rows);
}
