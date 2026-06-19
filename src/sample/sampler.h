#ifndef LIBPROF_SAMPLER_H
#define LIBPROF_SAMPLER_H

/* Statistical sampling profiler (CrayPAT/gprof-style). A per-thread timer fires
 * a signal; the handler records the interrupted PC into a per-thread histogram.
 * Symbolization (PC -> function/source line) is deferred to the postprocess
 * tool, so the hot path stays tiny and signal-safe. */

int  libprof_sample_enabled(void);   /* SCILIB_SAMPLE!=0 and setup succeeded */
int  libprof_sample_hz(void);
void libprof_sample_init(void);      /* install handler + arm the main thread */
void libprof_sample_stop_all(void);  /* disarm globally before emit */
void libprof_sample_thread_start(void);  /* arm the calling thread */
void libprof_sample_thread_stop(void);   /* disarm the calling thread */

#endif
