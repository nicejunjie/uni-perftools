#!/bin/bash
# Cross-MPI / launcher-agnostic validation for uaps.
#
# uaps claims to be "launcher-agnostic": each rank detects its OWN rank from the
# launcher's environment (rank_from_env in
# collectors/snapshot/crates/uaps-collect/src/lib.rs) and writes snap.<rank>.json,
# with NO launcher flag-parsing and NO -x propagation. That contract must hold for
# EVERY launcher family, and the rank var SET + PRECEDENCE must agree across the
# three implementations that all parse it:
#   - Rust  collectors/snapshot/crates/uaps-collect/src/lib.rs   (uaps -> snap.<r>.json)
#   - C     collectors/profile/src/core/util.c                   (upat -> prof.<r>.json)
#   - Python core/contract/contract.py                           (the shared contract)
#
# Part B (always runs): drive uaps with ONLY one rank env var set per scheme and
#   assert the right snap.<rank>.json appears (deterministic, no real MPI needed),
#   plus precedence and world-size-var detection.
# Part A (runs iff a locally-built MPICH is present): build the PMPI shim against
#   MPICH (different ABI: int handles, Hydra/PMI_RANK) and prove it still works.
#
# Self-contained: writes only under tests/scale/out and tests/scale/mpich.
# Usage: bash tests/scale/cross_mpi.sh
set -u
cd "$(dirname "$0")"
HERE=$(pwd); ROOT=$(cd ../.. && pwd)

UAPS=""
for c in release debug; do p="$ROOT/collectors/snapshot/target/$c/uaps"; [ -x "$p" ] && { UAPS="$p"; break; }; done
[ -n "$UAPS" ] || { echo "uaps not built — run 'cargo build --release' first"; exit 1; }

mkdir -p out
# Tiny no-MPI workload: burns a few FMAs and exits (exec'd, not forked). All uaps
# needs is a real process whose rank it reads from the env — no MPI lib required.
TINY="$HERE/out/tiny"
cc -O2 -o "$TINY" flops_by_rank.c || { echo "tiny build failed"; exit 1; }
trap 'rm -rf "$HERE"/out/cm_*.d "$HERE"/out/*.cm.json "$TINY" "$HERE"/out/uaps_mpi_mpich.so "$HERE"/out/mpich_hello "$HERE"/out/cm_mpich.d "$HERE"/out/cm_mpich.json' EXIT

