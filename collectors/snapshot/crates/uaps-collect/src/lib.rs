//! Data-collection backends behind the [`uaps_core::Collector`] trait.
//!
//! Phase 0 ships only [`ElapsedCollector`]. Future modules: `proc` (/proc
//! sampling), `perf` (perf_event_open counters), `ebpf` (syscall/off-CPU/IO).

// Cross-platform collectors.
mod elapsed;
mod mpi;
pub use elapsed::ElapsedCollector;
pub use mpi::MpiCollector;

// GPU-offload detection from the CPU side (reads /proc; a no-op off Linux).
pub mod gpu;

// Linux-only backends; other platforms use the stubs in `fallback`.
#[cfg(target_os = "linux")]
pub mod pmudb;
#[cfg(target_os = "linux")]
mod cpu;
#[cfg(target_os = "linux")]
mod perf;
#[cfg(target_os = "linux")]
mod pmu;
#[cfg(target_os = "linux")]
mod proc;
#[cfg(target_os = "linux")]
mod raw_pmu;
#[cfg(target_os = "linux")]
mod sw;
#[cfg(target_os = "linux")]
mod threads;
#[cfg(target_os = "linux")]
mod topdown;
#[cfg(target_os = "linux")]
pub use {
    perf::PerfCollector, pmu::raise_fd_limit, pmu::set_system_wide, pmudb::HwpcCollector,
    proc::ProcCollector, raw_pmu::RawPmuCollector, sw::SwCollector, threads::ThreadCollector,
    topdown::TopdownCollector,
};

/// Enable node-level (system-wide, per-CPU) HW counting. No-op off Linux.
#[cfg(not(target_os = "linux"))]
pub fn set_system_wide(_on: bool) {}

/// Raise the open-file limit ahead of opening many perf counters. No-op off Linux.
#[cfg(not(target_os = "linux"))]
pub fn raise_fd_limit() {}

/// Global MPI rank from the launcher environment (no MPI calls needed), else None.
/// Must stay in sync with `core/contract/contract.py:rank_from_env` and the C
/// profiler's `util.c` — a launcher missing here makes ranks collide on rank 0.
pub fn rank_from_env() -> Option<i64> {
    const KEYS: [&str; 7] = [
        "OMPI_COMM_WORLD_RANK",
        "PMI_RANK",
        "PMIX_RANK",
        "MV2_COMM_WORLD_RANK",
        "SLURM_PROCID",
        "PALS_RANKID",
        "ALPS_APP_PE",
    ];
    for k in KEYS {
        if let Ok(v) = std::env::var(k) {
            if let Ok(r) = v.trim().parse::<i64>() {
                return Some(r);
            }
        }
    }
    None
}

/// Total MPI job size (number of ranks) from the launcher environment, else None.
/// Lets the per-rank aggregator detect a SHORT rank set (a node-local rank dir, or
/// crashed ranks) instead of silently undercounting.
pub fn mpi_world_size_from_env() -> Option<i64> {
    const KEYS: [&str; 6] = [
        "OMPI_COMM_WORLD_SIZE",
        "PMI_SIZE",
        "MV2_COMM_WORLD_SIZE",
        "SLURM_NTASKS",
        "SLURM_NPROCS",
        "PALS_NRANKS",
    ];
    for k in KEYS {
        if let Ok(v) = std::env::var(k) {
            if let Ok(r) = v.trim().parse::<i64>() {
                if r > 0 {
                    return Some(r);
                }
            }
        }
    }
    None
}

// Compiled on every platform so Linux builds keep it valid, but only used as
// the public collectors off Linux.
#[cfg_attr(target_os = "linux", allow(dead_code))]
mod fallback;
#[cfg(not(target_os = "linux"))]
pub use fallback::{
    PerfCollector, ProcCollector, RawPmuCollector, SwCollector, ThreadCollector, TopdownCollector,
};
