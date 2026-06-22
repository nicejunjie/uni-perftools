"""Shared result-contract conventions (schema v1).

Imported by core/cli (writes results) and core/analysis (reads them) so the two
never drift. See SCHEMA.md for the on-disk format.
"""
import os
import glob

SCHEMA_VERSION = 1
MANIFEST = "manifest.json"
SNAP = "snap.json"


def prof_name(rank):
    return "prof.%d.json" % rank


def prof_glob(result_dir):
    return sorted(glob.glob(os.path.join(result_dir, "prof.*.json")))


def rank_from_env(env=None):
    """Global MPI rank from the launcher environment, else 0."""
    e = env if env is not None else os.environ
    for k in ("OMPI_COMM_WORLD_RANK", "PMI_RANK", "MV2_COMM_WORLD_RANK", "PMIX_RANK"):
        if e.get(k):
            return int(e[k])
    return 0


# ---- the single suite-wide imbalance definition --------------------------
def imbalance(counts, nranks):
    """CrayPAT-style imbalance over participating ranks.
    Returns (pct, absolute) where pct = (max-avg)/max*100 and absolute = max-avg.
    nranks = denominator for the average (participating ranks)."""
    if not counts or nranks <= 0:
        return 0.0, 0.0
    total = float(sum(counts))
    avg = total / nranks
    mx = float(max(counts))
    if mx <= 0:
        return 0.0, 0.0
    return (mx - avg) / mx * 100.0, (mx - avg)


# ---- bird's-eye category map (group/DSO -> category) ---------------------
# profile "group" field and snapshot DSO names both fold into these buckets.
CATEGORY = {
    "BLAS": "math-libs", "LAPACK": "math-libs", "PBLAS": "math-libs",
    "ScaLAPACK": "math-libs", "CBLAS": "math-libs", "LAPACKe": "math-libs",
    "FFTW": "math-libs",
    "MPI": "MPI",
    "IO": "IO",
    "USER": "compute", "WAIT": "idle", "ETC": "system",
}


def category_of(group):
    return CATEGORY.get(group, "compute")


# ---- result-directory discovery ------------------------------------------
def latest_result(cwd="."):
    cand = sorted(glob.glob(os.path.join(cwd, "perf.*")),
                  key=lambda p: os.path.getmtime(p) if os.path.exists(p) else 0)
    return cand[-1] if cand else None
