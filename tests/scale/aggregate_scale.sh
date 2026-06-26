#!/bin/bash
# Rigorous validation of `uaps report` AGGREGATION at true scale (10k+ ranks).
#
# Why this is the honest scale test: uaps collection is per-rank-INDEPENDENT (each
# rank counts its own process and writes snap.<rank>.json), so the only part whose
# cost grows with the job size is the aggregator. We therefore exercise it directly
# with N synthetic rank snapshots — no need to actually launch N MPI ranks — using a
# pattern whose SUM/MAX/MEAN/imbalance are known analytically, plus the failure modes
# that are NORMAL at scale: dead (truncated) ranks, a short world size, and ranks
# spread across heterogeneous nodes. Asserts correctness AND that cost stays bounded
# (wall time + peak RSS), which is the production-scale invariant.
#
# Self-contained: writes only under tests/scale/out/.  Usage:
#   bash tests/scale/aggregate_scale.sh [N]        (default N=20000)
set -u
cd "$(dirname "$0")"
ROOT=$(cd ../.. && pwd)
N=${1:-20000}

UAPS=""
for c in release debug; do
  p="$ROOT/collectors/snapshot/target/$c/uaps"
  [ -x "$p" ] && { UAPS="$p"; break; }
done
[ -n "$UAPS" ] || { echo "uaps not built — run 'make' first"; exit 1; }

DIR="$ROOT/tests/scale/out/agg_$N"
rm -rf "$DIR"; mkdir -p "$DIR"

pass=0; fail=0
ok()  { echo "  PASS: $1"; pass=$((pass+1)); }
bad() { echo "  FAIL: $1"; fail=$((fail+1)); }

echo "== uaps report aggregation at scale: N=$N ranks =="

# --- generate N rank snapshots with an analytically-known pattern -------------
#  rank i:  cpu_time=2.0 (const)            → SUM = 2*N
#           hw_instructions=1e9, cycles=5e8 → aggregate IPC = 2.0 (from summed raws)
#           gflops alternates 10 / 20       → SUM=15*N; imbalance (20-15)/20 = 25%
#           elapsed = 1.0 + (i%500)*1e-3    → MAX = 1.499
#           arch/host: half amdzen5@nodeA, half arm/neoverse-v2@nodeB (mixed-arch)
#  Plus: 5 truncated files (dead ranks, must be skipped), and world_size = N+7 so the
#  short-count warning fires for the 7 that "never reported".
WORLD=$((N + 7))
python3 - "$DIR" "$N" "$WORLD" <<'PY'
import sys, os
d, n, world = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
tmpl = ('{{ "host":"{host}", "arch":"{arch}", "metrics":[\n'
        '  {{"key":"elapsed_time","label":"E","value":{el},"unit":"s","display":"x"}},\n'
        '  {{"key":"cpu_time","label":"C","value":2.0,"unit":"s","display":"x"}},\n'
        '  {{"key":"hw_instructions","label":"i","value":1000000000,"unit":"","display":"x"}},\n'
        '  {{"key":"hw_cpu_cycles","label":"c","value":500000000,"unit":"","display":"x"}},\n'
        '  {{"key":"gflops","label":"FP","value":{g},"unit":"GFLOP/s","display":"x"}},\n'
        '  {{"key":"mpi_world_size","label":"W","value":{world},"unit":"","display":"x"}}\n'
        ']}}\n')
for i in range(n):
    half = i < n // 2
    host = "nodeA" if half else "nodeB"
    arch = "amdzen5" if half else "arm/neoverse-v2"
    el = 1.0 + (i % 500) * 1e-3
    g = 10.0 if (i % 2 == 0) else 20.0
    with open(os.path.join(d, "snap.%d.json" % i), "w") as f:
        f.write(tmpl.format(host=host, arch=arch, el=el, g=g, world=world))
# 5 truncated/dead ranks (must be skipped, not abort the report)
for k in range(n, n + 5):
    with open(os.path.join(d, "snap.%d.json" % k), "w") as f:
        f.write('{ "metrics":[ {"key":"gflops","val')   # cut mid-write
print("generated %d good + 5 truncated rank files" % n)
PY

val() { grep -oE "\"key\": \"$2\"[^}]*\"value\": [-0-9.eE]+" "$1" | grep -oE "[-0-9.eE]+$" | head -1; }

