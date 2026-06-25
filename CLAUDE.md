# CLAUDE.md

Guidance for Claude Code working in this repository.

## What this is

**Universal Performance Tool Suite** (uni-perftools) — two independent **cost-tier**
commands over a shared analysis core, branded after the vendor tools they descend
from (made portable). There is **no umbrella/driver command** — run one tier:

- **`uaps`** — Universal Application Performance *Snapshot* (← Intel APS). Cheap,
  **non-invasive** counter-based bird's-eye view (HWPC + MPI/OpenMP). Invoked exactly
  like APS — `mpirun -n N uaps ./app` places uaps INSIDE the launcher, so every rank
  counts ITS OWN process on its OWN node (per-process, needs only
  `perf_event_paranoid<=1`) and writes `./uaps_result/snap.<rank>.json`; then `uaps
  report uaps_result` aggregates across ranks (+ per-rank HW imbalance), like
  `aps-report`. This is **launcher-agnostic** (no launcher flag-parsing, no `-x` — each
  rank just reads its rank from the env). `-a` gives the old node-level system-wide mode
  (launcher node only). Rust, `collectors/snapshot/`.
- **`upat`** — Universal Performance Analysis *Tool* (← CrayPAT). Deep profiler:
  sci-lib tracing (BLAS/LAPACK/FFTW + Fortran/C MPI/IO), statistical + event-based
  sampling, per-function roofline, heap. C `.so` + `core/cli/upat`.
- **`core/`** — shared contract, symbolization, roofline ceilings, analysis
  viewpoints, insights engine, HTML reports, and the `upat` CLI.

`uaps` and `upat` are cost tiers (cheap-noninvasive vs deep-invasive); run one,
not both. `upat report` opportunistically folds in a `snap.json` if a `uaps` run
left one in the result dir.

### Invocation

```sh
mpirun -n 4 uaps ./app          # snapshot, APS form (launcher-agnostic) → ./uaps_result/
uaps report uaps_result         #   aggregate the per-rank dir (like aps-report)
upat run -- ./app               # deep profile + report (bare `upat ./app` auto-inserts `run`)
upat report  RESULT             # re-render a result dir (text); --format html for HTML
upat report  RESULT --detail mpi  # per-facility detail (comm matrix, per-shape calls)
upat roofline -- ./app          # per-function roofline (event sampling)
upat scale   R1 R2 ...          # strong/weak scaling across runs
```

`upat run`/`collect` honor `UPAT_*` env vars (see README.md table): `UPAT_SAMPLE`,
`UPAT_SAMPLE_HZ`, `UPAT_SAMPLE_STACK`, `UPAT_HEAP`, `UPAT_ROOFLINE`, `UPAT_OUTPUT`,
`UPAT_QUIET`. The human report goes to **stderr** (stdout belongs to the target).

## Self-contained project rule (IMPORTANT — applies to everything)

This project must be **fully self-contained**. Everything you create — test
inputs, downloaded workloads, pseudopotentials, build artifacts, run outputs —
**lives under the project directory**, in `tests/` (use a subdirectory per task,
e.g. `tests/qe/`). Concretely:

- **Never write to `/tmp`** or any path outside the project for test artifacts or
  outputs. Put them in `tests/<task>/`.
- **Never install toolchains or workloads system-wide.** If you need an external
  program (e.g. Quantum ESPRESSO) or data, fetch it **into the project test dir**
  (download + extract a package locally, build into a local prefix) and run it
  from there — do not `apt install` into `/usr`.
- Save command outputs to disk **in the test dir** (alongside, or instead of, just
  printing them) so a run is reproducible and inspectable later.
- Large downloaded/extracted artifacts should be git-ignored; keep small inputs
  and saved reports tracked so the validation is reproducible.

The goal: anyone can clone this repo and reproduce every check from within it,
without depending on machine-global state.

### Remote test hosts (cross-arch validation) — hygiene

Validation runs on remote machines (AMD Zen 3/4/5; ARM Grace via TACC Vista) live
under `~/scilib-prof-rearch/` on those hosts (shared `$HOME` on the cluster). Two
rules, both learned the hard way:

