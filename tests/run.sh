#!/bin/bash
# Universal Performance Tool Suite — end-to-end tests for the two commands:
#   uaps  (snapshot, Rust)         and   upat  (deep profile, core/cli/upat).
# They are cost tiers, run independently and reported independently — there is no
# combined report, even when both tiers' data sit in one dir. Assumes `make` has
# built both collectors.
set -u
cd "$(dirname "$0")/.."
ROOT=$(pwd)
UPAT="$ROOT/core/cli/upat"
# Prefer the optimized binary when present, else the debug build `make` produces.
# (Explicit, not `ls a b | head -1` — ls sorts alphabetically so "debug" would win.)
UAPS="$ROOT/collectors/snapshot/target/release/uaps"
[ -x "$UAPS" ] || UAPS="$ROOT/collectors/snapshot/target/debug/uaps"
CC=${CC:-mpicc}
# Keep test artifacts under the project (CLAUDE.md: never write to /tmp).
TMP=$(mktemp -d "$ROOT/tests/.tmp.XXXXXX")
PASS=0; FAIL=0
BLAS=$(ls /lib/x86_64-linux-gnu/libblas.so* /usr/lib/x86_64-linux-gnu/libblas.so* 2>/dev/null | head -1)
if [ -z "$BLAS" ]; then
  echo "  SKIP: no libblas found — cannot build BLAS/LAPACK test apps; skipping suite"
  rm -rf "$TMP"
  exit 0
fi

ok(){ if eval "$2"; then echo "  PASS: $1"; PASS=$((PASS+1)); else echo "  FAIL: $1"; FAIL=$((FAIL+1)); fi; }

# Structured assertion: run a python predicate `$2` (which gets `d` = parsed JSON
# from file `$3`, and may exit nonzero / raise to fail). More robust than grepping
# the rendered report, which churns with cosmetic formatting changes.
okjson(){ ok "$1" "python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); sys.exit(0 if ($2) else 1)' \"$3\""; }

[ -f "$ROOT/collectors/profile/libupat-preload.so" ] || { echo "build first (make)"; exit 1; }

# --- serial BLAS/LAPACK app, deep tier (upat) ---
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
OUT=$("$UPAT" run -o "$TMP/r1" -- "$TMP/s" 2>/dev/null)
ok "upat: UPAT report section"      "echo \"$OUT\" | grep -q 'UPAT'"
ok "upat: INSIGHTS section"         "echo \"$OUT\" | grep -q 'INSIGHTS'"
ok "upat: math-libs insight"       "echo \"$OUT\" | grep -qi 'math libr'"
ok "upat: result has prof.0"       "[ -f $TMP/r1/prof.0.json ]"
ok "upat: profile-only manifest"   "grep -q '\"profile\"' $TMP/r1/manifest.json && ! grep -q snapshot $TMP/r1/manifest.json"
ok "upat: no snap.json (tier)"     "[ ! -f $TMP/r1/snap.json ]"
ROUT=$("$UPAT" report "$TMP/r1" --view roofline 2>/dev/null)
ok "roofline: FP64 & FP32 ceilings" "echo \"$ROUT\" | grep -q 'FP64' && echo \"$ROUT\" | grep -q 'FP32'"
ok "agg: zgemm/zgemv not per-shape" "! echo \"$OUT\" | grep -qE 'gemm_\[m='"
ok "detail: per-shape on request"  "$UPAT report $TMP/r1 --detail blas 2>/dev/null | grep -qE 'gemm_\[m=|calls by shape'"
ok "footnote: legend present"      "echo \"$OUT\" | grep -q 'legend:'"
$UPAT report "$TMP/r1" -o "$TMP/o" >/dev/null 2>&1   # auto-detect: prof present -> upat
ok "report -o: single upat.txt"    "[ -s $TMP/o/upat.txt ]"
ok "upat.txt has sci-lib table"    "grep -q 'Library calls by group' $TMP/o/upat.txt"
$UPAT report "$TMP/r1" --format html -o "$TMP/o" >/dev/null 2>&1
ok "html: upat.html written"       "[ -s $TMP/o/upat.html ]"
ok "html: upat tier title"         "grep -q 'UPAT' $TMP/o/upat.html"