# --- run the aggregator, measuring wall time + peak RSS -----------------------
OUT="$DIR.json"; ERR="$DIR.err"
TIMED=$( { /usr/bin/time -v "$UAPS" report --format json -o "$OUT" "$DIR" 2>"$ERR.time" 1>/dev/null; } 2>&1; \
         grep -iE "Maximum resident|Elapsed \(wall" "$ERR.time" )
cp "$ERR.time" "$ERR"
rss_kb=$(grep -i "Maximum resident" "$ERR.time" | grep -oE "[0-9]+" | tail -1)
wall=$(grep -i "Elapsed (wall" "$ERR.time" | sed -E 's/.*: //')
echo "   aggregated $N ranks in wall=$wall, peak RSS=$(( ${rss_kb:-0} / 1024 )) MB"

# --- assertions: correctness of the reduction at scale ------------------------
nr=$(val "$OUT" nranks)
[ "${nr%.*}" = "$N" ] && ok "nranks == $N (truncated ranks skipped, not aborted)" \
  || bad "nranks=$nr expected $N"

g=$(val "$OUT" gflops)                                  # SUM = 15*N
exp_g=$(python3 -c "print(15.0*$N)")
awk "BEGIN{exit !(($g-$exp_g)/$exp_g < 0.0001 && ($g-$exp_g)/$exp_g > -0.0001)}" \
  && ok "gflops SUM exact at scale ($g == 15*N)" || bad "gflops=$g expected $exp_g"

cput=$(val "$OUT" cpu_time)                             # SUM = 2*N
exp_c=$(python3 -c "print(2.0*$N)")
awk "BEGIN{exit !(($cput-$exp_c)/$exp_c < 0.0001 && ($cput-$exp_c)/$exp_c > -0.0001)}" \
  && ok "cpu_time SUM exact ($cput == 2*N)" || bad "cpu_time=$cput expected $exp_c"

el=$(val "$OUT" elapsed_time)                           # MAX = 1.499
awk "BEGIN{exit !($el>1.498 && $el<1.500)}" && ok "elapsed_time MAX correct ($el ≈ 1.499)" \
  || bad "elapsed_time=$el expected ≈1.499"

ipc=$(val "$OUT" ipc)                                   # recomputed: sum(instr)/sum(cyc)=2.0
awk "BEGIN{exit !($ipc>1.999 && $ipc<2.001)}" && ok "IPC recomputed from summed raws ($ipc == 2.0)" \
  || bad "ipc=$ipc expected 2.0"

imb=$(val "$OUT" gflops_imbalance_pct)                  # (20-15)/20 = 25%
awk "BEGIN{exit !($imb>24.9 && $imb<25.1)}" && ok "FP imbalance exact at scale ($imb% == 25%)" \
  || bad "gflops_imbalance=$imb expected 25"

# --- assertions: the at-scale warnings all fire ------------------------------
grep -q "aggregated $N of $WORLD ranks" "$ERR" \
  && ok "short-count warning fires ($N of $WORLD reported)" || bad "no short-count warning"
grep -qiE "WARNING.*different CPU models|mix heterogeneous" "$ERR" \
  && ok "mixed-arch roofline warning fires (amdzen5 + neoverse-v2)" || bad "no mixed-arch warning"
grep -qiE "ranks span 2 nodes" "$ERR" \
  && ok "multi-node participation line (2 nodes)" || bad "no multi-node line"

# --- assertion: cost stays bounded (the production-scale invariant) ----------
# Per-rank resident cost must stay small (catches a regression to per-rank heavy
# structures, e.g. function-keyed Counters). N-independent: KB-per-rank, not a total.
maxrss_mb=$(( ${rss_kb:-0} / 1024 ))
per_rank_kb=$(( ${rss_kb:-0} / N ))
[ "${rss_kb:-0}" -gt 0 ] && [ "$per_rank_kb" -lt 16 ] \
  && ok "per-rank resident cost bounded (${per_rank_kb} KB/rank < 16; ${maxrss_mb}MB total — no blowup)" \
  || bad "per-rank resident ${per_rank_kb} KB/rank (${maxrss_mb}MB total) exceeded 16 KB/rank"

echo "== aggregate_scale: $pass passed, $fail failed =="
rm -rf "$DIR" "$OUT" "$ERR" "$ERR.time"
[ "$fail" = 0 ]