- **Keep scratch under the project dir, never the remote `$HOME` root.** Don't
  scp helper scripts / dump `o`/`e` output files into `~/` — stage them under the
  synced project tree (e.g. `tests/`).
- **Never rsync host-specific build artifacts between machines.** Exclude
  `build/`, `target/`, `*.so`, `*.a`, and `frida/` from deploys — a roofline cache
  calibrated on another CPU, an x86 Frida archive on aarch64, or a stale `build/`
  silently produced wrong results. Rebuild and recalibrate on the run host.

## Build / test

```sh
make                 # build both collectors (upat .so + cargo uaps)
make test            # validate-hwpc + profile tests + cargo tests + suite e2e tests
make validate-hwpc   # HW-independent: every vendored CPU model's top-down metrics
                     #   must fully resolve or be an explicit gap (no target HW needed)
```

`make test` chains four gates (see `Makefile`); run them individually while iterating:

```sh
bash collectors/profile/tests/run.sh preload   # C profiler (upat) e2e
cd collectors/snapshot && cargo test            # all Rust unit tests
cargo test -p uaps-collect                      # one crate
cargo test format_bytes                         # a single test by name
bash tests/run.sh                               # suite-level e2e (both commands)
bash tests/scale/run.sh                         # uaps per-rank at scale (oversubscription)
bash tests/scale/multinode.sh                   # uaps cross-NODE (2 containers, one mpirun)
```

