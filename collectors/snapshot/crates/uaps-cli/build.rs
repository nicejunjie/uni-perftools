//! Build the MPI timing shim with `cc`.
//!
//! The shim is now mpi.h-free (forwards to PMPI_* / pmpi_*_ via dlsym), so it
//! compiles with a plain C compiler — no mpicc, no MPI headers — and works
//! against any MPI at runtime (OpenMPI/MPICH/Cray) for C and Fortran codes.
//! The compiled path is exposed via the `UAPS_MPI_SHIM_BUILT` compile-time env.

use std::env;
use std::process::Command;

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out = env::var("OUT_DIR").unwrap();
    let src = format!("{manifest}/../../shim/mpi/uaps_mpi.c");
    let so = format!("{out}/uaps_mpi.so");

    println!("cargo:rerun-if-changed={src}");
    println!("cargo:rerun-if-env-changed=UAPS_MPI_SHIM");

    let cc = env::var("CC").unwrap_or_else(|_| "cc".into());
    let built = Command::new(cc)
        .args(["-shared", "-fPIC", "-O2", &src, "-o", &so, "-ldl", "-lpthread"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if built {
        println!("cargo:rustc-env=UAPS_MPI_SHIM_BUILT={so}");
    } else {
        println!("cargo:rustc-env=UAPS_MPI_SHIM_BUILT=");
        println!("cargo:warning=uaps: MPI shim build failed; set UAPS_MPI_SHIM to a prebuilt .so");
    }
}
