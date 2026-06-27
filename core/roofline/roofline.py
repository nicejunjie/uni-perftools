"""Shared roofline facility: machine ceilings + point placement.

Two ceilings per host, both reported (the achievable peak is the headline; %peak
is measured against it):
  * achievable — measured by the microbenchmark in calibrate.c (a register-
    resident FMA kernel + a STREAM triad), compiled and run once (~1 s), cached.
    Reflects the real all-core AVX frequency, which no spec sheet gives.
  * theoretical — computed from the hardware spec (cores × max-boost freq ×
    FLOPs/cycle for compute; memory channels × DDR rate for bandwidth). Instant,
    deterministic, an unreachable upper-bound reference roof above the achievable.
Both collectors plot points against the SAME ceilings: the snapshot's
whole-program point and the profile's per-kernel points.
"""
import os
import re
import glob
import json
import math
import subprocess
import textwrap

_HERE = os.path.dirname(os.path.abspath(__file__))

# ------------------------------------------------------ report comments
# Shared by every report section (this module is the common leaf both the
# roofline renderer and the analysis viewpoints import). Comments sit one step in
# from the 4-space metric lines and wrap narrow so the long explanations stay
# readable on small screens — every wrapped line carries its own '#'.
_CINDENT = "      "       # 6 spaces
_CWIDTH = 70             # total line width before wrapping


def comment(out, text):
    """Append `text` to `out` as one or more narrow, '#'-prefixed, indented
    comment lines. Continuation lines hang-indent under the first word."""
    for line in textwrap.wrap(text, width=_CWIDTH,
                              initial_indent=_CINDENT + "# ",
                              subsequent_indent=_CINDENT + "#   ",
                              break_long_words=False, break_on_hyphens=False):
        out.append(line)
_ROOT = os.path.dirname(os.path.dirname(_HERE))
_CACHE_DIR = os.path.join(_ROOT, "build", "roofline")
_PEAKS = os.path.join(_CACHE_DIR, "peaks.json")
_SRC = os.path.join(_HERE, "calibrate.c")


def _build(acc):
    """Compile calibrate.c with -DFMA_ACC=acc → a per-acc binary; path or None.
    aarch64 needs -mcpu=native (sets the part's SCHEDULING/tuning, not just the ISA
    features): on Neoverse V2 / Grace, -march=native alone left the FMA peak both
    unstable and ~30% low. x86 uses -march=native. (Verified on Vista Grace node
    i618-112 — see calibrate.c LIMITATIONS.) Falls back to plainer flags if the
    tuned ones don't compile."""
    os.makedirs(_CACHE_DIR, exist_ok=True)
    cc = os.environ.get("CC", "cc")
    arch = "-mcpu=native" if os.uname().machine.startswith("aarch64") else "-march=native"
    out = os.path.join(_CACHE_DIR, "calibrate_a%d" % acc)
    # Keep -fopenmp as long as possible — a bare -O2 measures single-threaded (~ncores×
    # too low). The final -O2 is a true last resort (now DCE-safe via volatile g_sink).
    for flags in (["-O3", arch, "-ffast-math", "-funroll-loops", "-fopenmp"],
                  ["-O3", "-ffast-math", "-funroll-loops", "-fopenmp"],
                  ["-O3", "-fopenmp"], ["-O2"]):
        if subprocess.run([cc] + flags + ["-DFMA_ACC=%d" % acc, _SRC, "-o", out],
                          capture_output=True).returncode == 0:
            return out
    return None


def _run(binary, mode):
    """Run a calibrate binary in 'compute' or 'bw' mode → its JSON dict, or None."""
    try:
        r = subprocess.run([binary, mode], capture_output=True, text=True, timeout=120,
                           env=_run_env())
        return json.loads(r.stdout.strip())
    except Exception:
        return None


def _stale():
    """True if the cached peaks.json is missing or older than the calibration
    sources (editing calibrate.c or this file forces a fresh measurement)."""
    if not os.path.exists(_PEAKS):
        return True
    try:
        t = os.path.getmtime(_PEAKS)
        return any(os.path.getmtime(s) > t for s in (_SRC, __file__))
    except OSError:
        return True


