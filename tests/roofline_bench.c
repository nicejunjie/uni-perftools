/* Dependency-free roofline workload — exercises the per-function event sampler on
 * any platform (no BLAS/MPI needed). Two clearly-separated hot functions:
 *
 *   compute_bound()  small in-cache array, many FMAs per element  → high AI,
 *                    lots of FP-event samples, ~no DRAM-access samples.
 *   memory_bound()   array >> last-level cache, streamed once per pass → low AI,
 *                    lots of DRAM-access samples, few FP samples.
 *
 * Build:  cc -O2 -march=native -o roofline_bench tests/roofline_bench.c -lm
 * Run:    ./roofline_bench [mib] [iters]
 */
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

/* keep these out-of-line so the sampler attributes PCs to named functions */
__attribute__((noinline))
double compute_bound(double *a, int n, int iters)
{
    double acc = 0.0;
    for (int it = 0; it < iters; it++)
        for (int i = 0; i < n; i++) {
            double x = a[i];
            /* a chain of FMAs — pure FP work on cache-resident data */
            x = x * 1.0000001 + 0.5;
            x = x * 0.9999999 + 0.25;
            x = x * 1.0000002 + 0.125;
            x = x * 0.9999998 + 0.0625;
            acc += x;
        }
    return acc;
}

__attribute__((noinline))
double memory_bound(double *big, size_t n, int passes)
{
    double acc = 0.0;
    for (int p = 0; p < passes; p++)
        for (size_t i = 0; i < n; i++)
            acc += big[i];          /* streamed read, working set >> LLC */
    return acc;
}

int main(int argc, char **argv)
{
    size_t mib = (argc > 1) ? (size_t)atoi(argv[1]) : 512;   /* big array size */
    int iters  = (argc > 2) ? atoi(argv[2]) : 1500;          /* compute repetitions */

    size_t big_n = mib * 1024 * 1024 / sizeof(double);
    double *big = malloc(big_n * sizeof(double));
    int small_n = 4096;                                       /* ~32 KiB, L1/L2-resident */
    double *small = malloc(small_n * sizeof(double));
    if (!big || !small) return 2;
    for (size_t i = 0; i < big_n; i++) big[i] = (double)(i & 255) * 0.5;
    for (int i = 0; i < small_n; i++) small[i] = (double)(i & 63) * 0.25;

    double s = 0.0;
    s += compute_bound(small, small_n, iters * 200);
    s += memory_bound(big, big_n, 12);
    return (s > 1e300);
}
