"""Analysis viewpoints — recipes over one result dir. Each appends lines to `out`.
Everything here is derived from data the collectors already emit (no collector
changes): snapshot counters (snap.json) + profile attribution (prof.*.json).
"""
import os
import re
import sys
import json
import math
_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(_HERE), "contract"))
sys.path.insert(0, os.path.join(os.path.dirname(_HERE), "roofline"))
import contract   # noqa: E402
import roofline   # noqa: E402


def _rule(header):
    """A horizontal divider matching a 4-space-indented table header's width."""
    return "    " + "─" * (len(header) - 4)


def _load_json(path):
    try:
        return json.load(open(path))
    except Exception:
        return None


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
def roofline_view(snap, profile, out):
    """Whole-program roofline from the snapshot's hardware counters: one point
    (measured FLOP/s vs DRAM traffic) placed against the empirical ceilings.

    Per-FUNCTION roofline is intentionally NOT here: shape-derived per-kernel
    points explode (same kernel × every input size), double-count nested calls,
    and only cover a few hand-coded formulas. That belongs to a two-pass profile
    feature (survey hot functions → characterize each with counters), where it
    works for library, user, and system code alike."""
    pk = roofline.peaks()
    points = []
    g = _m(snap, "gflops")
    fills = _m(snap, "mem_fills_dram")
    elapsed = _m(snap, "elapsed_time")
    if g and fills and elapsed:
        dram_bytes = fills * 64.0
        flops = g * 1e9 * elapsed
        if dram_bytes > 0:
            # whole-program FP-counter precision is vendor-dependent (Intel = DP
            # only; AMD = mixed SP+DP) → judged against the DP ceiling.
            points.append({"label": "whole-program (measured)", "ai": flops / dram_bytes,
                           "gflops": g, "prec": "dp"})
    out.append("\n══ Roofline (whole program) ══")
    roofline.render(points, pk, out)
    if not points:
        out.append("    (whole-program point needs FP + DRAM counters — check perf_event_paranoid)")
        return
    c = roofline.classify(points[0]["ai"], points[0]["gflops"], pk, "dp")
    if c:
        out.append("    whole program is %s-bound at %.0f%% of the DP ceiling." % (c[2], c[1]))
    out.append("    (per-function roofline → future profile two-pass: survey hotspots, then characterize)")


# ------------------------------------------------ per-function roofline (B)
def roofline_func_view(profile, out):
    """Per-function roofline from event-based sampling (the characterize pass):
    flops = FP-op samples x period, DRAM bytes = fill samples x period x line,
    time = exclusive (self) time samples / Hz. Works for any function — library,
    user, or system — ranked by exclusive time (the survey). Precision per
    arbitrary function isn't knowable from the FP counter, so points are judged
    against the DP ceiling."""
    rf = (profile or {}).get("roofline_functions")
    if not rf:
        return
    pk = roofline.peaks()
    fp_p, mem_p = rf.get("fp_period", 0), rf.get("mem_period", 0)
    bpf, hz = rf.get("bytes_per_fill", 64), rf.get("hz", 0)
    rows = []
    for fn, e in rf.get("functions", {}).items():
        self_s, fp, mem = e.get("self", 0), e.get("fp", 0), e.get("mem", 0)
        if self_s < 2 and fp < 4:               # below the noise floor
            continue
        t = self_s / hz if hz else 0.0
        if t <= 0:
            continue
        flops = fp * fp_p
        byts = mem * mem_p * bpf
        gflops = flops / t / 1e9
        ai = (flops / byts) if byts > 0 else None
        rows.append((self_s, fn, e.get("group", "ETC"), t, gflops, ai))
    if not rows:
        return
    rows.sort(reverse=True)                       # by exclusive time = the hotspots
    out.append("\n══ Roofline (per function — measured, event-based sampling) ══")
    out.append("  flops = FP-op samples x period (ops-based proxy); bytes = DRAM-fill samples x line.")
    hdr = ("    %-26s %8s %7s %9s %9s %7s  bound"
           % ("function", "self(s)", "AI", "GFLOP/s", "ceiling", "%peak"))
    out.append(hdr)
    out.append(_rule(hdr))
    for self_s, fn, grp, t, gflops, ai in rows[:12]:
        c = roofline.classify(ai if ai is not None else 1e12, gflops, pk, "dp")
        if not c:
            continue
        ceil, pct, bound = c
        ais = "%.1f" % ai if ai is not None else "  inf"
        if ai is None:
            bound = "compute"
        out.append("    %-26s %8.3f %7s %9.1f %9.0f %6.0f%%  %s"
                   % (fn[:26], t, ais, gflops, ceil, pct, bound))
    out.append(_rule(hdr))
    out.append("    (AI=inf → no DRAM traffic sampled = cache-resident/compute-bound; precision assumed DP)")


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
    hdr = "    %-24s %6s %10s %12s" % ("function", "imb%", "avg excl(s)", "recover(s)")
    out.append(hdr)
    out.append(_rule(hdr))
    for rec, imb, t, name, grp in rows[:10]:
        out.append("    %-24s %5.0f%% %10.4f %12.4f" % (name[:24], imb, t, rec))
    out.append(_rule(hdr))