def _phys_cores():
    """Physical core count (not SMT siblings). Prefer sysfs topology — each core's
    thread_siblings_list groups its SMT siblings, so the count of distinct groups is
    the physical-core count, and this is authoritative on x86 AND ARM. Fall back to
    /proc/cpuinfo physical/core id (x86), then to logical CPUs adjusted for SMT.

    The old unconditional `logical // 2` fallback was wrong on no-SMT ARM (Grace has
    1 thread/core), halving the thread count and yielding a ~2x-low ceiling — so we
    only halve when the kernel reports SMT actually active."""
    groups = set()
    for f in glob.glob("/sys/devices/system/cpu/cpu[0-9]*/topology/thread_siblings_list"):
        try:
            groups.add(open(f).read().strip())
        except OSError:
            pass
    if groups:
        return len(groups)
    try:
        cores, cur = set(), {}
        for line in open("/proc/cpuinfo"):
            if ":" not in line:
                if "physical id" in cur and "core id" in cur:
                    cores.add((cur["physical id"], cur["core id"]))
                cur = {}
                continue
            k, v = line.split(":", 1)
            cur[k.strip()] = v.strip()
        if "physical id" in cur and "core id" in cur:
            cores.add((cur["physical id"], cur["core id"]))
        if cores:
            return len(cores)
    except OSError:
        pass
    n = os.cpu_count() or 1
    try:
        if open("/sys/devices/system/cpu/smt/active").read().strip() == "1":
            return max(1, n // 2)
    except OSError:
        pass
    return max(1, n)


def _run_env():
    """Pin one thread per physical core so the bandwidth triad is reproducible
    (unpinned SMT oversubscription across CCDs is what causes run-to-run swing).
    The ceiling is the FULL-machine peak, so force the thread count to the
    physical-core count — never inherit the profiled workload's OMP_NUM_THREADS
    (a single-threaded run must still be measured against the whole-socket peak)."""
    env = dict(os.environ)
    env["OMP_NUM_THREADS"] = str(_phys_cores())
    env["OMP_PROC_BIND"] = "spread"
    env["OMP_PLACES"] = "cores"
    # Pin the team size exactly: with OMP_DYNAMIC the runtime may hand calibrate.c
    # fewer threads than requested, and the FLOP ceiling (which scales by the actual
    # team size) would then be measured below the true machine peak.
    env["OMP_DYNAMIC"] = "FALSE"
    env.pop("OPENBLAS_NUM_THREADS", None)   # calibrate.c is OpenMP, not BLAS, but be safe
    return env


def _cpu_model():
    """Live CPU identity from /proc/cpuinfo ('' if unavailable). Mirrors calibrate.c
    cpu_model(): x86 'model name', else (aarch64, which has no model name) a stable
    'arm:<implementer>:<part>' so the per-machine peaks-cache guard isn't a no-op on ARM."""
    impl = part = ""
    try:
        for line in open("/proc/cpuinfo"):
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
            if not impl and line.startswith("CPU implementer"):
                impl = line.split(":", 1)[1].strip()
            if not part and line.startswith("CPU part"):
                part = line.split(":", 1)[1].strip()
    except OSError:
        pass
    return ("arm:%s:%s" % (impl, part)) if (impl or part) else ""


def _max_freq_ghz():
    """Max core frequency in GHz — the spec ceiling. Prefer the cpufreq boost
    max; fall back to the highest 'cpu MHz' in /proc/cpuinfo."""
    try:
        khz = int(open("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq").read())
        if khz > 0:
            return khz / 1e6
    except (OSError, ValueError):
        pass
    try:
        return max(float(l.split(":")[1]) for l in open("/proc/cpuinfo")
                   if l.lower().startswith("cpu mhz")) / 1e3
    except (OSError, ValueError):
        return 0.0


def _cpu_flags():
    try:
        for line in open("/proc/cpuinfo"):
            if line.startswith(("flags", "Features")):
                return set(line.split(":", 1)[1].split())
    except OSError:
        pass
    return set()


def _dp_flops_per_cycle(flags):
    """Double-precision FLOPs/cycle/core = vector_DP_lanes × 2 (an FMA is mul+add)
    × FMA units. This is the THEORETICAL reference roof only (the empirical
    achievable peak is the headline ceiling), so approximations here are tolerable.

    LIMITATIONS / assumptions:
      - 2 FMA units is the standard for FMA-capable x86 (AMD Zen2-5, Intel
        Haswell-server onward). Parts with 1 FMA unit (some low-end / older) are
        over-estimated 2×; ARM Neoverse V2 (Grace) actually has 4× 128-bit pipes,
        which our 2-unit × wider-lane guess happens to land near but for the
        wrong reason.
      - SVE vector length is NOT detectable from /proc flags; we assume 256-bit.
        Real SVE is 128-bit on Neoverse V2/Grace and up to 512-bit on A64FX, so
        this lane count can be off by 2-4× on ARM. (x86 lanes are exact.)
      - Frequency uses MAX BOOST, which all-core AVX/SVE loads never sustain."""
    if "avx512f" in flags:
        lanes = 8                       # 512-bit / 64  (exact)
    elif "avx2" in flags or "avx" in flags:
        lanes = 4                       # 256-bit  (exact)
    elif "sve" in flags:
        lanes = 4                       # ASSUMED 256-bit SVE — see LIMITATIONS
    elif "sse2" in flags or "asimd" in flags or "neon" in flags:
        lanes = 2                       # 128-bit
    else:
        lanes = 1                       # scalar
    fma_capable = bool({"fma", "avx512f", "asimd", "sve"} & flags)
    return lanes * 2 * (2 if fma_capable else 1)


def _mem_bw_gbs():
    """Theoretical peak DRAM bandwidth from SMBIOS (dmidecode): for each populated
    DIMM, channels × 8 B × DDR transfer rate. Reads via dmidecode (root or
    passwordless sudo); None when unavailable — the roofline then shows the
    compute ceiling only."""
    txt = ""
    for cmd in (["dmidecode", "-t", "memory"], ["sudo", "-n", "dmidecode", "-t", "memory"]):
        try:
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=10)
            if r.returncode == 0 and "Memory Device" in r.stdout:
                txt = r.stdout
                break
        except Exception:
            continue
    if not txt:
        return None
    speed, channels = 0, set()
    cur = {}
    blocks = re.split(r"\n(?=Memory Device|Handle )", txt)
    for blk in blocks:
        if "Memory Device" not in blk:
            continue
        size = re.search(r"^\s*Size:\s*([0-9]+)\s*(?:MB|GB|TB)", blk, re.M)
        if not size:
            continue                    # empty slot
        mts = re.search(r"Configured Memory Speed:\s*([0-9]+)\s*MT/s", blk) \
            or re.search(r"\bSpeed:\s*([0-9]+)\s*MT/s", blk)
        if mts:
            speed = max(speed, int(mts.group(1)))
        ch = re.search(r"CHANNEL\s*(\w+)", blk, re.I)
        if ch:
            channels.add(ch.group(1).upper())
    nch = len(channels)
    if not (speed and nch):
        return None
    return nch * 8.0 * speed / 1e3      # bytes/transfer(8) × MT/s → GB/s


