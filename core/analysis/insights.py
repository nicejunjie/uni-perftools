"""Unified insights engine — the one place advice is generated, reasoning over
BOTH datasets (snapshot counters + profile attribution). Replaces the snapshot's
own insights[] and the profile's Observations when run in the suite."""
import os
import sys
_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(_HERE), "contract"))
import contract  # noqa: E402


def _snap(metrics, key):
    for m in metrics:
        if m.get("key") == key:
            return m.get("value")
    return None


def suite_insights(snap, profile):
    """snap = snap.json dict (or None); profile = profile json dict (or None).
    Returns a list of cross-layer recommendation strings."""
    out = []
    metrics = (snap or {}).get("metrics", [])
    fns = (profile or {}).get("functions", [])
    runtime = (profile or {}).get("runtime_s", 0.0) or 0.0

    # Time-by-category: prefer the sampling dominant-group breakdown (accurate —
    # it charges e.g. read()-under-MPI to MPI via the stack). Express each
    # category as a FRACTION of total. Fall back to tracing t_excl if no samples.
    catfrac = {}
    groups = (profile or {}).get("groups") or {}
    gtotal = (profile or {}).get("group_total") or 0
    if groups and gtotal:
        for g, n in groups.items():
            c = contract.category_of(g)
            catfrac[c] = catfrac.get(c, 0.0) + n / float(gtotal)
    elif runtime > 0:
        for f in fns:
            c = contract.category_of(f.get("group", ""))
            catfrac[c] = catfrac.get(c, 0.0) + f.get("t_excl", 0.0) / runtime

    # top function (by exclusive time) and worst load imbalance, from tracing
    top = None
    worst_imb = None
    for f in fns:
        if top is None or f.get("t_excl", 0) > top.get("t_excl", 0):
            top = f
        if f.get("t_excl", 0) > 0.01 and (worst_imb is None or
                                          f.get("imb_excl", 0) > worst_imb.get("imb_excl", 0)):
            worst_imb = f

    mem = _snap(metrics, "memory_bound")
    vec = _snap(metrics, "vectorization_pct")
    numa = _snap(metrics, "numa_remote_pct")
    core_pct = _snap(metrics, "cpu_core_pct")
    cores_used = _snap(metrics, "cpu_cores_used")
    threads = _snap(metrics, "max_threads")
    ipc = _snap(metrics, "ipc")
    mpi_f = catfrac.get("MPI", 0.0)
    io_f = catfrac.get("IO", 0.0)
    lib_f = catfrac.get("math-libs", 0.0)

    # --- cross-layer rules ------------------------------------------------
    if mem is not None and mem >= 30 and lib_f > 0.2 and top is not None and \
            contract.category_of(top.get("group", "")) == "math-libs":
        out.append("Memory-bound (%.0f%% of slots) and %s dominates compute — "
                   "cache-block / raise arithmetic intensity, check NUMA placement."
                   % (mem, top["name"]))
    elif mem is not None and mem >= 30:
        out.append("Memory-bound (%.0f%% of slots) — improve data locality / working-set size."
                   % mem)

    if mpi_f >= 0.2:
        msg = "MPI is %.0f%% of time" % (mpi_f * 100)
        mpi_imb = max((f.get("imb_excl", 0) for f in fns if f.get("group") == "MPI"), default=0)
        if mpi_imb >= 30:
            msg += (" with %.0f%% imbalance — rebalance work or check rank placement; "
                    "see the communication matrix." % mpi_imb)
        else:
            msg += " — communication-heavy; enlarge messages / overlap comm with compute."
        out.append(msg)

    if io_f >= 0.15:
        out.append("I/O is %.0f%% of time — batch/buffer I/O or use parallel I/O; see the I/O table."
                   % (io_f * 100))

    if vec is not None and vec < 30 and lib_f < 0.2 and mem is not None and mem < 30:
        out.append("Low vectorization (%.0f%%) on compute-bound code — check SIMD / compiler flags."
                   % vec)

    if numa is not None and numa >= 10:
        out.append("NUMA remote access %.0f%% — bind threads/memory (first-touch / numactl)." % numa)

    # Oversubscription / idle parallelism: many threads but few cores kept busy means
    # workers spin idle (OpenMP) rather than doing work — it surfaces as do_spin /
    # futex / poll dominating the sampling profile and buries the real hotspots.
    oversub = False
    if threads and cores_used and threads >= 2:
        par_eff = cores_used / threads * 100.0
        if par_eff < 50 and (threads - cores_used) >= 2:
            oversub = True
            out.append("Parallel efficiency ~%.0f%%: %d threads but only %.1f cores busy on "
                       "average — likely oversubscribed or idle OpenMP spin (do_spin/futex). "
                       "Match the thread count to the work you have, or widen the parallel "
                       "regions." % (par_eff, int(threads), cores_used))

    # Pipeline mostly stalled (low IPC) with no library/MPI/I-O hotspot to blame: the
    # cores are waiting (memory latency or synchronization), not computing.
    if not oversub and ipc is not None and ipc < 0.5 and lib_f < 0.3 \
            and mpi_f < 0.2 and io_f < 0.15:
        out.append("Low IPC %.2f (CPI %.1f) with no dominant library/MPI/I-O hotspot — the "
                   "pipeline is stalled (memory latency or synchronization), not "
                   "compute-throughput-bound." % (ipc, (1.0 / ipc) if ipc else 0.0))

    if worst_imb is not None and worst_imb.get("imb_excl", 0) >= 30 and \
            worst_imb.get("group") != "MPI":
        out.append("%s is %.0f%% load-imbalanced across ranks — uneven work distribution."
                   % (worst_imb["name"], worst_imb["imb_excl"]))

    # characterization: where the compute time concentrates (even if no problem)
    if lib_f >= 0.3 and top is not None and contract.category_of(top.get("group", "")) == "math-libs":
        eff = "" if mem is not None and mem >= 30 else " (running efficiently)"
        out.append("Compute is dominated by math libraries (%.0f%%, top: %s)%s — the main tuning target."
                   % (lib_f * 100, top["name"], eff))

    if not out:
        if core_pct is not None and core_pct < 50:
            out.append("CPU utilization is low (%.0f%% of cores) with no single dominant "
                       "hotspot — the run is latency- or idle-bound; look at thread activity "
                       "and synchronization rather than per-function tuning." % core_pct)
        else:
            out.append("No dominant bottleneck detected; the run looks reasonably balanced and efficient.")
    return out
