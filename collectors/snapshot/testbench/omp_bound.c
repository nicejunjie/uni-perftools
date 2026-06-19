/* OpenMP workload with a deliberate load imbalance: thread 0 does far more
 * work than the rest, so an APS-style report should flag OpenMP imbalance.
 * Build needs -fopenmp. */
#include <stdio.h>
#include <stdlib.h>
#include <omp.h>

int main(int argc, char **argv) {
    long base = (argc > 1) ? atol(argv[1]) : 100000000L;
    double total = 0.0;

    #pragma omp parallel reduction(+:total)
    {
        int tid = omp_get_thread_num();
        /* Imbalance: thread 0 iterates 4x the others. */
        long iters = base * (tid == 0 ? 4 : 1);
        double acc = 1.0;
        for (long i = 0; i < iters; i++) {
            acc = acc * 1.0000000001 + 0.5;
            acc -= 0.5;
        }
        total += acc;
    }
    printf("omp_bound: total=%f threads=%d\n", total, omp_get_max_threads());
    return 0;
}
