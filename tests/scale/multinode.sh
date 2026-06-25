#!/bin/bash
# Multi-NODE validation of uaps's APS-style per-rank collection using two containers
# as two "nodes" — the cross-host behavior single-node oversubscription can't cover:
#   * a single mpirun spanning both nodes runs `uaps run` INSIDE each rank
#     (launcher-agnostic — no flag-parsing, no -x); each writes snap.<rank>.json
#   * with the results dir on a SHARED filesystem `uaps report` aggregates every
#     rank across both nodes  (positive: nranks == total)
#   * with a NODE-LOCAL results dir the launch node only sees its own ranks, and the
#     short count is DETECTED + warned, not silently undercounted  (negative)
#
# Caveats (honest): containers share the host PMU, so HW counters are NOT
# per-node-independent here (and many sandboxes block perf_event_open in
# containers entirely) — this validates ORCHESTRATION (rank count, shared-FS
# aggregation, env propagation, binary placement), not per-node HW accuracy.
# mpirun spans the containers via a docker-exec launch agent (no sshd/apt needed);
# node0 therefore needs the docker socket + CLI mounted. Opt-in, local only.
#
# Usage: bash tests/scale/multinode.sh
set -u
cd "$(dirname "$0")"; ROOT=$(cd ../.. && pwd)
IMG=ubuntu:24.04
N0=uaps-mn0 N1=uaps-mn1 NET=uaps-mn
UAPS="$ROOT/collectors/snapshot/target/release/uaps"
[ -x "$UAPS" ] || UAPS="$ROOT/collectors/snapshot/target/debug/uaps"
MPIRUN="$ROOT/tests/qe/qenv/bin/mpirun"

command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1 \
  || { echo "SKIP: docker not usable"; exit 0; }
[ -x "$UAPS" ]   || { echo "uaps not built — run 'make' first"; exit 1; }
[ -x "$MPIRUN" ] || { echo "SKIP: bundled mpirun missing — build tests/qe/qenv first"; exit 0; }
[ -S /var/run/docker.sock ] || { echo "SKIP: no docker socket to mount for the launch agent"; exit 0; }

cleanup() { docker rm -f "$N0" "$N1" >/dev/null 2>&1; docker network rm "$NET" >/dev/null 2>&1; }
trap cleanup EXIT
cleanup

mkdir -p out
cc -O2 -o out/flops_by_rank flops_by_rank.c || { echo "build failed"; exit 1; }
FLOPS="$ROOT/tests/scale/out/flops_by_rank"

# rsh agent: prte calls `agent <host> <shell-cmd>`; run it via docker exec + a
# shell (prte's command sets PRTE_PREFIX/LD_LIBRARY_PATH then runs prted).
cat > out/docker-rsh.sh <<EOF
#!/bin/sh
host=\$1; shift
exec docker exec -i "\$host" sh -c "\$*"
EOF
chmod +x out/docker-rsh.sh
AGENT="$ROOT/tests/scale/out/docker-rsh.sh"

docker network create "$NET" >/dev/null 2>&1 || true
docker run -d --name "$N1" --hostname "$N1" --network "$NET" --security-opt seccomp=unconfined \
  -v "$ROOT":"$ROOT" -w "$ROOT" "$IMG" sleep 3000 >/dev/null
docker run -d --name "$N0" --hostname "$N0" --network "$NET" --security-opt seccomp=unconfined \
  -v "$ROOT":"$ROOT" -w "$ROOT" \
  -v /var/run/docker.sock:/var/run/docker.sock -v /usr/bin/docker:/usr/bin/docker:ro \
  "$IMG" sleep 3000 >/dev/null
sleep 1
docker exec "$N0" docker ps >/dev/null 2>&1 || { echo "SKIP: docker CLI unusable inside node0"; exit 0; }

val_in() { docker exec "$N0" sh -c "grep -oE '\"key\": \"nranks\"[^}]*\"value\": [0-9]+' '$1' | grep -oE '[0-9]+\$'"; }
# APS form: run uaps INSIDE the spanning mpirun (launcher-agnostic), each rank writes
# snap.<rank>.json to <rank-dir>, then `uaps report` aggregates. <rank-dir> on the
# shared mount → all ranks; node-local → only the launch node's ranks (undercount).
run_aps() { # <rank-dir> <out.json> <stderr-file>
  docker exec -w "$ROOT/tests/scale/out" \
    -e OPAL_PREFIX="$ROOT/tests/qe/qenv" -e PMIX_MCA_pcompress_base_silence_warning=1 \
    -e PRTE_MCA_plm_rsh_agent="$AGENT" -e PRTE_MCA_plm_ssh_agent="$AGENT" \
    "$N0" "$MPIRUN" --allow-run-as-root --host "$N0:2,$N1:2" -np 4 \
    "$UAPS" run --rank-dir "$1" -- "$FLOPS" 8000000 >/dev/null 2>"$3"
  docker exec "$N0" "$UAPS" report --format json -o "$2" "$1" >>"$3" 2>&1
}

pass=0; fail=0
ok()  { echo "  PASS: $1"; pass=$((pass+1)); }
bad() { echo "  FAIL: $1"; fail=$((fail+1)); }

echo "== uaps multi-node (2 containers, single spanning mpirun, APS form) =="

# POSITIVE: results dir on the SHARED mount → every rank across both nodes aggregates.
echo "-- shared results dir — must aggregate all 4 ranks across both nodes --"
SH="$ROOT/tests/scale/out/mn_shared"; docker exec "$N0" rm -rf "$SH" 2>/dev/null
run_aps "$SH" "$ROOT/tests/scale/out/mn_shared.json" out/mn_pos.err
nr=$(val_in "$ROOT/tests/scale/out/mn_shared.json")
[ "${nr:-0}" = 4 ] && ok "APS form aggregates all 4 ranks across both nodes (shared dir)" \
  || bad "expected nranks=4 across nodes, got '${nr:-none}'"
grep -qi "WARNING: aggregated" out/mn_pos.err \
  && bad "a complete run should NOT warn, but it did" || ok "no false-positive warning on a complete run"

# NEGATIVE: results dir is NODE-LOCAL (/tmp, per-container) → the launch node only
# sees its own ranks; the short count must be DETECTED + warned, not silent.
echo "-- node-local results dir (/tmp) — must detect the short count + warn --"
docker exec "$N0" rm -rf /tmp/mn_nl /tmp/mn_nl.json 2>/dev/null
run_aps /tmp/mn_nl /tmp/mn_nl.json out/mn_neg.err
nl=$(val_in /tmp/mn_nl.json)
[ "${nl:-0}" = 2 ] && ok "node-local dir → only launch-node ranks (nranks=2)" \
  || bad "expected nranks=2 on node-local dir, got '${nl:-none}'"
grep -qiE "WARNING: aggregated 2 of 4" out/mn_neg.err \
  && ok "short count DETECTED + warned (not silent)" \
  || bad "node-local undercount was SILENT — no warning"

echo "== multinode: $pass passed, $fail failed =="
[ "$fail" = 0 ]
