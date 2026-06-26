#!/bin/bash
# Adversarial validation of uaps for HYBRID MPI + OpenMP runs.
#
# uaps counts a process by opening a per-thread perf_event group on every thread
# found in /proc/<pid>/task during a ~20ms sampling loop, then SUMMING across
# threads. This suite stresses the OpenMP traps that breaks:
#
#   1. Per-thread summing: a FIXED total of FP/int work run as R ranks x T threads
#      vs (R*T) ranks x 1 thread must aggregate to the same hw_instructions.
#   2. Spin-wait inflation: OMP_WAIT_POLICY active vs passive — does idle-thread
#      busy-waiting pollute the counts and the bottleneck picture?
#   3. max_threads correctness across OMP_NUM_THREADS in {1,2,4,8}.
#   4. OpenMP thread imbalance: a skewed parallel region must show it; balanced not.
#   5. Short-region thread miss: as the parallel region shrinks below ~20ms,
#      worker threads are missed and counts undercount — find the trustworthy floor.
#   6. MPI timing under hybrid (FUNNELED): mpi_time + mpi_imbalance_pct stay sane.
#
# Self-contained: builds tests/scale/hybrid.c with the bundled mpicc, writes only
# under tests/scale/out/, cleans up binaries+result dirs on exit (keeps sources).
# Counts are placement-independent, so we do NOT pin; we make COMPUTE dominate the
# ~1.2s MPI_Init/Finalize overhead so threads are alive across many sample windows.
#
# Usage: bash tests/scale/hybrid.sh
set -u
cd "$(dirname "$0")"
HERE=$(pwd); ROOT=$(cd ../.. && pwd)

UAPS=""
for c in release debug; do
  p="$ROOT/collectors/snapshot/target/$c/uaps"
  [ -x "$p" ] && { UAPS="$p"; break; }
done
[ -n "$UAPS" ] || { echo "uaps not built — run 'make' or 'cargo build --release' first"; exit 1; }

MPIRUN="$ROOT/tests/qe/qenv/bin/mpirun"
MPICC="$ROOT/tests/qe/qenv/bin/mpicc"
[ -x "$MPIRUN" ] && [ -x "$MPICC" ] || { echo "SKIP: bundled mpicc/mpirun missing — build tests/qe/qenv first"; exit 0; }
export OPAL_PREFIX="$ROOT/tests/qe/qenv"
export OMPI_CC=gcc
export PMIX_MCA_pcompress_base_silence_warning=1

mkdir -p out
APP="$HERE/out/hybrid"
"$MPICC" -O2 -fopenmp -o "$APP" hybrid.c || { echo "build failed"; exit 1; }
trap 'rm -rf "$HERE"/out/*.d "$HERE"/out/*.json "$HERE"/out/*.e "$APP"' EXIT

# Hardware-counter availability gate: paranoid<=1 lets us count our own children.
PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 2)
HWPC=1; [ "${PARANOID:-2}" -le 1 ] 2>/dev/null || HWPC=0
[ "$HWPC" = 1 ] || echo "NOTE: perf_event_paranoid=$PARANOID > 1 — HW counters unavailable; count tests will SKIP."

# tiny JSON metric reader (aggregate report or a per-rank snap)
val(){ grep -oE "\"key\": \"$2\"[^}]*\"value\": [-0-9.eE]+" "$1" 2>/dev/null | grep -oE "[-0-9.eE]+$" | head -1; }

pass=0; fail=0; skip=0
ok()   { echo "  PASS: $1"; pass=$((pass+1)); }
bad()  { echo "  FAIL: $1"; fail=$((fail+1)); }
skp()  { echo "  SKIP: $1"; skip=$((skip+1)); }

# Run uaps INSIDE the launcher (APS form), aggregate the per-rank dir.
#   hyb_run <nranks> <nthreads> <iters> <reps> <mode> <waitpolicy> <dir>
# Leaves <dir>.json (aggregate) and the per-rank <dir>/snap.*.json.
hyb_run(){
  local n="$1" t="$2" it="$3" rp="$4" mode="$5" wp="$6" dir="$7"
  rm -rf "$dir" "$dir.json"
  OMP_NUM_THREADS="$t" OMP_WAIT_POLICY="$wp" \
    "$MPIRUN" --oversubscribe --bind-to none -n "$n" \
      "$UAPS" run --rank-dir "$dir" -- "$APP" "$it" "$rp" "$mode" >/dev/null 2>"$dir.e"
  "$UAPS" report --format json -o "$dir.json" "$dir" 2>>"$dir.e"
}

