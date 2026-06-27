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

    # GPU offload makes the CPU-only verdict misleading (the real compute is on the
    # device), so lead with it — it reframes every CPU metric below as host-side only.
    gpu = _snap(metrics, "gpu_offload")
    if gpu:
        out.append("GPU offload detected — uaps measures only CPU counters, so the metrics below "
                   "are host-side CPU work and MISS all device compute (the roofline is suppressed). "
                   "Profile GPU kernels with nsight / rocprof / VTune.")

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

    # I/O-bound (snapshot signal): the snapshot can't time-categorize like the profile
    # tier, but a large sampled I/O-wait fraction — or real I/O volume while the cores
    # sit idle — means time is going to I/O, not compute. Surface that instead of the
    # generic "stalled on memory / synchronization" verdict further down.
    io_wait = _snap(metrics, "io_wait")
    elapsed = _snap(metrics, "elapsed_time")
    io_vol = (_snap(metrics, "io_read") or 0) + (_snap(metrics, "io_write") or 0)
    io_wait_frac = (io_wait / elapsed) if (io_wait and elapsed and elapsed > 0) else 0.0
    io_bound = io_wait_frac >= 0.25 or (io_vol > 8e6 and (core_pct or 0) < 10 and io_f < 0.15)
    if io_bound:
        parts = ["Likely I/O-bound"]
        if io_wait_frac >= 0.25:
            parts.append("~%.0f%% of wall in I/O wait" % (io_wait_frac * 100))
        if io_vol > 0:
            parts.append("%.0f MB moved" % (io_vol / 1e6))
        out.append(", ".join(parts) + " — batch/buffer I/O or use parallel I/O; "
                   "profile with upat for the per-call I/O time breakdown.")

    # --- cross-layer rules ------------------------------------------------
    # (memory-bound suppressed when the run is I/O-bound — the few CPU cycles an idle
    #  I/O job spends can read as memory-bound, which would misdirect.)
    if not io_bound and mem is not None and mem >= 30 and lib_f > 0.2 and top is not None and \
            contract.category_of(top.get("group", "")) == "math-libs":
        out.append("Memory-bound (%.0f%% of slots) and %s dominates compute — "
                   "cache-block / raise arithmetic intensity, check NUMA placement."
                   % (mem, top["name"]))
    elif not io_bound and mem is not None and mem >= 30:
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

    # OpenMP active-spin caveat: idle threads busy-wait, so per-thread CPU time can't
    # reveal imbalance (every thread looks busy) and parallel efficiency reads high even
    # when work is skewed. The measured imbalance is a lower bound — say so.
    if _snap(metrics, "omp_spin_wait"):
        out.append("OpenMP active-spin (OMP_WAIT_POLICY≠passive): idle threads busy-wait, so the "
                   "measured thread imbalance / parallel efficiency are a LOWER BOUND — re-run "
                   "with OMP_WAIT_POLICY=passive to measure them accurately.")

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
    if not oversub and not io_bound and ipc is not None and ipc < 0.5 and lib_f < 0.3 \
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