# ----------------------------------------------------------- MPI wait-state
# Classify MPI calls by what dominates their time. Collectives and blocking
# receives spend most of their time *blocked on peers* (synchronization / wait);
# sends and nonblocking initiations are *transfer/initiation*. This is a call-type
# heuristic — true late-sender/late-receiver attribution needs cross-rank trace
# replay, which the collectors don't do.
_MPI_TRANSFER = re.compile(
    r"MPI_I?[BSR]?send|MPI_Irecv|MPI_Put|MPI_Get|MPI_R?Accumulate|MPI_[PU]npack|MPI_Start",
    re.I)
_MPI_WAIT = re.compile(
    r"MPI_(Barrier|Wait|Test|Recv|Sendrecv|M?Probe|Allreduce|Reduce|Allgather|Gather|"
    r"Scatter|Alltoall|Bcast|E?Scan|Reduce_scatter|Neighbor)",
    re.I)
# nonblocking initiations: I-prefixed p2p + nonblocking collectives + one-sided
_MPI_NONBLOCK = re.compile(
    r"MPI_(I[bsr]?send|Issend|Irecv|Iallreduce|Ireduce|Iallgather|Igather|Iscatter|"
    r"Ialltoall|Ibcast|Ibarrier|Iscan|Put|Get|R?Accumulate)", re.I)
_MPI_WAITCALL = re.compile(r"MPI_(Wait|Test)", re.I)


def _mpi_class(name):
    # wait first: e.g. MPI_Sendrecv contains "Send" but is a blocking exchange
    if _MPI_WAIT.search(name):
        return "wait"
    if _MPI_TRANSFER.search(name):
        return "transfer"
    return "transfer"            # default unknowns to transfer (conservative)


