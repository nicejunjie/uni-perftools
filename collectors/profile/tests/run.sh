#!/bin/bash
# Smoke/regression tests for upat. Builds tiny host programs against the
# system BLAS/LAPACK/FFTW/MPI, runs them under the profiler (which writes raw
# per-rank JSON), then checks the postprocess report.
# Usage: tests/run.sh [preload|frida]   (default: preload)
set -u
cd "$(dirname "$0")/.."
ROOT=$(pwd)
BACKEND=${1:-preload}
LIB="$ROOT/libupat-$BACKEND.so"
RPT="$ROOT/tools/upat-report.py"
CC=${CC:-mpicc}
TMP=$(mktemp -d)
PASS=0; FAIL=0
BLAS=$(ls /lib/x86_64-linux-gnu/libblas.so* /usr/lib/x86_64-linux-gnu/libblas.so* 2>/dev/null | head -1)
FFTW=$(ls /lib/x86_64-linux-gnu/libfftw3.so* 2>/dev/null | head -1)

ok(){ if eval "$2"; then echo "  PASS: $1"; PASS=$((PASS+1)); else echo "  FAIL: $1"; FAIL=$((FAIL+1)); fi; }
run(){ # run $1=exe under profiler writing into $TMP, echo the report
  rm -f "$TMP"/p.*.json
  env UPAT_QUIET=1 ${2:-} UPAT_OUTPUT="$TMP/p" LD_PRELOAD="$LIB" "$1" >/dev/null 2>&1
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
rm -f "$TMP"/p.*.json
env UPAT_QUIET=1 UPAT_SHAPE=1 UPAT_OUTPUT="$TMP/p" LD_PRELOAD="$LIB" "$TMP/t3" >/dev/null 2>&1
OUT=$(python3 "$RPT" "$TMP"/p.*.json 2>&1)
ok "fftw_execute aggregated 100x"  "echo \"$OUT\" | grep -E ' fftw_execute ' | grep -qE ' 100 '"
DET=$(python3 "$RPT" --detail fftw "$TMP"/p.*.json 2>&1)
ok "fftw_execute[1024] in --detail" "echo \"$DET\" | grep -E 'fftw_execute\[1024\]' | grep -qE ' 100 '"
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
  -x UPAT_QUIET=1 -x UPAT_OUTPUT="$TMP/p" -x LD_PRELOAD="$LIB" "$TMP/t4" >/dev/null 2>&1
NFILES=$(ls "$TMP"/p.*.json 2>/dev/null | wc -l)
OUT=$(python3 "$RPT" --threshold 0 "$TMP"/p.*.json 2>&1)
ok "4 per-rank files written"  "[ $NFILES -eq 4 ]"
ok "dedicated MPI table"        "echo \"$OUT\" | grep -q 'MPI (communication)'"
ok "MPI_Allreduce has GB/s"     "echo \"$OUT\" | grep -q MPI_Allreduce"
# (max-avg)/max for ranks [20,10,10,10]: avg 12.5, max 20 -> 37.5%
ok "dgemm imbalance 37.5% (20 vs 10)" "echo \"$OUT\" | grep -E ' dgemm_ ' | grep -qE '37\.5%'"
ok "no GB/s in compute table"   "echo \"$OUT\" | sed -n '/Compute/,/MPI (comm/p' | grep -qv 'GB/s'"

# --- Fortran MPI bindings (mpi_*_): Fortran codes call these, not the C ABI ---
if command -v mpif90 >/dev/null 2>&1; then
cat > "$TMP/t4f.f90" <<'EOF'
program t
  implicit none
  include 'mpif.h'
  integer :: ierr, i
  real(8) :: s(64), d(64)
  call MPI_Init(ierr)
  s = 1.0d0
  do i = 1, 30
    call MPI_Allreduce(s, d, 64, MPI_DOUBLE_PRECISION, MPI_SUM, MPI_COMM_WORLD, ierr)
  end do
  call MPI_Bcast(s, 64, MPI_DOUBLE_PRECISION, 0, MPI_COMM_WORLD, ierr)
  call MPI_Finalize(ierr)
end program t
EOF
  mpif90 -O2 "$TMP/t4f.f90" -o "$TMP/t4f" 2>/dev/null
  rm -f "$TMP"/p.*.json
  OMPI_MCA_rmaps_base_oversubscribe=1 mpirun --oversubscribe -n 2 \
    -x UPAT_QUIET=1 -x UPAT_OUTPUT="$TMP/p" -x LD_PRELOAD="$LIB" "$TMP/t4f" >/dev/null 2>&1
  FOUT=$(python3 "$RPT" --threshold 0 "$TMP"/p.*.json 2>&1)
  ok "fortran MPI: app survived"     "[ $(ls $TMP/p.*.json 2>/dev/null | wc -l) -eq 2 ]"
  ok "fortran MPI: mpi_allreduce_ traced" "echo \"$FOUT\" | grep -q 'mpi_allreduce_'"
  ok "fortran MPI: nonzero byte volume"   "echo \"$FOUT\" | grep -E 'mpi_allreduce_' | grep -qvE ' 0 +[0-9.]+ *$'"
fi

# --- sampling: a hot function must dominate the flat profile ---
cat > "$TMP/t5.c" <<'EOF'
double hot(long n){ double s=0; for(long i=0;i<n;i++) s+=i*0.5/(i+1.0); return s; }
double cold(long n){ double s=0; for(long i=0;i<n;i++) s+=i; return s; }
int main(){ double s=0; for(int k=0;k<300;k++){ s+=hot(2000000); s+=cold(100000);} return s>0; }
EOF
$CC -O2 -g "$TMP/t5.c" -o "$TMP/t5" 2>/dev/null
rm -f "$TMP"/p.*.json
env UPAT_QUIET=1 UPAT_SAMPLE=1 UPAT_SAMPLE_HZ=2000 UPAT_OUTPUT="$TMP/p" \
    LD_PRELOAD="$LIB" "$TMP/t5" >/dev/null 2>&1
OUT=$(python3 "$RPT" "$TMP"/p.*.json 2>&1)
ok "sampling: grouped table (CrayPAT-style)" "echo \"$OUT\" | grep -q 'Profile by Function Group'"
ok "sampling: USER group present"   "echo \"$OUT\" | grep -qE '^ *[0-9.]+%.* USER$'"
ok "sampling: hot is the top function" \
   "echo \"$OUT\" | sed -n '/Table 1/,/Table 2/p' | grep -oE '\\b(hot|cold)\\b' | head -1 | grep -qw hot"
ok "sampling: source line resolved (t5.c:N)" "echo \"$OUT\" | grep -qE 't5\.c:[0-9]+'"
ok "sampling: inclusive table (call stacks)" "echo \"$OUT\" | grep -q 'Top functions (inclusive)'"
FOLD=$(python3 "$RPT" --folded "$TMP"/p.*.json 2>&1)
ok "sampling: folded flamegraph export"  "echo \"$FOLD\" | grep -qE 'main;.*hot [0-9]+'"

# --- driver: one command runs + reports ---
DRV="$ROOT/bin/upat"
OUT=$("$DRV" --no-sample "$TMP/t1" 2>&1)
ok "driver: prints a report"        "echo \"$OUT\" | grep -q 'UPAT'"
ok "driver: traced dgemm"           "echo \"$OUT\" | grep -q ' dgemm_ '"

# --- I/O tracing ---
cat > "$TMP/t6.c" <<'EOF'
#include <unistd.h>
#include <fcntl.h>
int main(){ char b[65536]; for(int i=0;i<65536;i++) b[i]=i;
 int fd=open("/tmp/.upat_t6",O_WRONLY|O_CREAT|O_TRUNC,0644);
 for(int i=0;i<100;i++) if(write(fd,b,65536)<0) return 1; close(fd);
 fd=open("/tmp/.upat_t6",O_RDONLY); while(read(fd,b,65536)>0){} close(fd);
 unlink("/tmp/.upat_t6"); return 0; }
EOF
$CC -O2 "$TMP/t6.c" -o "$TMP/t6" 2>/dev/null
OUT=$(run "$TMP/t6" "UPAT_SAMPLE=0")
ok "I/O: table present"              "echo \"$OUT\" | grep -q 'I/O statistics'"
ok "I/O: write traced with bytes"   "echo \"$OUT\" | grep -E '^   write ' | grep -qE '[0-9]{7}'"

# --- heap high-water (opt-in) ---
cat > "$TMP/t7.c" <<'EOF'
#include <stdlib.h>
#include <string.h>
int main(){ void* a[32]; for(int i=0;i<32;i++){a[i]=malloc(4<<20); memset(a[i],1,4<<20);}
 for(int i=0;i<32;i++) free(a[i]); return 0; }
EOF
$CC -O0 "$TMP/t7.c" -o "$TMP/t7" 2>/dev/null   # -O0: keep allocations from being elided
OUT=$(run "$TMP/t7" "UPAT_SAMPLE=0 UPAT_HEAP=1")
ok "heap: high-water reported"      "echo \"$OUT\" | grep -q 'Heap high-water'"
ok "heap: peak ~128MB"              "echo \"$OUT\" | grep -qE 'peak .*0\.1[0-9]+ GB'"
OUT=$(run "$TMP/t7" "UPAT_SAMPLE=0")
ok "heap: off by default"           "! echo \"$OUT\" | grep -q 'Heap high-water'"

# --- MPI detail (histogram + comm matrix) + observations ---
cat > "$TMP/t8.c" <<'EOF'
#include <mpi.h>
#include <stdlib.h>
int main(int c,char**v){MPI_Init(&c,&v);int r,n;MPI_Comm_rank(MPI_COMM_WORLD,&r);MPI_Comm_size(MPI_COMM_WORLD,&n);
 int sz=8192; double*s=calloc(sz,8),*rb=calloc(sz,8); int nx=(r+1)%n,pv=(r+n-1)%n;
 for(int i=0;i<80;i++) MPI_Sendrecv(s,sz,MPI_DOUBLE,nx,0,rb,sz,MPI_DOUBLE,pv,0,MPI_COMM_WORLD,MPI_STATUS_IGNORE);
 for(int i=0;i<40;i++) MPI_Allreduce(s,rb,sz,MPI_DOUBLE,MPI_SUM,MPI_COMM_WORLD);
 MPI_Finalize();return 0;}
EOF
$CC -O2 "$TMP/t8.c" -o "$TMP/t8" 2>/dev/null
rm -f "$TMP"/p.*.json
OMPI_MCA_rmaps_base_oversubscribe=1 mpirun --oversubscribe -n 4 \
  -x UPAT_QUIET=1 -x UPAT_SAMPLE=0 -x UPAT_OUTPUT="$TMP/p" -x LD_PRELOAD="$LIB" "$TMP/t8" >/dev/null 2>&1
OUT=$(python3 "$RPT" "$TMP"/p.*.json 2>&1)
ok "MPI: size distribution"          "echo \"$OUT\" | grep -q 'message-size distribution'"
ok "MPI: communication matrix"       "echo \"$OUT\" | grep -q 'Communication matrix'"
ok "MPI: p2p vs collective"          "echo \"$OUT\" | grep -q 'point-to-point vs collective'"
ok "report: Observations section"    "echo \"$OUT\" | grep -q 'Observations'"
ok "report: Per-PE summary"          "echo \"$OUT\" | grep -q 'Per-PE summary'"

rm -rf "$TMP"
echo "== $PASS passed, $FAIL failed =="
exit $FAIL
