# Performance Suite — monorepo (profile + snapshot) with a shared core

## Context

Merge the two complementary HPC profilers into **one polyglot monorepo** (separate
binaries, built/versioned/tested together), organized like VTune: two **collectors**
feeding a shared **core** (result format → finalization → analysis/report), driven by
one CLI.

- **profile collector** (C `.so`, today's upat) — CrayPAT-like: sampling/
  call-stack, top functions/lines, sci-lib tracing (BLAS/LAPACK/PBLAS/ScaLAPACK/
  CBLAS/LAPACKe/FFTW), MPI (portable `mpi.h`-free PMPI), per-call I/O, heap.
- **snapshot collector** (Rust, today's uaps) — APS-like: HWPC (IPC, %peak, top-down,
  DRAM/NUMA, vectorization), peak RSS, `/proc` I/O, thread imbalance.

The collectors must stay in different languages/mechanisms (C `.so` injected into the
target vs Rust `perf` counting from outside); they compose at runtime as
**snapshot=outer parent, profile=inner injected** and share data via an on-disk result.

## VTune lessons applied
- **Collect → Result → Finalize → Report** as distinct stages (collectors only emit
  raw; symbolization is its own stage; reporting renders).
- **Analysis types are viewpoints over one result** (hotspots / microarch / MPI /
  memory / I/O / threading) — recipes, not separate programs.
- **One driver CLI** with `collect` and `report` verbs over a result directory.
- **Shared metric/imbalance definitions** live once in the core, used by all viewpoints.

## Directory structure (monorepo)

```
<repo>/                     # suite (name TBD; root replaces today's upat)
  collectors/               # the two tools
    profile/                #   C   → libprofile.so   (today's src/, gen/)
    snapshot/               #   Rust → snapshot bin    (today's uaps crates, moved in)
  core/                     # shared spine (was "shared"; renamed to avoid /usr/share clash)
    contract/               #   result-dir format + JSON schema + rank conventions   (VTune result)
    symbolize/              #   finalization: PC → function/source via addr2line       (VTune finalize)
    analysis/               #   viewpoints (hotspots/microarch/mpi/io/memory/threading),
                            #   cross-rank aggregation, unified imbalance, insights engine, formats
    cli/                    #   driver: `collect` + `report` verbs over a result dir
  testbench/                # validation workloads (from uaps)
  tests/  docs/             # suite + per-component tests; docs incl. the contract spec
  Makefile                  # top orchestrator: make profile (.so), cargo snapshot, install core
  build/  third_party/      # artifacts + frida devkit (gitignored)
```

## Locked decisions
- One monorepo; separate binaries; sharing via the on-disk **result/contract**, not FFI.
- Each facility has **one owner** (snapshot=cheap vs drill-down=detailed for overlaps):
  - **snapshot:** HWPC, FLOPs/%peak/vectorization, peak RSS, `/proc` I/O volume, thread-imbalance headline.
  - **profile:** sci-lib tracing, MPI (PMPI), per-call I/O detail, heap high-water, sampling/call-stack.
  - **core:** result format, symbolization, aggregation, imbalance, viewpoints, insights, formats, CLI.
- **Imbalance, suite-wide = `(max−avg)/max`** + absolute companion `max−avg`.
- MPI owned by the profile collector; snapshot does **not** run its old MPI shim in-suite.

## Phases

### Phase 0 — Restructure into the monorepo
Create `collectors/profile/` from today's upat (`src/`, `gen/`, `tests`), move
uaps into `collectors/snapshot/` (copy tree; history stays in the old repo), and stand
up `core/` + top `Makefile`. Each component still builds. (Pure move + build wiring.)

### Phase 1 — Contract (core/contract)
Pin `schema_version=1`, result-dir layout (`manifest.json` + `snap.json` +
`prof.<rank>.json`), field names, rank-from-env convention, and the single
**aggregation + imbalance** conventions both collectors obey.

### Phase 2 — Core analysis as viewpoints (core/analysis + core/symbolize + core/cli)
Move upat's report logic into `core/analysis` as an importable module; add the
snapshot viewpoint (from `snap.json`); organize as selectable **viewpoints** over the
result. `core/symbolize` owns PC→source. `core/cli` exposes `collect` (orchestrate
snapshot-outer + profile-inner → result) and `report` (render viewpoints, `--format
text|json`). Collectors keep only a thin standalone view.

### Phase 3 — Overlap ownership + cross-link
Snapshot viewpoint surfaces peak RSS, `/proc` I/O, thread-imbalance; profile viewpoint
keeps per-call I/O + heap as drill-down; label overlaps (RSS vs heap; logical vs
syscall I/O). **Cross-link** bottleneck→viewpoint (memory-bound→hotspots; MPI-bound→
MPI/comm-matrix; I/O-bound→I/O).

### Phase 4 — Unified insights engine (core/analysis) — headline
One engine over both datasets replaces uaps `insights[]` + profile "Observations":
e.g. *memory-bound ∧ `dgemm` dominates → cache-block*; *MPI% high ∧ imbalance ∧ matrix
hotspot → rebalance/placement*. Collectors' own advice suppressed in-suite.

### Phase 5 — Imbalance + aggregation unification
Apply `(max−avg)/max` + absolute in `core/analysis` (one place); align the profile
collector's standalone module and the snapshot collector's standalone view so a metric
reads identically standalone vs in-suite. Update profile tests.

### Phase 6 — MPI consolidation (snapshot collector)
Gate/mark the old `uaps_mpi.c` standalone-only; document in-suite MPI comes from the
profile collector (no double interception). Update its docs.

### Phase 7 — Packaging, tests, docs
Suite name/branding; `make install` (libprofile.so, snapshot bin, `core/cli` driver);
CI building all three; end-to-end tests in `tests/` (serial + MPI: correct result
files — N `prof.*` + 1 `snap`, no stray file; unified imbalance; cross-link + an
insight). One top README + the contract spec in `docs/`.

## Critical files / moves
- `collectors/profile/` ← current `src/`, `gen/`, `tools/upat-report.py` (report
  logic migrates to `core/analysis`), `tests/run.sh`, `Makefile`.
- `collectors/snapshot/` ← `../aps-profiler-universal/{crates,Cargo.*,build.rs,testbench}`.
- `core/contract/` (schema), `core/symbolize/` (addr2line), `core/analysis/` (viewpoints
  + insights + imbalance), `core/cli/` (the `perfsuite`-style driver, evolved).
- snapshot collector: `crates/uaps-core/derive.rs` imbalance alignment; `uaps_mpi` gating.

## Verification
- `<driver> collect -- ./test_prof && <driver> report <result>` → snapshot + profile
  viewpoints + an insight; `--format json` validates.
- `<driver> collect -- mpirun -n4 ./test_mpi3` → exactly 4 `prof.*` + 1 `snap` (no
  stray uaps/mpirun prof), MPI table + comm matrix, imbalance via `(max−avg)/max`.
- A metric (e.g. dgemm imbalance) identical standalone vs in-suite (one analyzer).
- Each collector still builds/tests standalone (profile `make && tests`; snapshot `cargo test`).

## Open-gap decisions (resolved)

1. **HWPC is run-level, not per-rank.** The snapshot collector counts the whole
   process tree from outside, so `snap.json` is **one run-level result** (no rank
   key). The combined report shows snapshot as a single run-level section and
   profile as per-rank. The contract reflects this: `snap.json` (run-level) +
   `prof.<rank>.json` (per-rank); "join" = compose the two sections, not key both
   by rank. Per-rank counters = explicit future (would need per-rank perf attach).
2. **Interference / two-pass.** Default **one pass** (convenient); document that
   counters then include the profiler's own overhead (sampling + wrappers, small).
   Add **`collect --two-pass`**: pass 1 = snapshot only (clean counters, no
   injection), pass 2 = profile only (injection+sampling, no perf), merged into one
   result. Use when counter accuracy matters.
3. **Finalization / portability.** `core/symbolize` runs as a **finalize step at
   collect-end by default**: resolve PC→func/line then, storing results in the
   result dir so it is self-contained and re-reportable on another machine. Record
   binary paths + build-ids; `report` re-symbolizes only if not finalized. Skippable
   with `collect --no-finalize`.
4. **CLI & result-dir lifecycle** (`core/cli`, driver name TBD, default `perfsuite`):
   - `collect [-o RESULT] [--two-pass] [--no-finalize] -- <cmd>` → runs collectors →
     result dir (default `./perf.<timestamp>/`; `-o` overrides).
   - `report [RESULT] [--view hotspots|microarch|mpi|io|memory|all] [--format text|json]`
     → render viewpoints; default RESULT = most recent.
   - `run -- <cmd>` = collect + finalize + report (the one-liner).
   - Driver discovers in-tree binaries via build/install paths + env overrides
     (`PROFILE_LIB`, `SNAPSHOT_BIN`).
5. **Build/test orchestration.** Top `Makefile`: `all` (profile `.so` via its make +
   `cargo build` for snapshot), `install`, `test` (profile `tests/run.sh` + `cargo
   test` + suite end-to-end), `clean`. Fix paths after the move (gen/, frida devkit →
   `collectors/profile/third_party/` or top `third_party/`). Rust stays a cargo
   workspace under `collectors/snapshot/`.
6. **Standalone story.** profile keeps its current built-in text report (today's
   `upat-report.py`) so it works standalone without the Python core; `core/analysis`
   *reuses/extends* that module and adds the snapshot view + insights. snapshot keeps
   its Rust report standalone. Suite report = `core/analysis` over the result.
7. **Naming / license / history.** Driver `perfsuite`; collector artifacts renamed in
   the packaging phase (`libprofile-{preload,frida}.so`, `snapshot`); keep internal
   names during the move to limit churn. License the suite **MIT OR Apache-2.0**
   (matches snapshot). uaps git history intentionally not carried (code copied;
   original repo retained as the archive).
8. **Scale (future):** per-node rollup of per-rank files + comm matrix for 1000s of ranks.
9. **Degradation:** when `perf_event_paranoid` blocks counters, the report prints an
   explicit "snapshot degraded to OS metrics" banner instead of silent zeros.

## Non-goals (this pass)
- Per-function hardware counters; per-rank HWPC; code/language merge; GPU/OMPT/energy/GUI.