def mpi_view(profile, out):
    fns = [f for f in (profile or {}).get("functions", []) if f.get("group") == "MPI"]
    if not fns:
        return
    nr = (profile or {}).get("nranks", 1)
    rows, wait_t, xfer_t = [], 0.0, 0.0
    for f in fns:
        t = f.get("t_excl", 0.0)
        cls = _mpi_class(f.get("name", ""))
        if cls == "wait":
            wait_t += t
        else:
            xfer_t += t
        rows.append((t, cls, f.get("name", ""), f.get("imb_excl", 0.0)))
    total = wait_t + xfer_t
    if total <= 0:
        return
    rows.sort(reverse=True)
    out.append("\n══ MPI wait-state (call-type heuristic) ══")
    out.append("    synchronization/wait %6.4fs (%2.0f%%) | transfer/initiation %6.4fs (%2.0f%%)"
               % (wait_t, wait_t / total * 100, xfer_t, xfer_t / total * 100))
    hdr = "    %-22s %10s %7s %6s" % ("call", "excl(s)", "class", "imb%")
    out.append(hdr)
    out.append(_rule(hdr))
    for t, cls, name, imb in rows[:10]:
        out.append("    %-22s %10.5f %7s %5.0f%%" % (name[:22], t, cls, imb))
    out.append(_rule(hdr))
    # late-sender / load-imbalance signal: wait dominates AND a wait call is imbalanced
    wait_imb = max((imb for t, cls, name, imb in rows if cls == "wait"), default=0.0)
    if nr >= 2 and wait_t / total >= 0.6 and wait_imb >= 15:
        out.append("    → wait-dominated MPI with imbalanced collectives/receives "
                   "(%.0f%% imb): likely late-sender / load imbalance — rebalance work "
                   "across ranks before tuning communication." % wait_imb)
    elif nr >= 2 and wait_t / total >= 0.6:
        out.append("    → wait-dominated but balanced: ranks are well-balanced; "
                   "reduce synchronization (fewer/larger collectives, overlap with compute).")
    # comm/compute overlap: does the app even attempt overlap, and does Wait eat it?
    names = [n for _, _, n, _ in rows]
    has_nb = any(_MPI_NONBLOCK.search(n) for n in names)
    wait_call_t = sum(t for t, _, n, _ in rows if _MPI_WAITCALL.search(n))
    # blocking comm = wait-class time that isn't just polling Wait/Test calls
    has_blocking_comm = (wait_t - wait_call_t) > 0
    if not has_nb and has_blocking_comm:
        out.append("    overlap: all communication is blocking — no comm/compute overlap "
                   "possible. Consider nonblocking calls (Isend/Irecv or Iallreduce-style "
                   "collectives) + compute + Wait.")
    elif has_nb and wait_call_t / total >= 0.3:
        out.append("    overlap: nonblocking comm in use but Wait* is %.0f%% of MPI time — "
                   "little compute overlapped; move independent work between init and Wait."
                   % (wait_call_t / total * 100))
    elif has_nb:
        out.append("    overlap: nonblocking comm in use with low Wait* time — "
                   "good comm/compute overlap.")


# ------------------------------------------------------- anomaly / variance
def _stats(xs):
    n = len(xs)
    mean = sum(xs) / n
    var = sum((x - mean) ** 2 for x in xs) / n
    return mean, math.sqrt(var)


def _median(xs):
    s = sorted(xs)
    n = len(s)
    return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2.0


