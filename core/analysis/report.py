"""Suite analysis brain: render a result directory as one combined report.

Snapshot section (bird's-eye, from snap.json) then profile section (drill-down,
delegated to the profile collector's reporter over prof.*.json). This is the one
place suite reporting lives; the collectors only emit data.
"""
import os
import sys
import json
import subprocess

_HERE = os.path.dirname(os.path.abspath(__file__))
_ROOT = os.path.dirname(os.path.dirname(_HERE))           # repo root
sys.path.insert(0, os.path.join(_ROOT, "core", "contract"))
import contract   # noqa: E402
import insights   # noqa: E402
import viewpoints  # noqa: E402

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
}
VIEW_ORDER = ["roofline", "roofline-func", "microarch", "memory", "vectorization",
              "threading", "mpi", "imbalance", "anomaly"]

PROFILE_REPORT = os.path.join(_ROOT, "collectors", "profile", "tools", "upat-report.py")


def _profile_json(profs):
    if not profs:
        return None
    r = subprocess.run([sys.executable, PROFILE_REPORT, "--format", "json"] + profs,
                       capture_output=True, text=True)
    try:
        return json.loads(r.stdout)
    except Exception:
        return None

# roofline + characterization keys worth surfacing, in order
SNAP_KEYS = ["elapsed_time", "cpu_core_pct", "ipc", "cpi",
             "dp_gflops", "sp_gflops", "arith_intensity", "peak_gflops", "peak_bw_gbs",
             "memory_bound", "dram_bound_pct", "numa_remote_pct", "vectorization_pct",
             "retiring_pct", "backend_bound_pct", "peak_rss", "disk_read", "disk_write"]


def _load(path):
    try:
        return json.load(open(path))
    except Exception:
        return None


# which viewpoints belong to which collector's report
UAPS_VIEWS = ["roofline", "microarch", "memory", "vectorization", "threading"]
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
    m = {x["key"]: x for x in snap.get("metrics", [])}
    # NOTE: snapshot's own insights[] are intentionally not shown here — the
    # unified suite insights engine (below) replaces them.
    # roofline line if the collector provided the pieces
    if "arith_intensity" in m and "dp_gflops" in m:
        out.append("  roofline: AI %s, achieved %s (peak %s, BW %s)" % (
            m["arith_intensity"]["display"], m["dp_gflops"]["display"],
            m.get("peak_gflops", {}).get("display", "?"),
            m.get("peak_bw_gbs", {}).get("display", "?")))
    cells = ["%s %s" % (m[k]["label"], m[k]["display"]) for k in SNAP_KEYS if k in m]
    for i in range(0, len(cells), 2):
        out.append("    " + "   |   ".join(cells[i:i + 2]))


def render(result_dir, fmt="text", view="all", collector="both", detail=None, threshold=0.1):
    manifest = _load(os.path.join(result_dir, contract.MANIFEST)) or {}
    snap = _load(os.path.join(result_dir, contract.SNAP))
    profs = contract.prof_glob(result_dir)
    profile = _profile_json(profs)

    if fmt == "json":
        suite = insights.suite_insights(snap, profile)
        json.dump({"schema_version": contract.SCHEMA_VERSION, "manifest": manifest,
                   "snapshot": snap, "profile": profile, "insights": suite},
                  sys.stdout, indent=2)
        print()
        return

    if fmt == "html":
        import htmlrep
        suite = insights.suite_insights(snap, profile)
        print(htmlrep.build(result_dir, manifest, snap, profile, suite, detail=detail, threshold=threshold))
        return

    # focused per-facility detail (a UPAT post-recording analysis)
    if detail:
        if not profs:
            print("(no profile data for --detail %s)" % detail)
            return
        print("\n".join(UPAT_BANNER))
        sys.stdout.flush()
        subprocess.run([sys.executable, PROFILE_REPORT, "--detail", detail] + profs)
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
                            "--threshold", str(threshold)] + profs)
        return

    suite = insights.suite_insights(snap, profile)
    # snapshot section only when snap.json is actually present (e.g. a uaps run
    # into the same dir); otherwise this is a pure upat (deep-tier) report.
    do_uaps = collector in ("both", "uaps") and snap is not None
    do_upat = collector in ("both", "upat")

    head = []
    if collector == "both":
        head += ["=" * 78,
                 "                        Universal Performance Tools",
                 "=" * 78]
    if manifest.get("command"):
        head.append(" command: %s" % " ".join(manifest["command"]))
    head.append("\n── INSIGHTS " + "─" * 66)
    for s in suite:
        head.append("  ▶ " + s)
    print("\n".join(head))

    if do_uaps:
        out = ["", ""] + UAPS_BANNER
        if snap:
            _render_snapshot(snap, out)
        else:
            out.append("  (no snapshot — hardware counters unavailable or not collected)")
        for v in UAPS_VIEWS:
            _run_view(v, snap, profile, result_dir, out)
        print("\n".join(out))

    if do_upat:
        out = ["", ""] + UPAT_BANNER
        for v in UPAT_VIEWS:
            _run_view(v, snap, profile, result_dir, out)
        print("\n".join(out))
        if profs:
            sys.stdout.flush()   # our banner precedes the subprocess tables
            subprocess.run([sys.executable, PROFILE_REPORT, "--no-observations",
                            "--no-header", "--threshold", str(threshold)] + profs)
        else:
            print("\n(no profile data)")