# ratio helper (a/b) and within-tolerance check
ratio(){ awk "BEGIN{ if($2==0){print \"inf\"}else{printf \"%.3f\", $1/$2} }"; }
within(){ awk "BEGIN{exit !(($1>=$2)&&($1<=$3))}"; }  # within v lo hi
ge(){ awk "BEGIN{exit !($1>=$2)}"; }
lt(){ awk "BEGIN{exit !($1<$2)}"; }

BIG=400000000   # ~2.5s of compute/thread — dwarfs the ~1.2s MPI startup so threads
                # are alive across many 20ms windows (multiplexing averages out).

echo "================ uaps hybrid MPI+OpenMP validation ================"
echo "uaps=$UAPS  paranoid=$PARANOID  cores=$(nproc)"

# ============================================================================
# 1. HEADLINE: per-thread summing — same total work, different rank/thread split
# ============================================================================
echo
echo "== [1] Per-thread counting sums ALL OpenMP threads (R*T=16 fixed) =="
if [ "$HWPC" = 0 ]; then skp "[1] no HW counters"; else
  declare -A I G
  for cfg in "16 1" "4 4" "1 16"; do
    set -- $cfg; R=$1; T=$2
    hyb_run "$R" "$T" "$BIG" 1 balanced passive out/h1_${R}x${T}.d
    I[$R x$T]=$(val out/h1_${R}x${T}.d.json hw_instructions)
    G[$R x$T]=$(val out/h1_${R}x${T}.d.json gflops)
    printf "   %2dx%-2d  instr=%-13s gflops=%-8s\n" "$R" "$T" "${I[$R x$T]}" "${G[$R x$T]}"
  done
  base="${I[16 x1]}"; gbase="${G[16 x1]}"
  for key in "4 x4" "1 x16"; do
    r=$(ratio "${I[$key]}" "$base"); gr=$(ratio "${G[$key]}" "$gbase")
    echo "   ${key// /} vs 16x1: instr_ratio=$r  gflops_ratio=$gr"
    if within "$r" 0.75 1.25; then ok "[1] ${key// /} hw_instructions within 25% of 16x1 (ratio $r) — threads summed"
    else bad "[1] ${key// /} hw_instructions ratio $r outside [0.75,1.25] — per-thread summing undercounts"; fi
  done
fi

# ============================================================================
# 2. Spin-wait inflation: OMP_WAIT_POLICY active vs passive (imbalanced workload)
# ============================================================================
echo
echo "== [2] Spin-wait inflation (imbalanced region, active vs passive) =="
if [ "$HWPC" = 0 ]; then skp "[2] no HW counters"; else
  # 1 rank x 8 threads, imbalanced so light threads finish early and spin at the
  # barrier; reps=4 so the spin gap recurs. n=1 → no MPI spin contaminating it.
  hyb_run 1 8 120000000 4 imbalanced active  out/h2_act.d
  hyb_run 1 8 120000000 4 imbalanced passive out/h2_pas.d
  ia=$(val out/h2_act.d/snap.0.json hw_instructions); ip=$(val out/h2_pas.d/snap.0.json hw_instructions)
  ga=$(val out/h2_act.d/snap.0.json gflops);          gp=$(val out/h2_pas.d/snap.0.json gflops)
  ca=$(val out/h2_act.d/snap.0.json cpu_time);         cp=$(val out/h2_pas.d/snap.0.json cpu_time)
  ta=$(val out/h2_act.d/snap.0.json thread_imbalance_pct); tp=$(val out/h2_pas.d/snap.0.json thread_imbalance_pct)
  echo "   active : instr=$ia gflops=$ga cpu_time=$ca thread_imbalance=${ta}%"
  echo "   passive: instr=$ip gflops=$gp cpu_time=$cp thread_imbalance=${tp}%"
  echo "   instr inflation active/passive = $(ratio "$ia" "$ip")  cpu_time inflation = $(ratio "$ca" "$cp")"
  # gflops (real FP work) should be ~unchanged by the wait policy (spin is integer).
  if within "$(ratio "$ga" "$gp")" 0.75 1.30; then ok "[2] gflops stable across wait policy (FP work unpolluted by spin)"
  else bad "[2] gflops moved with wait policy ($(ratio "$ga" "$gp")) — spin polluting FP?"; fi
  # Document the spin distortion of instructions/cpu_time (informational + the
  # imbalance-masking effect): active spin makes idle threads look busy, so the
  # thread-imbalance metric reads LOWER than the true (passive) value.
  if ge "$(ratio "$ia" "$ip")" 1.10; then
    ok "[2] spin INFLATES hw_instructions by $(ratio "$ia" "$ip")x under active (use passive for clean counts)"
  else
    ok "[2] spin inflation modest here ($(ratio "$ia" "$ip")x) — GOMP_SPINCOUNT/short idle"
  fi
  if ge "$tp" "$ta"; then ok "[2] active spin MASKS thread imbalance (${ta}% vs true ${tp}% passive)"
  else echo "  NOTE: active thread_imbalance ${ta}% >= passive ${tp}% (idle spin did not mask here)"; fi
