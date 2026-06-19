# uaps — Universal Application Performance Snapshot

A cross-platform, low-overhead application profiler aiming for feature parity
with Intel Application Performance Snapshot (APS), without being tied to Intel
hardware or software.

`uaps` gives a one-screen "snapshot" of where an application spends its time —
CPU, memory stalls, vectorization, I/O, and (for HPC workloads) MPI and OpenMP
imbalance — to point you at which deeper analysis to run next.

> **Status:** early development (Phase 0). Linux-first, written in Rust.
> See [CLAUDE.md](CLAUDE.md) for the full roadmap and architecture.

## Build

```sh
cargo build
```

## Usage

```sh
# Launch a command and profile it to completion:
cargo run -- run -- sleep 1

# (installed binary)
uaps run -- ./my_app --flag
```

## Test

```sh
cargo test                 # all tests
cargo test format_bytes    # a single test by name
```

## Workspace layout

| crate          | responsibility                                          |
| -------------- | ------------------------------------------------------- |
| `uaps-cli`     | `uaps` binary: launch/attach, orchestrate, print report |
| `uaps-core`    | `Collector` trait, normalized `Metric`/`Snapshot` model |
| `uaps-collect` | collection backends (`/proc`, perf, eBPF, …)            |
| `uaps-report`  | terminal snapshot + (later) JSON/HTML export            |
