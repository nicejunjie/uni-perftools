#!/bin/bash
# Large-scale (oversubscription) validation of uaps PER-RANK collection on a single
# node. Simulates a many-rank job by oversubscribing the cores, and checks:
#   A) high rank count: all N ranks (uaps inside the launcher), counted + aggregated
#   B) known imbalance: a skewed workload shows up as cross-rank imbalance
#   C) homogeneous baseline: a balanced workload shows ~no imbalance
#   D) robustness: a rank that dies mid-run doesn't sink the whole report
#
# Self-contained: builds the synthetic workload locally, uses the bundled qenv
# mpirun, writes only under tests/scale/.  Usage: bash tests/scale/run.sh
set -u
cd "$(dirname "$0")"
HERE=$(pwd)
ROOT=$(cd ../.. && pwd)

UAPS=""
for c in release debug; do
  p="$ROOT/collectors/snapshot/target/$c/uaps"
  [ -x "$p" ] && { UAPS="$p"; break; }
done
[ -n "$UAPS" ] || { echo "uaps not built — run 'make' first"; exit 1; }

MPIRUN="$ROOT/tests/qe/qenv/bin/mpirun"
# An MPI launcher is required; skip cleanly (not a failure) where it's absent
# (e.g. CI runners without the bundled qenv) so this stays opt-in.
[ -x "$MPIRUN" ] || { echo "SKIP: bundled mpirun missing ($MPIRUN) — build tests/qe/qenv first"; exit 0; }
export OPAL_PREFIX="$ROOT/tests/qe/qenv"
export PMIX_MCA_pcompress_base_silence_warning=1

CORES=$(nproc)
mkdir -p out
cc -O2 -o out/flops_by_rank flops_by_rank.c || { echo "build failed"; exit 1; }
APP="$HERE/out/flops_by_rank"

# tiny JSON field reader (value of a metric key) — avoids a python dep on the hot path
val() { grep -oE "\"key\": \"$2\"[^}]*\"value\": [-0-9.eE]+" "$1" | grep -oE "[-0-9.eE]+$" | head -1; }

pass=0; fail=0
ok()  { echo "  PASS: $1"; pass=$((pass+1)); }
bad() { echo "  FAIL: $1"; fail=$((fail+1)); }

# Oversubscribe to 4x the cores (capped) to simulate a many-rank job on one box.
NR=$(( CORES * 4 )); [ "$NR" -gt 128 ] && NR=128; [ "$NR" -lt 8 ] && NR=8
MPIFLAGS="--oversubscribe --bind-to none"
ITERS=20000000   # ~tens of ms/rank single-core; modest so oversubscription stays quick

echo "== uaps large-scale (per-rank) on $CORES cores =="

# Physical-core count for clean (un-oversubscribed, bound) imbalance baselines.
PHYS=$(lscpu -p=Socket,Core 2>/dev/null | grep -v '^#' | sort -u | wc -l)
[ "$PHYS" -ge 2 ] 2>/dev/null || PHYS=$CORES
BAL=$(( PHYS < 16 ? PHYS : 16 ))   # ranks for the bound balanced/skew tests

# APS-form helper: run uaps INSIDE the launcher (launcher-agnostic — no flag-parsing,
# no -x), then aggregate the per-rank results dir with `uaps report` (like aps-report).
#   aps <result-dir> <out.json> <n-ranks> <bind-flags> <app> [app-args...]
aps() {
  local dir="$1" out="$2" n="$3" bind="$4" app="$5"; shift 5
  rm -rf "$dir"
  "$MPIRUN" --oversubscribe $bind -n "$n" "$UAPS" run --rank-dir "$dir" -- "$app" "$@" \
    >/dev/null 2>"$dir.err"
  "$UAPS" report --format json -o "$out" "$dir" >>"$dir.err" 2>&1
}

# --- A) high rank count -------------------------------------------------------
echo "-- A) $NR ranks (oversubscribed ${NR}/$CORES), homogeneous --"
t0=$SECONDS
aps out/scale.d out/scale.json "$NR" "--bind-to none" "$APP" "$ITERS"
echo "   $(( SECONDS - t0 ))s wall (collect $NR ranks + report)"
grep -iE "per-rank|aggregated|WARNING" out/scale.d.err | head -2 | sed 's/^/   /'
nr=$(val out/scale.json nranks)
[ "${nr%.*}" = "$NR" ] && ok "A: aggregated all $NR ranks (nranks=$nr)" || bad "A: nranks=$nr expected $NR"
g=$(val out/scale.json gflops)
awk "BEGIN{exit !($g>0)}" && ok "A: nonzero aggregate FP throughput ($g GFLOP/s)" || bad "A: gflops=$g"
ipc=$(val out/scale.json ipc)
awk "BEGIN{exit !($ipc>0)}" && ok "A: IPC recomputed from summed raws ($ipc)" || bad "A: ipc=$ipc"

# B/C bind one rank per physical core (no SMT/turbo jitter) so the imbalance the
# test asserts on is the WORKLOAD's, not the scheduler's. (Unbound/oversubscribed,
# even "identical" work shows real cpu-time spread — which the tool correctly sees,
# but that is system jitter, not what these two cases are checking.)

# --- B) known imbalance (skew: odd ranks 2x work) -----------------------------
echo "-- B) $BAL ranks bound, SKEWED (odd ranks 2x) — expect imbalance --"
aps out/skew.d out/skew.json "$BAL" "--bind-to core" "$APP" "$ITERS" skew
skew_ct=$(val out/skew.json cpu_time_imbalance_pct)
skew_el=$(val out/skew.json elapsed_imbalance_pct)
echo "   cpu_time_imbalance=${skew_ct}%  elapsed_imbalance=${skew_el}%"
awk "BEGIN{exit !($skew_ct>=12)}" && ok "B: 2x skew detected (cpu-time imbalance ${skew_ct}%)" \
  || bad "B: cpu-time imbalance ${skew_ct}% too low for a 2x skew"

# --- C) homogeneous baseline: imbalance must be clearly LESS than the skew case -
echo "-- C) $BAL ranks bound, homogeneous — expect LESS imbalance than skew --"
aps out/even.d out/even.json "$BAL" "--bind-to core" "$APP" "$ITERS"
even_ct=$(val out/even.json cpu_time_imbalance_pct)
echo "   cpu_time_imbalance=${even_ct}%  (skew was ${skew_ct}%)"
awk "BEGIN{exit !($even_ct < $skew_ct - 8)}" \
  && ok "C: homogeneous (${even_ct}%) clearly below skew (${skew_ct}%) — metric tracks real work" \
  || bad "C: homogeneous ${even_ct}% not clearly below skew ${skew_ct}%"

# --- D) robustness: a rank that dies mid-run does not abort the report ---------
echo "-- D) a rank aborts mid-run — report still produced --"
cat > out/flaky.sh <<EOF
#!/bin/sh
r=\${OMPI_COMM_WORLD_RANK:-0}
[ "\$r" = 1 ] && kill -9 \$\$    # rank 1 dies hard
exec "$APP" "$ITERS"
EOF
chmod +x out/flaky.sh
aps out/flaky.d out/flaky.json 8 "--bind-to none" "$PWD/out/flaky.sh"
if [ -s out/flaky.json ] && [ -n "$(val out/flaky.json gflops)" ]; then
  ok "D: report produced despite a dead rank ($(val out/flaky.json nranks) ranks aggregated)"
else
  bad "D: no report produced when a rank died"
fi

echo "== scale: $pass passed, $fail failed =="
[ "$fail" = 0 ]
