#define _POSIX_C_SOURCE 200809L
#include "timer.h"
#include <time.h>
#include <stdlib.h>

double libprof_overhead = 0.0;

double libprof_now(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
}

/* Measure the floor cost of two back-to-back clock reads (the irreducible
 * overhead bracketing every wrapped call) and take the median. We deliberately
 * measure only the timer pair, not the full enter/exit, because the store
 * update is attributed to the runtime, not to the kernel being timed. */
static int cmp_double(const void *a, const void *b)
{
    double da = *(const double *)a, db = *(const double *)b;
    return (da > db) - (da < db);
}

void libprof_timer_calibrate(void)
{
    enum { N = 256 };
    double samples[N];
    for (int i = 0; i < N; i++) {
        double a = libprof_now();
        double b = libprof_now();
        samples[i] = b - a;
    }
    qsort(samples, N, sizeof(double), cmp_double);
    libprof_overhead = samples[N / 2];
}
