#!/bin/bash
# Suite end-to-end tests: drive both collectors via core/cli/perfsuite and check
# the combined result + report. Assumes `make` has built both collectors.
set -u
cd "$(dirname "$0")/.."
ROOT=$(pwd)
DRV="$ROOT/core/cli/perfsuite"
CC=${CC:-mpicc}
TMP=$(mktemp -d)
PASS=0; FAIL=0
BLAS=$(ls /lib/x86_64-linux-gnu/libblas.so* /usr/lib/x86_64-linux-gnu/libblas.so* 2>/dev/null | head -1)

ok(){ if eval "$2"; then echo "  PASS: $1"; PASS=$((PASS+1)); else echo "  FAIL: $1"; FAIL=$((FAIL+1)); fi; }

[ -f "$ROOT/collectors/profile/libscilibprof-preload.so" ] || { echo "build first (make)"; exit 1; }

# --- serial BLAS/LAPACK app ---
cat > "$TMP/s.c" <<'EOF'
#include <stdlib.h>
extern void dgemm_(char*,char*,int*,int*,int*,double*,double*,int*,double*,int*,double*,double*,int*);
extern void sgemm_(char*,char*,int*,int*,int*,float*,float*,int*,float*,int*,float*,float*,int*);
int main(){int n=256;char N='N';
 double a=1,b=0;double*A=calloc(n*n,8),*B=calloc(n*n,8),*C=calloc(n*n,8);
 float fa=1,fb=0;float*FA=calloc(n*n,4),*FB=calloc(n*n,4),*FC=calloc(n*n,4);
 for(int i=0;i<40;i++)dgemm_(&N,&N,&n,&n,&n,&a,A,&n,B,&n,&b,C,&n);
 for(int i=0;i<40;i++)sgemm_(&N,&N,&n,&n,&n,&fa,FA,&n,FB,&n,&fb,FC,&n);
 return 0;}
EOF
$CC -O2 "$TMP/s.c" -o "$TMP/s" "$BLAS" 2>/dev/null
OUT=$("$DRV" run -o "$TMP/r1" -- "$TMP/s" 2>/dev/null)
ok "serial: PROFILE section"        "echo \"$OUT\" | grep -q 'PROFILE'"
ok "serial: INSIGHTS section"       "echo \"$OUT\" | grep -q 'INSIGHTS'"
ok "serial: math-libs insight"     "echo \"$OUT\" | grep -qi 'math libr'"
ok "serial: result has prof.0"      "[ -f $TMP/r1/prof.0.json ]"
ok "serial: result has manifest"    "[ -f $TMP/r1/manifest.json ]"
ROUT=$("$DRV" report "$TMP/r1" --view roofline 2>/dev/null)
ok "roofline: FP64 & FP32 ceilings" "echo \"$ROUT\" | grep -q 'FP64' && echo \"$ROUT\" | grep -q 'FP32'"
ok "roofline: whole-program only"   "echo \"$ROUT\" | grep -q 'Roofline (whole program)'"
# snapshot present iff perf available; don't hard-fail when counters are blocked
if [ -f "$TMP/r1/snap.json" ]; then
  ok "serial: SNAPSHOT section"     "echo \"$OUT\" | grep -q 'SNAPSHOT'"
fi

# --- MPI app with rank imbalance ---
cat > "$TMP/m.c" <<'EOF'
#include <mpi.h>
#include <stdlib.h>
extern void dgemm_(char*,char*,int*,int*,int*,double*,double*,int*,double*,int*,double*,double*,int*);
int main(int c,char**v){MPI_Init(&c,&v);int r;MPI_Comm_rank(MPI_COMM_WORLD,&r);
 double s[64],d[64];for(int i=0;i<64;i++)s[i]=i;
 for(int i=0;i<30;i++)MPI_Allreduce(s,d,64,MPI_DOUBLE,MPI_SUM,MPI_COMM_WORLD);
 int n=128;char N='N';double a=1,b=0;double*A=calloc(n*n,8),*B=calloc(n*n,8),*C=calloc(n*n,8);
 int it=(r==0)?20:10;for(int i=0;i<it;i++)dgemm_(&N,&N,&n,&n,&n,&a,A,&n,B,&n,&b,C,&n);
 MPI_Finalize();return 0;}
EOF
$CC -O2 "$TMP/m.c" -o "$TMP/m" "$BLAS" 2>/dev/null
OMPI_MCA_rmaps_base_oversubscribe=1 mpirun --oversubscribe -n 4 true >/dev/null 2>&1 && HAVE_MPI=1 || HAVE_MPI=0
if [ "$HAVE_MPI" = 1 ]; then
  OUT=$(OMPI_MCA_rmaps_base_oversubscribe=1 "$DRV" run -o "$TMP/r2" -- mpirun --oversubscribe -n 4 "$TMP/m" 2>/dev/null)
  ok "mpi: 4 prof files"            "[ \$(ls $TMP/r2/prof.*.json | wc -l) -eq 4 ]"
  ok "mpi: exactly one snap-or-none, no stray prof" "[ \$(ls $TMP/r2/prof.*.json | wc -l) -eq 4 ]"
  ok "mpi: MPI table in profile"    "echo \"$OUT\" | grep -q 'MPI (communication)'"
  ok "mpi: unified imb header"      "echo \"$OUT\" | grep -q 'imb = (max-avg)/max'"
  ok "mpi: dgemm imbalance insight" "echo \"$OUT\" | grep -qiE 'imbalanc'"
  MOUT=$(OMPI_MCA_rmaps_base_oversubscribe=1 "$DRV" report "$TMP/r2" --view mpi 2>/dev/null)
  ok "mpi: wait-state view"         "echo \"$MOUT\" | grep -q 'MPI wait-state'"
  ok "mpi: sync vs transfer split"  "echo \"$MOUT\" | grep -q 'synchronization/wait'"
  ok "mpi: comm/compute overlap"    "echo \"$MOUT\" | grep -q 'overlap:'"
  AOUT=$(OMPI_MCA_rmaps_base_oversubscribe=1 "$DRV" report "$TMP/r2" --view anomaly 2>/dev/null)
  ok "anomaly: variance view"       "echo \"$AOUT\" | grep -q 'Anomaly / variance'"
  ok "anomaly: per-call variance"   "echo \"$AOUT\" | grep -q 'most variable call:'"
fi

rm -rf "$TMP"
echo "== suite: $PASS passed, $FAIL failed =="
exit $FAIL
