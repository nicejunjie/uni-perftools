# Performance Suite (snapshot + profile)

A polyglot HPC performance suite combining two collectors and a shared analysis
core, modeled on VTune's *collect → result → finalize → report* pipeline:

- **snapshot** (`collectors/snapshot`, Rust) — APS-like bird's-eye view from
  hardware counters: **roofline** + microarchitecture characterization (IPC,
  %peak, top-down, DRAM/NUMA, vectorization) and coarse time-by-category. Answers
  *"what kind of bottleneck, how efficient?"*
- **profile** (`collectors/profile`, C `.so`) — CrayPAT-like detail via call
  interception + call-stack sampling: top functions/lines, scientific-library
  tracing (BLAS/LAPACK/PBLAS/ScaLAPACK/CBLAS/LAPACKe/FFTW), MPI (portable PMPI),
  per-call I/O, heap. Answers *"where exactly does the time go?"*
- **core** (`core/`) — the shared spine: result/contract format, symbolization,
  cross-rank aggregation + the one imbalance metric, the analysis **viewpoints**,
  the unified insights engine, and the driver CLI.

## Layout
```
collectors/profile/    C profiler  → libprofile (LD_PRELOAD / Frida)
collectors/snapshot/   Rust counters → snapshot binary
core/{contract,symbolize,analysis,cli}   shared spine
testbench/  tests/  docs/
```

## Build
```
make            # builds both collectors
make test       # profile tests + snapshot tests
```
Each collector also builds standalone (`make -C collectors/profile`,
`cd collectors/snapshot && cargo build`).

> Status: mid-restructure into the monorepo. See `SUITE_PLAN.md` for the roadmap.
