#!/bin/bash
# Cross-check harness: run ONE parallel workload under our tools (uaps snapshot +
# upat deep profile) and under AMD uProf (AMDuProfPcm --msr for IPC/FP/L3, and
# AMDuProfCLI tbp for hotspots), then stash everything under out/<NAME>/.
#
# Usage:  xcheck.sh <NAME> <WORKDIR> -- <full command incl. mpirun ...>
#
# Pure-MPI runs: we pin OMP_NUM_THREADS=1 so an N-rank job uses N cores (these
# mini-apps are hybrid MPI+OpenMP and would otherwise spawn N×ncpu threads).
set -u
SUITE=/home/junjie/vibe-coding/uni-perftools
UPAT="$SUITE/core/cli/upat"
UAPS="$SUITE/collectors/snapshot/target/release/uaps"
NAME=$1; WORKDIR=$2; shift 2; shift   # drop the "--"
CMD=("$@")
OUT="$SUITE/tests/bench/out/$NAME"
rm -rf "$OUT"; mkdir -p "$OUT"
cd "$WORKDIR" || exit 1
export OMP_NUM_THREADS=${OMP_NUM_THREADS:-1} OMP_PROC_BIND=close OMP_PLACES=cores
source /etc/profile.d/modules.sh 2>/dev/null; module load uprof/5.1 2>/dev/null
PCM=$(which AMDuProfPcm); CLI=$(which AMDuProfCLI)

echo "### $NAME   (OMP_NUM_THREADS=$OMP_NUM_THREADS)"
echo "    cmd: ${CMD[*]}"

echo "    [1/4] uaps snapshot (-a, system-wide HWPC) ..."
# Report to its own file via -o (it goes to stderr, like `perf stat`); send the
# app's own stdout/stderr to /dev/null so uaps.txt is the clean report, not mingled.
"$UAPS" run -a --format text -o "$OUT/uaps.txt" -- "${CMD[@]}" >/dev/null 2>&1
DUR=$(grep -oiE 'Elapsed time +[0-9.]+' "$OUT/uaps.txt" | grep -oE '[0-9.]+' | head -1)
DUR=${DUR%.*}; [ "${DUR:-0}" -ge 1 ] 2>/dev/null || DUR=10
WIN=$((DUR + 2))
echo "        (app ~${DUR}s; uProf window ${WIN}s)"

echo "    [2/4] upat deep profile ..."
"$UPAT" run -o "$OUT/upat" -- "${CMD[@]}" > "$OUT/upat.run.log" 2>&1
"$UPAT" report "$OUT/upat" --collector upat > "$OUT/upat.txt" 2>&1
"$UPAT" report "$OUT/upat" --view mpi > "$OUT/upat.mpi.txt" 2>&1

echo "    [3/4] uProf AMDuProfPcm --msr (IPC/FP/L3), full-run system-wide ..."
( "${CMD[@]}" > "$OUT/pcm.apprun.log" 2>&1 ) &
APP=$!
sudo -E "$PCM" --msr -m ipc,fp,l3 -a -A system -C -d "$WIN" -o "$OUT/pcm.csv" > "$OUT/pcm.log" 2>&1
wait "$APP" 2>/dev/null
sudo chown "$(id -u):$(id -g)" "$OUT/pcm.csv" 2>/dev/null

echo "    [4/4] uProf tbp hotspots ..."
# AMDuProfCLI's parser doesn't honor '--', so wrap the launcher in a script.
WRAP="$OUT/_run.sh"
{ echo '#!/bin/bash'; echo "cd '$WORKDIR'"; echo "export OMP_NUM_THREADS=$OMP_NUM_THREADS OMP_PROC_BIND=close OMP_PLACES=cores"; echo "exec $(printf '%q ' "${CMD[@]}")"; } > "$WRAP"
chmod +x "$WRAP"
sudo -E "$CLI" collect --config tbp -o "$OUT/tbp" -- "$WRAP" > "$OUT/tbp.log" 2>&1
SESS=$(find "$OUT/tbp" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | head -1)
[ -n "$SESS" ] && sudo -E "$CLI" report -i "$SESS" -o "$OUT/tbp_report.csv" > "$OUT/tbp.report.log" 2>&1
sudo chown -R "$(id -u):$(id -g)" "$OUT/tbp" "$OUT/tbp_report.csv" 2>/dev/null

# ---- auto comparison ----
echo "    ---- $NAME: uaps vs uProf ----"
python3 - "$OUT" <<'PY'
import sys,re,os,glob
out=sys.argv[1]
def g(f,pat):
    try: s=open(f,errors='ignore').read()
    except: return None
    m=re.search(pat,s,re.I); return m.group(1) if m else None
ua=os.path.join(out,'uaps.txt'); pc=os.path.join(out,'pcm.csv')
rows=[
 ("IPC",            g(ua,r'IPC \(instructions/cycle\)\s+([0-9.]+)'),        g(pc,r'IPC \(Sys \+ User\),([0-9.]+)')),
 ("FP GFLOP/s",     g(ua,r'FP throughput\s+([0-9.]+)'),                     g(pc,r'Retired SSE/AVX Flops\(GFLOPs\),([0-9.]+)')),
 ("DRAM BW GB/s",   g(ua,r'DRAM bandwidth\s+([0-9.]+)'),                    None),
 ("Util %",         g(ua,r'core utilization\s+([0-9.]+)'),                  g(pc,r'Utilization \(%\),([0-9.]+)')),
]
l3a=g(pc,r'L3 Access,([0-9.]+)'); l3m=g(pc,r'L3 Miss,([0-9.]+)')
dfa=g(ua,r'Demand fills \(all sources\)\s+([0-9]+)'); el=g(ua,r'Elapsed time\s+([0-9.]+)')
print("    %-16s %12s %12s"%("metric","uaps","uProf"))
for n,a,b in rows: print("    %-16s %12s %12s"%(n,a or '-',b or '-'))
if l3m and el: print("    %-16s %12s %12.1f  (L3miss*64/t)"%("DRAM BW proxy",'-',float(l3m)*64/float(el)/1e9))
if dfa and l3a: print("    %-16s %12s %12s  (fills vs L3 access)"%("fill traffic",dfa,l3a))
PY
echo "    done -> $OUT"
