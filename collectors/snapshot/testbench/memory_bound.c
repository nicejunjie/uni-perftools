/* Memory-bound: pointer-chase over a buffer far larger than cache, so most
 * accesses miss to DRAM. Expected snapshot: low IPC, high cache-miss rate. */
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    size_t mb = (argc > 1) ? (size_t)atol(argv[1]) : 512;
    size_t n = mb * 1024 * 1024 / sizeof(size_t);
    size_t *a = malloc(n * sizeof(size_t));
    if (!a) { perror("malloc"); return 1; }

    /* Build a random-ish permutation cycle to defeat the prefetcher. */
    for (size_t i = 0; i < n; i++) a[i] = i;
    for (size_t i = n - 1; i > 0; i--) {
        size_t j = (i * 2654435761u) % (i + 1);
        size_t t = a[i]; a[i] = a[j]; a[j] = t;
    }

    size_t idx = 0, sum = 0;
    for (long step = 0; step < 200000000L; step++) {
        idx = a[idx];
        sum += idx;
    }
    printf("memory_bound: sum=%zu\n", sum);
    free(a);
    return 0;
}
