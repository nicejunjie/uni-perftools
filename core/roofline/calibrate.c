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
#include <unistd.h>
#ifdef _OPENMP
#include <omp.h>
#endif

/* Non-temporal (streaming) stores bypass the cache, so the triad writes a[]
 * straight to DRAM with no read-for-ownership. That makes the real traffic
 * 24 B/iter (2 reads + 1 streaming write) and yields the true sustained DRAM
 * bandwidth (what STREAM reports on x86). Without NT stores a normal store
 * incurs a write-allocate read → 32 B/iter. */
#if defined(__AVX512F__) || defined(__AVX__)
#include <immintrin.h>
#define TRIAD_NT 1
#define TRIAD_BYTES 24.0
#else
#define TRIAD_NT 0
#define TRIAD_BYTES 32.0
#endif

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec * 1e-9;
}

/* triad: a = b + s*c, with non-temporal stores where the ISA supports them. */
static void triad(double *restrict a, const double *restrict b,
                  const double *restrict c, double s, long n)
{
#if TRIAD_NT
#ifdef _OPENMP
    #pragma omp parallel
#endif
    {
#if defined(__AVX512F__)
        __m512d vs = _mm512_set1_pd(s);
#ifdef _OPENMP
        #pragma omp for schedule(static) nowait
#endif
        for (long i = 0; i < n; i += 8)
            _mm512_stream_pd(&a[i], _mm512_fmadd_pd(vs, _mm512_loadu_pd(&c[i]),
                                                    _mm512_loadu_pd(&b[i])));
#else  /* __AVX__ */
        __m256d vs = _mm256_set1_pd(s);
#ifdef _OPENMP
        #pragma omp for schedule(static) nowait
#endif
        for (long i = 0; i < n; i += 4) {
            __m256d v = _mm256_add_pd(_mm256_loadu_pd(&b[i]),
                                      _mm256_mul_pd(vs, _mm256_loadu_pd(&c[i])));
            _mm256_stream_pd(&a[i], v);
        }
#endif
        _mm_sfence();              /* make NT stores globally visible before join */
    }
#else
#ifdef _OPENMP
    #pragma omp parallel for schedule(static)
#endif
    for (long i = 0; i < n; i++) a[i] = b[i] + s * c[i];
#endif
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

/* Last-level cache size in bytes (for sizing the triad arrays out of cache). */
static long llc_bytes(void)
{
    long s = 0;
#ifdef _SC_LEVEL3_CACHE_SIZE
    s = sysconf(_SC_LEVEL3_CACHE_SIZE);
#endif
#ifdef _SC_LEVEL2_CACHE_SIZE
    if (s <= 0) { long s2 = sysconf(_SC_LEVEL2_CACHE_SIZE); if (s2 > 0) s = s2 * 16; }
#endif
    return s > 0 ? s : (32L << 20);   /* fall back to 32 MiB */
}

/* Peak bandwidth: STREAM triad on arrays far larger than LLC, with NT stores.
 * TRIAD_BYTES/iter (24 with NT, 32 otherwise). Array size is auto-derived from
 * the detected LLC so the working set always spills to DRAM on any machine. */
static double peak_bw_gbs(void)
{
    /* each array >= 8x LLC (>=256 MiB), so 3 arrays clearly exceed any cache */
    long per_array = 8 * llc_bytes();
    if (per_array < (256L << 20)) per_array = 256L << 20;
    long n = per_array / (long)sizeof(double);
    n &= ~7L;                          /* multiple of 8 doubles for AVX-512 NT loop */
    double *a = aligned_alloc(64, n * sizeof(double));   /* 64B align for NT stores */
    double *b = aligned_alloc(64, n * sizeof(double));
    double *c = aligned_alloc(64, n * sizeof(double));
    if (!a || !b || !c) { free(a); free(b); free(c); return 0.0; }
    /* first-touch with the SAME static schedule the triad uses → NUMA-local pages */
#ifdef _OPENMP
    #pragma omp parallel for schedule(static)
#endif
    for (long i = 0; i < n; i++) { b[i] = i * 1e-9; c[i] = i * 2e-9; a[i] = 0; }
    const double s = 1.5;
    /* warmup (untimed): ramp clocks + ensure pages are resident. STREAM
     * likewise discards the first iterations. */
    for (int w = 0; w < 3; w++) triad(a, b, c, s, n);
    double best = 0.0;
    for (int rep = 0; rep < 10; rep++) {
        double t0 = now();
        triad(a, b, c, s, n);
        double dt = now() - t0;
        double gb = (TRIAD_BYTES * n) / dt / 1e9;
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
