/* Empirical roofline ceilings (ERT-style): measure achievable peak FP throughput
 * and memory bandwidth on this host. Shared by both collectors — the snapshot's
 * whole-program point and the profile's per-kernel points plot against these.
 *
 * Self-tuning: the driver (roofline.py) compiles this with `-DFMA_ACC=N` for a
 * few N and the host-tuned flags (-mcpu=native on aarch64, -march=native on x86),
 * runs each in "compute" mode, and keeps the best — so no platform needs hand-
 * tuning. argv[1] = "compute" | "bw" | (default) both.
 *
 * Prints one JSON line: {"peak_gflops":..,"peak_bw_gbs":..,"fma_acc":N,"cpu":".."}
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
 * bandwidth (what STREAM reports). Without NT stores a normal store incurs a
 * write-allocate read → 32 B/iter. NT instructions are ISA-specific:
 *   x86  → _mm{256,512}_stream_pd      (AVX / AVX-512)
 *   ARM  → STNP (store-pair non-temporal) over NEON float64x2
 * both compile under gcc and clang; anything else falls back to scalar. */
#if (defined(__x86_64__) || defined(__i386__)) && (defined(__AVX512F__) || defined(__AVX__))
#include <immintrin.h>
#define NT_X86 1
#define TRIAD_BYTES 24.0
#elif defined(__aarch64__)
#include <arm_neon.h>
#define NT_ARM 1
#define TRIAD_BYTES 24.0
#else
#define TRIAD_BYTES 32.0
#endif

/* ---- register-resident FMA kernel (peak compute) -------------------------
 * The accumulators live in vector registers (no array load/store in the hot
 * loop) and run FMA_ACC INDEPENDENT FMA chains, so the loop saturates the core's
 * FMA units instead of the L1 load/store ports — the true achievable FLOP peak.
 * Vxx_FMA(a,b,c) computes a*b + c.
 *
 * Platform coverage & LIMITATIONS (compute peak):
 *   - x86 AVX-512  (Intel Xeon SP / recent client, AMD Zen4/5): 8 DP / 16 SP
 *     lanes. Built and RUN on AMD Zen5.
 *   - x86 AVX2+FMA (Intel Haswell+, AMD Zen1-3): 4 DP / 8 SP lanes. Compile-
 *     checked only (identical kernel, different width) — not run on that HW.
 *   - ARM NEON (any aarch64): FIXED 128-bit = 2 DP / 4 SP lanes. RUN on a
 *     dual-socket NVIDIA Grace (Neoverse V2, 144 cores; Vista node i618-112):
 *     ~2960 GFLOP/s DP and ~1.15 TB/s — in line with published Grace DGEMM
 *     (~3 TFLOPS) and its LPDDR5X bandwidth. IMPORTANT: it must be built with
 *     `-mcpu=native` (the tuning/scheduling), NOT `-march=native` alone, which
 *     left the FMA peak ~30% low and noisy (roofline.py's _build does this).
 *     FMA_ACC=16 is the sweet spot — more accumulators spill past the 32 NEON
 *     registers and REGRESS. Caveat: SVE on V2 is 128-bit so NEON loses nothing
 *     here, but NEON WILL UNDER-REPORT on wide-SVE parts (e.g. A64FX 512-bit
 *     SVE), which need an SVE (vector-length-agnostic) path added here. Whether
 *     the kernel fully saturates V2's FP pipes vs. a tuned SVE DGEMM is unverified.
 *   - scalar fallback (no SIMD intrinsics): leans on compiler auto-vectorization,
 *     so it is functional but the least precise and may under-report the peak.
 * The bandwidth triad (below) has matching x86 NT-store / ARM STNP / scalar paths. */
