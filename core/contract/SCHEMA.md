# Result contract (schema v1)

The two collectors never talk directly — they meet on disk in a **result
directory**. The CLI writes a manifest; each collector writes its own
namespaced file; the analysis core reads all of them.

```
<result>/
  manifest.json        # written by core/cli
  snap.json            # written by the snapshot collector (run-level)
  prof.<rank>.json     # written by the profile collector (one per rank)
```

## manifest.json
```json
{ "schema_version": 1,
  "command": ["mpirun","-n","4","./app"],
  "nranks": 4,
  "collectors": ["snapshot","profile"],
  "two_pass": false,
  "finalized": true,
  "timestamp": "<iso8601>" }
```

## snap.json  (snapshot collector — RUN-LEVEL)
```json
{ "metrics":  [ {"key","label","value","unit","display"} ],
  "insights": [ {"headline","detail"} ] }     # insights[] suppressed in-suite
```
Roofline/characterization keys live in `metrics` (e.g. `dp_gflops`,
`arith_intensity`, `peak_gflops`, `peak_bw_gbs`, `memory_bound`, `ipc`, …).

The snap.json is run-level, but it is produced one of two ways:
- **single / node-level (`-a`)**: counters read on the process (or whole node)
  from outside — one snapshot, no rank dimension.
- **per-rank (APS-style)**: `mpirun -n N uaps ./app` places uaps inside the launcher;
  each rank counts its OWN process on its own node and writes `snap.<rank>.json` to a
  shared results dir, which `uaps report <dir>` aggregates into this run-level snap.json
  (like `aps-report`) — counts/throughput SUM, wall time MAX, percentages MEAN, and the
  ratios (IPC, CPI, bandwidth…) RECOMPUTED from the summed raws. The aggregate
  additionally carries `nranks`, `mpi_world_size`, and per-rank HW imbalance
  `<key>_imbalance_pct` (`(max-avg)/max` over ranks, for `gflops`, `ipc`,
  `memory_bound`, `cpu_time`, `elapsed_time`). If `nranks < mpi_world_size` (ranks that
  crashed, or a node-local results dir the launch node can't fully see) the report warns
  rather than silently undercounting.

The transient per-rank `snap.<rank>.json` use the same `{ "metrics": [...] }`
shape as the aggregate; they are not part of the saved result dir.

## prof.<rank>.json  (profile collector — PER-RANK)
```json
{ "rank": 0, "application": "...", "runtime_s": 0.37,
  "functions": [ {"group","function","count","t_incl","t_excl","bytes"} ],
  "sampling":  { "hz", "stack", "samples"|"stacks", "maps", "total", "dropped" },
  "mpi_detail":{ "bins", "sent", "recv" },
  "heap":      { "peak","live_at_exit","allocs" } }
```

## Conventions (obeyed by both collectors + the analyzer)
- **Rank** is taken from the launcher environment
  (`OMPI_COMM_WORLD_RANK` / `PMI_RANK` / `MV2_COMM_WORLD_RANK` / `PMIX_RANK`), else 0.
- **Imbalance (suite-wide)** = `(max − avg) / max` over participating ranks,
  reported with the absolute companion `max − avg`. One definition, everywhere.
- **Cross-rank aggregation**: reduce by key (group, function[, shape]); for each
  key keep sum, and per-rank min/avg/max for the imbalance.
- **Group categories** (for the bird's-eye time view): `compute`, `math-libs`,
  `MPI`, `IO`, `system` — see `categories` in `contract.py`.
