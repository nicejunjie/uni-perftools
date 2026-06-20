"""Analysis viewpoints — recipes over one result dir. Each appends lines to `out`.
Everything here is derived from data the collectors already emit (no collector
changes): snapshot counters (snap.json) + profile attribution (prof.*.json).
"""
import os
import re
import sys
_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(_HERE), "contract"))
sys.path.insert(0, os.path.join(os.path.dirname(_HERE), "roofline"))
import contract   # noqa: E402
import roofline   # noqa: E402

ELSIZE = {"s": 4, "d": 8, "c": 8, "z": 16}   # bytes per element by BLAS type char
FACTOR = {"s": 2, "d": 2, "c": 8, "z": 8}    # flops per mul-add


def _m(snap, key):
    for x in (snap or {}).get("metrics", []):
        if x.get("key") == key:
            return x.get("value")
    return None


def _disp(snap, key):
    for x in (snap or {}).get("metrics", []):
        if x.get("key") == key:
            return x.get("display")
    return None


# ----------------------------------------------------------------- roofline
def _kernel_point(row):
    """Parse a shaped BLAS row name -> (label, ai, gflops) or None."""
    name = row.get("name", "")
    m = re.match(r"(?:cblas_)?([sdcz])(gemm|gemv|syrk|trsm)_?\[(.*)\]", name)
    if not m:
        return None
    t, kind, shape = m.group(1), m.group(2), m.group(3)
    dims = dict(re.findall(r"([a-z]+)=(\d+)", shape))
    g = lambda k: int(dims.get(k, 0))
    el, fac = ELSIZE[t], FACTOR[t]
    if kind == "gemm":
        mm, nn, kk = g("m"), g("n"), g("k")
        if not (mm and nn and kk):
            return None
        flops = fac * mm * nn * kk
        byts = (mm * kk + kk * nn + mm * nn) * el
    elif kind == "gemv":
        mm, nn = g("m"), g("n")
        if not (mm and nn):
            return None
        flops = fac * mm * nn
        byts = (mm * nn + mm + nn) * el
    elif kind == "syrk":
        nn, kk = g("n"), g("k")
        flops = fac * nn * nn * kk
        byts = (nn * kk + nn * nn) * el
    else:  # trsm
        mm, nn = g("m"), g("n")
        flops = fac * mm * mm * nn
        byts = (mm * mm + mm * nn) * el
    t_incl = row.get("t_incl", 0.0)
    if t_incl <= 1e-3 or byts <= 0:        # skip sub-ms kernels (timer noise)
        return None
    total_flops = flops * row.get("count", 1)
    return {"label": name, "ai": flops / byts, "gflops": total_flops / t_incl / 1e9,
            "flops": total_flops}


def roofline_view(snap, profile, out):
    pk = roofline.peaks()
    points = []
    # whole-program point from snapshot counters
    g = _m(snap, "gflops")
    fills = _m(snap, "mem_fills_dram")
    elapsed = _m(snap, "elapsed_time")
    if g and fills and elapsed:
        dram_bytes = fills * 64.0
        flops = g * 1e9 * elapsed
        if dram_bytes > 0:
            points.append({"label": "whole-program (measured)", "ai": flops / dram_bytes, "gflops": g})
    # per-kernel points from profile shaped rows
    kerns = []
    for r in (profile or {}).get("functions", []):
        p = _kernel_point(r)
        if p:
            kerns.append(p)
    kerns.sort(key=lambda p: -p["flops"])    # rank by work done, not noisy rate
    points += kerns[:6]
    out.append("\n══ Roofline ══")
    roofline.render(points, pk, out)
    if not kerns:
        out.append("    (per-kernel points need SCILIB_SHAPE=1 — the suite sets it)")
        return
    # headroom note for the dominant (most work) kernel
    dom = kerns[0]
    c = roofline.classify(dom["ai"], dom["gflops"], pk)
    if c and c[1] < 25:                       # < 25% of its ceiling
        head = c[0] / dom["gflops"] if dom["gflops"] > 0 else 0
        out.append("    → %s runs at %.0f%% of its roofline ceiling — up to ~%.0fx headroom "
                   "(optimized library / better blocking / vectorization)."
                   % (dom["label"].split("[")[0], c[1], head))


# ------------------------------------------------------- microarch / memory
def microarch_view(snap, out):
    if not snap:
        return
    out.append("\n══ Microarchitecture (top-down pipeline slots) ══")
    for k, lbl in [("topdown_retiring_pct", "retiring (useful)"),
                   ("topdown_frontend_pct", "frontend-bound"),
                   ("topdown_backend_pct", "backend-bound"),
                   ("topdown_backend_mem_pct", "  └ memory"),
                   ("topdown_backend_core_pct", "  └ core"),
                   ("topdown_badspec_pct", "bad speculation")]:
        v = _m(snap, k)
        if v is not None:
            out.append("    %-18s %5.1f%%" % (lbl, v))
    for k in ("ipc", "cpi"):
        if _disp(snap, k):
            out.append("    %-18s %s" % (k.upper(), _disp(snap, k)))


def memory_view(snap, out):
    if not snap:
        return
    out.append("\n══ Memory access ══")
    for k, lbl in [("cache_miss_rate", "cache-miss rate"), ("llc_mpki", "LLC MPKI"),
                   ("dram_dpki", "DRAM fills / 1K-instr"), ("dram_bound_pct", "DRAM-bound"),
                   ("numa_remote_pct", "NUMA remote access"), ("memory_bound", "memory-bound (slots)"),
                   ("peak_rss", "peak RSS")]:
        if _disp(snap, k) is not None:
            out.append("    %-22s %s" % (lbl, _disp(snap, k)))


# ----------------------------------------------------------- load imbalance
def imbalance_view(profile, out):
    fns = (profile or {}).get("functions", [])
    nr = (profile or {}).get("nranks", 1)
    if nr < 2:
        return
    rows = []
    for f in fns:
        imb = f.get("imb_excl", 0.0)
        t = f.get("t_excl", 0.0)
        if t > 0.005 and imb >= 5:
            recoverable = t * imb / 100.0   # ~ time the slow rank could shed
            rows.append((recoverable, imb, t, f.get("name", ""), f.get("group", "")))
    if not rows:
        return
    rows.sort(reverse=True)
    out.append("\n══ Load imbalance (across %d ranks, ranked by recoverable time) ══" % nr)
    out.append("    %-24s %6s %10s %12s" % ("function", "imb%", "avg excl(s)", "recover(s)"))
    for rec, imb, t, name, grp in rows[:10]:
        out.append("    %-24s %5.0f%% %10.4f %12.4f" % (name[:24], imb, t, rec))


# ------------------------------------------------------------- threading
def threading_view(snap, out):
    if not snap:
        return
    cores = _m(snap, "cpu_cores_used")
    threads = _m(snap, "max_threads")
    timb = _m(snap, "thread_imbalance_pct")
    if not threads or threads < 2:
        return
    out.append("\n══ Threading ══")
    if cores is not None:
        out.append("    threads %d | avg cores used %.2f | parallel efficiency %.0f%%"
                   % (threads, cores, (cores / threads * 100.0) if threads else 0))
    if timb is not None:
        out.append("    thread imbalance %.0f%%  ((max-avg)/max of per-thread time)" % timb)
