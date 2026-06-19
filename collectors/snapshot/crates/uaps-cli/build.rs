//! Build the MPI PMPI shim with `mpicc`, if available.
//!
//! The compiled `uaps_mpi.so` path is exposed to the crate via the
//! `UAPS_MPI_SHIM_BUILT` compile-time env var (empty string when mpicc is
//! absent, so `uaps run --mpi` can report a clear error instead of failing
//! the whole build on machines without MPI).

use std::env;
use std::process::Command;

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out = env::var("OUT_DIR").unwrap();
    let src = format!("{manifest}/../../shim/mpi/uaps_mpi.c");
    let so = format!("{out}/uaps_mpi.so");

    println!("cargo:rerun-if-changed={src}");
    println!("cargo:rerun-if-env-changed=UAPS_MPI_SHIM");

    let built = Command::new("mpicc")
        .args(["-shared", "-fPIC", "-O2", &src, "-o", &so])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if built {
        println!("cargo:rustc-env=UAPS_MPI_SHIM_BUILT={so}");
    } else {
        println!("cargo:rustc-env=UAPS_MPI_SHIM_BUILT=");
        println!(
            "cargo:warning=uaps: mpicc unavailable or MPI shim build failed; \
             `uaps run --mpi` will be disabled (set UAPS_MPI_SHIM to a prebuilt .so to enable)"
        );
    }
}