#if defined(__AVX512F__)
#define VDP __m512d
#define VDP_LANES 8
#define VDP_SET _mm512_set1_pd
#define VDP_FMA _mm512_fmadd_pd
#define VDP_SUM _mm512_reduce_add_pd
#define VSP __m512
#define VSP_LANES 16
#define VSP_SET _mm512_set1_ps
#define VSP_FMA _mm512_fmadd_ps
#define VSP_SUM _mm512_reduce_add_ps
#define HAVE_FMA_KERNEL 1
#elif defined(__AVX__) && defined(__FMA__)
static inline double hsum256d(__m256d v) {
    __m128d lo = _mm256_castpd256_pd128(v), hi = _mm256_extractf128_pd(v, 1);
    lo = _mm_add_pd(lo, hi);
    return _mm_cvtsd_f64(_mm_add_sd(lo, _mm_unpackhi_pd(lo, lo)));
}
static inline float hsum256s(__m256 v) {
    __m128 lo = _mm256_castps256_ps128(v), hi = _mm256_extractf128_ps(v, 1);
    lo = _mm_add_ps(lo, hi);
    lo = _mm_add_ps(lo, _mm_movehl_ps(lo, lo));
    return _mm_cvtss_f32(_mm_add_ss(lo, _mm_shuffle_ps(lo, lo, 1)));
}
#define VDP __m256d
#define VDP_LANES 4
#define VDP_SET _mm256_set1_pd
#define VDP_FMA _mm256_fmadd_pd
#define VDP_SUM hsum256d
#define VSP __m256
#define VSP_LANES 8
#define VSP_SET _mm256_set1_ps
#define VSP_FMA _mm256_fmadd_ps
#define VSP_SUM hsum256s
#define HAVE_FMA_KERNEL 1
#elif defined(__aarch64__)
#define VDP float64x2_t
#define VDP_LANES 2
#define VDP_SET vdupq_n_f64
#define VDP_FMA(a, b, c) vfmaq_f64(c, a, b)     /* c + a*b = a*b + c */
#define VDP_SUM vaddvq_f64
#define VSP float32x4_t
#define VSP_LANES 4
#define VSP_SET vdupq_n_f32
#define VSP_FMA(a, b, c) vfmaq_f32(c, a, b)
#define VSP_SUM vaddvq_f32
#define HAVE_FMA_KERNEL 1
#endif

/* Independent FMA chains. Enough chains hide the FMA latency across the FP units,
 * but the sweet spot is part-dependent: it must fit the VECTOR REGISTER FILE or
 * spills cause a sharp regression. x86 AVX2/SSE have 16 vector regs → ~12 is best
 * and 16 already falls off a cliff; AVX-512 and ARM (NEON/SVE) have 32 → ~16-24.
 * roofline.py sweeps -DFMA_ACC across that range and keeps the best, so no
 * platform needs hand-tuning. Overridable on the command line; 16 is the default. */
#ifndef FMA_ACC
#define FMA_ACC 16
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
#if defined(NT_X86)
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
#elif defined(NT_ARM)
#ifdef _OPENMP
    #pragma omp parallel
#endif
    {
        /* two float64x2 (= 4 doubles) per STNP, the NEON non-temporal store-pair */
#ifdef _OPENMP
        #pragma omp for schedule(static) nowait
#endif
        for (long i = 0; i < n; i += 4) {
            float64x2_t a0 = vfmaq_n_f64(vld1q_f64(&b[i]),     vld1q_f64(&c[i]),     s);
            float64x2_t a1 = vfmaq_n_f64(vld1q_f64(&b[i + 2]), vld1q_f64(&c[i + 2]), s);
            __asm__ volatile("stnp %q[v0], %q[v1], [%[p]]"
                             :: [v0] "w"(a0), [v1] "w"(a1), [p] "r"(&a[i]) : "memory");
        }
        __asm__ volatile("dsb st" ::: "memory");   /* drain NT stores before join */
    }
#else
#ifdef _OPENMP
    #pragma omp parallel for schedule(static)
#endif
    for (long i = 0; i < n; i++) a[i] = b[i] + s * c[i];
#endif
}

/* Peak FP: FMA_ACC independent FMA chains on register-resident vector
 * accumulators (a = a*b + c; b<1 keeps values bounded). No array load/store in
 * the hot loop, so it saturates the FMA units, not the L1 ports — the true
 * achievable peak. Aggregate flops/wall over all threads. Measured at BOTH
 * precisions: SP packs 2x the lanes of DP, so the FP32 peak is ~2x the FP64
 * peak — an SP-heavy app must be judged against the FP32 ceiling. */
static double g_sink = 0.0;

