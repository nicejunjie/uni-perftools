#!/bin/bash
# Orchestrate the full local review run: complete uaps+upat report set for every
# real HPC app under tests/bench (via fullreport.sh). No uProf / no sudo needed.
# STREAM is OpenMP (16 threads); the rest are pure-MPI (16 ranks, 1 thread/rank).
set -u
SUITE=/home/junjie/vibe-coding/uni-perftools
BENCH="$SUITE/tests/bench"
FR="$BENCH/fullreport.sh"

run() {  # name  workdir  ompthreads  -- cmd...
  local name=$1 wd=$2 omp=$3; shift 3; shift   # drop "--"
  echo; echo "================================================================"
  echo "### APP: $name   (OMP_NUM_THREADS=$omp)   cmd: $*"
  echo "================================================================"
  OMP_NUM_THREADS=$omp bash "$FR" "$name" "$wd" -- "$@"
}

run cloverleaf "$BENCH/CloverLeaf_ref"   1 -- mpirun --oversubscribe -np 16 --bind-to core ./clover_leaf
run tealeaf    "$BENCH/TeaLeaf_ref"      1 -- mpirun --oversubscribe -np 16 --bind-to core ./tea_leaf
run hpcg       "$BENCH/hpcg/build/bin"   1 -- mpirun --oversubscribe -np 16 --bind-to core ./xhpcg
run hpl        "$BENCH/hpl-2.3/bin/zen5" 1 -- mpirun --oversubscribe -np 16 --bind-to core ./xhpl
run pot3d      "$BENCH/POT3D/run"        1 -- mpirun --oversubscribe -np 16 --bind-to core ../bin/pot3d
run stream     "$BENCH"                 16 -- ./stream

echo; echo "== ALL BENCH REPORTS DONE =="
for d in "$BENCH"/out/*/report; do
  [ -d "$d" ] && printf "  %-12s -> %s files\n" "$(basename "$(dirname "$d")")" "$(ls "$d" | wc -l)"
done