val(){ grep -oE "\"key\": \"$2\"[^}]*\"value\": [-0-9.eE]+" "$1" 2>/dev/null | grep -oE "[-0-9.eE]+$" | head -1; }
pass=0; fail=0; skip=0
ok(){ echo "  PASS: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
skp(){ echo "  SKIP: $1"; skip=$((skip+1)); }

# Clear every rank/size var so a leaked one can't taint a single-scheme test.
unset OMPI_COMM_WORLD_RANK PMI_RANK PMIX_RANK MV2_COMM_WORLD_RANK SLURM_PROCID PALS_RANKID ALPS_APP_PE
unset OMPI_COMM_WORLD_SIZE PMI_SIZE MV2_COMM_WORLD_SIZE SLURM_NTASKS SLURM_NPROCS PALS_NRANKS

# Run uaps with a given env assignment (string of VAR=VAL pairs) into a fresh dir.
#   detect <dir> <"VAR=VAL ...">
detect(){ local dir="$1"; shift; rm -rf "$dir"; env "$@" "$UAPS" run --rank-dir "$dir" -- "$TINY" 2000000 >/dev/null 2>"$dir.e"; }

echo "================ uaps cross-MPI / launcher-agnostic validation ================"

# ============================================================================
# PART B.0 — SOURCE consistency: the rank-var list (set + precedence) must be
# IDENTICAL in all three implementations, or the tiers disagree on a process's
# rank. This is the regression guard for the cross-impl drift fixed earlier.
# ============================================================================
echo
echo "== [B0] rank-var list source consistency (Rust == C == Python) =="
python3 - "$ROOT" <<'PY'
import re, sys
root = sys.argv[1]
def grab(path, pat):
    m = re.search(pat, open(path).read(), re.S)
    return re.findall(r'"([A-Z0-9_]+)"', m.group(1)) if m else []
rust = grab(root + "/collectors/snapshot/crates/uaps-collect/src/lib.rs",
            r'const KEYS:\s*\[&str;\s*7\]\s*=\s*\[(.*?)\]')
c    = grab(root + "/collectors/profile/src/core/util.c",
           r'RANK_ENV\[\]\s*=\s*\{(.*?)\}')
py   = grab(root + "/core/contract/contract.py",
           r'def rank_from_env.*?for k in \((.*?)\):')
print("   Rust  :", " ".join(rust))
print("   C     :", " ".join(c))
print("   Python:", " ".join(py))
sys.exit(0 if (rust == c == py and len(rust) == 7) else 1)
PY
if [ $? -eq 0 ]; then ok "[B0] all three rank-var lists identical (set + precedence in sync)"
else bad "[B0] rank-var lists DIVERGE across Rust/C/Python — tiers will disagree on a rank"; fi

# ============================================================================
# PART B.1 — each rank scheme detected, writes the RIGHT snap.<rank>.json
# ============================================================================
echo
echo "== [B1] per-scheme rank detection (one var set -> snap.<rank>.json) =="
printf "   %-26s %-6s %-14s %s\n" "ENV VAR" "rank" "snap file" "result"
# scheme:var:rank (distinct rank per scheme so a collision on snap.0 is obvious)
for spec in \
  "OpenMPI:OMPI_COMM_WORLD_RANK:1" \
  "MPICH/IntelMPI/Hydra:PMI_RANK:2" \
  "PMIx:PMIX_RANK:3" \
  "MVAPICH2:MV2_COMM_WORLD_RANK:4" \
  "Slurm-srun:SLURM_PROCID:5" \
  "Cray-PALS:PALS_RANKID:6" \
  "Cray-ALPS:ALPS_APP_PE:7" ; do
  name="${spec%%:*}"; rest="${spec#*:}"; var="${rest%%:*}"; rank="${rest##*:}"
  d="out/cm_$var.d"
  detect "$d" "$var=$rank"
  if [ -f "$d/snap.$rank.json" ] && { [ "$rank" = 0 ] || [ ! -f "$d/snap.0.json" ]; }; then
    printf "   %-26s %-6s snap.%-9s ok\n" "$var" "$rank" "$rank.json"
    ok "[B1] $name: $var=$rank -> snap.$rank.json (no collision on snap.0)"
  else
    printf "   %-26s %-6s %-14s MISS\n" "$var" "$rank" "snap.$rank.json?"
    ls "$d" 2>/dev/null | sed 's/^/        have: /'
    bad "[B1] $name: $var=$rank did NOT produce snap.$rank.json"
  fi
done

# ============================================================================
# PART B.2 — precedence (two vars set; which one wins?)  canonical order, now
# identical across Rust/C/Python:  OMPI > PMI > MV2 > PMIX > SLURM > PALS > ALPS
# ============================================================================
echo
echo "== [B2] precedence when multiple vars are set (canonical rank_from_env order) =="
prec(){ # <label> <"VARS"> <expected_rank> <loser_note>
  local label="$1" vars="$2" exp="$3" note="$4"
  local d="out/cm_prec_$exp.d"
  detect "$d" $vars
  if [ -f "$d/snap.$exp.json" ] && [ ! -f "$d/snap.0.json" ]; then
    ok "[B2] $label -> snap.$exp.json wins ($note)"
  else
    bad "[B2] $label expected snap.$exp.json — got: $(ls "$d" 2>/dev/null | tr '\n' ' ')"
  fi
}
prec "OMPI(10) vs SLURM(50)"  "OMPI_COMM_WORLD_RANK=10 SLURM_PROCID=50" 10 "OMPI outranks Slurm"
prec "PMI(20) vs PMIX(30)"    "PMI_RANK=20 PMIX_RANK=30"                20 "PMI outranks PMIx"
# Regression guard for the fixed divergence: MV2 must win over PMIX in the Rust
# detector, matching C(util.c) and Python(contract). (Was: Rust picked PMIX.)
prec "MV2(44) vs PMIX(33)"    "PMIX_RANK=33 MV2_COMM_WORLD_RANK=44"      44 \
     "MV2 outranks PMIx — Rust now agrees with C/Python (no cross-tier disagreement)"

# ============================================================================
# PART B.3 — world-size var detection (drives the short-count warning)
#   Rust mpi_world_size_from_env: OMPI_COMM_WORLD_SIZE, PMI_SIZE,
#   MV2_COMM_WORLD_SIZE, SLURM_NTASKS, SLURM_NPROCS, PALS_NRANKS
# ============================================================================
echo
echo "== [B3] world-size detection -> mpi_world_size in snap (short-count warning input) =="
ws(){ # <label> <rankvar=val> <sizevar=val> <expect_size>
  local label="$1" rv="$2" sv="$3" exp="$4" d="out/cm_ws_$4.$RANDOM.d"
  detect "$d" $rv $sv
  local snap; snap=$(ls "$d"/snap.*.json 2>/dev/null | head -1)
  local got; got=$(val "$snap" mpi_world_size)
  if [ "${got%.*}" = "$exp" ]; then ok "[B3] $label: mpi_world_size=$exp detected"
  else bad "[B3] $label: mpi_world_size=$got expected $exp ($sv)"; fi
}
ws "OpenMPI size"   "OMPI_COMM_WORLD_RANK=0" "OMPI_COMM_WORLD_SIZE=8" 8
ws "PMI/MPICH size" "PMI_RANK=0"             "PMI_SIZE=8"             8
ws "MVAPICH2 size"  "MV2_COMM_WORLD_RANK=0"  "MV2_COMM_WORLD_SIZE=8"  8
ws "Slurm ntasks"   "SLURM_PROCID=0"         "SLURM_NTASKS=8"         8
ws "Cray PALS size" "PALS_RANKID=0"          "PALS_NRANKS=8"          8
# PMIx has a rank var but NO size var in the list -> world size stays unknown.
d=out/cm_ws_pmix.d; detect "$d" PMIX_RANK=0
if [ -z "$(val "$d/snap.0.json" mpi_world_size)" ]; then
  echo "  NOTE: PMIx (PMIX_RANK) has no world-size var in mpi_world_size_from_env —"
  echo "        a pure-PMIx job can't trigger the short-count undercount warning."
fi

# ============================================================================
# PART A — real MPICH: ABI-safe PMPI shim + Hydra/PMI_RANK detection
# ============================================================================
echo
echo "== [A] real MPICH (different ABI + Hydra launcher) =="
MPICH_PFX="$HERE/mpich/prefix"
if [ ! -x "$MPICH_PFX/bin/mpicc" ] || [ ! -x "$MPICH_PFX/bin/mpiexec" ]; then
  skp "[A] MPICH not built at $MPICH_PFX (build it to run the ABI test)"
else
  echo "   MPICH: $("$MPICH_PFX/bin/mpichversion" 2>/dev/null | head -1)"
  # 1) Build the (mpi.h-free) PMPI shim with MPICH's mpicc.
  if "$MPICH_PFX/bin/mpicc" -shared -fPIC -O2 -o out/uaps_mpi_mpich.so "$ROOT/collectors/snapshot/shim/mpi/uaps_mpi.c" 2>out/shim_build.e; then
    ok "[A] PMPI shim compiles against MPICH (mpi.h-free, no ABI assumptions)"
  else
    bad "[A] shim failed to build against MPICH"; sed 's/^/      /' out/shim_build.e
  fi
  # 2) A small real-MPI hybrid program with MPICH's mpicc (Allreduce -> mpi_time).
  if "$MPICH_PFX/bin/mpicc" -O2 -fopenmp -o out/mpich_hello hybrid.c 2>out/app_build.e; then
    : ; else echo "      (hybrid.c needs OpenMP; building no-OMP fallback)";
    "$MPICH_PFX/bin/mpicc" -O2 -o out/mpich_hello hybrid.c 2>>out/app_build.e || bad "[A] app build failed"
  fi
  # 3) Run uaps INSIDE Hydra; shim via UAPS_MPI_SHIM; aggregate.
  d=out/cm_mpich.d; rm -rf "$d"
  export LD_LIBRARY_PATH="$MPICH_PFX/lib:${LD_LIBRARY_PATH:-}"
  UAPS_MPI_SHIM="$HERE/out/uaps_mpi_mpich.so" OMP_NUM_THREADS=2 \
    "$MPICH_PFX/bin/mpiexec" -n 4 "$UAPS" run --rank-dir "$d" -- "$HERE/out/mpich_hello" 40000000 4 balanced \
    >/dev/null 2>"$d.e"
  nsnap=$(ls "$d"/snap.*.json 2>/dev/null | wc -l)
  echo "   per-rank snaps written: $(ls "$d" 2>/dev/null | grep -c '^snap\.') ; rank files: $(ls "$d" 2>/dev/null | grep -c '^rank_')"
  if [ "$nsnap" -eq 4 ] && [ -f "$d/snap.0.json" ] && [ -f "$d/snap.3.json" ]; then
    ok "[A] Hydra/PMI_RANK detected: 4 distinct snap.<rank>.json (0..3) written"
  else
    bad "[A] expected 4 snaps (0..3) under MPICH/Hydra — got $nsnap: $(ls "$d" 2>/dev/null | grep '^snap\.' | tr '\n' ' ')"
  fi
  "$UAPS" report --format json -o "$d.json" "$d" 2>"$d.rep.e"
  nr=$(val "$d.json" nranks); mt=$(val "$d.json" mpi_time)
  echo "   aggregate: nranks=$nr  mpi_time=$mt"
  if [ "${nr%.*}" = 4 ]; then ok "[A] aggregation across 4 MPICH ranks correct (nranks=$nr)"; else bad "[A] nranks=$nr expected 4"; fi
  if [ -n "$mt" ] && awk "BEGIN{exit !($mt>0)}"; then
    ok "[A] MPICH-built shim captured mpi_time=$mt s (PMPI interposition ABI-safe across MPIs)"
  else
    bad "[A] no mpi_time from the MPICH shim — PMPI interposition or PMI_RANK env failed"
  fi
fi

echo
echo "==============================================================================="
echo "== cross_mpi: $pass passed, $fail failed, $skip skipped =="
[ "$fail" = 0 ]
