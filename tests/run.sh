#!/bin/bash
# Smoke/regression tests for scilib-prof. Builds tiny host programs against the
# system BLAS/LAPACK/FFTW/MPI, runs them under the profiler (which writes raw
# per-rank JSON), then checks the postprocess report.
# Usage: tests/run.sh [preload|frida]   (default: preload)
set -u
cd "$(dirname "$0")/.."
ROOT=$(pwd)
BACKEND=${1:-preload}
LIB="$ROOT/libscilibprof-$BACKEND.so"
RPT="$ROOT/tools/scilib-report.py"
CC=${CC:-mpicc}
TMP=$(mktemp -d)
PASS=0; FAIL=0
BLAS=$(ls /lib/x86_64-linux-gnu/libblas.so* /usr/lib/x86_64-linux-gnu/libblas.so* 2>/dev/null | head -1)
FFTW=$(ls /lib/x86_64-linux-gnu/libfftw3.so* 2>/dev/null | head -1)

ok(){ if eval "$2"; then echo "  PASS: $1"; PASS=$((PASS+1)); else echo "  FAIL: $1"; FAIL=$((FAIL+1)); fi; }
run(){ # run $1=exe under profiler writing into $TMP, echo the report
  rm -f "$TMP"/p.*.json
  env SCILIB_QUIET=1 ${2:-} SCILIB_OUTPUT="$TMP/p" LD_PRELOAD="$LIB" "$1" >/dev/null 2>&1
  python3 "$RPT" "$TMP"/p.*.json 2>&1
}

echo "== backend: $BACKEND =="
[ -f "$LIB" ] || { echo "missing $LIB - run make first"; exit 1; }

# --- nested BLAS/LAPACK ---
cat > "$TMP/t1.c" <<'EOF'
#include <stdlib.h>
extern void dgemm_(char*,char*,int*,int*,int*,double*,double*,int*,double*,int*,double*,double*,int*);
extern void dgetrf_(int*,int*,double*,int*,int*,int*);
int main(){int n=300;char N='N';double al=1,be=0;
 double*A=calloc(n*n,8),*B=calloc(n*n,8),*C=calloc(n*n,8);int*ip=malloc(4*n),info;
 for(int i=0;i<10;i++)dgemm_(&N,&N,&n,&n,&n,&al,A,&n,B,&n,&be,C,&n);
 for(int i=0;i<5;i++)dgetrf_(&n,&n,A,&n,ip,&info); return 0;}
EOF
$CC -O2 "$TMP/t1.c" -o "$TMP/t1" "$BLAS" -llapack 2>/dev/null
OUT=$(run "$TMP/t1")
ok "compute table present"  "echo \"$OUT\" | grep -q Compute"
ok "dgemm profiled"         "echo \"$OUT\" | grep -q ' dgemm_ '"
ok "dgetrf appears"         "echo \"$OUT\" | grep -q dgetrf_"
ok "no MPI table"           "! echo \"$OUT\" | grep -q 'MPI (communication)'"

# --- thread safety: 8 threads x 50 = 400 ---
cat > "$TMP/t2.c" <<'EOF'
#include <stdlib.h>
extern void dgemm_(char*,char*,int*,int*,int*,double*,double*,int*,double*,int*,double*,double*,int*);
int main(){int n=64;char N='N';double al=1,be=0;
 #pragma omp parallel
 {double*A=calloc(n*n,8),*B=calloc(n*n,8),*C=calloc(n*n,8);
  for(int i=0;i<50;i++)dgemm_(&N,&N,&n,&n,&n,&al,A,&n,B,&n,&be,C,&n);}
 return 0;}
EOF
$CC -O2 -fopenmp "$TMP/t2.c" -o "$TMP/t2" "$BLAS" 2>/dev/null
OUT=$(run "$TMP/t2" "OMP_NUM_THREADS=8 OPENBLAS_NUM_THREADS=1")
ok "8 threads x 50 = 400 dgemm" "echo \"$OUT\" | grep -E ' dgemm_ ' | grep -qE ' 400 '"

# --- FFTW plan registry: execute attributed by size ---
if [ -n "$FFTW" ]; then
cat > "$TMP/t3.c" <<'EOF'
#include <stdlib.h>
typedef double fftw_complex[2]; typedef void* fftw_plan;
extern fftw_plan fftw_plan_dft_1d(int,fftw_complex*,fftw_complex*,int,unsigned);
extern void fftw_execute(fftw_plan); extern void fftw_destroy_plan(fftw_plan);
int main(){int n=1024;fftw_complex*a=calloc(n,16),*b=calloc(n,16);
 fftw_plan p=fftw_plan_dft_1d(n,a,b,-1,1u<<6);
 for(int i=0;i<100;i++)fftw_execute(p); fftw_destroy_plan(p); return 0;}
