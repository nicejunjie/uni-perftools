# Quantum ESPRESSO validation

End-to-end check of the suite (`uaps` snapshot + `upat` profiler + per-function
roofline) on a real HPC application — a bulk-silicon SCF calculation with
`pw.x`. Self-contained: the workload lives entirely under this directory.

## Layout

```
si.scf.in            bulk Si (diamond, 2 atoms) SCF input  [tracked]
pseudo/Si.pz-vbc.UPF norm-conserving Si pseudopotential     [tracked]
pw                   launcher → qenv/bin/pw.x               [tracked]
out/*.txt, out/*.out rendered reports + QE stdout           [tracked]
qenv/                conda-forge QE env (pw.x + libs)       [git-ignored, ~770 MB]
local/ pkg/          (unused) Ubuntu QE deb extract         [git-ignored]
scratch/ out/run_*/  QE scratch + raw per-run result dirs   [git-ignored]
```

## Reproduce

The system Ubuntu `quantum-espresso` 6.7 binary aborts at startup on glibc 2.39
(`snprintf` fortify), so we use a working conda-forge build, installed **into
this directory** (not system-wide):

```sh
conda create -y -p ./qenv --override-channels -c conda-forge qe
```

Then, from this directory, with `UPAT=../../core/cli/upat` and `UAPS` pointing at
the built `uaps` binary. The two are cost tiers — run one. To get the combined
view, drop a `uaps` snapshot into the same result dir and `upat report` folds it in.

**Pin to physical cores.** This box is 16 physical cores / 32 logical (SMT2).
Letting QE/OpenMP/OpenBLAS spread across all 32 logical CPUs oversubscribes the
SMT siblings and both slows the run and skews the counters — on this Si SCF it
made the serial run ~3× slower (8.5 s vs 2.9 s). So we set `OMP_PLACES=cores`
(one thread per physical core, never a hyperthread) and cap the thread count at
the 16 physical cores. Get the count with `lscpu -p=Socket,Core | grep -v '^#' |
sort -u | wc -l`.

```sh
export OMP_PLACES=cores OMP_PROC_BIND=close   # physical-core placement, no SMT oversubscribe

# A) deep profile (upat), serial — 16 threads on 16 physical cores
OMP_NUM_THREADS=16 OPENBLAS_NUM_THREADS=16 \
  $UPAT run -o out/run_serial -- ./pw -in si.scf.in > out/qe.serial.out
$UPAT report  out/run_serial                        > out/report.serial.txt
# optional: add the snapshot tier into the same dir → combined report
OMP_NUM_THREADS=16 OPENBLAS_NUM_THREADS=16 \
  $UAPS run --format json -o out/run_serial/snap.json -- ./pw -in si.scf.in

# B) MPI, 2 ranks × 8 threads = 16 physical cores (conda openmpi, bound to cores)
OMP_NUM_THREADS=8 OPENBLAS_NUM_THREADS=8 \
  $UPAT run -o out/run_mpi -- qenv/bin/mpirun -np 2 --bind-to core --map-by socket:PE=8 \
    ./pw -in si.scf.in > out/qe.mpi.out
$UPAT report  out/run_mpi                                            > out/report.mpi.txt

# C) per-function roofline (event sampling), single-threaded for clean hotspots
OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 \
  $UPAT roofline -o out/run_rf1 -- ./pw -in si.scf.in > out/qe.rf1.out
$UPAT report out/run_rf1 --view roofline-func          > out/roofline_func.txt

# the snapshot tier on its own (APS-style bird's-eye, no injection)
OMP_NUM_THREADS=8 OPENBLAS_NUM_THREADS=8 \
  $UAPS run -- qenv/bin/mpirun -np 2 --bind-to core --map-by socket:PE=8 ./pw -in si.scf.in
```

Reporting options (calls aggregate over input sizes by default):

```sh
$UPAT report out/run_serial --detail blas      # per-shape BLAS breakdown (post analysis)
$UPAT report out/run_mpi    --detail mpi        # MPI volume + size histogram (text)

# HTML reports (self-contained; comm matrix as a heatmap figure, like Intel APS)
$UPAT report out/run_serial --format html -o out            # → out/report.html
$UPAT report out/run_mpi --detail mpi --format html -o out  # → out/report.mpi.html
```

The text MPI report shows the full rank×rank matrix only for small jobs (≤8
ranks); beyond that it lists per-rank volume and points to the HTML, whose
heatmap down-samples into ≤256×256 buckets so it stays legible at thousand-rank
scale.

## Saved reports (`out/`)

| file | what |
|------|------|
| `report.serial.txt` / `report.mpi.txt` | `upat report` (deep tier; combined if snap.json present) |
| `roofline_func.txt`                     | per-function roofline (single-threaded) |
| `detail.blas.txt`                       | `--detail blas` per-shape breakdown |
| `detail.mpi.txt`                        | `--detail mpi` comm matrix + size histogram (text) |
| `snapshot.serial.txt` / `snapshot.mpi.txt` | `uaps` snapshots (APS-style; MPI section on the MPI run) |
| `report.html`                           | HTML report (SVG roofline + styled tables) |
| `report.mpi.html`                       | HTML MPI analysis (comm-matrix heatmap + histogram) |
| `qe.*.out`                              | QE's own stdout for each run |

## What it validated

- **`upat` traces QE's real sci-lib calls**: BLAS (`zgemv`, `zdotc`, …),
  LAPACK, and FFTW (`fftw_execute_dft`, 2.5M calls dominating compute), with
  per-shape rows. FFT-heavy character of QE comes through clearly.
- **`uaps` snapshot**: elapsed, core utilization, IPC/CPI, memory-bound %,
  top-down pipeline slots, peak RSS, disk I/O — all populated.
- **Whole-program roofline**: this small Si SCF is **memory-bound** (AI ≈ 9.4,
  below the FP64 ridge ≈ 20.8).
- **Per-function roofline (mechanism B)** surfaces real functions with no name
  knowledge — FFTW codelets (`n1bv_32`, `n1fv_32` ≈ 20 GFLOP/s), QE's own
  `vloc_psi_k_acc_` (≈ 22 GFLOP/s) and `__fft_scalar_fftw3_MOD_cfft3d`, plus
  system/runtime frames — library, user, and system alike.
- **MPI (2 ranks), traced by name**: upat now wraps the **Fortran MPI bindings**
  (`mpi_*_`), so QE's MPI is traced exactly. `mpi_alltoall_` dominates at **603 MB**
  — QE's parallel 3-D FFT transposes — followed by bcast/allreduce/send/recv. The
  wait-state view, point-to-point-vs-collective split, message-size histogram, and
  the rank×rank communication matrix all populate (`report --detail mpi`). The
  late-sender/imbalance insight fires.
