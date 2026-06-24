#include "libprof.h"
#include "config.h"
#include "report.h"
#include "util.h"
#include <stdlib.h>
#include <pthread.h>
#include <signal.h>

__attribute__((weak)) void libprof_register_analyzers(void);
__attribute__((weak)) void libprof_sample_init(void);
__attribute__((weak)) void libprof_sample_stop_all(void);
__attribute__((weak)) void libprof_roofline_init(void);
__attribute__((weak)) void libprof_roofline_stop_all(void);
__attribute__((weak)) void libprof_heap_init(void);

volatile int libprof_shutdown = 0;
static double apptime;

/* Set in a forked child. The child inherits this process's rank (from the
 * launcher env), so its destructor would write the SAME prof.<rank>.json and
 * clobber the parent's profile. fork()+exec() is safe (exec discards this .so),
 * but a fork-WITHOUT-exec child (e.g. a solver shelling out, Python
 * multiprocessing) runs our destructor — so we suppress its report write rather
 * than overwrite the real rank's data. */
static volatile sig_atomic_t libprof_in_forked_child = 0;
static void libprof_atfork_child(void) { libprof_in_forked_child = 1; }

void libprof_init(void)
{
    libprof_config_parse();
    libprof_timer_calibrate();
    pthread_atfork(NULL, NULL, libprof_atfork_child);
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
    if (libprof_in_forked_child) {
        /* Don't clobber the parent's rank file; also leave the inherited timers
         * alone (the child has no live sampler timer). */
        libprof_shutdown = 1;
        return;
    }
    if (libprof_sample_stop_all) libprof_sample_stop_all();  /* quiesce handler */
    if (libprof_roofline_stop_all) libprof_roofline_stop_all();
    libprof_shutdown = 1;        /* stop tracing our own report-writing I/O */
    apptime += libprof_now();

    libprof_row_t *rows = NULL;
    int n = libprof_collect_local(&rows);
    libprof_write_raw(rows, n, apptime);
    free(rows);
}
