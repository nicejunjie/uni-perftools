# Scientific Libraries Profiler

A low-overhead, no-recompile profiler for HPC applications — CrayPAT-like in
spirit, with two complementary views from a single run:

- **Sampling** (works for *any* binary): a statistical timer profile of the whole
  application → **top time-consuming functions and source lines**, like `gprof`
  but with no `-pg` rebuild and full support for threads and MPI ranks.
- **Tracing** (scientific libraries): exact call interception of **BLAS, LAPACK,
  PBLAS, ScaLAPACK, CBLAS, LAPACKe, FFTW, MPI** → per-function counts,
  inclusive/exclusive time, MPI communication volume (GB/s), and per-rank
  **load imbalance**.

## Quick start (no barrier)

```
make
bin/scilib-prof ./your_app                 # run + report, one step
bin/scilib-prof mpirun -n 4 ./your_app     # parallel; env propagated for you
bin/scilib-prof --dbi ./static_app         # Frida backend (also static libs)
make install PREFIX=~/.local               # then just: scilib-prof ./your_app
```

`scilib-prof` sets `LD_PRELOAD` + config for the child, runs it, and aggregates
the per-rank raw files into a report automatically. Example:

```
  Top functions  (sampling @ 1000 Hz, whole application)
   function / file:line                       samples  time(s)      %     imb
   dgemm_                                          365    0.365  97.9%   0.0%
   my_kernel                  solver.c:142          22    0.022   5.9%  41.0%
   ...
  Compute (BLAS / LAPACK / ...)        count[imb]  incl(s)  excl(s)
   BLAS  dgemm_                          4010 0.0%   0.357    0.357
  MPI (communication)            count[imb]  r/R   incl(s)      bytes    GB/s
   MPI_Allreduce                    102  9.8%  4/4   0.000     819840    8.5
```

## Manual use (no driver)

```
LD_PRELOAD=./libscilibprof-preload.so ./app      # writes scilib-prof.<rank>.json
tools/scilib-report.py scilib-prof.*.json        # symbolize + aggregate
```

The library only ever *writes raw per-process data*; all analysis (symbolization
via `addr2line`, cross-rank reduction, imbalance, formatting) happens in the
postprocess tool. You can re-analyze a finished run any number of ways without
re-running it.

## Configuration

Library (measurement) — environment variables:

| Variable            | Default | Meaning                                          |
|---------------------|---------|--------------------------------------------------|
| `SCILIB_SAMPLE`     | `1`     | statistical sampling on/off                      |
| `SCILIB_SAMPLE_HZ`  | `1000`  | sampling rate (Hz)                               |
| `SCILIB_SAMPLE_CPU` | `0`     | sample CPU time (`1`) vs wall time (`0`)         |
| `SCILIB_SHAPE`      | `0`     | per-shape tracing rows (`dgemm_[m=…,n=…,k=…]`)   |
| `SCILIB_OUTPUT`     | `scilib-prof` | raw-file path prefix (`<prefix>.<rank>.json`)|
| `SCILIB_QUIET`      | `0`     | suppress the "wrote …" note                      |

Wall-clock sampling (default) shows where real time goes, *including* MPI waits
and load imbalance; `SCILIB_SAMPLE_CPU=1` focuses on compute hotspots.

Postprocess (analysis): `scilib-report.py [--imbalance active|world]
[--format table|json|csv] [--sort t_excl|t_incl|count] [--top N] FILES...`.

## Build

```
make                 # libscilibprof-preload.so + libscilibprof-frida.so
make ILP64=1         # profile 64-bit-integer BLAS/LAPACK (MKL/NVPL ILP64)
make install PREFIX=...
tests/run.sh [preload|frida]
```

Plain C — no `mpicc`, no MPI/FFTW headers needed. Compiler + OpenMP flag are
auto-detected (`gcc`/`clang` → `-fopenmp`, nvhpc → `-mp`); the Frida backend
downloads the Frida-gum devkit on first build.

## How it works

```
src/core/       runtime: timer, per-thread store, call stack (incl/excl), raw emit
src/sample/     sampling: per-thread timer -> signal -> leaf-PC histogram
src/backends/   preload.c (dlsym RTLD_NEXT), frida.c (gum replace)
src/analyzers/  blas.c (+cblas shapes), fftw.c (plan registry), mpi.c (bytes)
gen/gen.py      reads gen/prototypes/*.txt, emits one thin wrapper per symbol
tools/scilib-report.py   symbolize + reduce across ranks -> tables
bin/scilib-prof          one-command driver (run + auto-report)
```

A single **universal** pipeline serves every library group; there is no
per-group code in the runtime/backends. Group-specific logic lives only in
opt-in analyzers and the declarative prototype lists (dialects `fortran` /
`c` / `opaque`). Sampling adds a second raw stream that the same per-rank
file/postprocess machinery carries.

**MPI portability:** the MPI wrappers are `mpi.h`-free (opaque dialect: uniform
`void*` args, `PMPI_Type_size` via `dlsym`). They reference no MPI constants or
handle types, so one binary works under OpenMPI *or* MPICH — avoiding the
constant/handle ABI mismatch that breaks tools like mpiP across implementations.

**Adding coverage** = editing `gen/prototypes/*.txt` (and optionally binding an
analyzer). No core changes.

## Notes & caveats

- Sampling symbolizes with `addr2line`; functions in libraries without debug info
  show as `func  libfoo.so` (no line). Build your app with `-g` for source lines.
- Flop-rate (GFLOP/s) is intentionally omitted: an exact static flop count exists
  only for a few routines. GB/s (MPI volume) is exact and reported.
- Only library calls crossing the public symbol boundary are traced; a single
  internally-threaded kernel is timed once.
- MPICH and aarch64 are designed-for (opaque MPI dialect, ucontext PC on both
  arches) but were validated here only on x86-64 + OpenMPI.
- Launcher executables (`mpirun`, `srun`, `numactl`, …) are skipped automatically.
