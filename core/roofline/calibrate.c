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

/* Peak FP: a vectorizable FMA over an L1-resident array (x[i]=x[i]*b+c), many
 * reps; b<1 keeps values bounded. Independent across i → the compiler emits
 * packed FMA (AVX/AVX-512); aggregate flops/wall over all threads. */
static double g_sink = 0.0;
static double peak_gflops(void)
{
    enum { N = 2048 };                          /* 16 KiB — L1-resident */
    const long reps = 4000000L;
    int nth = 1;
#ifdef _OPENMP
    nth = omp_get_max_threads();
#endif
    double best = 0.0;
    for (int rep = 0; rep < 3; rep++) {
        double t0 = now();
#ifdef _OPENMP
        #pragma omp parallel
#endif
        {
            double x[N];
            for (int i = 0; i < N; i++) x[i] = 0.001 * i + 0.1;
            const double b = 0.99999, c = 1.0;
            for (long r = 0; r < reps; r++) {
                #pragma omp simd
                for (int i = 0; i < N; i++) x[i] = x[i] * b + c;
            }
            double s = 0.0;
            for (int i = 0; i < N; i++) s += x[i];
#ifdef _OPENMP
            #pragma omp atomic
#endif
            g_sink += s;                        /* defeat DCE */
        }
        double dt = now() - t0;
        double gf = (2.0 * (double)N * reps * nth) / dt / 1e9;
        if (gf > best) best = gf;
    }
    return best;
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
    /* 32 bytes/iter: read b, read c, write a + write-allocate (read-for-ownership)
     * of a — the actual DRAM traffic a normal (non-NT) store stream incurs. */
    for (int rep = 0; rep < 8; rep++) {
        double t0 = now();
#ifdef _OPENMP
        #pragma omp parallel for schedule(static)
#endif
        for (long i = 0; i < n; i++) a[i] = b[i] + s * c[i];
        double dt = now() - t0;
        double gb = (4.0 * sizeof(double) * n) / dt / 1e9;
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