def _spec_peaks():
    """Theoretical ceilings computed from the hardware spec — no microbenchmark,
    deterministic and instant. Compute = cores × max-freq × DP-FLOPs/cycle;
    bandwidth = channels × 8 B × DDR MT/s. The *achievable* peak sits below these."""
    cores, ghz = _phys_cores(), _max_freq_ghz()
    dp_pc = _dp_flops_per_cycle(_cpu_flags())
    if not (cores and ghz and dp_pc):
        return None
    dp = cores * ghz * dp_pc            # GFLOP/s (the 1e9 in GHz cancels per-second)
    p = {"peak_gflops": round(dp, 1), "peak_gflops_dp": round(dp, 1),
         "peak_gflops_sp": round(dp * 2.0, 1), "cpu": _cpu_model(), "method": "spec"}
    bw = _mem_bw_gbs()
    if bw:
        p["peak_bw_gbs"] = round(bw, 1)
    return p


def _measure_empirical():
    """Self-tuning achievable peak. The register-blocking factor (FMA accumulator
    count) that saturates the FP units is part-dependent — too few starves the
    pipes, too many spills the register file and regresses — so we SWEEP it and
    keep the best-measured compute peak. Bandwidth is accumulator-independent, so
    it is measured once. No per-platform hand-tuning, no hardware to test on."""
    # Span both register-file sizes: ISAs with 16 vector regs (x86 AVX2/SSE) peak
    # around 12 accumulators and fall off a cliff once 16+constants spill; ISAs
    # with 32 regs (AVX-512, ARM NEON/SVE) peak around 16-24. (Verified: ser5/Zen3
    # AVX2 best at ~12-14, Zen5 AVX-512 at 16, Grace NEON at 16.)
    best, bins = {}, []
    for acc in (8, 12, 16, 24):
        b = _build(acc)
        if not b:
            continue
        bins.append(b)
        d = _run(b, "compute")
        if not d:
            continue
        for k in ("peak_gflops_dp", "peak_gflops_sp"):
            if d.get(k, 0) > best.get(k, 0):
                best[k] = d[k]
                if k == "peak_gflops_dp":
                    best["fma_acc"] = d.get("fma_acc")     # the winning blocking factor
        best.setdefault("cpu", d.get("cpu", ""))
    if not (bins and best.get("peak_gflops_dp")):
        return None
    bwd = _run(bins[0], "bw")                              # FMA_ACC-independent
    if bwd and bwd.get("peak_bw_gbs"):
        best["peak_bw_gbs"] = bwd["peak_bw_gbs"]
    best["peak_gflops"] = best["peak_gflops_dp"]
    best["method"] = "empirical"
    return best


