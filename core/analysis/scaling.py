"""Scaling viewpoint: compare multiple result dirs (strong/weak scaling)."""
import os
import sys
import json
import glob
_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(_HERE), "contract"))
import contract  # noqa: E402


def _info(result):
    man = {}
    try:
        man = json.load(open(os.path.join(result, contract.MANIFEST)))
    except Exception:
        pass
    nranks = man.get("nranks") or len(contract.prof_glob(result)) or 1
    runtime = 0.0
    snap = os.path.join(result, contract.SNAP)
    if os.path.exists(snap):
        try:
            for m in json.load(open(snap)).get("metrics", []):
                if m["key"] == "elapsed_time":
                    runtime = m["value"]
        except Exception:
            pass
    if runtime <= 0:                       # fall back to profile runtime
        for p in contract.prof_glob(result):
            try:
                runtime = max(runtime, json.load(open(p)).get("runtime_s", 0.0))
            except Exception:
                pass
    return nranks, runtime


def render(results, weak=False):
    rows = []
    for r in results:
        for d in (sorted(glob.glob(r)) if any(c in r for c in "*?[") else [r]):
            n, t = _info(d)
            rows.append((n, t, d))
    rows.sort()
    if not rows:
        sys.exit("scale: no results")
    base_n, base_t = rows[0][0], rows[0][1]
    if base_t <= 0:                        # no runtime for the baseline → speedups meaningless
        print("\n══ Scaling ══")
        print("  (scaling unavailable — no runtime recorded for the smallest run)")
        return
    print("\n══ Scaling (%s) ══" % ("weak" if weak else "strong"))
    print("  %-30s %6s %10s %9s %11s" % ("result", "ranks", "time(s)", "speedup", "efficiency"))
    for n, t, d in rows:
        sp = base_t / t if t > 0 else 0.0
        if weak:
            # WEAK scaling: work grows with ranks, so the IDEAL runtime is CONSTANT and
            # efficiency = T_base / T_n (NOT divided by n — that's the strong-scaling
            # formula, which would report perfect weak scaling as ~1/N ≈ 0%).
            eff = sp * 100.0
        else:
            ideal = n / base_n if base_n else 1
            eff = sp / ideal * 100.0 if ideal else 0.0
        print("  %-30s %6d %10.3f %8.2fx %10.0f%%" % (os.path.basename(d.rstrip("/")), n, t, sp, eff))
    if weak:
        print("  (weak: work grows with ranks → ideal time is constant; efficiency = T_base/T_n)")
    else:
        print("  (strong: speedup & efficiency vs the smallest run; >100%% = super-linear)")
