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
# Prefer the release binary (faster, and the one we cap for node-level perf);
# fall back to debug. Explicit order — `ls` would sort "debug" before "release".
UAPS=""
for _c in release debug; do
  _p="$HERE/../../collectors/snapshot/target/$_c/uaps"
  [ -x "$_p" ] && { UAPS="$_p"; break; }
done

[ -x qenv/bin/pw.x ] || { echo "QE not installed (qenv/bin/pw.x missing) — see README.md"; exit 1; }
[ -x "$UPAT" ]       || { echo "upat not built — run 'make' first"; exit 1; }
[ -n "$UAPS" ]       || { echo "uaps not built — run 'make' first"; exit 1; }

# Node-level (system-wide) HW counting is allowed if perf_event_paranoid<=0, OR uaps
# carries cap_perfmon/cap_sys_admin (sudo setcap cap_perfmon+ep "$UAPS"), OR we're root.
node_perf_ok() {
  local p; p=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 99)
  [ "${p:-99}" -le 0 ] 2>/dev/null && return 0
  getcap "$UAPS" 2>/dev/null | grep -qiE 'cap_perfmon|cap_sys_admin' && return 0
  [ "$(id -u)" = 0 ] && return 0
  return 1
}

# --- physical-core pinning (one thread per physical core, never an SMT sibling) ---
PHYS=$(lscpu -p=Socket,Core 2>/dev/null | grep -v '^#' | sort -u | wc -l)
[ "$PHYS" -ge 1 ] 2>/dev/null || PHYS=$(nproc)
HALF=$(( PHYS / 2 )); [ "$HALF" -ge 1 ] || HALF=1
# The bundled conda OpenMPI bakes in the absolute install prefix from wherever the
# project tree lived when it was unpacked. After the tree is moved/renamed, mpirun
# fails with `prterun-exec-failed` and the MPI runs collect nothing — point it at
# the current qenv so it stays self-contained regardless of the checkout path.
[ -d "$HERE/qenv" ] && export OPAL_PREFIX="$HERE/qenv"
# This conda PMIx warns that its `pcompress` component is unavailable and can't
# find its (missing) help file, printing a "Sorry!" box AND a stray NUL byte to
# stderr — harmless, but the NUL makes the captured run-logs read as binary to
# grep/text tools. Silence it so the logs stay clean text.
export PMIX_MCA_pcompress_base_silence_warning=1
export OMP_PLACES=cores OMP_PROC_BIND=close
echo "== QE validation: $PHYS physical cores (serial=$PHYS threads, MPI=2x$HALF, large=${PHYS}x MPI) =="

# --- wipe old garbage: result dirs, scratch, and every regenerable out/ file ---
echo "-- cleaning old outputs --"
rm -rf out scratch scratch_big
mkdir -p out scratch

# --- A) serial deep profile (upat) + snapshot tier folded into the same dir ---
echo "-- A) serial (upat + uaps snapshot) --"
OMP_NUM_THREADS=$PHYS OPENBLAS_NUM_THREADS=$PHYS \
  "$UPAT" run -o out/run_serial -- ./pw -in si.scf.in > out/qe.serial.out 2>&1
OMP_NUM_THREADS=$PHYS OPENBLAS_NUM_THREADS=$PHYS \
  "$UAPS" run --format json -o out/run_serial/snap.json -- ./pw -in si.scf.in > out/qe.snap_serial.out 2>&1
# Two independent cost tiers, two separate reports — never combined.
# upat (deep profile) view:
"$UPAT" report out/run_serial --collector upat               > out/upat.serial.txt  2>&1
"$UPAT" report out/run_serial --collector upat --format html > out/upat.serial.html 2>&1
"$UPAT" report out/run_serial --detail blas                  > out/detail.blas.txt   2>&1
# uaps (snapshot) view — ASCII roofline in txt, real SVG roofline in html:
"$UPAT" report out/run_serial --collector uaps               > out/uaps.serial.txt  2>&1
"$UPAT" report out/run_serial --collector uaps --format html > out/uaps.serial.html 2>&1

# --- B) MPI, 2 ranks x HALF threads = PHYS cores, bound to cores ---
echo "-- B) MPI 2 ranks (upat profile) --"
OMP_NUM_THREADS=$HALF OPENBLAS_NUM_THREADS=$HALF \
  "$UPAT" run -o out/run_mpi -- qenv/bin/mpirun -np 2 --bind-to core --map-by socket:PE=$HALF \
    ./pw -in si.scf.in > out/qe.mpi.out 2>&1