def measure():
    """Fresh ceilings for THIS host — BOTH the empirical (microbenchmark-measured)
    ACHIEVABLE peak and the theoretical (spec) peak. The empirical achievable peak
    is the primary/headline ceiling (it reflects the real all-core AVX frequency,
    which no spec sheet gives); the theoretical values are folded in under `*_theo`
    keys as the absolute-upper-bound reference roof above it."""
    emp = _measure_empirical() or {}
    spec = _spec_peaks() or {}
    if not (emp or spec):
        return None
    keys = ("peak_gflops", "peak_gflops_dp", "peak_gflops_sp", "peak_bw_gbs")
    out = dict(emp)                                # primary = achievable
    out.setdefault("cpu", spec.get("cpu", ""))
    # Fall back to the theoretical value for anything the microbenchmark couldn't
    # measure (e.g. it failed to build → spec-only).
    for k in keys:
        if not out.get(k) and spec.get(k):
            out[k] = spec[k]
    # Theoretical reference roof + the "(theoretical)" annotation — but ONLY when
    # it's genuinely above the achievable (a real upper bound). Frequency detection
    # can under-read (some AMD parts expose base, not boost, as the cpufreq max),
    # making theoretical land below empirical; show no broken "ceiling" beneath the
    # measured point in that case.
    for k in keys:
        s, a = spec.get(k), out.get(k) or 0
        if s and s > a * 1.005:
            out[k + "_theo"] = s
    # Sanity: a measured compute peak far below the theoretical bound usually means
    # the microbenchmark didn't saturate the FP units (wrong -mcpu, no ISA path,
    # register spills) — surface it rather than silently trusting a low number.
    dp_a, dp_t = out.get("peak_gflops_dp"), out.get("peak_gflops_dp_theo")
    if dp_a and dp_t and dp_a < 0.40 * dp_t:
        out["compute_warn"] = ("compute peak is %.0f%% of theoretical — likely not "
                               "saturating the FP units; check the compiler / -mcpu "
                               "and the ISA kernel path" % (dp_a / dp_t * 100.0))
    out["method"] = ("empirical+spec" if (emp and spec)
                     else "empirical" if emp else "spec")
    return out


# Per-run calibration: each profiling run recalibrates on its own host and stores
# the ceilings beside its profile, so the report reflects the machine the workload
# actually ran on — never a stale build cache copied in from another box (e.g. a
# shared $HOME / NFS checkout, or a different-CPU machine).
_RUN_PEAKS = "peaks.json"   # filename written into each result dir
_ACTIVE = None              # calibration bound for the current report (use_result)


def calibrate_into(result_dir):
    """Recalibrate ceilings on this host and persist them with the run. Called
    once per profiling run (core/cli/upat). Also refreshes the per-build cache."""
    p = measure()
    if not p:
        return None
    for path in (os.path.join(result_dir, _RUN_PEAKS), _PEAKS):
        try:
            os.makedirs(os.path.dirname(path), exist_ok=True)
            json.dump(p, open(path, "w"))
        except OSError:
            pass
    return p


def use_result(result_dir):
    """Bind the report to the calibration captured at this run's collection time.
    Falls back to the per-build cache for results that predate per-run peaks."""
    global _ACTIVE
    _ACTIVE = None
    if result_dir:
        try:
            _ACTIVE = json.load(open(os.path.join(result_dir, _RUN_PEAKS)))
        except (OSError, ValueError):
            _ACTIVE = None
    return _ACTIVE


