# Universal Performance Tool Suite (uni-perftools)

A portable HPC performance suite — two independent **cost-tier** commands over a
shared analysis core. They descend from the vendor tools they emulate, made
universal (vendor-neutral):

- **`uaps`** — *Universal Application Performance Snapshot* (← Intel **APS**).
  Cheap, **non-invasive** bird's-eye view from hardware counters: roofline +
  microarchitecture (IPC, %peak, top-down, DRAM/NUMA, vectorization), MPI &
  OpenMP/thread imbalance, memory, I/O — aggregated across ranks. Run it first
  on anything (no injection needed): *"what kind of bottleneck, how efficient?"*
  Rust, `collectors/snapshot/`.
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
uaps run -- mpirun -n 4 ./app      # snapshot: HWPC + MPI/OpenMP, one screen across ranks
upat run -- ./app                  # deep profile + report
upat run -- mpirun -n 4 ./app
upat report  RESULT                # re-render (text); --format html for the HTML report
upat report  RESULT --detail mpi   # per-facility detail (comm matrix, per-shape calls)
upat roofline -- ./app             # per-function roofline (event sampling)
upat scale   R1 R2 ...             # strong/weak scaling across runs
```

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
