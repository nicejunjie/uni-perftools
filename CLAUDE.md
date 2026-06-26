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
bash tests/scale/aggregate_scale.sh [N]         # `uaps report` at 10k–100k ranks (synthetic)
bash tests/scale/hybrid.sh                      # hybrid MPI×OpenMP (per-thread counting)
bash tests/scale/cross_mpi.sh                   # launcher-agnostic: 7 rank schemes + MPICH ABI
bash tests/scale/instr_crosscheck.sh            # hw_instructions vs `perf stat` ground truth
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

Three deeper opt-in tests (heavier; not in `make test`):
- `tests/scale/aggregate_scale.sh [N]` — the honest "thousands of ranks" test:
  collection is per-rank-independent, so only `uaps report` is N-dependent. It
  synthesizes N rank snapshots (default 20k; validated to 100k → 0.63s, 151MB, ~1.5
  KB/rank) with an analytically-known SUM/MAX/MEAN/imbalance pattern plus the
  at-scale failure modes (truncated/dead ranks, short world size, mixed arch/host),
  and asserts the reduction is exact AND cost stays bounded (<16 KB/rank resident).
- `tests/scale/hybrid.sh` — hybrid MPI×OpenMP. Validates that per-thread counting
  sums ALL OpenMP threads (R×T vs (R·T)×1 same total → same aggregate counts), that
  `gflops` is spin-immune while `/proc`-cputime thread imbalance is masked under
  active-spin — now DETECTED + flagged (`omp_spin_wait`), `max_threads`, the
  sub-interval thread-miss floor, and per-rank MPI time under FUNNELED.
- `tests/scale/cross_mpi.sh` — launcher-agnostic + cross-MPI. [B0] asserts the rank-var
  list (set + precedence) is IDENTICAL across the Rust (`lib.rs`), C (`util.c`), and
  Python (`contract.py`) detectors — the guard against tier-disagreement drift. [B1-3]
  drives each of the 7 rank schemes by env injection. [A] (auto-skips without a local
  MPICH) builds the PMPI shim against MPICH's different ABI (int vs pointer handles,
  Hydra/`PMI_RANK`) and proves interposition + aggregation still work.
- `tests/scale/instr_crosscheck.sh` — validates `hw_instructions` against `perf stat -e
  instructions` (one counter, `inherit=1`: all threads, NO multiplexing/scaling — the
  ground truth). Result: uaps matches perf to **within ~1% across T=1..16 threads** — the
  ×~6.7 multiplexing extrapolation in `pmu.rs` (groups share AMD's 6 PMCs) is unbiased.
  The only residual is a few-% **start-latency** loss on SHORT parallel regions (counter
  opened after the thread starts), shown by the duration sweep (short 0.2s → 0.94, long
  5s → 0.998) and gone by ~1s. NB: an earlier "~15% undercount" was a methodology artifact
  — it compared N×1 vs 1×N MPI layouts (N×1 carries N× the per-rank MPI runtime, inflating
  the reference), not a measurement vs ground truth. Auto-skips without a working `perf`.

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
- **Roofline precision is unknowable from the AMD/ARM FP counter — place against BOTH
  roofs.** AMD `fp_ret_sse_avx_ops` and ARM `fp_*_ops_spec` are element-weighted, so the
  achieved GFLOP/s and AI are exact, but they expose no SP/DP split — and the *compute*
  roof is precision-dependent (FP32 peak ≈ 2× FP64). The collector tags these with a
  `fp_mixed_precision` metric (`raw_pmu.rs`; Intel is exempt — it counts DP umasks, so
  its point is unambiguously FP64). The renderer (`roofline.precision_unknown_summary`,
  used by `viewpoints.roofline_view` + `htmlrep`) then classifies against both roofs:
  the bandwidth (slanted) roof is precision-independent, and the FP64/FP32 ridge points
  sit at AI and 2·AI, so the ambiguity is confined to the band between them — **left of
  the FP64 ridge → memory-bound either way; right of the FP32 ridge → compute-bound either
  way (report %-of-peak as a bracket); between → the bound itself flips with precision**
  (flag it, defer to `upat` sci-lib trace / sampling for the real split). Never silently
  pick one roof.
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
- **OpenMP active-spin masks thread imbalance — detect + flag, don't trust silently.**
  Thread imbalance comes from per-thread CPU time (`/proc/<pid>/task/*/stat`), which can't
  tell real work from a busy-WAIT. Under a non-passive `OMP_WAIT_POLICY` (libgomp's default
  once threads are bound — the HPC norm) idle threads spin at barriers, so every thread
  looks busy and `(max-avg)/max` reads ~0 even when work is badly skewed (validated: 1.3%
  shown vs 40.6% true). The spin IS CPU time, so the metric can't be fixed — instead
  `omp::runtime_loaded` (maps has libgomp/libomp/libiomp5) + `spin_masks_imbalance`
  (`OMP_WAIT_POLICY` ≠ `passive`) push an `omp_spin_wait` flag (gated on `max_threads>1`);
  the report then marks the imbalance + parallel-efficiency as a LOWER BOUND and tells the
  user to re-run with `OMP_WAIT_POLICY=passive`. `gflops`/instruction counts are unaffected
  (spin is integer work). Same `/proc`-during-sampling pattern as GPU detection.
- **Per-process counting misses wrapped/forked work** (no `inherit`): `numactl`/
  `taskset`/shell wrappers measure the idle parent (each rank's app must `exec`, not
  fork). The C profiler suppresses its report write in `fork`-without-`exec` children
  (`pthread_atfork` in `libprof.c`) so they don't clobber the parent's rank file. uaps
  can't suppress (it has no view into the uncounted child), so it **warns** instead:
  `wrapper_warning` (`uaps-cli`) fires when a process did near-zero CPU work (<1%
  utilization, no billions of retired instructions) over a non-trivial wall time — the
  fork-instead-of-exec signature — rank 0 only at scale, and skipped under GPU offload
  (which already explains idle CPU). Genuinely idle/sleep/IO-bound runs trip it too; the
  note says so.
- **Cross-rank imbalance has a noise floor.** The `(max-avg)/max` companion metrics
  (`imbalance_pct` in `aggregate.rs`) are skipped when a metric's max is below a small
  per-metric floor (gflops 0.1, ipc 0.05, memory_bound 1%, times 1ms) — otherwise a
  metric that's ~zero on every rank reports a bogus ~100% "imbalance" from counter dust.
- **MPI aggregates are per-rank means, uniformly.** `mpi_time` and the top-function
  seconds (`mpi_top1..5`, `build_mpi_metrics` in `uaps-collect/mpi.rs`) are both avg/rank
  so a single function's time can't appear to exceed the total; the %-of-MPI share is a
  rank-invariant ratio. `mpi_imbalance_pct` is the `(max-avg)/max` spread.

Regenerating the QE validation reports (`tests/qe/out/`) requires the run host (QE
binary + per-host roofline). The node-level `uaps` snapshots need `cap_perfmon` on the
`uaps` binary (`sudo setcap cap_perfmon+ep <uaps>`) or `perf_event_paranoid<=0`;
rebuilding the binary drops the capability, so re-`setcap` after `cargo build`.

The reports are **strictly single-tier** — there is no combined report; only the
Environment header is shared. The human report goes to **stderr** (the target owns
stdout, like `perf stat`).