def peaks(force=False):
    """Return {peak_gflops, peak_bw_gbs, cpu}. Prefers the per-run calibration
    bound by use_result(); otherwise the per-build cache — but only if it was
    calibrated on the CPU we're running on now (else it recalibrates, so a cache
    copied in from another machine can never silently report the wrong ceilings)."""
    if _ACTIVE is not None and not force:
        return _ACTIVE
    if not force and os.path.exists(_PEAKS) and not _stale():
        try:
            cached = json.load(open(_PEAKS))
            me = _cpu_model()
            # Trust the cache only on a NON-EMPTY exact CPU match. An empty identity
            # (couldn't read /proc/cpuinfo) must force recalibration, never match an
            # empty cached "cpu" — that would silently reuse a cross-machine ceiling.
            if me and cached.get("cpu", "") == me:
                return cached
        except Exception:
            pass
    p = measure()
    if p:
        try:
            os.makedirs(_CACHE_DIR, exist_ok=True)
            json.dump(p, open(_PEAKS, "w"))
        except OSError:
            pass
    return p


def peak_compute(pk, prec="dp"):
    """Compute ceiling for the given precision ('sp' or 'dp'); falls back to the
    legacy single peak_gflops if a precision-specific peak isn't present."""
    if not pk:
        return None
    if prec == "sp":
        return pk.get("peak_gflops_sp") or pk.get("peak_gflops")
    return pk.get("peak_gflops_dp") or pk.get("peak_gflops")


def classify(ai, gflops, pk, prec="dp"):
    """Return (ceiling_gflops, pct_of_ceiling, bound) for a point, against the
    roofline of its precision (FP32 vs FP64)."""
    peak = peak_compute(pk, prec)
    if not peak or ai <= 0 or not pk.get("peak_bw_gbs"):
        return None
    bw = pk["peak_bw_gbs"]
    ridge = peak / bw
    ceil = min(peak, ai * bw)
    pct = (gflops / ceil * 100.0) if ceil > 0 else 0.0
    # A point far below its ceiling is not bound by compute throughput OR bandwidth —
    # it is latency/overhead/idle-bound (stalls, serialization, under-utilization).
    if pct < 25.0:
        bound = "latency"
    else:
        bound = "memory" if ai < ridge else "compute"
    return ceil, pct, bound


def precision_unknown_summary(ai, gflops, pk):
    """Roofline verdict for a MIXED-precision FP count (AMD `fp_ret_sse_avx_ops`,
    ARM `fp_*_ops_spec`): the counter is element-weighted so the achieved GFLOP/s and
    AI are EXACT, but it exposes no SP/DP split — and the compute roof is
    precision-dependent (FP32 peak ~2x FP64). So we can't pick one roof; classify
    against BOTH and report what is / isn't precision-dependent. The bandwidth
    (slanted) roof does NOT depend on precision, and the FP64/FP32 ridge points sit
    at AI and 2·AI, so the ambiguity is confined to the band between them. Returns a
    one-line verdict string, or None if ceilings are unavailable."""
    dp = peak_compute(pk, "dp")
    sp = peak_compute(pk, "sp") or dp
    bw = (pk or {}).get("peak_bw_gbs")
    if not dp or not bw or ai <= 0:
        return None
    ridge_dp, ridge_sp = dp / bw, sp / bw
    cdp = classify(ai, gflops, pk, "dp")
    csp = classify(ai, gflops, pk, "sp")
    if not cdp or not csp:
        return None
    # Far below the roofline under BOTH precisions → latency/overhead/idle-bound, not
    # compute- or bandwidth-bound. This is precision-independent (which compute roof you'd
    # pick is moot), so it must be caught before the ridge-band branches — otherwise a
    # point at e.g. 8% of peak above the FP64 ridge would be wrongly called "compute-bound".
    if cdp[2] == "latency" and csp[2] == "latency":
        return ("%.0f%% of the FP64 ceiling — far below the roofline: latency/overhead/idle-bound, "
                "not compute- or bandwidth-bound (precision choice is moot here)." % cdp[1])
    if ai < ridge_dp:
        # left of both ridges → bound by the (precision-independent) bandwidth roof
        return ("memory-bandwidth-bound at %.0f%% of the DRAM roof — precision-independent "
                "(the bandwidth roof does not depend on FP precision)." % cdp[1])
    if ai >= ridge_sp:
        # right of both ridges → compute-bound either way; only %-of-peak differs
        return ("compute-bound — %.0f%% of the FP64 ceiling / %.0f%% of the FP32 ceiling. "
                "SP/DP mix is not measurable on this CPU, so %%-of-peak is a range; run `upat` "
                "for the precision split." % (cdp[1], csp[1]))
    # between the two ridges → the BOUND itself depends on precision
    return ("AI %.2f sits between the FP64 and FP32 ridge points (%.2f / %.2f): compute-bound "
            "if this code is FP64, memory-bandwidth-bound if FP32 — FP precision is not "
            "measurable on this CPU, so run `upat` (sci-lib trace / sampling) for the split."
            % (ai, ridge_dp, ridge_sp))