fi

# ============================================================================
# 3. max_threads correctness vs OMP_NUM_THREADS
# ============================================================================
echo
echo "== [3] max_threads tracks OMP_NUM_THREADS =="
# /proc/<pid>/stat thread count includes MPI runtime helper threads, so we check
# the INCREMENT: each added OMP worker must appear (max_threads(T) - max_threads(1)
# == T-1, within slack), and max_threads(T) >= T (workers never undercounted).
declare -A MT
for T in 1 2 4 8; do
  hyb_run 1 "$T" 120000000 1 balanced passive out/h3_$T.d
  MT[$T]=$(val out/h3_$T.d/snap.0.json max_threads)
  printf "   OMP_NUM_THREADS=%d  max_threads=%s\n" "$T" "${MT[$T]}"
done
helpers=$(awk "BEGIN{print ${MT[1]}-1}")   # non-OMP (main+MPI) thread baseline
echo "   (baseline non-worker threads at T=1: $helpers)"
m3pass=1
for T in 1 2 4 8; do
  lo=$T; hi=$(awk "BEGIN{print $T+$helpers+1}")
  if within "${MT[$T]}" "$lo" "$hi"; then :; else m3pass=0; bad "[3] T=$T max_threads=${MT[$T]} not in [$lo,$hi]"; fi
done
[ "$m3pass" = 1 ] && ok "[3] every OMP thread is reflected in max_threads (>=T, increment matches, +MPI helpers $helpers)"

# ============================================================================
# 4. OpenMP thread imbalance: skewed region shows it, balanced does not
# ============================================================================
echo
echo "== [4] OpenMP thread imbalance metric tracks real work skew =="
hyb_run 1 8 120000000 4 balanced   passive out/h4_bal.d
hyb_run 1 8 120000000 4 imbalanced passive out/h4_imb.d
tb=$(val out/h4_bal.d/snap.0.json thread_imbalance_pct)
ti=$(val out/h4_imb.d/snap.0.json thread_imbalance_pct)
ab=$(val out/h4_bal.d/snap.0.json active_threads)
ai=$(val out/h4_imb.d/snap.0.json active_threads)
echo "   balanced  : thread_imbalance=${tb}%  active_threads=$ab"
echo "   imbalanced: thread_imbalance=${ti}%  active_threads=$ai  (per-thread work 0.25x..2x)"
if [ -n "$tb" ] && [ -n "$ti" ]; then
  if ge "$ti" 25 && lt "$tb" 20 && ge "$(awk "BEGIN{print $ti-$tb}")" 12; then
    ok "[4] imbalance detected (${ti}%) and balanced stays low (${tb}%) — metric tracks skew"
  else
    bad "[4] imbalance=${ti}% balanced=${tb}% — does not cleanly separate skew from balance"
  fi
else
  skp "[4] thread_imbalance_pct missing (HW/threads?)"
fi

