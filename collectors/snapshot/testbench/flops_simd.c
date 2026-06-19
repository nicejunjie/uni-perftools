/* Vectorizable FP workload: repeated fused multiply-add over arrays. Built
 * with -O3 -march=native -ffast-math so the compiler emits packed AVX FMAs.
 * Expected snapshot: high GFLOPS, and on Intel a high vectorization %.
 * 2 FLOPs per element per pass (one multiply + one add / FMA). */
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    size_t n = (argc > 1) ? (size_t)atol(argv[1]) : 4096;     /* fits in cache */
    long passes = (argc > 2) ? atol(argv[2]) : 2000000;
    double *x = malloc(n * sizeof(double));
    double *y = malloc(n * sizeof(double));
    if (!x || !y) { perror("malloc"); return 1; }
    for (size_t i = 0; i < n; i++) { x[i] = 1.0000001; y[i] = 0.5; }

    double a = 1.0000001;
    for (long p = 0; p < passes; p++) {
        for (size_t i = 0; i < n; i++) {
            y[i] = a * x[i] + y[i];
        }
        a += 1e-12; /* keep passes from being collapsed */
    }

    double sum = 0.0;
    for (size_t i = 0; i < n; i++) sum += y[i];
    printf("flops_simd: n=%zu passes=%ld sum=%g\n", n, passes, sum);
    free(x); free(y);
    return 0;
}