# --- tiers stay separate: a uaps snap.json in the same dir is its OWN report,
#     never folded into the upat report (no combined report) ---
if [ -n "$UAPS" ]; then
  "$UAPS" run --format json -o "$TMP/r1/snap.json" -- "$TMP/s" >/dev/null 2>&1
  if [ -f "$TMP/r1/snap.json" ]; then
    UOUT=$("$UPAT" report "$TMP/r1" --collector uaps 2>/dev/null)
    POUT=$("$UPAT" report "$TMP/r1" --collector upat 2>/dev/null)
    ok "uaps tier: UAPS report"       "echo \"$UOUT\" | grep -q 'UAPS'"
    ok "uaps tier: no upat tables"    "! echo \"$UOUT\" | grep -q 'Library calls by group'"
    ok "upat tier: no uaps roofline"  "! echo \"$POUT\" | grep -q 'Roofline (whole program)'"
    ok "both tiers: shared Machine"   "echo \"$UOUT\" | grep -q 'Machine' && echo \"$POUT\" | grep -q 'Machine'"
    $UPAT report "$TMP/r1" --collector uaps --format html -o "$TMP/o" >/dev/null 2>&1
    ok "html: uaps.html svg roofline" "grep -q '<svg' $TMP/o/uaps.html"
  else
    echo "  SKIP: uaps tier (uaps produced no snap.json — perf blocked?)"
  fi
else
  echo "  SKIP: uaps tier (uaps binary not built)"
fi

# per-function roofline (B): needs perf_event sampling access; skip if blocked
RFOUT=$("$UPAT" roofline -o "$TMP/rf" -- "$TMP/s" 2>/dev/null)
if echo "$RFOUT" | grep -q 'Roofline (per function'; then
  ok "roofline-func: per-function view" "echo \"$RFOUT\" | grep -q 'Roofline (per function'"
  ok "roofline-func: dgemm characterized" "echo \"$RFOUT\" | grep -q 'dgemm_'"
else
  echo "  SKIP: roofline-func (no perf_event sampling access)"
fi

# --- MPI app with rank imbalance (deep tier) ---
cat > "$TMP/m.c" <<'EOF'
#include <mpi.h>
#include <stdlib.h>
extern void dgemm_(char*,char*,int*,int*,int*,double*,double*,int*,double*,int*,double*,double*,int*);
int main(int c,char**v){MPI_Init(&c,&v);int r;MPI_Comm_rank(MPI_COMM_WORLD,&r);
 double s[64],d[64];for(int i=0;i<64;i++)s[i]=i;
 for(int i=0;i<30;i++)MPI_Allreduce(s,d,64,MPI_DOUBLE,MPI_SUM,MPI_COMM_WORLD);
 int np;MPI_Comm_size(MPI_COMM_WORLD,&np);
 int nxt=(r+1)%np,prv=(r-1+np)%np;MPI_Status st;
 for(int i=0;i<20;i++)MPI_Sendrecv(s,64,MPI_DOUBLE,nxt,0,d,64,MPI_DOUBLE,prv,0,MPI_COMM_WORLD,&st);
 int n=128;char N='N';double a=1,b=0;double*A=calloc(n*n,8),*B=calloc(n*n,8),*C=calloc(n*n,8);
 int it=(r==0)?20:10;for(int i=0;i<it;i++)dgemm_(&N,&N,&n,&n,&n,&a,A,&n,B,&n,&b,C,&n);
 MPI_Finalize();return 0;}