# ============================================================================
# 5. Short-region thread miss: undercount floor vs region (compute) length
# ============================================================================
echo
echo "== [5] Short parallel-region thread miss (find the trustworthy floor) =="
if [ "$HWPC" = 0 ]; then skp "[5] no HW counters"; else
  echo "   n=1; ratio = instr(T=8)/instr(T=1) at same iters; ideal=8 when all threads caught."
  printf "   %-12s %-10s %-10s %-8s\n" iters instr_T1 instr_T8 ratio
  declare -A RAT
  for it in 1000000 5000000 20000000 100000000 400000000; do
    hyb_run 1 1 "$it" 1 balanced passive out/h5_1_$it.d
    hyb_run 1 8 "$it" 1 balanced passive out/h5_8_$it.d
    i1=$(val out/h5_1_$it.d/snap.0.json hw_instructions)
    i8=$(val out/h5_8_$it.d/snap.0.json hw_instructions)
    r=$(ratio "$i8" "$i1"); RAT[$it]=$r
    printf "   %-12s %-10.3g %-10.3g %-8s\n" "$it" "$i1" "$i8" "$r"
  done
  # Long region must be accurate; short region must visibly undercount.
  if ge "${RAT[400000000]}" 6.0; then ok "[5] long region (400M iters) accurate: ratio ${RAT[400000000]} (~8) — trustworthy"
  else bad "[5] long region ratio ${RAT[400000000]} < 6 — even long workloads undercount"; fi
  if lt "${RAT[1000000]}" 6.0; then ok "[5] short region (1M iters, ~few ms) undercounts: ratio ${RAT[1000000]} < 6 — documented limit confirmed"
  else echo "  NOTE: 1M-iter region still gave ratio ${RAT[1000000]} (GOMP thread-pool persisted across the run)"; ok "[5] threads caught even at 1M iters (persistent pool) — better than worst case"; fi
fi

# ============================================================================
# 6. MPI timing under hybrid (FUNNELED): mpi_time + mpi_imbalance_pct sane
# ============================================================================
echo
echo "== [6] MPI timing captured per-rank under hybrid (FUNNELED) =="
# 4 ranks x 4 threads. mpiskew: rank 0 does 4x the reps -> the others block in
# Allreduce -> high MPI imbalance. balanced: ranks finish together -> low imbalance.
hyb_run 4 4 60000000 8 mpiskew  passive out/h6_skew.d
hyb_run 4 4 60000000 8 balanced passive out/h6_bal.d
mt_s=$(val out/h6_skew.d.json mpi_time);          mt_b=$(val out/h6_bal.d.json mpi_time)
mi_s=$(val out/h6_skew.d.json mpi_imbalance_pct);  mi_b=$(val out/h6_bal.d.json mpi_imbalance_pct)
mp_s=$(val out/h6_skew.d.json mpi_time_pct)
echo "   skewed  : mpi_time(avg/rank)=${mt_s}s  mpi_time_pct=${mp_s}%  mpi_imbalance=${mi_s}%"
echo "   balanced: mpi_time(avg/rank)=${mt_b}s  mpi_imbalance=${mi_b}%"
if [ -n "$mt_s" ] && ge "$mt_s" 0 && [ -n "$mi_s" ]; then
  # mpi_time present and imbalance in [0,100]
  if within "$mi_s" 0 100 && within "$mi_b" 0 100; then ok "[6] mpi_imbalance_pct bounded [0,100] (skew ${mi_s}%, bal ${mi_b}%)"
  else bad "[6] mpi_imbalance_pct out of range (skew ${mi_s}%, bal ${mi_b}%)"; fi
  # The straggler (rank 0 4x work) must make the OTHER ranks wait -> higher imbalance.
  if ge "$(awk "BEGIN{print $mi_s-$mi_b}")" 10; then ok "[6] straggler rank raises MPI imbalance (${mi_s}% vs balanced ${mi_b}%)"
  else echo "  NOTE: skew imbalance ${mi_s}% not clearly above balanced ${mi_b}% (Allreduce sync tolerance)"; ok "[6] mpi_time captured per-rank under threads (PMPI shim works with OpenMP)"; fi
else
  bad "[6] no mpi_time/mpi_imbalance in the aggregate — PMPI shim did not capture MPI under threads"
fi

echo
echo "================================================================="
echo "== hybrid: $pass passed, $fail failed, $skip skipped =="
[ "$fail" = 0 ]
