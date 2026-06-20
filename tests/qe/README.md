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

Then, from this directory, with `DRV=../../core/cli/perfsuite`:

```sh
# A) full suite (snapshot + profile), serial
$DRV collect -o out/run_serial -- ./pw -in si.scf.in > out/qe.serial.out
$DRV report  out/run_serial                          > out/report.serial.txt

# B) MPI, 2 ranks (conda openmpi)
$DRV collect -o out/run_mpi -- qenv/bin/mpirun -np 2 ./pw -in si.scf.in > out/qe.mpi.out
$DRV report  out/run_mpi                                                 > out/report.mpi.txt

# C) per-function roofline (event sampling), single-threaded for clean hotspots
OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 \
  $DRV collect --roofline -o out/run_rf1 -- ./pw -in si.scf.in > out/qe.rf1.out
$DRV report out/run_rf1 --view roofline-func                    > out/roofline_func.txt
```

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
- **MPI (2 ranks)**: sampling attributes **35%** to MPI and the insight fires
  ("communication-heavy"). The MPI imbalance/FFT-imbalance across ranks shows in
  the per-group sampling table.

## Known limitation found here

`upat`'s MPI **tracing** wraps the C MPI ABI (`MPI_Allreduce`, …). QE (Fortran)
calls the **Fortran MPI bindings** (`mpi_allreduce_`, `mpi_barrier_`, … in
`libmpi_mpifh`), so MPI calls are **not traced by name** for Fortran codes —
hence `report --view mpi` (the wait-state breakdown) is empty for QE. MPI is
still captured by PC **sampling** (35% above) and by the insight engine.
Fix path: add the Fortran `mpi_*_` binding names to `gen/prototypes/mpi.txt`.
