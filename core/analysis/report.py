"""Suite analysis brain: render a result directory as a single-tier report.

uaps and upat are independent cost tiers; a report covers exactly one of them —
either the snapshot bird's-eye (from snap.json) or the deep profile (from
prof.*.json, delegated to the profile collector's reporter). There is no combined
report. The shared Environment header (run/machine/software/measurement) appears
in both, but the analysis never mixes tiers. This is the one place suite reporting
lives; the collectors only emit data.
"""
import os
import sys
import json
import subprocess

_HERE = os.path.dirname(os.path.abspath(__file__))
_ROOT = os.path.dirname(os.path.dirname(_HERE))           # repo root
sys.path.insert(0, os.path.join(_ROOT, "core", "contract"))
sys.path.insert(0, os.path.join(_ROOT, "core", "roofline"))
import contract   # noqa: E402
import insights   # noqa: E402
import viewpoints  # noqa: E402
import roofline    # noqa: E402

VIEWS = {
    "roofline":  lambda snap, prof, out: viewpoints.roofline_view(snap, prof, out),
    "microarch": lambda snap, prof, out: viewpoints.microarch_view(snap, out),
    "memory":    lambda snap, prof, out: viewpoints.memory_view(snap, out),
    "imbalance": lambda snap, prof, out: viewpoints.imbalance_view(prof, out),
    "threading": lambda snap, prof, out: viewpoints.threading_view(snap, out),
    "mpi":       lambda snap, prof, out: viewpoints.mpi_view(prof, out),
    "mpi-summary": lambda snap, prof, out: viewpoints.mpi_summary_view(snap, prof, out),
    "vectorization": lambda snap, prof, out: viewpoints.vectorization_view(snap, prof, out),
    "roofline-func": lambda snap, prof, out: viewpoints.roofline_func_view(prof, out),
    "mpi-snapshot": lambda snap, prof, out: viewpoints.mpi_snapshot_view(snap, out),
    "scheduling": lambda snap, prof, out: viewpoints.scheduling_view(snap, out),
}
VIEW_ORDER = ["roofline", "roofline-func", "microarch", "memory", "vectorization",
              "threading", "mpi", "imbalance", "anomaly"]

PROFILE_REPORT = os.path.join(_ROOT, "collectors", "profile", "tools", "upat-report.py")


def _profile_json(result_dir, profs):
    if not profs:
        return None
    # Pass the result dir, not the N expanded prof.*.json paths: the child globs
    # them itself, avoiding an E2BIG argv at tens of thousands of ranks.
    r = subprocess.run([sys.executable, PROFILE_REPORT, "--format", "json", result_dir],
                       capture_output=True, text=True)
    try:
        return json.loads(r.stdout)
    except Exception:
        return None

# roofline + characterization keys worth surfacing, in order.
# NOTE: keys must match what the snapshot collector actually emits (see snap.json):
# it produces `gflops` and `topdown_*_pct`, not dp/sp_gflops or retiring/backend_pct.
SNAP_KEYS = ["elapsed_time", "cpu_core_pct", "ipc", "cpi",
             "gflops", "memory_bound", "dram_bound_pct", "numa_remote_pct",
             "topdown_retiring_pct", "topdown_backend_pct", "peak_rss",
             "disk_read", "disk_write"]


def _load(path):
    try:
        return json.load(open(path))
    except Exception:
        return None


# which viewpoints belong to which collector's report
UAPS_VIEWS = ["roofline", "microarch", "memory", "vectorization", "mpi-snapshot",
              "scheduling", "threading"]
UPAT_VIEWS = ["roofline-func", "mpi-summary", "mpi", "imbalance", "anomaly"]

UAPS_BANNER = ["─" * 78,
               "  UAPS  —  Universal Application Performance Snapshot   (bird's-eye: HW counters)",
               "─" * 78]
UPAT_BANNER = ["─" * 78,
               "  UPAT  —  Universal Performance Analysis Tool   (deep profile: tracing + sampling)",
               "─" * 78]


