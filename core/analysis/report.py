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
import contract  # noqa: E402
import insights  # noqa: E402

PROFILE_REPORT = os.path.join(_ROOT, "collectors", "profile", "tools", "scilib-report.py")


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


def _render_snapshot(snap, out):
    out.append("\n── SNAPSHOT  (bird's-eye: hardware counters / roofline) " + "─" * 22)
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


def render(result_dir, fmt="text"):
    manifest = _load(os.path.join(result_dir, contract.MANIFEST)) or {}
    snap = _load(os.path.join(result_dir, contract.SNAP))
    profs = contract.prof_glob(result_dir)
    profile = _profile_json(profs)
    suite = insights.suite_insights(snap, profile)

    if fmt == "json":
        json.dump({"schema_version": contract.SCHEMA_VERSION, "manifest": manifest,
                   "snapshot": snap, "profile": profile, "insights": suite},
                  sys.stdout, indent=2)
        print()
        return

    out = ["=" * 78,
           "                        Performance Suite  (snapshot + profile)",
           "=" * 78]
    if manifest.get("command"):
        out.append(" command: %s" % " ".join(manifest["command"]))
    if snap:
        _render_snapshot(snap, out)
    else:
        out.append("\n(no snapshot — hardware counters unavailable or not collected)")
    out.append("\n── INSIGHTS  (combined snapshot + profile) " + "─" * 35)
    for s in suite:
        out.append("  ▶ " + s)
    print("\n".join(out))

    if profs:
        print("\n── PROFILE  (drill-down: sampling + sci-lib + MPI tracing) " + "─" * 19)
        sys.stdout.flush()   # ensure our header precedes the subprocess output
        subprocess.run([sys.executable, PROFILE_REPORT, "--no-observations"] + profs)
    else:
        print("\n(no profile data)")
