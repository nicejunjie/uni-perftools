#!/bin/bash
# Self-contained Quantum ESPRESSO validation for the suite (uaps + upat).
#
# Wipes ALL prior outputs first (so stale files from older runs can never be
# mistaken for fresh results), then re-runs every tier pinned to PHYSICAL cores
# (no SMT oversubscription) and regenerates every saved report under out/.
#
# Usage:  bash tests/qe/run.sh
set -u
cd "$(dirname "$0")"
HERE=$(pwd)

UPAT="$HERE/../../core/cli/upat"
UAPS=$(ls "$HERE"/../../collectors/snapshot/target/release/uaps \
          "$HERE"/../../collectors/snapshot/target/debug/uaps 2>/dev/null | head -1)

[ -x qenv/bin/pw.x ] || { echo "QE not installed (qenv/bin/pw.x missing) — see README.md"; exit 1; }
[ -x "$UPAT" ]       || { echo "upat not built — run 'make' first"; exit 1; }
[ -n "$UAPS" ]       || { echo "uaps not built — run 'make' first"; exit 1; }

# --- physical-core pinning (one thread per physical core, never an SMT sibling) ---
PHYS=$(lscpu -p=Socket,Core 2>/dev/null | grep -v '^#' | sort -u | wc -l)
[ "$PHYS" -ge 1 ] 2>/dev/null || PHYS=$(nproc)
HALF=$(( PHYS / 2 )); [ "$HALF" -ge 1 ] || HALF=1
export OMP_PLACES=cores OMP_PROC_BIND=close
echo "== QE validation: $PHYS physical cores (serial=$PHYS threads, MPI=2x$HALF) =="

# --- wipe old garbage: result dirs, scratch, and every regenerable out/ file ---
echo "-- cleaning old outputs --"
rm -rf out scratch
mkdir -p out scratch

# --- A) serial deep profile (upat) + snapshot tier folded into the same dir ---
echo "-- A) serial (upat + uaps snapshot) --"
OMP_NUM_THREADS=$PHYS OPENBLAS_NUM_THREADS=$PHYS \
  "$UPAT" run -o out/run_serial -- ./pw -in si.scf.in > out/qe.serial.out 2>&1
OMP_NUM_THREADS=$PHYS OPENBLAS_NUM_THREADS=$PHYS \
  "$UAPS" run --format json -o out/run_serial/snap.json -- ./pw -in si.scf.in > out/qe.snap_serial.out 2>&1
"$UPAT" report out/run_serial               > out/report.serial.txt 2>&1
"$UPAT" report out/run_serial --detail blas > out/detail.blas.txt   2>&1
"$UPAT" report out/run_serial --format html -o out >/dev/null 2>&1   # out/report.html

# --- B) MPI, 2 ranks x HALF threads = PHYS cores, bound to cores ---
echo "-- B) MPI 2 ranks (upat) --"
OMP_NUM_THREADS=$HALF OPENBLAS_NUM_THREADS=$HALF \
  "$UPAT" run -o out/run_mpi -- qenv/bin/mpirun -np 2 --bind-to core --map-by socket:PE=$HALF \
    ./pw -in si.scf.in > out/qe.mpi.out 2>&1
"$UPAT" report out/run_mpi                       > out/report.mpi.txt 2>&1
"$UPAT" report out/run_mpi --detail mpi          > out/detail.mpi.txt 2>&1
"$UPAT" report out/run_mpi --detail mpi --format html -o out >/dev/null 2>&1  # out/report.mpi.html

# --- C) per-function roofline (event sampling), single-threaded for clean hotspots ---
echo "-- C) per-function roofline (upat) --"
OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 \
  "$UPAT" roofline -o out/run_rf1 -- ./pw -in si.scf.in > out/qe.rf1.out 2>&1
"$UPAT" report out/run_rf1 --view roofline-func > out/roofline_func.txt 2>&1

# --- standalone snapshots (uaps report → stderr; target owns stdout) ---
echo "-- snapshots (uaps standalone) --"
OMP_NUM_THREADS=$PHYS OPENBLAS_NUM_THREADS=$PHYS \
  "$UAPS" run -- ./pw -in si.scf.in > /dev/null 2> out/snapshot.serial.txt
OMP_NUM_THREADS=$HALF OPENBLAS_NUM_THREADS=$HALF \
  "$UAPS" run -- qenv/bin/mpirun -np 2 --bind-to core --map-by socket:PE=$HALF ./pw -in si.scf.in \
    > /dev/null 2> out/snapshot.mpi.txt

# --- summary ---
echo ""
echo "== results =="
for f in qe.serial qe.mpi qe.rf1; do
  printf "  %-12s JOB DONE x%s\n" "$f" "$(grep -c 'JOB DONE' out/$f.out)"
done
printf "  serial prof.*: %s + snap.json:%s | mpi prof.*: %s\n" \
  "$(ls out/run_serial/prof.*.json 2>/dev/null | wc -l)" \
  "$([ -f out/run_serial/snap.json ] && echo yes || echo no)" \
  "$(ls out/run_mpi/prof.*.json 2>/dev/null | wc -l)"
echo "  saved reports:"; ls -1 out/*.txt out/*.html 2>/dev/null | sed 's,^,    ,'
