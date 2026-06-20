/* Empirical roofline ceilings (ERT-style): measure achievable peak FP throughput
 * and memory bandwidth on this host. Shared by both collectors — the snapshot's
 * whole-program point and the profile's per-kernel points plot against these.
 *
 * Prints one JSON line: {"peak_gflops":..,"peak_bw_gbs":..,"cpu":"..."}
 * Build with -O3 -march=native -ffast-math -fopenmp.
 */
#define _POSIX_C_SOURCE 200809L
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#ifdef _OPENMP
#include <omp.h>
#endif

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec * 1e-9;
}

/* Peak FP: many independent FMA chains kept in registers (hide latency), summed
 * so the compiler can't elide them. flops counted as 2 per FMA. */
static double peak_gflops(void)
{
    const long iters = 200000000L;
    double best = 0.0;
#ifdef _OPENMP
    #pragma omp parallel reduction(max:best)
#endif
    {
        double a0 = 0.1, a1 = 0.2, a2 = 0.3, a3 = 0.4, a4 = 0.5, a5 = 0.6, a6 = 0.7, a7 = 0.8;
        const double b = 1.0000001, c = 0.9999999;
        double t0 = now();
        for (long i = 0; i < iters; i++) {
            a0 = a0 * b + c; a1 = a1 * b + c; a2 = a2 * b + c; a3 = a3 * b + c;
            a4 = a4 * b + c; a5 = a5 * b + c; a6 = a6 * b + c; a7 = a7 * b + c;
        }
        double dt = now() - t0;
        volatile double sink = a0 + a1 + a2 + a3 + a4 + a5 + a6 + a7;
        (void)sink;
        double gf = (2.0 * 8.0 * iters) / dt / 1e9;   /* 8 FMA chains * 2 flops */
#ifdef _OPENMP
        #pragma omp critical
#endif
        { if (gf > best) best = gf; }
    }
    /* scale single-thread chain throughput by core count for an aggregate ceiling */
#ifdef _OPENMP
    return best * omp_get_max_threads();
#else
    return best;
#endif
}

/* Peak bandwidth: STREAM triad on arrays far larger than LLC. 24 bytes/iter
 * (2 reads + 1 write). */
static double peak_bw_gbs(void)
{
    const long n = 1L << 25;          /* 32M doubles * 3 arrays = 768 MiB */
    double *a = malloc(n * sizeof(double));
    double *b = malloc(n * sizeof(double));
    double *c = malloc(n * sizeof(double));
    if (!a || !b || !c) { free(a); free(b); free(c); return 0.0; }
#ifdef _OPENMP
    #pragma omp parallel for
#endif
    for (long i = 0; i < n; i++) { b[i] = i * 1e-9; c[i] = i * 2e-9; a[i] = 0; }
    const double s = 1.5;
    double best = 0.0;
    for (int rep = 0; rep < 5; rep++) {
        double t0 = now();
#ifdef _OPENMP
        #pragma omp parallel for
#endif
        for (long i = 0; i < n; i++) a[i] = b[i] + s * c[i];
        double dt = now() - t0;
        double gb = (3.0 * sizeof(double) * n) / dt / 1e9;
        if (gb > best) best = gb;
        if (a[rep] < 0) printf("%f", a[rep]);   /* defeat DCE */
    }
    free(a); free(b); free(c);
    return best;
}

static void cpu_model(char *out, size_t n)
{
    out[0] = 0;
    FILE *f = fopen("/proc/cpuinfo", "r");
    if (!f) return;
    char line[512];
    while (fgets(line, sizeof(line), f)) {
        if (strncmp(line, "model name", 10) == 0) {
            char *p = strchr(line, ':');
            if (p) { p += 2; p[strcspn(p, "\n")] = 0; snprintf(out, n, "%s", p); }
            break;
        }
    }
    fclose(f);
}

int main(void)
{
    double gf = peak_gflops();
    double bw = peak_bw_gbs();
    char cpu[256];
    cpu_model(cpu, sizeof(cpu));
    printf("{\"peak_gflops\": %.1f, \"peak_bw_gbs\": %.1f, \"cpu\": \"%s\"}\n", gf, bw, cpu);
    return 0;
}
