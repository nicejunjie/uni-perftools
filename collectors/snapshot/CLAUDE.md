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
uaps-cli      launcher / attach, arg parsing, runs target to completion, prints report
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
- **Phase 3b — Vendor raw PMU ✅:** `raw_pmu.rs` programs `PERF_TYPE_RAW` events selected by CPU (`cpu.rs`), set via `Builder::attrs_mut()` (the `perf-event` crate has no Raw variant). Validated on Ryzen 9 9950X / Zen 5:
    - **FP throughput / GFLOPS:** AMD `FpRetSseAvxOps` (PMCx003, umask 0x0F); Intel `FP_ARITH_INST_RETIRED` (0xC7) per-width umasks also give **vectorization %** (Intel encodings per SDM, unvalidated on this host).
    - **Memory data source:** AMD `ls_dmnd_fills_from_sys` (0x43) — demand fills by source (local DRAM 0x08, remote DRAM 0x40, remote-node cache 0x10, all 0xFF). Derived into **DRAM fills/1K-instr (DPKI)**, **DRAM-bound % of demand fills**, and **NUMA remote-access %** — all *measured*, replacing the LLC-miss estimate when present. AMD has no Intel-style "% pipeline-slots stalled on memory" counter, so `memory_bound` is modeled as measured-DRAM-fills × DRAM-latency (≈ stall cycles); `memory_bound_est` (LLC-miss penalty model) is the fallback when fill events are absent.
    - **Top-down (pipeline slots), L1:** `topdown.rs` computes retiring / frontend-bound / backend-bound / bad-speculation for AMD Zen 5 using the kernel `amdzen` formulas (slots = 8 × `ls_not_halted_cyc`; events `de_no_dispatch_per_slot` 0x1A0, `de_src_op_disp.all` 0xAA, `ex_ret_ops` 0xC1). The five events run in **one `perf_event` group** opened directly via `perf-event-open-sys` (the `perf-event` crate's `Group` leader is hardwired to pid 0), so the ratios are exact even under PMU multiplexing. Not shelling out to `perf` — that binary isn't installed and AMD top-down is just formulas over these events.
    - **Multi-threaded top-down:** grouped reads can't use `inherit`, so the collector opens a separate group **per thread** (discovered via `/proc/<pid>/task` during the sampling loop) and sums the raw counts across threads before computing ratios. Validated on Zen 5: single- and multi-threaded (pthreads + OpenMP) workloads all sum to ~100% (memory→backend-heavy, SIMD→higher retiring). Threads that start and finish entirely within one sample interval are missed (minor for workloads longer than the interval).
    - **L2 backend split:** of backend-bound slots, the memory-bound vs core-bound share, from `ex_no_retire.load_not_complete`/`.not_complete` (0xD6). Opened as a **second 2-event group** per thread (all 7 events won't fit AMD's 6 PMCs in one group); combined as `backend × (load_not_complete/not_complete)`. Validated on Zen 5: pointer-chase → ~88% memory, dependent-FP chain → ~72% core, AVX FMA → mixed.
    - Still TODO: Intel memory-source breakdown (needs offcore/PEBS), AMD vectorization split, top-down on non-Zen5 families (needs per-family-verified umasks).

  **Counting model (`pmu.rs`):** all hardware counters — generic (`perf.rs`), vendor FP/fill (`raw_pmu.rs`), and top-down (`topdown.rs`) — are opened as **per-thread `perf_event` groups** (no `inherit`), discovered via `/proc/<pid>/task` during the sampling loop and summed across threads. This replaced an earlier `inherit`+individual-counter design whose `time_running` accounting was unreliable under multiplexing and **intermittently undercounted by ~8×** on short multithreaded runs. Groups are kept ≤5 events (a group must fit the core PMUs at once; one PMC may be held by the NMI watchdog) and ratio pairs (instructions+cycles, refs+misses, branches) are co-located so IPC/CPI/miss-rate are exact. Cross-group quantities (GFLOPS, DPKI) use `time_enabled/time_running` scaling. Verified: repeated numpy-matmul runs now give stable GFLOPS (~690) vs. the old 87–690 swing.
- **Phase 4 — Report ✅:** grouped APS-style sections, bottleneck headline + insights, `--format text|json|html`, `--output`.
- **Phase 5 — HPC ✅:** MPI via PMPI `LD_PRELOAD` shim (`shim/mpi/uaps_mpi.c`, built by `build.rs`) → per-rank MPI time/imbalance/top call; thread imbalance via `/proc/<pid>/task` (works for OpenMP *and* pthreads — chosen over OMPT for portability). **mpi4py works too** (validated): the shim intercepts mpi4py's `MPI_*` because they're C-ABI calls into `libmpi` resolved through the global symbol scope — the Python module being runtime-imported does not block interposition. The hard requirement is **ABI matching**: mpi4py must be built against the same MPI `mpirun` launches (e.g. `MPICC=$(command -v mpicc) venv/bin/pip install mpi4py` against system OpenMPI). See `testbench/mpi_ring.py`. (LD_PRELOAD interposition does *not* work for pure-Python-level symbols, but uaps never relies on that — Python/general profiling is done purely via PMU counters + `/proc`, which are language-agnostic.)
- **Phase 6 — Portability ✅ (scaffold):** Linux backends are `cfg(target_os = "linux")`; other platforms get stub collectors (`src/fallback.rs`). Real kperf/ETW backends are future work.

### Key behaviors & limitations to know
- **In the perf-suite:** this tool is the **snapshot** collector (roofline + microarch). MPI is owned by the **profile** collector (portable `mpi.h`-free PMPI), so the suite runs `uaps` **without `--mpi`** and `uaps_mpi.c` is standalone-only — there is no double MPI interception. The unified imbalance metric is `(max-avg)/max` (matches the profile collector and `core/contract`).
- **MPI runs (standalone `--mpi`):** the `/proc` and perf collectors profile the *launcher* process tree, not individual ranks; for MPI the authoritative metrics come from the shim (`mpi_*`). The "Mostly single-threaded" insight is intentionally suppressed under `--mpi`.
- **AMD host:** development/validation is on an AMD CPU, so the code deliberately uses *generic* perf events (portable) rather than Intel raw event codes.
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
