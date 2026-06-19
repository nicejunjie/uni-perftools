//! Data-collection backends behind the [`uaps_core::Collector`] trait.
//!
//! Phase 0 ships only [`ElapsedCollector`]. Future modules: `proc` (/proc
//! sampling), `perf` (perf_event_open counters), `ebpf` (syscall/off-CPU/IO).

// Cross-platform collectors.
mod elapsed;
mod mpi;
pub use elapsed::ElapsedCollector;
pub use mpi::MpiCollector;

// Linux-only backends; other platforms use the stubs in `fallback`.
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
mod threads;
#[cfg(target_os = "linux")]
mod topdown;
#[cfg(target_os = "linux")]
pub use {
    perf::PerfCollector, proc::ProcCollector, raw_pmu::RawPmuCollector, threads::ThreadCollector,
    topdown::TopdownCollector,
};

// Compiled on every platform so Linux builds keep it valid, but only used as
// the public collectors off Linux.
#[cfg_attr(target_os = "linux", allow(dead_code))]
mod fallback;
#[cfg(not(target_os = "linux"))]
pub use fallback::{
    PerfCollector, ProcCollector, RawPmuCollector, ThreadCollector, TopdownCollector,
};