`tests/scale/run.sh` simulates a large parallel job on one node: it oversubscribes
the cores with a synthetic per-rank workload (uaps INSIDE the launcher) to exercise
`uaps`'s per-rank collection + `uaps report` aggregation at high rank counts,
validates that a skewed workload shows up as cross-rank imbalance (and a balanced one
doesn't), and that a dead rank doesn't sink the report. `tests/scale/multinode.sh`
covers the cross-host behavior single-node can't: two containers act as two "nodes"
with one `mpirun` spanning both (via a docker-exec launch agent, no sshd) — it asserts
`uaps report` aggregates ranks from BOTH nodes when the results dir is on the shared
mount, and DETECTS+warns the short count when it's node-local (the undercount case).
Containers share the host PMU, so this validates orchestration, not per-node HW
accuracy. The aggregation MATH is covered deterministically by unit tests in
`crates/uaps-cli/src/aggregate.rs` (sum/max/mean, ratios recomputed from summed
raws, truncated-file skip) and the launcher arg-parser in `main.rs`.

CI mirrors this: `.github/workflows/ci.yml` (build + all e2e on x86 and arm; runners
have no PMU, so perf-gated checks self-skip) and the HWPC structural sweep.
Per-component details are in `collectors/profile/` and `collectors/snapshot/`
(the latter has its own CLAUDE.md with the full HWPC/PMU design).

## Architecture (cross-file picture)

Two collectors emit a **shared on-disk contract**; one analysis spine reads it.

- **Result-dir contract** (`core/contract/`, schema in `SCHEMA.md`): a run produces
  a result dir holding `prof.<rank>.json` (per-rank profile data from `upat`),
  optionally `snap.json` (from `uaps`), and `manifest.json`. `contract.py` is
  imported by both the writer (`core/cli`) and the readers (`core/analysis`) so the
  format never drifts. It also defines the **one** suite-wide imbalance metric
  `(max-avg)/max` — used identically by both collectors and the reports.
- **`core/cli/upat`** is the Python entry point for the deep tier. Verbs:
  `collect | run | roofline | scale | report`. A bare `upat ./app` auto-inserts
  `run`. `run`/`collect` set `LD_PRELOAD` (preload `.so`, or `--dbi` Frida for
  static bins) + `UPAT_*` env, run the child, then symbolize + aggregate per-rank
  raw files into a report. `report` re-renders an existing result dir and folds in a
  `snap.json` if present.
- **`core/analysis/`** turns parsed results into output: `viewpoints.py` (the
  selectable views — hotspots, roofline, roofline-func, microarch, memory, mpi, …),
  `insights.py` (bottleneck headline + observations), `report.py` (text),
  `htmlrep.py` (HTML), `scaling.py` (strong/weak scaling across runs).
- **`core/roofline/`**: `calibrate.c` empirically measures the machine's
  FLOP/bandwidth ceilings **per run on the run host** (never reuse a cross-machine
  cache); `roofline.py` places measured points against them.
- **`core/symbolize/`**: address→`func [file:line]` resolution shared by the tiers.
- **Profiler interception is generated, not hand-written**: the `upat` C profiler's
  per-symbol wrappers come from `collectors/profile/gen/gen.py` reading
  `gen/prototypes/*.txt` (one C signature per line; `fortran` dialect = all-pointer
  args, `c` dialect = by-value). To trace a new BLAS/LAPACK/MPI/IO symbol you add its
  prototype line — you don't write C. One generated `.c` serves both the `preload` and
  `frida` backends via the `WRAP()` macro. The Makefile regenerates on prototype change.

## Scale & robustness invariants (target: thousands of nodes / 10k+ ranks)

These are load-bearing for production scale — don't regress them when editing:

- **Aggregation is streaming, never O(nranks)-resident.** The report path folds each
  rank into cross-rank `[sum, max]` accumulators (the only inputs the `(max-avg)/max`
  metric needs) and discards it — see `reduce_rows` and `symbolize_samples` in
  `collectors/profile/tools/upat-report.py`. Never reintroduce per-rank lists/Counters
  keyed by function (that's O(nranks·funcs) and OOMs). The comm matrix / size histogram
  already stream one file at a time (`htmlrep.py`); copy that shape.
- **Partial inputs are normal at scale.** A rank killed mid-run leaves a truncated
  `prof.<rank>.json`; `load()` skips+counts unreadable files instead of aborting. Keep
  per-file `json.load` guarded.
- **Don't expand N rank paths onto argv** (E2BIG ~60k ranks). `report.py` passes the
  result *dir* to `upat-report.py`, which globs `prof.*.json` itself.
- **Per-rank `prof.<rank>.json` storage is O(degree), not O(nranks).** The MPI comm
  matrix uses a peer-keyed hashmap (`collectors/profile/src/analyzers/mpi.c`), not a
  dense `[max_peer]` array. The uaps MPI shim accumulates lock-free per-thread.
- **fds:** node-level/MPI counting opens ~7-9 perf groups *per online CPU*;
  `uaps_collect::raise_fd_limit()` lifts `RLIMIT_NOFILE` at startup and `EMFILE` is
  surfaced (not silently gapped).
- **Rank detection** must match `contract.rank_from_env` (`OMPI/PMI/PMIX/SLURM_PROCID/
  PALS_RANKID/ALPS_APP_PE`) in both `core/contract/contract.py` and
  `collectors/profile/src/core/util.c`, else ranks collide on `prof.0.json`.
- **Roofline byte-source consistency:** the plotted arithmetic-intensity point and the
  DRAM-bandwidth ceiling must use the *same* traffic source (prefetch-inclusive
  `mem_dram_reads` / `l2_fill_rsp_src.dram_io_*`), not demand-only fills — mixing them
  mis-places memory-bound kernels ~3×. See `_whole_program_point` (`htmlrep.py`),
  `ROOFLINE_EVENTS` (`core/cli/upat`), and `derive.rs`.
- **uaps MPI is per-rank, APS-style (`mpirun -n N uaps ./app`)** — the ONE invocation;
  there is deliberately no launcher-wrapping form (that needs launcher-specific flag
  parsing + `-x` propagation; not portable, and two ways to launch confuse users).
  uaps is placed INSIDE the launcher, so every rank counts ITS OWN process on its OWN
  node (per-process; `collect_rank` in `uaps-cli`), detects its rank from the env
  (`rank_from_env`, matching `contract.rank_from_env` — so it's **launcher-agnostic**:
  no flag-parsing, no `-x`), and writes `snap.<rank>.json` to a shared results dir
  (`./uaps_result`, or `--rank-dir`). `uaps report <dir>` then aggregates (like
  `aps-report`): reduce raws by policy (SUM counts/throughput, MAX wall/threads, MEAN %)
  then re-run `derive()` on the summed raws for exact ratios — keep that split (don't
  average ratios) — plus per-rank HW imbalance. The results dir must be on a **shared
  filesystem** (standard for this tool class: APS/CrayPAT). Each rank records its MPI
  world size, so if fewer ranks report than the job had (crash, or a node-local results
  dir the launch node can't fully see) the aggregator **warns on the short count**
  (`short_count_warning` in `aggregate.rs`) instead of silently undercounting. `-a`
  gives the old node-level (launcher-node only) system-wide path. Validated across two
  real machines (Zen 5 + Zen 4); see `tests/scale/multinode.sh`.
- **Every node must find the pmu-events DB, or vendor HWPC gaps** — `data_root()`
  (`pmudb.rs`) locates the DB at `<exe>/../../pmu-events` (or `UAPS_PMU_EVENTS`). A bare
  binary copied to a node WITHOUT its DB silently loses ALL FP/roofline/DRAM/top-down on
  that node, and the aggregate undercounts. So **deploy via `make install` onto a shared
  FS** (it co-locates the DB + the `core/` renderer next to the binary) — or stage the
  `pmu-events` tree alongside the binary. This is now LOUD not silent: collection warns
  (rank 0) when the DB is absent, and `uaps report` warns via `partial_hwpc_warning`
  when only some ranks carry vendor counters (`aggregate.rs`). Same for the text/HTML
  report: it needs the `core/cli/upat` renderer found next to the binary (install does
  this) or `UAPS_CORE_UPAT`; `--format json` needs neither.
- **Each rank tags its `snap.<rank>.json` with its node** — top-level `host`
  (hostname) + `arch` (`pmudb::node_arch()`, the CPU model e.g. `amdzen5`). `uaps
  report` (`node_participation` in `aggregate.rs`) shows per-node rank spread and
  **warns when ranks span >1 CPU model AND a roofline exists** — the aggregated
  roofline/GFLOPS/top-down would mix heterogeneous FLOP+bandwidth ceilings, so the
  single job-level point is not physically meaningful (group ranks by node type).
- **GPU offload makes the CPU-only roofline meaningless** — uaps reads only CPU
  counters, so a job that offloads compute to a GPU shows near-zero CPU FLOPs that
  would misplace it as "idle"/"memory-bound". `gpu::detect` (`uaps-collect`) flags it
  from `/proc/<pid>` during collection: an open compute device node (`/dev/nvidia*`,
  `/dev/kfd` → unambiguous) or a `/dev/dri/renderD*` render node **with** a mapped
  compute runtime (CUDA/ROCm/Level-Zero/OpenCL — the runtime requirement avoids
  desktop-graphics false positives). It pushes a `gpu_offload` metric (vendor in the
  label); `aggregate.rs` MAXes it (any rank → flagged) and `gpu_offload_warning` warns,
  and the shared renderer (`roofline_view`/`htmlrep`) **suppresses the roofline** with a
  "profile the device separately" note (and leads the insights with it). Best-effort +
  sticky (checked each sample until found); a GPU job that exits between samples is missed.
- **Per-process counting misses wrapped/forked work** (no `inherit`): `numactl`/
  `taskset`/shell wrappers measure the idle parent (each rank's app must `exec`, not
  fork). The C profiler suppresses its report write in `fork`-without-`exec` children
  (`pthread_atfork` in `libprof.c`) so they don't clobber the parent's rank file.

Regenerating the QE validation reports (`tests/qe/out/`) requires the run host (QE
binary + per-host roofline). The node-level `uaps` snapshots need `cap_perfmon` on the
`uaps` binary (`sudo setcap cap_perfmon+ep <uaps>`) or `perf_event_paranoid<=0`;
rebuilding the binary drops the capability, so re-`setcap` after `cargo build`.

The reports are **strictly single-tier** — there is no combined report; only the
Environment header is shared. The human report goes to **stderr** (the target owns
stdout, like `perf stat`).