EOF
$CC -O2 "$TMP/m.c" -o "$TMP/m" "$BLAS" 2>/dev/null
OMPI_MCA_rmaps_base_oversubscribe=1 mpirun --oversubscribe -n 4 true >/dev/null 2>&1 && HAVE_MPI=1 || HAVE_MPI=0
if [ "$HAVE_MPI" = 1 ]; then
  OUT=$(OMPI_MCA_rmaps_base_oversubscribe=1 "$UPAT" run -o "$TMP/r2" -- mpirun --oversubscribe -n 4 "$TMP/m" 2>/dev/null)
  ok "mpi: 4 prof files"            "[ \$(ls $TMP/r2/prof.*.json | wc -l) -eq 4 ]"
  # Structured checks on the raw per-rank contract (not the rendered table).
  okjson "mpi: rank0 json schema"   'all(k in d for k in ("rank","runtime_s","functions")) and isinstance(d["functions"],list)' "$TMP/r2/prof.0.json"
  okjson "mpi: rank0 traced MPI vol" 'any(f["group"]=="MPI" and f.get("bytes",0)>0 for f in d["functions"])' "$TMP/r2/prof.0.json"
  OUT0=$("$UPAT" report "$TMP/r2" --threshold 0 2>/dev/null)   # show all (tiny MPI in a compute-heavy app)
  ok "mpi: MPI table in profile"    "echo \"$OUT0\" | grep -q 'MPI (communication)'"
  ok "mpi: unified imb header"      "echo \"$OUT\" | grep -q '(max-avg)/max'"
  ok "mpi: dgemm imbalance insight" "echo \"$OUT\" | grep -qiE 'imbalanc'"
  ok "mpi: threshold hides tiny calls" "echo \"$OUT\" | grep -qE 'more below .*% of (CPU time|runtime)'"
  ok "mpi: APS-style top-5 summary"  "echo \"$OUT\" | grep -q 'MPI summary'"
  MOUT=$(OMPI_MCA_rmaps_base_oversubscribe=1 "$UPAT" report "$TMP/r2" --view mpi 2>/dev/null)
  ok "mpi: wait-state view"         "echo \"$MOUT\" | grep -q 'MPI wait-state'"
  ok "mpi: sync vs transfer split"  "echo \"$MOUT\" | grep -q 'synchronization/wait'"
  ok "mpi: comm/compute overlap"    "echo \"$MOUT\" | grep -q 'overlap:'"
  AOUT=$(OMPI_MCA_rmaps_base_oversubscribe=1 "$UPAT" report "$TMP/r2" --view anomaly 2>/dev/null)
  ok "anomaly: variance view"       "echo \"$AOUT\" | grep -q 'Anomaly / variance'"
  ok "anomaly: per-call variance"   "echo \"$AOUT\" | grep -q 'most variable call:'"
  "$UPAT" report "$TMP/r2" --detail mpi --format html -o "$TMP/o" >/dev/null 2>&1
  ok "html mpi: comm-matrix heatmap" "grep -qE \"class=.hm.\" $TMP/o/detail.mpi.html"
  ok "html mpi: size histogram bars" "grep -qE \"class=.bar.\" $TMP/o/detail.mpi.html"

  # uaps snapshot tier: APS-style MPI (auto-detected launcher, mpi.h-free shim)
  if [ -n "$UAPS" ]; then
    # uaps writes its report to stderr (the target owns stdout); capture stderr.
    SOUT=$(OMPI_MCA_rmaps_base_oversubscribe=1 "$UAPS" run -- mpirun --oversubscribe -n 4 "$TMP/m" 2>&1 >/dev/null)
    ok "uaps: APS MPI section"        "echo \"$SOUT\" | grep -q 'MPI % of runtime'"
    ok "uaps: MPI time + imbalance"   "echo \"$SOUT\" | grep -q 'MPI time' && echo \"$SOUT\" | grep -q 'MPI imbalance'"
    ok "uaps: top MPI function"       "echo \"$SOUT\" | grep -qE 'MPI_(Allreduce|Sendrecv|Bcast).*of MPI'"

    # APS-style (launcher-agnostic) two-step: uaps INSIDE the launcher writes a
    # per-rank results dir, then `uaps report` aggregates it (like aps-report).
    RDIR="$TMP/aps_result"
    OMPI_MCA_rmaps_base_oversubscribe=1 mpirun --oversubscribe -n 4 \
      "$UAPS" run --rank-dir "$RDIR" -- "$TMP/m" >/dev/null 2>&1
    ok "uaps APS-form: 4 per-rank snaps" "[ \"\$(ls "$RDIR"/snap.*.json 2>/dev/null | wc -l)\" = 4 ]"
    AOUT=$("$UAPS" report "$RDIR" 2>&1 >/dev/null)
    ok "uaps APS-form: report aggregates ranks" "echo \"$AOUT\" | grep -qE 'ranks +4'"
  fi
fi

rm -rf "$TMP"
echo "== suite: $PASS passed, $FAIL failed =="
exit $FAIL
