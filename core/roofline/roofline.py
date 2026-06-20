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
        r = subprocess.run([_BIN], capture_output=True, text=True, timeout=120)
        p = json.loads(r.stdout.strip())
    except Exception:
        return None
    try:
        json.dump(p, open(_PEAKS, "w"))
    except Exception:
        pass
    return p


def classify(ai, gflops, pk):
    """Return (ceiling_gflops, pct_of_ceiling, bound) for a point."""
    if not pk or ai <= 0:
        return None
    ridge = pk["peak_gflops"] / pk["peak_bw_gbs"] if pk["peak_bw_gbs"] else 0
    ceil = min(pk["peak_gflops"], ai * pk["peak_bw_gbs"])
    bound = "memory" if ai < ridge else "compute"
    pct = (gflops / ceil * 100.0) if ceil > 0 else 0.0
    return ceil, pct, bound


def render(points, pk, out):
    """points: list of {label, ai, gflops}. Appends a roofline table to `out`."""
    if not pk:
        out.append("  (roofline unavailable — calibration failed)")
        return
    ridge = pk["peak_gflops"] / pk["peak_bw_gbs"] if pk["peak_bw_gbs"] else 0
    out.append("  Roofline (empirical peaks for %s):" % (pk.get("cpu") or "this host"))
    out.append("    peak compute %.0f GFLOP/s | peak BW %.0f GB/s | ridge AI %.2f FLOP/byte"
               % (pk["peak_gflops"], pk["peak_bw_gbs"], ridge))
    if not points:
        return
    out.append("    %-22s %8s %10s %10s %8s  bound" % ("point", "AI", "GFLOP/s", "ceiling", "%peak"))
    for p in points:
        c = classify(p["ai"], p["gflops"], pk)
        if not c:
            continue
        ceil, pct, bound = c
        out.append("    %-22s %8.2f %10.1f %10.1f %7.0f%%  %s"
                   % (p["label"][:22], p["ai"], p["gflops"], ceil, pct, bound))
