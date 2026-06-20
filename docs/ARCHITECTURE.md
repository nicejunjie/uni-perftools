# Architecture

A polyglot monorepo modeled on VTune's **collect → result → finalize → report**
pipeline: two collectors feed a shared core over an on-disk result.

```
            collect                         result dir            analyze / report
  ┌───────────────────────────┐        ┌──────────────┐      ┌───────────────────────┐
  │ snapshot (Rust, outside)  │──snap──▶│ manifest.json │      │ core/analysis         │
  │   perf counters: roofline │        │ snap.json     │─────▶│  viewpoints (snapshot │
  │   + characterization      │        │ prof.<rank>.. │      │  + profile), unified  │
  ├───────────────────────────┤        └──────────────┘      │  insights, formats    │
  │ profile (C .so, inside)   │──prof──▶                      └───────────────────────┘
  │   interception + sampling │              ▲                         ▲
  └───────────────────────────┘              │ core/contract           │ core/cli (driver)
        snapshot = outer parent,             │ (schema, imbalance,     │ collect/report/run
        profile = injected inside            │  categories)            │
```

## The two collectors — boundary by vantage point + mechanism
| | snapshot (`collectors/snapshot`) | profile (`collectors/profile`) |
|---|---|---|
| Vantage | **outside** the target (parent) | **inside** (LD_PRELOAD / Frida) |
| Mechanism | `perf_event_open` **counting + IP sampling** | function **interception + call-stack sampling** |
| Answers | *how efficiently is the HW used?* → **roofline** + IPC/%peak/top-down/DRAM/NUMA/vec, and coarse time-by-category | *where does time go?* → top functions/lines, sci-lib + MPI tracing (counts, sizes, comm matrix), per-call I/O, heap |
| Granularity | whole-program, run-level | per-call, per-rank |
| Overhead | bounded, always-on | scales with calls; detailed |

They compose in one run; they never call each other — they meet in the **result dir**.

## One owner per facility (no duplication)
- **snapshot:** hardware counters, roofline, FLOPs/%peak/vectorization, RSS, `/proc` I/O volume, thread-imbalance headline.
- **profile:** sci-lib tracing (BLAS/LAPACK/PBLAS/ScaLAPACK/CBLAS/LAPACKe/FFTW), MPI (portable `mpi.h`-free PMPI), per-call I/O detail, heap, sampling/call-stack, symbolization.
- **core:** result/contract format, cross-rank aggregation, the single imbalance metric, the analysis viewpoints, the unified insights engine, output formats, the driver CLI.

Overlaps (I/O, memory, thread imbalance) split snapshot=cheap-aggregate vs
profile=detailed-attribution. MPI is profile's; the snapshot tool runs without
its own MPI shim in-suite (no double interception).

## Shared conventions (`core/contract`)
- **Imbalance, suite-wide:** `(max − avg) / max` (CrayPAT `Imb%`, bounded), with
  the absolute companion `max − avg`. One definition, used by both collectors and core.
- **Rank** from the launcher env (`OMPI_COMM_WORLD_RANK` / `PMI_RANK` / …).
- **Categories** (bird's-eye time): compute / math-libs / MPI / I/O / system.

## Unified insights
`core/analysis/insights.py` reasons over *both* datasets (snapshot counters +
profile attribution) and replaces each tool's own advice in-suite — e.g.
"memory-bound ∧ dgemm dominates → cache-block", "MPI 64% with imbalance →
rebalance / see comm matrix". Time-by-category uses the sampling dominant-group
breakdown (charges `read()`-under-MPI to MPI via the stack).

## Standalone vs suite
Each collector also runs standalone (profile: `collectors/profile` make + its
`upat-report.py`; snapshot: `cargo run`). The suite (`core/cli/perfsuite`)
composes them and renders one report. See `../SUITE_PLAN.md` for the roadmap and
open-gap decisions; `../core/contract/SCHEMA.md` for the on-disk format.
