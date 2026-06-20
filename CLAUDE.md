# CLAUDE.md

Guidance for Claude Code working in this repository.

## What this is

A performance-analysis **suite** (`perfsuite`) with two collectors over a shared
analysis core, branded after the vendor tools they descend from (made portable):

- **`uaps`** — Universal Application Performance *Snapshot* (← Intel APS). Cheap,
  counter-based bird's-eye view. Rust, `collectors/snapshot/`.
- **`upat`** — Universal Performance Analysis *Tool* (← CrayPAT). Deep profiler:
  sci-lib tracing (BLAS/LAPACK/FFTW/MPI), statistical + event-based sampling,
  call stacks, per-function roofline, I/O, heap. C, `collectors/profile/`.
- **`core/`** — shared contract, symbolization, roofline ceilings, analysis
  viewpoints, and the `perfsuite` driver CLI.

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

## Build / test

```sh
make            # build both collectors (upat .so + cargo uaps)
make test       # profile (upat) tests + cargo tests + suite end-to-end tests
```

Per-component details are in `collectors/profile/` and `collectors/snapshot/`
(the latter has its own CLAUDE.md).
