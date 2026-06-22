# CLAUDE.md

Guidance for Claude Code working in this repository.

## What this is

**Universal Performance Tool Suite** (uni-perftools) — two independent **cost-tier**
commands over a shared analysis core, branded after the vendor tools they descend
from (made portable). There is **no umbrella/driver command** — run one tier:

- **`uaps`** — Universal Application Performance *Snapshot* (← Intel APS). Cheap,
  **non-invasive** counter-based bird's-eye view (HWPC + MPI/OpenMP, aggregated
  across ranks). Rust, `collectors/snapshot/`.
- **`upat`** — Universal Performance Analysis *Tool* (← CrayPAT). Deep profiler:
  sci-lib tracing (BLAS/LAPACK/FFTW + Fortran/C MPI/IO), statistical + event-based
  sampling, per-function roofline, heap. C `.so` + `core/cli/upat`.
- **`core/`** — shared contract, symbolization, roofline ceilings, analysis
  viewpoints, insights engine, HTML reports, and the `upat` CLI.

`uaps` and `upat` are cost tiers (cheap-noninvasive vs deep-invasive); run one,
not both. `upat report` opportunistically folds in a `snap.json` if a `uaps` run
left one in the result dir.

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
make            # build both collectors (upat .so + cargo uaps)
make test       # profile (upat) tests + cargo tests + suite end-to-end tests
```

Per-component details are in `collectors/profile/` and `collectors/snapshot/`
(the latter has its own CLAUDE.md).
