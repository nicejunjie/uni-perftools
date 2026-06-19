/* Compute-bound: tight FP loop, tiny memory footprint, no I/O.
 * Expected snapshot: high IPC, low cache-miss rate, ~1 core. */
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    long iters = (argc > 1) ? atol(argv[1]) : 800000000L;
    double acc = 1.0;
    for (long i = 0; i < iters; i++) {
        acc = acc * 1.0000000001 + 0.5;
        acc -= 0.5;
    }
    printf("compute_bound: acc=%f\n", acc);
    return 0;
}
