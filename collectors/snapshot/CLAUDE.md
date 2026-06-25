# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Goal

Build **`uaps`** (Universal Application Performance Snapshot) — a cross-platform profiler that **matches Intel Application Performance Snapshot (APS) feature-for-feature**, but is not tied to Intel hardware/software. APS gives a low-overhead, one-screen "snapshot" of where an application's time goes and which deeper analysis to run next.

**Feature target = exact APS parity.** The derivation engine must ultimately produce the full APS metric set:
- Elapsed time, physical core utilization
- SP/DP GFLOPS, CPI (cycles per instruction retired)
- MPI time, MPI imbalance, top MPI functions
- OpenMP imbalance, serial time, parallel-region time
- Memory stalls (% of pipeline slots): cache-bound, DRAM-bound, NUMA remote-access %
- Vectorization % (packed FP ops), instruction mix
- Memory footprint (resident/virtual, per rank/node)
- I/O bound: read/write volume, I/O wait time

## Locked Decisions

- **Language:** Rust.
- **First platform:** Linux (open PMU access via `perf_event_open` + eBPF). macOS/Windows are later phases behind the same trait.
- **Collection approach:** Hybrid — wrap/abstract OS facilities (`/proc`, perf, eBPF) behind one `Collector` trait now; add native/other-platform backends later without changing the core.
- **Scope ordering:** general single-node metrics first, MPI/OpenMP (HPC) after the core works — but all are committed phases, since the goal is full APS parity.

## Intended Architecture (Cargo workspace, trait-based)

```
uaps-cli      launcher, arg parsing, runs target to completion, prints report;
                per-rank reinjection + cross-rank aggregation (aggregate.rs) and the
                TCP per-rank rendezvous (net.rs) for MPI launches
uaps-core     Collector trait, normalized Metric model, metric-derivation engine
uaps-collect  backend impls behind the Collector trait:
                proc  → /proc sampling   (no privileges, works on any Linux)
                perf  → perf_event_open  (PMU counters: cycles, cache, FP)
                ebpf  → syscall/off-CPU/IO (later)
uaps-report   terminal snapshot + JSON/HTML export
```

Key design principle: a **metric-derivation layer** maps raw, vendor-specific PMU events (Intel/AMD/ARM differ significantly) onto the stable APS-style derived metric set. Backends emit raw events; derivation owns the per-vendor event→metric mapping. New platform = new backend, same derivation contract.

## Roadmap status

Phases 0–6 are implemented (Linux). Remaining work is depth, not breadth: true
top-down pipeline-slot stalls, vectorization %/GFLOPS (need vendor-specific raw
PMU events), real macOS/Windows backends, and `attach` for running processes.

