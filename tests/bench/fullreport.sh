#!/bin/bash
# Generate the COMPLETE set of reports for one app, exercising every feature:
#   - full uaps snapshot report (text + html)
#   - full upat deep-profile report (text + html)
#   - every upat --view lens, every --detail facility
#   - per-function roofline (event-sampling characterization pass)
# Everything lands under out/<NAME>/report/ for inspection.
#
# Usage:  fullreport.sh <NAME> <WORKDIR> -- <full command incl. mpirun ...>
set -u
SUITE=/home/junjie/vibe-coding/uni-perftools
UPAT="$SUITE/core/cli/upat"
UAPS="$SUITE/collectors/snapshot/target/release/uaps"
NAME=$1; WORKDIR=$2; shift 2; shift
CMD=("$@")
R="$SUITE/tests/bench/out/$NAME/report"
rm -rf "$R"; mkdir -p "$R"
cd "$WORKDIR" || exit 1
export OMP_NUM_THREADS=${OMP_NUM_THREADS:-1} OMP_PROC_BIND=close OMP_PLACES=cores

echo "### full report: $NAME  (OMP=$OMP_NUM_THREADS)"

# 1) FULL uaps snapshot — collect the JSON contract ONCE (-a, system-wide), then
#    render text + a single-file HTML from it via the shared core renderer.
#    (`uaps run --format html -o X` treats X as a result DIRECTORY, nesting the
#    file at X/uaps.html; rendering from snap.json yields a clean uaps.html FILE
#    and leaves snap.json behind for cheap re-renders.)
echo "    uaps snapshot (collect once -> text + html) ..."
mkdir -p "$R/uaps_data"
"$UAPS" run -a --format json -o "$R/uaps_data/snap.json" -- "${CMD[@]}" >/dev/null 2>&1
"$UPAT" report "$R/uaps_data" --collector uaps               > "$R/uaps.full.txt" 2>&1
"$UPAT" report "$R/uaps_data" --collector uaps --format html > "$R/uaps.html"     2>&1

# 2) upat deep-profile run (tracing + call-stack sampling).
echo "    upat deep profile ..."
"$UPAT" run -o "$R/upat_data" -- "${CMD[@]}" >/dev/null 2>&1
echo "      prof files: $(ls "$R"/upat_data/prof.*.json 2>/dev/null | wc -l)"

# 3) FULL upat report (text + html) from that data.
"$UPAT" report "$R/upat_data" --collector upat > "$R/upat.full.txt" 2>&1
"$UPAT" report "$R/upat_data" --collector upat --format html -o "$R/upat_html" >/dev/null 2>&1

# 4) every analysis VIEW (cheap re-renders of the same data).
for v in roofline imbalance threading mpi mpi-summary vectorization anomaly; do
  "$UPAT" report "$R/upat_data" --view "$v" > "$R/view.$v.txt" 2>&1
done

# 5) every per-facility DETAIL.
for d in blas lapack fftw mpi io; do
  "$UPAT" report "$R/upat_data" --detail "$d" > "$R/detail.$d.txt" 2>&1
done

# 6) per-function ROOFLINE characterization pass (FP/DRAM event sampling → measured points).
echo "    upat roofline characterization ..."
"$UPAT" roofline -o "$R/roof_data" -- "${CMD[@]}" >/dev/null 2>&1
"$UPAT" report "$R/roof_data" --view roofline-func > "$R/roofline_func.txt" 2>&1

echo "    done -> $R"
ls -1 "$R" | sed 's/^/        /'
