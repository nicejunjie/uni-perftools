#ifndef LIBPROF_TIMER_H
#define LIBPROF_TIMER_H

/* Monotonic, nanosecond-resolution wall clock, returned as seconds.
 * Replaces the old gettimeofday() microsecond timer whose quantization
 * dominated per-call measurements of sub-microsecond kernels. */
double libprof_now(void);

/* Estimated per-call wrapper/timer overhead in seconds, measured once at
 * init by libprof_timer_calibrate(). Subtracted from each measured sample. */
extern double libprof_overhead;
void libprof_timer_calibrate(void);

#endif