- **Phase 0 — Scaffold ✅:** Cargo workspace, four crates, `Collector` trait + `Metric` model, CLI launch + elapsed time.
- **Phase 1 — `/proc` collector ✅:** CPU time/utilization, peak RSS, thread peak, disk + logical I/O, sampled on a timer loop.
- **Phase 2 — `perf_event_open` ✅:** 6 generic PMU events over the process tree (`perf-event` crate), multiplexing-scaled, degrades gracefully.
- **Phase 3 — Derivation ✅:** CPI/IPC, cache-miss rate, LLC MPKI, branch-mispredict rate, estimated memory-bound %.
- **Phase 3b — Vendor raw PMU ✅ (data-driven):** `raw_pmu.rs` holds a per-vendor
  policy of `(role, event-name)` pairs and resolves each **by name** from the
  vendored pmu-events db (`pmudb::resolve_config_in` → `(config, perf_type)`) — no
  hard-coded config bytes, one uniform path for AMD/Intel/ARM, carrying the correct
  PMU type. A `raw_pmu_policy_resolves` test asserts the AMD names reproduce the
  historically-validated encodings (FP `0x0F03`, fills `0xFF43/0x4843/0x5043`).
  Validated on Ryzen 9 9950X / Zen 5 and ARM Neoverse V2 / Grace:
    - **FP throughput / GFLOPS:** AMD `FpRetSseAvxOps` (PMCx003, umask 0x0F); Intel `FP_ARITH_INST_RETIRED` (0xC7) per-width umasks also give **vectorization %** (Intel encodings per SDM, unvalidated on this host).
    - **Memory data source:** AMD `ls_dmnd_fills_from_sys` (0x43) — demand fills by source (local DRAM 0x08, remote DRAM 0x40, remote-node cache 0x10, all 0xFF). Derived into **DRAM fills/1K-instr (DPKI)**, **DRAM-bound % of demand fills**, and **NUMA remote-access %** — all *measured*, replacing the LLC-miss estimate when present. AMD has no Intel-style "% pipeline-slots stalled on memory" counter, so `memory_bound` is modeled as measured-DRAM-fills × DRAM-latency (≈ stall cycles); `memory_bound_est` (LLC-miss penalty model) is the fallback when fill events are absent.
    - **Top-down (pipeline slots), L1:** `topdown.rs` computes retiring / frontend-bound / backend-bound / bad-speculation for AMD Zen 5 using the kernel `amdzen` formulas (slots = 8 × `ls_not_halted_cyc`; events `de_no_dispatch_per_slot` 0x1A0, `de_src_op_disp.all` 0xAA, `ex_ret_ops` 0xC1). The five events run in **one `perf_event` group** opened directly via `perf-event-open-sys` (the `perf-event` crate's `Group` leader is hardwired to pid 0), so the ratios are exact even under PMU multiplexing. Not shelling out to `perf` — that binary isn't installed and AMD top-down is just formulas over these events.
    - **Multi-threaded top-down:** grouped reads can't use `inherit`, so the collector opens a separate group **per thread** (discovered via `/proc/<pid>/task` during the sampling loop) and sums the raw counts across threads before computing ratios. Validated on Zen 5: single- and multi-threaded (pthreads + OpenMP) workloads all sum to ~100% (memory→backend-heavy, SIMD→higher retiring). Threads that start and finish entirely within one sample interval are missed (minor for workloads longer than the interval).
    - **L2 backend split:** of backend-bound slots, the memory-bound vs core-bound share, from `ex_no_retire.load_not_complete`/`.not_complete` (0xD6). Opened as a **second 2-event group** per thread (all 7 events won't fit AMD's 6 PMCs in one group); combined as `backend × (load_not_complete/not_complete)`. Validated on Zen 5: pointer-chase → ~88% memory, dependent-FP chain → ~72% core, AVX FMA → mixed.
    - **ARM (Neoverse V2 / Grace) ✅:** `raw_pmu.rs` `Vendor::Arm` resolves events by
      *name* from the pmu-events db (not hard-coded raw codes) via
      `pmudb::resolve_config` → `(config, perf_type)`: GFLOPS from
      `FP_SCALE_OPS_SPEC` + `FP_FIXED_OPS_SPEC` (element-ops, **speculative** — a
      proxy like AMD's), DRAM fills from `LL_CACHE_MISS_RD`. The ARM core PMU is
      **not** `PERF_TYPE_RAW` (it's a dynamic type, e.g. 10), so the resolved type
      must be carried through — see `core_pmu()` below. Validated live on TACC
      Vista Grace: FP throughput, DRAM bandwidth, memory-bound, and the
      whole-program roofline point now populate. Top-down is a gap on
      `arm/neoverse-n2-v2` (that model's pmu-events has no TMA metrics).
    - Still TODO: Intel memory-source breakdown (needs offcore/PEBS), AMD vectorization split, ARM vectorization (NEON-vs-SVE) split.
- **Phase 3c — Data-driven HWPC engine (`pmudb.rs`) ✅:** top-down is no longer
  hand-coded per family. The engine derives everything from perf's vendored
  `pmu-events` source data (`collectors/snapshot/pmu-events/`, committed): event
  codes + metric formulas from the JSON, the config bit-layout from the kernel's
  `/sys/devices/<pmu>/format/*`, CPU→model from `mapfile.csv`. It detects the
  model, resolves the canonical metrics (AMD `*_bound` / Intel `tma_*` / ARM),
  discovers the events each formula references, encodes + counts them
  (metric-aware grouping keeps a metric's events co-scheduled in one ≤5-event
  group), and evaluates the formula. **Nothing is guessed** — a missing event,
  unsupported construct, or absent sysfs field is reported as a gap, never
  fabricated. `HwpcCollector` supersedes `topdown.rs` when it resolves for the
  CPU (else the hand-coded path is the fallback). The formula evaluator handles
  d_ratio/min/max/if, arithmetic, recursive metric refs, both ternary spellings
  (`?:` and `a if c else b`), comparisons, `#smt_on`/`#num_cpus`/`#core_wide`,
  escaped event names (`topdown\-retiring`), kernel event aliases (Intel `slots`
  + `topdown-*` PERF_METRICS), and ARM **ArchStdEvent** references. Validated:
  Zen5 live top-down matches the hand-coded path; `make validate-hwpc` structurally
  proves all canonical metrics resolve (or are explicit gaps) for every vendored
  model — AMD Zen4-6, Intel Haswell→Granite Rapids, NVIDIA Grace — without that
  hardware (CI: `.github/workflows/hwpc-validate.yml`).

  **Counting model (`pmu.rs`):** all hardware counters — generic (`perf.rs`), vendor FP/fill (`raw_pmu.rs`), and top-down (`topdown.rs`) — are opened as **per-thread `perf_event` groups** (no `inherit`), discovered via `/proc/<pid>/task` during the sampling loop and summed across threads. This replaced an earlier `inherit`+individual-counter design whose `time_running` accounting was unreliable under multiplexing and **intermittently undercounted by ~8×** on short multithreaded runs. Groups are kept ≤5 events (a group must fit the core PMUs at once; one PMC may be held by the NMI watchdog) and ratio pairs (instructions+cycles, refs+misses, branches) are co-located so IPC/CPI/miss-rate are exact. Cross-group quantities (GFLOPS, DPKI) use `time_enabled/time_running` scaling. Verified: repeated numpy-matmul runs now give stable GFLOPS (~690) vs. the old 87–690 swing.
- **Phase 4 — Report ✅:** grouped APS-style sections, bottleneck headline + insights, `--format text|json|html`, `--output`. The human report goes to **stderr** (the target owns stdout, like `perf stat`) so it never mingles with the profiled program's output; `uaps run -- app > app.out 2> snap.txt` separates them, and `-o file` writes it directly (use this for JSON).
- **Phase 5 — HPC ✅:** MPI via PMPI `LD_PRELOAD` shim (`shim/mpi/uaps_mpi.c`, built by `build.rs`) → per-rank MPI time/imbalance/top call; thread imbalance via `/proc/<pid>/task` (works for OpenMP *and* pthreads — chosen over OMPT for portability). **mpi4py works too** (validated): the shim intercepts mpi4py's `MPI_*` because they're C-ABI calls into `libmpi` resolved through the global symbol scope — the Python module being runtime-imported does not block interposition. The hard requirement is **ABI matching**: mpi4py must be built against the same MPI `mpirun` launches (e.g. `MPICC=$(command -v mpicc) venv/bin/pip install mpi4py` against system OpenMPI). See `testbench/mpi_ring.py`. (LD_PRELOAD interposition does *not* work for pure-Python-level symbols, but uaps never relies on that — Python/general profiling is done purely via PMU counters + `/proc`, which are language-agnostic.)
- **Phase 6 — Portability ✅ (scaffold):** Linux backends are `cfg(target_os = "linux")`; other platforms get stub collectors (`src/fallback.rs`). Real kperf/ETW backends are future work.
- **Cross-arch (AMD / Intel / ARM) ✅:** validated live on AMD Zen 3/4/5 and ARM
  Neoverse V2 (NVIDIA Grace). The enabling fix was `core_pmu()` in `pmudb.rs`: the
  HWPC encoder hard-coded the core PMU sysfs device as `"cpu"` (x86-only); on ARM
  it is `armv8_pmuv3_0`, so **every** event encoding silently returned a gap until
  the device — and the perf `type` — were detected from sysfs instead of assumed.
  This unblocked both the data-driven top-down and the ARM `raw_pmu` counters.

#### What we take from perf, and what is ours

- **PMC info — perf's data.** Event codes, umasks, the config bit-layout, metric
  formulas, and CPU-model→events mapping all come from the vendored **pmu-events**
  DB (the same data the kernel `perf` tool ships).
- **Reading PMCs — perf's mechanism.** Counters are programmed/read via
  **`perf_event_open`**; configs are encoded from `/sys/devices/<pmu>/format/*`,
  the PMU `type` from sysfs, plus kernel event aliases (Intel `slots`/PERF_METRICS).
- **Ours (by design):** the *access logic* around that data — PMU discovery, event
  grouping, encoding — is reimplemented here (not perf's C lib), which is exactly
  where x86 assumptions crept in; it is now sysfs-/data-driven. And the
  **counts→metric derivation** is partly hand-coded: top-down uses perf's metric
  *formulas* (`pmudb` evaluator), but `raw_pmu`'s GFLOPS/vectorization/DRAM math is
  our own arithmetic over perf-resolved event *encodings*.

### Key behaviors & limitations to know
- **In the perf-suite:** this tool is the **snapshot** collector (roofline + microarch);
  the deep **profile** collector (`upat`) owns sci-lib/MPI *tracing*. They run as
  separate invocations, so there is no double MPI interception. The unified imbalance
  metric is `(max-avg)/max` (matches the profile collector and `core/contract`).
- **MPI runs are PER-RANK (APS-style), the default for a launcher:** `uaps run -- mpirun`
  reinjects `uaps` into each rank (`run_per_rank`/`collect_rank` in `uaps-cli`), so the
  `/proc`+perf collectors count **each rank's own process on its own node** — not the
  idle launcher — and the parent aggregates across ranks (SUM counts/throughput, MAX
  wall, MEAN %, ratios re-derived from summed raws) plus per-rank HW imbalance. Per-rank
  snapshots come back over a **TCP rendezvous** (`net.rs`), so it needs no shared FS and
  works from any cwd; the PMPI shim still supplies per-rank `mpi_*` timing. `-a` forces
  the old node-level (launcher-node, system-wide) path. See the top-level CLAUDE.md for
  the full per-rank invariants.
- **AMD host:** primary development is on an AMD CPU, but the engine is validated
  cross-arch (AMD Zen 3–5, ARM Neoverse V2). Generic events (`perf.rs`) are
  portable; vendor *raw* events (`raw_pmu.rs`) and top-down are per-vendor.
- **Portability lessons (ARM bring-up) — read before adding a counter:**
  - *Data-driven data isn't enough if the access path is hardcoded.* The
    pmu-events DB is vendor-neutral, but the code reading it assumed x86: PMU
    device name `"cpu"` and `PERF_TYPE_RAW`. Always resolve the PMU device + type
    from sysfs (`core_pmu()` / `pmudb::resolve_config`), never assume.
  - *Resolve raw events by name, don't hard-code codes.* `raw_pmu.rs` AMD/Intel
    paths hard-code config bytes; the ARM path resolves names from pmu-events,
    which is the pattern to prefer (works across Zen gens too, no magic numbers).
  - *Silent gaps hide whole missing subsystems.* "Never fabricate, report a gap"
    is correct, but a report that quietly empties out still *looks* fine — an
    empty ARM snapshot read as "working" until scrutinized section-by-section. When
    a whole vendor is unsupported, make it loud.
  - *Compare against stock `perf` to assign blame.* When a target misbehaves under
    instrumentation, run stock `perf record`/`perf stat` on the same binary first:
    if it's fine, the bug is ours (this is how the sampler-unwind crash was pinned
    — see `collectors/profile/README.md`).
  - *Don't trust host-specific build artifacts across machines.* Roofline cache,
    the x86 Frida archive, and `build/` carried between hosts all caused wrong
    results; exclude them from deploys and recalibrate on the run host.
- **Counters include kernel time** by default, which inflates cache-miss counts for I/O-heavy code; the insight engine ranks I/O-bound before memory-bound to compensate.
- **`testbench/`** holds six validation workloads (compute/memory/io/threaded/openmp/mpi); build locally with `make -C testbench`. Per project policy, never install toolchains system-wide — build test artifacts locally.

## Cross-cutting Requirements

- **Privilege degradation:** every PMU feature must fall back cleanly to `/proc` and report what it could not measure.
- **Low overhead:** prefer counting mode over sampling; configurable interval. The value of a snapshot is being cheap enough to always run.
- **Testing:** parser unit tests + integration tests that run known microbenchmarks (a memory-bound loop, a compute-bound loop) and assert the classifier labels them correctly.

## Reference

Intel APS is part of Intel VTune Profiler / oneAPI. Match its report layout and metric definitions when in doubt about exact semantics.

## Build / Test Commands

```sh
cargo build                       # build the workspace (build.rs compiles the MPI shim if mpicc exists)
cargo test                        # all unit tests across crates
cargo test -p uaps-collect        # one crate
cargo test format_bytes           # a single test by name
cargo run -- run -- sleep 1       # profile a command

make -C testbench                 # build local validation workloads into testbench/bin
./target/debug/uaps run -- ./testbench/bin/memory_bound 512
./target/debug/uaps run --mpi -- mpirun --oversubscribe -n 4 ./testbench/bin/mpi_ring 8000
./target/debug/uaps run --format json -o report.json -- ./testbench/bin/threaded 8 200000000
```

`UAPS_MPI_SHIM=/path/to/uaps_mpi.so` overrides the shim location if `mpicc`
wasn't present at build time. `perf_event_paranoid` must allow measuring own
children (≤2) for hardware counters; otherwise the report degrades to OS metrics.