def _run_view(v, snap, profile, result_dir, out):
    try:
        if v == "anomaly":                       # needs raw per-rank files
            viewpoints.anomaly_view(result_dir, out)
        elif v in VIEWS:
            VIEWS[v](snap, profile, out)
    except Exception as e:
        out.append("\n(%s viewpoint failed: %s)" % (v, e))


def _render_snapshot(snap, out):
    """Headline Performance section + a dedicated I/O section, one metric per line.
    Detailed top-down/cache/DRAM/vec live in their own ══ sections. Snapshot's own
    insights[] are omitted — the unified engine supplies them."""
    m = {x["key"]: x for x in snap.get("metrics", [])}

    def disp(k):
        return m[k]["display"] if k in m else None

    def val(k):
        return m.get(k, {}).get("value")

    pk = roofline.peaks() or {}
    out.append("\n══ Performance ══")
    if disp("elapsed_time"):
        out.append("    %-26s %s" % ("Elapsed time", disp("elapsed_time")))
    if disp("gflops"):
        peak, g = roofline.peak_compute(pk, "dp"), val("gflops")
        extra = ""
        if peak and g:
            p = g / peak * 100.0
            # Keep precision for small fractions — rounding 0.18% to "0%" reads
            # like missing data on memory-bound code.
            pct = ("%.0f%%" % p if p >= 10 else "%.1f%%" % p if p >= 1 else "%.2f%%" % p)
            extra = " (%s of FP64 peak)" % pct
        out.append("    %-26s %s%s" % ("FP throughput", disp("gflops"), extra))
    if disp("cpu_freq_ghz"):
        out.append("    %-26s %s" % ("Avg CPU frequency", disp("cpu_freq_ghz")))
    if disp("cpu_core_pct"):
        cu = val("cpu_cores_used")
        nr = int(val("nranks") or 1)
        # In an aggregate cpu_cores_used is SUMMED across ranks while cpu_core_pct is the
        # per-rank MEAN — so the cores-busy is a job total, not "% of this line". Label it
        # so "4% (100 cores busy)" can't be misread as one rank using 100 cores.
        if cu and nr > 1:
            extra = " (avg per rank; %.0f cores busy summed across %d ranks)" % (cu, nr)
        elif cu:
            extra = " (%.1f cores busy)" % cu
        else:
            extra = ""
        out.append("    %-26s %s%s" % ("Physical core utilization", disp("cpu_core_pct"), extra))
    if disp("ipc"):
        out.append("    %-26s %s" % ("IPC (instructions/cycle)", disp("ipc")))
    if disp("cpi"):
        out.append("    %-26s %s" % ("CPI (cycles/instruction)", disp("cpi")))
    if "peak_rss" in m:        # memory footprint (a resource metric, not a cache/access one)
        out.append("    %-26s %s" % (m["peak_rss"]["label"], disp("peak_rss")))

    # Per-rank HW imbalance (max vs avg across ranks) — only present for a per-rank
    # (APS-style) aggregate. This is the microarchitectural spread the old
    # launcher-node snapshot could not see; (max-avg)/max, suite-wide definition.
    imb = [(lbl, disp(k)) for k, lbl in
           [("gflops_imbalance_pct", "FP throughput"),
            ("ipc_imbalance_pct", "IPC"),
            ("memory_bound_imbalance_pct", "memory bound"),
            ("cpu_time_imbalance_pct", "CPU time"),
            ("elapsed_imbalance_pct", "wall time")] if disp(k)]
    if imb:
        out.append("\n══ Cross-rank imbalance (HW, max vs avg) ══")
        for lbl, v in imb:
            out.append("    %-26s %s" % (lbl, v))

    elapsed = val("elapsed_time") or 0

    def _bw(b, secs):
        if not b or not secs or secs <= 0:
            return None
        r = b / secs
        for unit, div in (("GB/s", 1e9), ("MB/s", 1e6), ("KB/s", 1e3)):
            if r >= div:
                return "%.1f %s" % (r / div, unit)
        return "%.0f B/s" % r

    def _rate(k):
        return _bw(val(k), elapsed)

    io = []
    for k, lbl in [("disk_read", "disk read"), ("disk_write", "disk write"),
                   ("io_read", "logical read"), ("io_write", "logical write")]:
        if disp(k):
            spd = _rate(k)                       # bandwidth = volume ÷ elapsed
            io.append((lbl, disp(k) + ("   @ %s" % spd if spd else "")))
    # Render the section if there's byte volume OR an I/O-wait time — a job blocked in
    # 'D' on mmap page-faults-over-NFS has io_wait but no read()/write() byte counters,
    # and the headline insight may already call it I/O-bound, so the detail must show.
    if io or disp("io_wait"):
        out.append("\n══ I/O ══")
        for lbl, v in io:
            out.append("    %-26s %s" % (lbl, v))
        # I/O-wait time (sampled D-state fraction × wall). The per-process refinements
        # below (confidence, CPU cross-check, I/O-active rate) pair io_wait with the
        # sample count / cpu_time / byte volume; under `uaps report` aggregation io_wait
        # is the worst rank's (MAX) while bytes & cpu_time are SUMMED, so those pairings
        # are nonsense across ranks — gate them to a single-process snapshot.
        if disp("io_wait") and elapsed:
            w = val("io_wait") or 0.0
            frac = min(1.0, w / elapsed) if elapsed > 0 else 0.0
            if int(val("nranks") or 1) > 1:
                # MAX io_wait and MAX elapsed can come from different ranks, so this %
                # is a lower bound over the JOB wall, not that rank's own wall.
                out.append("    %-26s %s  (worst rank; ≥%.0f%% of job wall)"
                           % ("I/O wait (est.)", disp("io_wait"), frac * 100))
            else:
                n = int(val("io_wait_samples") or 0)
                notes = ["%.0f%% of elapsed" % (frac * 100)]
                # statistical confidence: SE = sqrt(p(1-p)/N); ±1.96·SE for ~95%.
                if n >= 1:
                    ci = 1.96 * (max(0.0, frac * (1 - frac) / n) ** 0.5) * 100.0
                    if n < 20:
                        notes.append("LOW confidence: only %d samples" % n)
                    elif ci >= 1.0:
                        notes.append("±%.0f%%, %d samples" % (ci, n))
                    else:
                        notes.append("%d samples" % n)
                # cross-check vs measured off-CPU time (= 1 − CPU-time/threads ÷ elapsed).
                thr = val("max_threads") or 1
                off = max(0.0, 1.0 - (val("cpu_time") or 0.0) / (elapsed * max(thr, 1)))
                if off > 0.02:
                    if frac > off * 1.15:
                        notes.append("⚠ exceeds %.0f%% off-CPU — may overcount" % (off * 100))
                    elif frac >= off * 0.85:
                        notes.append("✓ matches %.0f%% off-CPU" % (off * 100))
                    else:
                        notes.append("of %.0f%% off-CPU (rest non-I/O idle)" % (off * 100))
                out.append("    %-26s %s  (%s)" % ("I/O wait (est.)", disp("io_wait"), "; ".join(notes)))
                # I/O-ACTIVE bandwidth = bytes ÷ time actually in I/O — recovers the device
                # rate a mixed compute+I/O run's avg (÷ elapsed) understates. Uses PHYSICAL
                # block bytes (disk_*), not logical (rchar/wchar): io_wait is uncached
                # D-state time, so logical bytes — which include page-cache hits that never
                # block — paired with it would blow up to absurd rates. Skip when ≈ avg rate.
                if w and w < 0.8 * elapsed:
                    tot = (val("disk_read") or 0) + (val("disk_write") or 0)  # block I/O ↔ io_wait
                    act = _bw(tot, w)
                    if act:
                        out.append("    %-26s %s  (physical block I/O, over %.1fs of I/O wait)"
                                   % ("I/O-active rate", act, w))