def ascii_plot(points, pk, prec="dp", width=52, height=14):
    """A log-log roofline as ASCII art: the bandwidth roof ('/') rising to the
    compute ceiling ('‾'), with measured points marked. Returns indented lines
    (empty list if ceilings are unavailable)."""
    peak = peak_compute(pk, prec)
    bw = (pk or {}).get("peak_bw_gbs")
    if not peak or not bw:
        return []
    pts = [(p["ai"], p["gflops"], p.get("label", "")) for p in points
           if p.get("ai", 0) > 0 and p.get("gflops", 0) > 0]
    ridge = peak / bw
    # The theoretical roof (upper bound) sits above the achievable one; size the
    # plot to whichever is higher so both fit.
    peak_t = (pk.get("peak_gflops_sp_theo") if prec == "sp" else pk.get("peak_gflops_dp_theo"))
    bw_t = pk.get("peak_bw_gbs_theo")
    top = max(peak, peak_t or 0)
    # The machine sets the baseline window (ridge ±1.5 decades in x; ~3 decades
    # down from the compute ceiling in y) so the roof's shape is stable across runs
    # on the same host. But we then GROW it to include every measured point: a
    # memory-bound kernel can sit well left of the ridge (e.g. HPCG at AI≈0.15,
    # ridge≈46), and clamping it to the left edge made the ● look detached from the
    # bandwidth roof instead of sitting on it. Growing only ever adds decades — the
    # roof's slope is unchanged, there's just more of the diagonal drawn at the side.
    lr = math.log10(ridge)
    lx0 = math.floor(lr - 1.5)
    lx1 = math.ceil(lr + 1.5)
    ly1 = math.log10(top) + 0.08                   # top of plot ≈ the highest ceiling
    ly0 = ly1 - 3.2
    if pts:
        lx0 = min(lx0, math.floor(math.log10(min(p[0] for p in pts))))
        lx1 = max(lx1, math.ceil(math.log10(max(p[0] for p in pts))))
        ly0 = min(ly0, math.floor(math.log10(min(p[1] for p in pts))))

    def cx(ai):
        return int(round((math.log10(ai) - lx0) / (lx1 - lx0) * (width - 1)))

    def cy(g):
        return int(round((ly1 - math.log10(g)) / (ly1 - ly0) * (height - 1)))

    grid = [[" "] * width for _ in range(height)]
    # Theoretical (upper-bound) roof first, in a lighter char, so the achievable
    # roof and the measured points draw over it where they overlap.
    peak_e, bw_e = peak_t, bw_t
    if peak_e and bw_e:
        for col in range(width):
            ai = 10 ** (lx0 + col / (width - 1) * (lx1 - lx0))
            row = cy(min(peak_e, ai * bw_e))
            if 0 <= row < height:
                grid[row][col] = "·"
    for col in range(width):                       # the theoretical roofline itself
        ai = 10 ** (lx0 + col / (width - 1) * (lx1 - lx0))
        row = cy(min(peak, ai * bw))
        if 0 <= row < height:
            grid[row][col] = "‾" if ai >= ridge else "/"
    legend = []
    for i, (ai, g, label) in enumerate(pts):       # measured points on top
        col = min(max(cx(ai), 0), width - 1)
        row = min(max(cy(g), 0), height - 1)
        m = "●" if len(pts) == 1 else (chr(ord("A") + i) if i < 26 else "*")
        grid[row][col] = m
        if len(pts) > 1:
            legend.append("      %s %s" % (m, label[:48]))

    ylab = {}                                       # decade gridlines on the y-axis
    for L in range(int(math.ceil(ly0)), int(math.floor(ly1)) + 1):
        r = cy(10 ** L)
        if 0 <= r < height:
            ylab[r] = 10 ** L
    out = ["    GFLOP/s"]
    for r in range(height):
        lab = ("%7g" % ylab[r]) if r in ylab else " " * 7
        out.append("    %s │%s" % (lab, "".join(grid[r])))
    out.append("    %s └%s" % (" " * 7, "─" * width))
    axis = [" "] * width                            # decade ticks on the x-axis
    for L in range(int(math.ceil(lx0)), int(math.floor(lx1)) + 1):
        c, s = cx(10 ** L), ("%g" % (10 ** L))
        start = min(max(c - len(s) // 2, 0), width - len(s))   # center on tick, clamp
        for k, ch in enumerate(s):
            axis[start + k] = ch
    out.append("    %s  %s  AI (FLOP/byte)" % (" " * 7, "".join(axis)))
    marker = "● measured   " if len(pts) == 1 else ""   # multi-point uses the lettered legend
    theo = "   · theoretical roof" if (peak_e and bw_e) else ""
    out.append("    %s  %s/ bandwidth roof   ‾ compute ceiling%s" % (" " * 7, marker, theo))
    out.extend(legend)
    return out


def render(points, pk, out):
    """points: list of {label, ai, gflops}. Appends a roofline table to `out`."""
    if not pk:
        out.append("  (roofline unavailable — calibration failed)")
        return
    bw = pk.get("peak_bw_gbs") or 0
    dp, sp = peak_compute(pk, "dp"), peak_compute(pk, "sp")
    if not dp:                              # peaks present but no compute ceiling
        out.append("  (roofline unavailable — no compute ceiling in calibration)")
        return
    out.append("  Roofline ceilings for %s:" % (pk.get("cpu") or "this host"))

    def _both(achiev, theo, unit):
        s = "%-7.0f %-9s achievable" % (achiev, unit)
        return s + ("   (%.0f theoretical)" % theo if theo else "")

    out.append("    peak compute FP64    %s" % _both(dp, pk.get("peak_gflops_dp_theo"), "GFLOP/s"))
    if sp and sp != dp:
        out.append("    peak compute FP32    %s" % _both(sp, pk.get("peak_gflops_sp_theo"), "GFLOP/s"))
    out.append("    peak DRAM bandwidth  %s" % _both(bw, pk.get("peak_bw_gbs_theo"), "GB/s"))
    out.append("    ridge AI FP64        %.2f FLOP/byte" % (dp / bw if bw else 0))
    if sp and sp != dp:
        out.append("    ridge AI FP32        %.2f FLOP/byte" % (sp / bw if bw else 0))
    if pk.get("compute_warn"):
        out.append("    ⚠ %s" % pk["compute_warn"])
    comment(out, "achievable = microbenchmark-measured peak (register-resident FMA "
                 "kernel for compute, a STREAM triad with non-temporal stores for "
                 "bandwidth) — the realistic ceiling; %peak is measured against this")
    comment(out, "theoretical = hardware-spec upper bound (cores × max-boost freq × "
                 "FLOPs/cycle; memory channels × DDR rate) — unreachable in practice "
                 "(AVX clocks throttle below max boost under all-core load)")
    comment(out, "AI = arithmetic intensity: FLOPs performed per byte read from DRAM")
    comment(out, "FP64/FP32 = double/single precision floating point")
    comment(out, "ridge AI = AI where the compute roof meets the bandwidth roof; "
                 "below it a kernel is memory-bandwidth-bound, above it compute-bound "
                 "— the minimum AI at which this machine's peak GFLOP/s is reachable")
    # Always draw the roofline itself (the ceilings) — even with no measured
    # point, the plot is the machine's roofline shape, never blank. A measured
    # point (●) is overlaid when present.
    plot = ascii_plot(points, pk, points[0].get("prec", "dp") if points else "dp")
    if plot:
        out.append("")
        out.extend(plot)
        out.append("")
    if not points:
        return
    hdr = ("    %-26s %4s %8s %10s %10s %8s  bound"
           % ("point", "prec", "AI", "GFLOP/s", "ceiling", "%peak"))
    rule = "    " + "─" * (len(hdr) - 4)
    out.append(hdr)
    out.append(rule)
    for p in points:
        prec = p.get("prec", "dp")
        c = classify(p["ai"], p["gflops"], pk, prec)
        if not c:
            continue
        ceil, pct, bound = c
        out.append("    %-26s %4s %8.2f %10.1f %10.1f %7.0f%%  %s"
                   % (p["label"][:26], prec.upper(), p["ai"], p["gflops"], ceil, pct, bound))
    out.append(rule)