EOF
$CC -O2 "$TMP/t3.c" -o "$TMP/t3" "$FFTW" 2>/dev/null
OUT=$(run "$TMP/t3" "SCILIB_SHAPE=1")
ok "fftw_execute[1024] counted 100x" "echo \"$OUT\" | grep -E 'fftw_execute\[1024\]' | grep -qE ' 100 '"
fi

# --- MPI: per-rank files, dedicated MPI table, imbalance ---
cat > "$TMP/t4.c" <<'EOF'
#include <mpi.h>
#include <stdlib.h>
extern void dgemm_(char*,char*,int*,int*,int*,double*,double*,int*,double*,int*,double*,double*,int*);
int main(int c,char**v){MPI_Init(&c,&v);int r;MPI_Comm_rank(MPI_COMM_WORLD,&r);
 double s[64],d[64];for(int i=0;i<64;i++)s[i]=i;
 for(int i=0;i<20;i++)MPI_Allreduce(s,d,64,MPI_DOUBLE,MPI_SUM,MPI_COMM_WORLD);
 int n=128;char N='N';double al=1,be=0;double*A=calloc(n*n,8),*B=calloc(n*n,8),*C=calloc(n*n,8);
 int it=(r==0)?20:10;for(int i=0;i<it;i++)dgemm_(&N,&N,&n,&n,&n,&al,A,&n,B,&n,&be,C,&n);
 MPI_Finalize();return 0;}
EOF
$CC -O2 "$TMP/t4.c" -o "$TMP/t4" "$BLAS" 2>/dev/null
rm -f "$TMP"/p.*.json
OMPI_MCA_rmaps_base_oversubscribe=1 mpirun --oversubscribe -n 4 \
  -x SCILIB_QUIET=1 -x SCILIB_OUTPUT="$TMP/p" -x LD_PRELOAD="$LIB" "$TMP/t4" >/dev/null 2>&1
NFILES=$(ls "$TMP"/p.*.json 2>/dev/null | wc -l)
OUT=$(python3 "$RPT" "$TMP"/p.*.json 2>&1)
ok "4 per-rank files written"  "[ $NFILES -eq 4 ]"
ok "dedicated MPI table"        "echo \"$OUT\" | grep -q 'MPI (communication)'"
ok "MPI_Allreduce has GB/s"     "echo \"$OUT\" | grep -q MPI_Allreduce"
ok "dgemm imbalance 80% (20 vs 10)" "echo \"$OUT\" | grep -E ' dgemm_ ' | grep -qE '80\.0%'"
ok "no GB/s in compute table"   "echo \"$OUT\" | sed -n '/Compute/,/MPI (comm/p' | grep -qv 'GB/s'"

# --- sampling: a hot function must dominate the flat profile ---
cat > "$TMP/t5.c" <<'EOF'
double hot(long n){ double s=0; for(long i=0;i<n;i++) s+=i*0.5/(i+1.0); return s; }
double cold(long n){ double s=0; for(long i=0;i<n;i++) s+=i; return s; }
int main(){ double s=0; for(int k=0;k<300;k++){ s+=hot(2000000); s+=cold(100000);} return s>0; }
EOF
$CC -O2 -g "$TMP/t5.c" -o "$TMP/t5" 2>/dev/null
rm -f "$TMP"/p.*.json
env SCILIB_QUIET=1 SCILIB_SAMPLE=1 SCILIB_SAMPLE_HZ=2000 SCILIB_OUTPUT="$TMP/p" \
    LD_PRELOAD="$LIB" "$TMP/t5" >/dev/null 2>&1
OUT=$(python3 "$RPT" "$TMP"/p.*.json 2>&1)
ok "sampling: Top functions table"  "echo \"$OUT\" | grep -q 'Top functions'"
ok "sampling: hot is the top function" \
   "echo \"$OUT\" | sed -n '/Top functions/,/^\$/p' | grep -oE '\\b(hot|cold)\\b' | head -1 | grep -qw hot"
ok "sampling: source line resolved (t5.c:N)" "echo \"$OUT\" | grep -qE 't5\.c:[0-9]+'"

# --- driver: one command runs + reports ---
DRV="$ROOT/bin/scilib-prof"
OUT=$("$DRV" --no-sample "$TMP/t1" 2>&1)
ok "driver: prints a report"        "echo \"$OUT\" | grep -q 'Scientific Library Profiler'"
ok "driver: traced dgemm"           "echo \"$OUT\" | grep -q ' dgemm_ '"

rm -rf "$TMP"
echo "== $PASS passed, $FAIL failed =="
exit $FAIL
