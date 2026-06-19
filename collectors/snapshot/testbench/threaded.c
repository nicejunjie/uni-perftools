/* Multithreaded compute: N pthreads each spinning on FP work.
 * Expected snapshot: CPU utilization ~N cores, thread peak ~N+1. */
#include <stdio.h>
#include <stdlib.h>
#include <pthread.h>

static long g_iters;

static void *worker(void *arg) {
    (void)arg;
    double acc = 1.0;
    for (long i = 0; i < g_iters; i++) {
        acc = acc * 1.0000000001 + 0.5;
        acc -= 0.5;
    }
    /* Prevent the loop from being optimized away. */
    volatile double sink = acc; (void)sink;
    return NULL;
}

int main(int argc, char **argv) {
    int nthreads = (argc > 1) ? atoi(argv[1]) : 4;
    g_iters = (argc > 2) ? atol(argv[2]) : 400000000L;

    pthread_t *t = malloc(sizeof(pthread_t) * nthreads);
    for (int i = 0; i < nthreads; i++) pthread_create(&t[i], NULL, worker, NULL);
    for (int i = 0; i < nthreads; i++) pthread_join(t[i], NULL);
    free(t);
    printf("threaded: %d threads done\n", nthreads);
    return 0;
}
