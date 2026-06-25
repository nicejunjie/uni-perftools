# Universal Performance Tool Suite (uni-perftools)

A portable HPC performance suite — two independent **cost-tier** commands over a
shared analysis core. They descend from the vendor tools they emulate, made
universal (vendor-neutral):

- **`uaps`** — *Universal Application Performance Snapshot* (← Intel **APS**).
  Cheap, **non-invasive** bird's-eye view from hardware counters: roofline +
  microarchitecture (IPC, %peak, top-down, DRAM/NUMA, vectorization), MPI &
  OpenMP/thread imbalance, memory, I/O. Like APS it collects **per-rank** — each
  rank counts itself on its own node and the results aggregate across ranks (+
  per-rank HW imbalance), so it's correct on multi-node jobs and needs only
  `perf_event_paranoid<=1`. Run it first on anything: *"what kind of bottleneck,
  how efficient, and is it balanced across ranks?"* Rust, `collectors/snapshot/`.
- **`upat`** — *Universal Performance Analysis Tool* (← **CrayPAT**). The deep
  dive via LD_PRELOAD interception + call-stack/event sampling: sci-lib tracing
  (BLAS/LAPACK/FFTW + Fortran/C **MPI** + I/O), per-function roofline, comm
  matrix, heap. *"where exactly does the time go?"* C `.so` + report,
  `collectors/profile/`.
- **`core/`** — shared spine: result/contract format, symbolization, the one
  imbalance metric, empirical roofline ceilings, analysis **viewpoints**, the
  insights engine, HTML reports, and the `upat` CLI.

They are **cost tiers, run one or the other** — there is no umbrella driver. If a
`snap.json` from a `uaps` run happens to sit in the result dir, `upat report`
folds it in automatically.

## Usage
```
# snapshot, per-rank (APS-style) — uaps INSIDE the launcher (works with ANY launcher):
mpirun -n 4 uaps ./app             #   each rank counts its OWN process on its own node
uaps report uaps_result            #   aggregate the per-rank dir (like `aps-report`)
uaps run -a -- mpirun -n 4 ./app   #   node-level (system-wide) alternative — launcher node only
upat run -- ./app                  # deep profile + report
upat run -- mpirun -n 4 ./app
upat report  RESULT                # re-render (text); --format html for the HTML report
upat report  RESULT --detail mpi   # per-facility detail (comm matrix, per-shape calls)
upat roofline -- ./app             # per-function roofline (event sampling)
upat scale   R1 R2 ...             # strong/weak scaling across runs
```

### Options (env, `upat` collection)
The `upat run`/`collect` collector reads a few env vars (defaults shown):

| Variable | Default | Effect |
| --- | --- | --- |
| `UPAT_SAMPLE` | `1` | Call-stack sampling on; `0` traces sci-lib/MPI/IO only. |
| `UPAT_SAMPLE_HZ` | `1000` | Sampling frequency in Hz (clamped 1–100000). |
| `UPAT_SAMPLE_STACK` | `64` | Max call-stack depth captured (1–128). |
| `UPAT_HEAP` | `0` | `1` adds heap high-water (peak / live-at-exit / allocs). Off by default — leaves the allocator untouched. |
| `UPAT_ROOFLINE` | `0` | `1` runs the per-function roofline characterization pass (or use `upat roofline`). |
| `UPAT_OUTPUT` | `upat` | Output prefix/dir for the raw per-rank JSON. |
| `UPAT_QUIET` | `0` | `1` silences the startup/finish banner. |

## Layout
```
collectors/profile/    C profiler  → libupat-{preload,frida}.so + upat-report.py
collectors/snapshot/   Rust counters → uaps
core/{contract,symbolize,roofline,analysis,cli}   shared spine (upat command in core/cli/upat)
tests/  docs/
```

## Build
```
make            # builds both collectors
make test       # profile tests + snapshot tests + suite end-to-end tests
make install    # installs bin/uaps + bin/upat under PREFIX
```
Each collector also builds standalone (`make -C collectors/profile`,
`cd collectors/snapshot && cargo build`).
