#!/bin/bash
# Cross-check uaps's per-thread, multiplexed `hw_instructions` against the clean
# ground truth: `perf stat -e instructions` — ONE counter, inherit=1 (all threads),
# so NO PMU multiplexing and NO time-scaling. This quantifies the high-thread-density
# instruction undercount and separates its two hypothesized causes:
#   (a) PMU multiplexing extrapolation bias  — ~duration-INDEPENDENT (a rate-stationarity
#       error amplified by the ~6.7x time_enabled/time_running scale at coverage ~0.15);
#   (b) per-thread counter START-LATENCY loss — work before a thread's counter is opened
#       is lost; SHRINKS with longer runs and with a shorter sampling interval.
#
# uaps counts user+kernel (only exclude_hv), so the reference is plain `instructions`
# (no :u). Threads pinned to physical cores (counts are placement-independent, but this
# removes SMT scheduling noise). Self-contained: builds + writes only under out/.
# Usage: bash tests/scale/instr_crosscheck.sh [per_thread_iters]
set -u
cd "$(dirname "$0")"; HERE=$(pwd); ROOT=$(cd ../.. && pwd)

UAPS=""; for c in release debug; do p="$ROOT/collectors/snapshot/target/$c/uaps"; [ -x "$p" ] && { UAPS="$p"; break; }; done
[ -n "$UAPS" ] || { echo "uaps not built — run 'cargo build --release'"; exit 1; }

# Prefer a working `perf`; fall back to any linux-tools perf (ABI is stable, so an
# older build still counts `instructions` on a newer kernel).
PERF=""
for cand in "$(command -v perf 2>/dev/null)" /usr/lib/linux-tools-*/perf; do
  [ -x "$cand" ] && "$cand" stat -e instructions -- true >/dev/null 2>&1 && { PERF="$cand"; break; }
done
[ -n "$PERF" ] || { echo "SKIP: no working 'perf stat -e instructions' here (needs perf + paranoid<=1)"; exit 0; }
echo "perf: $PERF   uaps: $UAPS   paranoid=$(cat /proc/sys/kernel/perf_event_paranoid)"

mkdir -p out
cat > out/omp_flops.c <<'EOF'
#include <omp.h>
#include <stdio.h>
#include <stdlib.h>
int main(int argc, char **argv){
  long it = argc > 1 ? atol(argv[1]) : 1500000000L;   // per-thread iterations
  double tot = 0;
  #pragma omp parallel reduction(+:tot)
  { double x = 1.0 + omp_get_thread_num()*1e-9;
    for (long i=0;i<it;i++) x = x*1.0000000001 + 0.5;
    tot += x; }
  printf("%g\n", tot); return 0;
}
EOF
cc -O2 -fopenmp -o out/omp_flops out/omp_flops.c || { echo "build failed"; exit 1; }
APP="$HERE/out/omp_flops"
trap 'rm -f "$HERE"/out/omp_flops "$HERE"/out/omp_flops.c "$HERE"/out/ic_*.json "$HERE"/out/ic_*.perf' EXIT

PHYS=$(lscpu -p=Core 2>/dev/null | grep -v '^#' | sort -u | wc -l); [ "$PHYS" -ge 1 ] 2>/dev/null || PHYS=8
export OMP_PROC_BIND=close OMP_PLACES=cores

perf_instr(){ # T iters -> instruction count (ground truth)
  OMP_NUM_THREADS="$1" "$PERF" stat -e instructions -- "$APP" "$2" 2>"out/ic_p.perf" >/dev/null
  grep -iE "[0-9,]+[ ]+instructions" out/ic_p.perf | grep -oE "[0-9,]+" | head -1 | tr -d ','
}
uaps_instr(){ # T iters interval_ms -> hw_instructions
  OMP_NUM_THREADS="$1" "$UAPS" run --interval-ms "${3:-20}" --format json -o "out/ic_u.json" -- "$APP" "$2" >/dev/null 2>/dev/null
  grep -oE '"key": "hw_instructions"[^}]*"value": [0-9.eE+]+' out/ic_u.json | grep -oE '[0-9.eE+]+$' | head -1
}
ratio(){ awk "BEGIN{ if($2>0) printf \"%.3f\", $1/$2; else print \"n/a\" }"; }

pass=0; fail=0
TOL=0.05   # uaps's multiplexing-scaled count must track perf ground truth within 5%
check(){ # label ratio
  awk "BEGIN{exit !(($2>1-$TOL)&&($2<1+$TOL))}" \
    && { echo "  PASS: $1 (uaps/perf=$2, within ${TOL})"; pass=$((pass+1)); } \
    || { echo "  FAIL: $1 (uaps/perf=$2, off by >${TOL})"; fail=$((fail+1)); }
}

ITERS=${1:-1500000000}
echo
echo "== [1] uaps/perf instruction ratio vs thread density (per-thread iters=$ITERS, ${PHYS} phys cores) =="
printf "  %-4s %18s %18s %9s\n" "T" "perf (truth)" "uaps" "uaps/perf"
declare -A R
for T in 1 2 4 8 16; do
  [ "$T" -gt "$PHYS" ] && break
  p=$(perf_instr "$T" "$ITERS"); u=$(uaps_instr "$T" "$ITERS" 20)
  R[$T]=$(ratio "${u%.*}" "$p")
  printf "  %-4s %18s %18s %9s\n" "$T" "$p" "${u%.*}" "${R[$T]}"
done
for T in 1 2 4 8 16; do [ -n "${R[$T]:-}" ] && check "T=$T thread density" "${R[$T]}"; done

echo
echo "== [2] cause separation @ T=8 =="
T8=8; [ "$PHYS" -lt 8 ] && T8=$PHYS
echo "  (a) duration sweep — start-latency loss shrinks with longer runs; multiplexing bias ~constant"
for mult in "short:/6" "base:*1" "long:*4"; do
  lab="${mult%%:*}"; op="${mult#*:}"
  it=$(awk "BEGIN{printf \"%.0f\", $ITERS${op}}")
  p=$(perf_instr "$T8" "$it"); u=$(uaps_instr "$T8" "$it" 20)
  printf "      %-6s iters=%-12s uaps/perf=%s\n" "$lab" "$it" "$(ratio "${u%.*}" "$p")"
done
echo "  (b) sampling-interval sweep @ T=$T8 — shorter interval discovers/opens threads sooner"
for iv in 5 20 50; do
  p=$(perf_instr "$T8" "$ITERS"); u=$(uaps_instr "$T8" "$ITERS" "$iv")
  printf "      interval=%-4sms uaps/perf=%s\n" "$iv" "$(ratio "${u%.*}" "$p")"
done

echo
echo "== instr_crosscheck: $pass passed, $fail failed =="
echo "  uaps hw_instructions matches perf-stat ground truth within ${TOL} across thread densities."
echo "  (A prior 'undercount' was a methodology artifact: comparing N×1 vs 1×N MPI configs"
echo "   instead of vs perf — N×1 carries N× the per-rank MPI runtime, inflating the 'truth'.)"
[ "$fail" = 0 ]