def anomaly_view(result_dir, out):
    """Cross-rank outlier + variance detection from the raw per-rank profiles.
    Surfaces the rank(s) deviating from the pack and the call that varies most
    across ranks — the signal that says *which* rank/region to chase."""
    profs = contract.prof_glob(result_dir)
    ranks = []
    for p in profs:
        d = _load_json(p)
        if d is None:
            continue
        busy = sum(f.get("t_excl", 0.0) for f in d.get("functions", []))
        per_fn = {f.get("function", ""): f.get("t_excl", 0.0) for f in d.get("functions", [])}
        ranks.append({"rank": d.get("rank", 0), "busy": busy, "fns": per_fn})
    if len(ranks) < 3:                     # <3 ranks → use the imbalance view instead
        return
    busies = [r["busy"] for r in ranks]
    mean, _ = _stats(busies)
    mx, mn = max(busies), min(busies)
    if mean <= 0:
        return
    out.append("\n══ Anomaly / variance (across %d ranks) ══" % len(ranks))
    spread = (mx - mn) / mx * 100.0 if mx else 0.0
    out.append("    per-rank busy time: min %.4fs  max %.4fs  median %.4fs  spread %.0f%%"
               % (mn, mx, _median(busies), spread))
    # robust outlier test: median + MAD (resistant to the outlier inflating σ).
    med = _median(busies)
    mad = _median([abs(b - med) for b in busies])
    worst = max(ranks, key=lambda r: abs(r["busy"] - med))
    dev = abs(worst["busy"] - med)
    if mad > 0:
        mz = 0.6745 * dev / mad           # modified z-score
        flag = mz > 3.5
        detail = "%.1f modified-z" % mz
    else:                                  # ties at the median → use relative gap
        flag = med > 0 and dev / med > 0.25
        detail = "%.0f%% off median" % (dev / med * 100.0 if med else 0)
    if flag and dev / med > 0.1:
        side = "slower" if worst["busy"] > med else "faster"
        out.append("    → rank %d is an outlier: %.0f%% %s than the median (%s)"
                   % (worst["rank"], dev / med * 100.0, side, detail))
    else:
        out.append("    → no outlier rank (all within the pack).")
    # call with the highest cross-rank variability (present on >=half the ranks)
    allfns = set().union(*(r["fns"].keys() for r in ranks))
    best = None
    for fn in allfns:
        vals = [r["fns"].get(fn, 0.0) for r in ranks]
        present = [v for v in vals if v > 0]
        if len(present) < len(ranks) / 2:
            continue
        fmean, fsd = _stats(vals)
        if fmean <= 1e-4:                  # ignore sub-0.1ms noise
            continue
        cv = fsd / fmean
        if best is None or cv > best[0]:
            best = (cv, fn, min(vals), max(vals))
    if best and best[0] > 0.15:
        out.append("    most variable call: %s (CV %.0f%%, %.4f..%.4fs across ranks)"
                   % (best[1][:24], best[0] * 100.0, best[2], best[3]))


# ----------------------------------------------------------- vectorization
def vectorization_view(snap, profile, out):
    """FP/SIMD efficiency. Prefers a real HWPC vectorization% if the platform
    exposes one; otherwise uses achieved GFLOP/s vs the empirical vector peak as
    a proxy and flags compute-bound code running far below it (scalar/under-vec).
    True per-loop SIMD width needs a compiler vec-report — pointed to below."""
    if not snap:
        return
    vec_pct = _m(snap, "vectorization_pct")
    g = _m(snap, "gflops")
    retiring = _m(snap, "topdown_retiring_pct")
    membound = _m(snap, "topdown_backend_mem_pct")
    pk = roofline.peaks()
    lines = []
    if vec_pct is not None:                       # real HWPC metric (e.g. Intel)
        lines.append("    vectorized FP ops %.1f%% (hardware counter)" % vec_pct)
        if vec_pct < 60 and (retiring or 0) > 30:
            lines.append("    → low SIMD utilization in compute-heavy code — see vec report below.")
    elif g and pk and pk.get("peak_gflops"):
        eff = g / pk["peak_gflops"] * 100.0
        lines.append("    FP efficiency %.1f%% of vector peak (%.0f / %.0f GFLOP/s)"
                     % (eff, g, pk["peak_gflops"]))
        # only meaningful when compute is the limiter, not memory
        compute_bound = (membound or 0) < 30
        if eff < 15 and compute_bound:
            lines.append("    → FP throughput far below vector peak in compute-bound code: "
                         "likely scalar / under-vectorized (or a reference library).")
    if not lines:
        return
    out.append("\n══ Vectorization ══")
    out.extend(lines)
    # candidates: hottest non-library compute kernels (library SIMD is the lib's job)
    fns = (profile or {}).get("functions", [])
    cand = sorted((f for f in fns if f.get("group") in ("USER", "ETC")
                   and f.get("t_excl", 0) > 0.005),
                  key=lambda f: -f.get("t_excl", 0))[:5]
    if cand:
        out.append("    inspect (hot user code): " + ", ".join(f.get("name", "")[:20] for f in cand))
    out.append("    confirm SIMD per loop with a compiler vec report "
               "(gcc -fopt-info-vec / icc -qopt-report=5) and build -O3 -march=native.")


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
