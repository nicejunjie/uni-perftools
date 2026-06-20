"""Shared roofline facility: empirical machine ceilings + point placement.

Ceilings come from core/roofline/calibrate.c (compiled + run once, cached).
Both collectors plot points against the SAME ceilings: the snapshot's
whole-program point and the profile's per-kernel points.
"""
import os
import json
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
    (unpinned SMT oversubscription across CCDs is what causes run-to-run swing)."""
    env = dict(os.environ)
    env.setdefault("OMP_NUM_THREADS", str(_phys_cores()))
    env.setdefault("OMP_PROC_BIND", "spread")
    env.setdefault("OMP_PLACES", "cores")
    return env


def peaks(force=False):
    """Return {peak_gflops, peak_bw_gbs, cpu} — empirical, cached per build."""
    if not force and os.path.exists(_PEAKS) and not _stale():
        try:
            return json.load(open(_PEAKS))
        except Exception:
            pass
    if (force or _stale()) and not _build():
        return None
    if not os.path.exists(_BIN):
        return None
    try:
        r = subprocess.run([_BIN], capture_output=True, text=True, timeout=120,
                           env=_run_env())
        p = json.loads(r.stdout.strip())
    except Exception:
        return None
    try:
        json.dump(p, open(_PEAKS, "w"))
    except Exception:
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
    bound = "memory" if ai < ridge else "compute"
    pct = (gflops / ceil * 100.0) if ceil > 0 else 0.0
    return ceil, pct, bound


def render(points, pk, out):
    """points: list of {label, ai, gflops}. Appends a roofline table to `out`."""
    if not pk:
        out.append("  (roofline unavailable — calibration failed)")
        return
    bw = pk.get("peak_bw_gbs") or 0
    dp, sp = peak_compute(pk, "dp"), peak_compute(pk, "sp")
    out.append("  Roofline (empirical peaks for %s):" % (pk.get("cpu") or "this host"))
    if sp and sp != dp:
        out.append("    peak compute  FP64 %.0f | FP32 %.0f GFLOP/s   peak BW %.0f GB/s"
                   % (dp, sp, bw))
        out.append("    ridge AI  FP64 %.2f | FP32 %.2f FLOP/byte  (point judged vs its own precision)"
                   % (dp / bw if bw else 0, sp / bw if bw else 0))
    else:
        out.append("    peak compute %.0f GFLOP/s | peak BW %.0f GB/s | ridge AI %.2f FLOP/byte"
                   % (dp, bw, dp / bw if bw else 0))
    if not points:
        return
    out.append("    %-22s %4s %8s %10s %10s %8s  bound"
               % ("point", "prec", "AI", "GFLOP/s", "ceiling", "%peak"))
    for p in points:
        prec = p.get("prec", "dp")
        c = classify(p["ai"], p["gflops"], pk, prec)
        if not c:
            continue
        ceil, pct, bound = c
        out.append("    %-22s %4s %8.2f %10.1f %10.1f %7.0f%%  %s"
                   % (p["label"][:22], prec.upper(), p["ai"], p["gflops"], ceil, pct, bound))