static double peak_gflops_dp(void)
{
    const long reps = 80000000L;
    int nth = 1;
#ifdef _OPENMP
    nth = omp_get_max_threads();
#endif
    double best = 0.0;
    for (int trial = 0; trial < 3; trial++) {
        double t0 = now();
#ifdef _OPENMP
        #pragma omp parallel
#endif
        {
#if defined(HAVE_FMA_KERNEL)
            VDP bb = VDP_SET(0.9999999), cc = VDP_SET(1e-7);
            VDP a[FMA_ACC];
            for (int j = 0; j < FMA_ACC; j++) a[j] = VDP_SET(1.0 + 0.001 * j);
            for (long r = 0; r < reps; r++) {
#if defined(__GNUC__)
                #pragma GCC unroll 64    /* fully unrolls any FMA_ACC<=64 (sweep uses <=32) */
#endif
                for (int j = 0; j < FMA_ACC; j++) a[j] = VDP_FMA(a[j], bb, cc);
            }
            double s = 0.0;
            for (int j = 0; j < FMA_ACC; j++) s += VDP_SUM(a[j]);
            double lanes = VDP_LANES;
#else   /* no FMA intrinsics: independent scalar accumulators, compiler-vectorized */
            double a[FMA_ACC];
            for (int j = 0; j < FMA_ACC; j++) a[j] = 1.0 + 0.001 * j;
            const double bb = 0.9999999, cc = 1e-7;
            for (long r = 0; r < reps; r++)
                #pragma omp simd
                for (int j = 0; j < FMA_ACC; j++) a[j] = a[j] * bb + cc;
            double s = 0.0;
            for (int j = 0; j < FMA_ACC; j++) s += a[j];
            double lanes = 1.0;
#endif
#ifdef _OPENMP
            #pragma omp atomic
#endif
            g_sink += s;                        /* defeat DCE */
            (void)lanes;
        }
        double dt = now() - t0;
        double lanes =
#if defined(HAVE_FMA_KERNEL)
            VDP_LANES;
#else
            1.0;
#endif
        double gf = ((double)FMA_ACC * lanes * 2.0 * (double)reps * nth) / dt / 1e9;
        if (gf > best) best = gf;
    }
    return best;
}

static double peak_gflops_sp(void)
{
    const long reps = 80000000L;
    int nth = 1;
#ifdef _OPENMP
    nth = omp_get_max_threads();
#endif
    double best = 0.0;
    for (int trial = 0; trial < 3; trial++) {
        double t0 = now();
#ifdef _OPENMP
        #pragma omp parallel
#endif
        {
#if defined(HAVE_FMA_KERNEL)
            VSP bb = VSP_SET(0.9999999f), cc = VSP_SET(1e-7f);
            VSP a[FMA_ACC];
            for (int j = 0; j < FMA_ACC; j++) a[j] = VSP_SET(1.0f + 0.001f * j);
            for (long r = 0; r < reps; r++) {
#if defined(__GNUC__)
                #pragma GCC unroll 64    /* fully unrolls any FMA_ACC<=64 (sweep uses <=32) */
#endif
                for (int j = 0; j < FMA_ACC; j++) a[j] = VSP_FMA(a[j], bb, cc);
            }
            float s = 0.0f;
            for (int j = 0; j < FMA_ACC; j++) s += VSP_SUM(a[j]);
#else
            float a[FMA_ACC];
            for (int j = 0; j < FMA_ACC; j++) a[j] = 1.0f + 0.001f * j;
            const float bb = 0.9999999f, cc = 1e-7f;
            for (long r = 0; r < reps; r++)
                #pragma omp simd
                for (int j = 0; j < FMA_ACC; j++) a[j] = a[j] * bb + cc;
            float s = 0.0f;
            for (int j = 0; j < FMA_ACC; j++) s += a[j];
#endif
#ifdef _OPENMP
            #pragma omp atomic
#endif
            g_sink += s;                        /* defeat DCE */
        }
        double dt = now() - t0;
        double lanes =
#if defined(HAVE_FMA_KERNEL)
            VSP_LANES;
#else
            1.0;
#endif
        double gf = ((double)FMA_ACC * lanes * 2.0 * (double)reps * nth) / dt / 1e9;
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

/* Mode (argv[1]): "compute" → FP peaks only, "bw" → bandwidth only, else both.
 * The driver (roofline.py) sweeps -DFMA_ACC and runs "compute" for each variant
 * (cheap), then "bw" once (accumulator-independent), keeping the best of each. */
int main(int argc, char **argv)
{
    const char *mode = argc > 1 ? argv[1] : "all";
    int do_c = strcmp(mode, "bw") != 0;
    int do_b = strcmp(mode, "compute") != 0;
    double gf_dp = do_c ? peak_gflops_dp() : 0.0;
    double gf_sp = do_c ? peak_gflops_sp() : 0.0;
    double bw = do_b ? peak_bw_gbs() : 0.0;
    char cpu[256];
    cpu_model(cpu, sizeof(cpu));
    /* "peak_gflops" kept as an alias for DP (backward compat). Unmeasured fields
     * are 0 and ignored by the driver. */
    printf("{\"peak_gflops\": %.1f, \"peak_gflops_dp\": %.1f, \"peak_gflops_sp\": %.1f, "
           "\"peak_bw_gbs\": %.1f, \"fma_acc\": %d, \"cpu\": \"%s\"}\n",
           gf_dp, gf_dp, gf_sp, bw, FMA_ACC, cpu);
    return 0;
}
