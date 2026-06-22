"""Shared roofline facility: empirical machine ceilings + point placement.

Ceilings come from core/roofline/calibrate.c (compiled + run once, cached).
Both collectors plot points against the SAME ceilings: the snapshot's
whole-program point and the profile's per-kernel points.
"""
import os
import json
import math
import subprocess

_HERE = os.path.dirname(os.path.abspath(__file__))
_ROOT = os.path.dirname(os.path.dirname(_HERE))
_CACHE_DIR = os.path.join(_ROOT, "build", "roofline")
_PEAKS = os.path.join(_CACHE_DIR, "peaks.json")
_BIN = os.path.join(_CACHE_DIR, "calibrate")
_SRC = os.path.join(_HERE, "calibrate.c")


def _build():
    os.makedirs(_CACHE_DIR, exist_ok=True)
    cc = os.environ.get("CC", "cc")
    for flags in (["-O3", "-march=native", "-ffast-math", "-fopenmp"],
                  ["-O3", "-ffast-math", "-fopenmp"], ["-O2"]):
        if subprocess.run([cc] + flags + [_SRC, "-o", _BIN],
                          capture_output=True).returncode == 0:
            return True
    return False


def _stale():
    """True if the binary is missing or older than calibrate.c."""
    if not os.path.exists(_BIN):
        return True
    try:
        return os.path.getmtime(_SRC) > os.path.getmtime(_BIN)
    except OSError:
        return True


def _phys_cores():
    """Physical core count (not SMT siblings) from /proc/cpuinfo; fall back to
    half the logical CPUs (typical with SMT) or all of them."""
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
    return max(1, n // 2)


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
    env.pop("OPENBLAS_NUM_THREADS", None)   # calibrate.c is OpenMP, not BLAS, but be safe
    return env


def _cpu_model():
    """Live CPU brand string from /proc/cpuinfo ('' if unavailable)."""
    try:
        for line in open("/proc/cpuinfo"):
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return ""


def measure():
    """Run the calibration binary NOW and return fresh ceilings for THIS host
    (no cache read). None if it can't build or run."""
    if _stale() and not _build():
        return None
    if not os.path.exists(_BIN):
        return None
    try:
        r = subprocess.run([_BIN], capture_output=True, text=True, timeout=120,
                           env=_run_env())
        return json.loads(r.stdout.strip())
    except Exception:
        return None


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
            if cached.get("cpu", "") == _cpu_model():
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
    # Axis ranges come ONLY from the machine (peak/BW/ridge), never the measured
    # point — so the roofline shape is identical for every run on the same host and
    # only the ● moves. Points outside the window clamp to the plot edge. x spans
    # the ridge ±1.5 decades; y spans ~3 decades down from the compute ceiling.
    lr = math.log10(ridge)
    lx0 = math.floor(lr - 1.5)
    lx1 = math.ceil(lr + 1.5)
    ly1 = math.log10(peak) + 0.08                  # top of plot ≈ the compute ceiling
    ly0 = ly1 - 3.2

    def cx(ai):
        return int(round((math.log10(ai) - lx0) / (lx1 - lx0) * (width - 1)))

    def cy(g):
        return int(round((ly1 - math.log10(g)) / (ly1 - ly0) * (height - 1)))

    grid = [[" "] * width for _ in range(height)]
    for col in range(width):                       # the roofline itself
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
    out.append("    %s  %s/ bandwidth roof   ‾ compute ceiling" % (" " * 7, marker))
    out.extend(legend)
    return out


def render(points, pk, out):
    """points: list of {label, ai, gflops}. Appends a roofline table to `out`."""
    if not pk:
        out.append("  (roofline unavailable — calibration failed)")
        return
    bw = pk.get("peak_bw_gbs") or 0
    dp, sp = peak_compute(pk, "dp"), peak_compute(pk, "sp")
    out.append("  Roofline (empirical peaks for %s):" % (pk.get("cpu") or "this host"))
    out.append("    peak compute FP64    %.0f GFLOP/s" % dp)
    if sp and sp != dp:
        out.append("    peak compute FP32    %.0f GFLOP/s" % sp)
    out.append("    peak DRAM bandwidth  %.0f GB/s" % bw)
    out.append("    ridge AI FP64        %.2f FLOP/byte" % (dp / bw if bw else 0))
    if sp and sp != dp:
        out.append("    ridge AI FP32        %.2f FLOP/byte" % (sp / bw if bw else 0))
    out.append("    # AI = arithmetic intensity: FLOPs performed per byte read from DRAM")
    out.append("    # FP64/FP32 = double/single precision floating point")
    out.append("    # ridge AI = AI where the compute roof meets the bandwidth roof; below it a")
    out.append("    #   kernel is memory-bandwidth-bound, above it compute-bound — the minimum AI")
    out.append("    #   at which this machine's peak GFLOP/s is reachable")
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
