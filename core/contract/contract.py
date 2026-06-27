"""Shared result-contract conventions (schema v1).

Imported by core/cli (writes results) and core/analysis (reads them) so the two
never drift. See SCHEMA.md for the on-disk format.
"""
import os
import re
import glob

SCHEMA_VERSION = 1
MANIFEST = "manifest.json"
SNAP = "snap.json"


def prof_name(rank):
    return "prof.%d.json" % rank


def _prof_rank(path):
    """Embedded rank integer from a prof.<rank>.json path (-1 if absent), so we
    sort numerically — lexical sort would put prof.10.json before prof.2.json."""
    m = re.search(r"prof\.(\d+)\.json$", path)
    return int(m.group(1)) if m else -1


def prof_glob(result_dir):
    return sorted(glob.glob(os.path.join(result_dir, "prof.*.json")), key=_prof_rank)


def rank_from_env(env=None):
    """Global MPI rank from the launcher environment, else 0."""
    e = env if env is not None else os.environ
    # Same vars, SAME ORDER as the Rust `rank_from_env` (uaps-collect/src/lib.rs)
    # and the C profiler (collectors/profile/src/core/util.c) — all three must agree
    # or the tiers disagree on a process's rank. PALS_RANKID covers HPE/Cray PALS.
    for k in ("OMPI_COMM_WORLD_RANK", "PMI_RANK", "MV2_COMM_WORLD_RANK", "PMIX_RANK",
              "SLURM_PROCID", "PALS_RANKID", "ALPS_APP_PE"):
        v = e.get(k)
        if v:
            # Skip an empty/non-integer value and fall through (matches the Rust `parse`
            # and C `strtol` guards) rather than crashing the whole report on `int("x")`.
            try:
                return int(v)
            except (ValueError, TypeError):
                continue
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
    def _mtime(p):
        try:
            return os.path.getmtime(p)
        except OSError:
            return 0
    cand = sorted(glob.glob(os.path.join(cwd, "perf.*")),
                  key=lambda p: (_mtime(p), p))
    return cand[-1] if cand else None
