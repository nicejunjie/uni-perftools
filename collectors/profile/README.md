# Scientific Libraries Profiler

A low-overhead, no-recompile profiler for HPC applications — CrayPAT-like in
spirit, with two complementary views from a single run:

- **Sampling** (works for *any* binary): a statistical timer profile of the whole
  application → **top time-consuming functions and source lines**, like `gprof`
  but with no `-pg` rebuild and full support for threads and MPI ranks. Captures
  **call stacks** by default, so time is attributed inclusively and charged to the
  right group (e.g. blocking in MPI shows as **MPI**, not libc `poll`); exports
  **folded stacks** for flame graphs (`upat-report --folded`).
- **Tracing** (scientific libraries + MPI + I/O): exact call interception of
  **BLAS, LAPACK, PBLAS, ScaLAPACK, CBLAS, LAPACKe, FFTW, MPI, POSIX I/O** →
  per-function counts, inclusive/exclusive time, communication/I/O volume (GB/s),
  **MPI message-size histogram + communication matrix**, optional **heap
  high-water**, per-rank **load imbalance**, and auto **observations**.

## Quick start (no barrier)

```
make
bin/upat ./your_app                 # run + report, one step
bin/upat mpirun -n 4 ./your_app     # parallel; env propagated for you
bin/upat --dbi ./static_app         # Frida backend (also static libs)
make install PREFIX=~/.local               # then just: upat ./your_app
```

`upat` sets `LD_PRELOAD` + config for the child, runs it, and aggregates
the per-rank raw files into a report automatically. The report mimics CrayPAT —
a sampling "Profile by Function Group and Function" followed by exact library
tracing tables:

```
Table 1:  Profile by Function Group and Function  (sampling @ 1000 Hz, 4 PEs)
   Samp%      Samp  Imb.Samp  Imb.Samp%  Group
                                         Function=[file:line]
 100.0%      4615      96.2      7.7%  Total
  94.9%      4379      64.2      5.5%  ETC          # MPI wait/progress (poll,...)
   5.1%       236      32.0     35.2%  BLAS
   5.1%       236      32.0     35.2%    dgemm_  [libblas.so.3]
   ...                                  USER         # your code: solver.c:142, ...

Table 2:  Library calls by group and function  (tracing)
   BLAS  dgemm_                  4010 0.0%   0.357    0.357
Table 3:  MPI message statistics  (tracing)
   MPI_Allreduce        102  9.8%  4/4   0.000     819840    8.5
```

Groups: **USER** (your code), **MPI**, **BLAS**, **LAPACK**, **FFTW**, **ETC**
(libc/runtime). Use `UPAT_SAMPLE_CPU=1` to focus on compute and push MPI wait
time out of the picture.

## Manual use (no driver)

```
LD_PRELOAD=./libupat-preload.so ./app      # writes upat.<rank>.json
tools/upat-report.py upat.*.json        # symbolize + aggregate
```

The library only ever *writes raw per-process data*; all analysis (symbolization
via `addr2line`, cross-rank reduction, imbalance, formatting) happens in the
postprocess tool. You can re-analyze a finished run any number of ways without
re-running it.

## Configuration

Library (measurement) — environment variables:

| Variable            | Default | Meaning                                          |
|---------------------|---------|--------------------------------------------------|
| `UPAT_SAMPLE`     | `1`     | statistical sampling on/off                      |
| `UPAT_SAMPLE_HZ`  | `1000`  | sampling rate (Hz)                               |
| `UPAT_SAMPLE_CPU` | `0`     | sample CPU time (`1`) vs wall time (`0`)         |
| `UPAT_SAMPLE_STACK`| `64`   | call-stack depth to unwind per sample (`1`=leaf only) |
| `UPAT_HEAP`       | `0`     | track heap high-water mark (interposes malloc)   |
| `UPAT_SHAPE`      | `0`     | per-shape tracing rows (`dgemm_[m=…,n=…,k=…]`)   |
| `UPAT_OUTPUT`     | `upat` | raw-file path prefix (`<prefix>.<rank>.json`)|
| `UPAT_QUIET`      | `0`     | suppress the "wrote …" note                      |

Wall-clock sampling (default) shows where real time goes, *including* MPI waits
and load imbalance; `UPAT_SAMPLE_CPU=1` focuses on compute hotspots.

Postprocess (analysis): `upat-report.py [--imbalance active|world]
[--format table|json|csv] [--sort t_excl|t_incl|count] [--top N] FILES...`.

## Build

```
make                 # libupat-preload.so + libupat-frida.so
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
src/analyzers/  blas.c (+cblas shapes), fftw.c (plan registry), mpi.c (bytes,
                size histogram, comm matrix), io.c (POSIX bytes), heap.c (high-water)
gen/gen.py      reads gen/prototypes/*.txt, emits one thin wrapper per symbol
tools/upat-report.py   symbolize + reduce across ranks -> tables
bin/upat          one-command driver (run + auto-report)
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