# MPI snapshot (uaps, PER-RANK / APS-style): uaps is placed INSIDE the launcher
# (`mpirun … uaps run …` — launcher-agnostic, no flag-parsing/-x), so each rank counts
# ONLY its own process on its own node and writes snap.<rank>.json to a results dir;
# `uaps report` aggregates (+ per-rank HW imbalance). Needs only paranoid<=1 — no -a/cap.
echo "   + MPI per-rank snapshot (uaps, APS-style — each rank counts itself)"
OMP_NUM_THREADS=$HALF OPENBLAS_NUM_THREADS=$HALF \
  qenv/bin/mpirun -np 2 --bind-to core --map-by socket:PE=$HALF \
    "$UAPS" run --rank-dir out/run_mpi/uaps_ranks -- ./pw -in si.scf.in > out/qe.snap_mpi.out 2>&1
"$UAPS" report --format json -o out/run_mpi/snap.json out/run_mpi/uaps_ranks >> out/qe.snap_mpi.out 2>&1
"$UPAT" report out/run_mpi --collector uaps               > out/uaps.mpi.txt  2>&1
"$UPAT" report out/run_mpi --collector uaps --format html > out/uaps.mpi.html 2>&1

"$UPAT" report out/run_mpi --collector upat               > out/upat.mpi.txt  2>&1
"$UPAT" report out/run_mpi --collector upat --format html  > out/upat.mpi.html 2>&1  # deep MPI: top fns + matrix
"$UPAT" report out/run_mpi --detail mpi                    > out/detail.mpi.txt 2>&1
"$UPAT" report out/run_mpi --detail mpi --format html -o out >/dev/null 2>&1  # out/detail.mpi.html

# --- C) per-function roofline (event sampling), single-threaded for clean hotspots ---
echo "-- C) per-function roofline (upat) --"
OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 \
  "$UPAT" roofline -o out/run_rf1 -- ./pw -in si.scf.in > out/qe.rf1.out 2>&1
"$UPAT" report out/run_rf1 --view roofline-func > out/roofline_func.txt 2>&1

# --- D) large parallel: 54-atom Si supercell across ALL physical cores (pure MPI) ---
# Heavier, well-utilized run (real BLAS/LAPACK/FFT, not idle spin) — exercises the
# tools at scale and the node-level snapshot on a multi-rank job.
echo "-- D) large parallel: 54-atom supercell, $PHYS MPI ranks (upat profile) --"
rm -rf scratch_big; mkdir -p scratch_big
OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 \
  "$UPAT" run -o out/run_big -- qenv/bin/mpirun -np "$PHYS" --bind-to core \
    ./pw -in si_big.scf.in > out/qe.big.out 2>&1
echo "   + large per-rank snapshot (uaps, APS-style, $PHYS ranks)"
rm -rf scratch_big; mkdir -p scratch_big
OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 \
  qenv/bin/mpirun -np "$PHYS" --bind-to core \
    "$UAPS" run --rank-dir out/run_big/uaps_ranks -- ./pw -in si_big.scf.in > out/qe.big.snap.out 2>&1
"$UAPS" report --format json -o out/run_big/snap.json out/run_big/uaps_ranks >> out/qe.big.snap.out 2>&1
"$UPAT" report out/run_big --collector uaps               > out/uaps.big.txt  2>&1
"$UPAT" report out/run_big --collector uaps --format html > out/uaps.big.html 2>&1
"$UPAT" report out/run_big --collector upat > out/upat.big.txt      2>&1
"$UPAT" report out/run_big --detail mpi     > out/detail.big.mpi.txt 2>&1


# --- summary ---
echo ""
echo "== results =="
for f in qe.serial qe.mpi qe.rf1 qe.big; do
  printf "  %-12s JOB DONE x%s\n" "$f" "$(grep -c 'JOB DONE' out/$f.out)"
done
printf "  serial prof.*: %s + snap.json:%s | mpi prof.*: %s | big prof.*: %s + snap.json:%s\n" \
  "$(ls out/run_serial/prof.*.json 2>/dev/null | wc -l)" \
  "$([ -f out/run_serial/snap.json ] && echo yes || echo no)" \
  "$(ls out/run_mpi/prof.*.json 2>/dev/null | wc -l)" \
  "$(ls out/run_big/prof.*.json 2>/dev/null | wc -l)" \
  "$([ -f out/run_big/snap.json ] && echo yes || echo no)"
echo "  saved reports:"; ls -1 out/*.txt out/*.html 2>/dev/null | sed 's,^,    ,'