def render(result_dir, fmt="text", view="all", collector="upat", detail=None, threshold=0.1):
    # Use the calibration captured when this run was collected (its own host's
    # ceilings), not whatever is in the per-build cache.
    roofline.use_result(result_dir)
    manifest = _load(os.path.join(result_dir, contract.MANIFEST)) or {}
    snap = _load(os.path.join(result_dir, contract.SNAP))
    profs = contract.prof_glob(result_dir)
    profile = _profile_json(result_dir, profs)

    # uaps and upat are independent cost tiers — a report covers exactly ONE of
    # them, never both. The active tier owns the analysis (insights + sections);
    # the other tier's data is NOT folded in. (The Environment header is shared
    # run/machine/software *metadata*, so both reports describe the full system.)
    a_snap = snap if collector == "uaps" else None
    a_profile = profile if collector == "upat" else None

    if fmt == "json":
        suite = insights.suite_insights(a_snap, a_profile)
        payload = {"schema_version": contract.SCHEMA_VERSION, "manifest": manifest,
                   "insights": suite}
        payload["snapshot" if collector == "uaps" else "profile"] = \
            snap if collector == "uaps" else profile
        json.dump(payload, sys.stdout, indent=2)
        print()
        return

    if fmt == "html":
        import htmlrep
        suite = insights.suite_insights(a_snap, a_profile)
        print(htmlrep.build(result_dir, manifest, snap, profile, suite, detail=detail,
                            threshold=threshold, collector=collector))
        return

    # focused per-facility detail (a UPAT post-recording analysis)
    if detail:
        if not profs:
            print("(no profile data for --detail %s)" % detail)
            return
        print("\n".join(UPAT_BANNER))
        sys.stdout.flush()
        subprocess.run([sys.executable, PROFILE_REPORT, "--detail", detail, result_dir])
        return

    # focused single viewpoint: print just that section (no banners/insights)
    if view not in ("all", None):
        if view != "hotspots":
            out = []
            _run_view(view, snap, profile, result_dir, out)
            if out:
                print("\n".join(out))
        if view == "hotspots" and profs:
            subprocess.run([sys.executable, PROFILE_REPORT, "--no-observations",
                            "--threshold", str(threshold), result_dir])
        return

    do_uaps = collector == "uaps"
    suite = insights.suite_insights(a_snap, a_profile)

    # Shared header: the tier's own banner, the full Environment (run/machine/
    # software/measurement — shared metadata), then tier-scoped insights. The
    # profiler additionally leads with where wall time went (a sampling product).
    head = list(UAPS_BANNER if do_uaps else UPAT_BANNER)
    viewpoints.environment_view(result_dir, manifest, snap, profile, head, collector=collector)
    head.append("\n── INSIGHTS " + "─" * 66)
    for s in suite:
        head.append("  ▶ " + s)
    if not do_uaps:
        viewpoints.time_breakdown_view(profile, head)
    print("\n".join(head))

    out = [""]
    if do_uaps:
        if snap:
            _render_snapshot(snap, out)
        else:
            out.append("  (no snapshot — hardware counters unavailable or not collected)")
        for v in UAPS_VIEWS:                       # snapshot analysis only (a_profile=None)
            _run_view(v, snap, None, result_dir, out)
        print("\n".join(out))
        return

    for v in UPAT_VIEWS:                           # deep-profile analysis only (a_snap=None)
        _run_view(v, None, profile, result_dir, out)
    print("\n".join(out))
    if profs:
        sys.stdout.flush()                         # our banner precedes the subprocess tables
        subprocess.run([sys.executable, PROFILE_REPORT, "--no-observations",
                        "--no-header", "--threshold", str(threshold), result_dir])
    else:
        print("\n(no profile data)")
