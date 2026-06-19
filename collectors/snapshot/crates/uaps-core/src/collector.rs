use anyhow::Result;

use crate::metric::Metric;

/// The process being profiled.
///
/// Phase 0 only carries a pid; later phases will extend this (e.g. cgroup,
/// MPI rank, launch metadata) without changing the [`Collector`] contract.
#[derive(Debug, Clone)]
pub struct Target {
    pub pid: u32,
}

/// A data-collection backend.
///
/// Lifecycle, driven by the CLI:
/// 1. [`start`](Collector::start) once the target exists / is attached,
/// 2. [`sample`](Collector::sample) zero or more times while it runs,
/// 3. [`finish`](Collector::finish) after it exits, returning its metrics.
///
/// Backends must degrade gracefully: if a privileged source is unavailable,
/// return the metrics that *could* be gathered rather than failing the run.
pub trait Collector {
    /// Stable short name, e.g. `"elapsed"`, `"proc"`, `"perf"`.
    fn name(&self) -> &'static str;

    /// Called once, immediately after the target process exists.
    fn start(&mut self, target: &Target) -> Result<()>;

    /// Called periodically while the target runs. Default: no-op (for
    /// collectors that only need start/finish snapshots).
    fn sample(&mut self) -> Result<()> {
        Ok(())
    }

    /// Called once after the target exits (or detach). Returns the metrics
    /// this collector produced.
    fn finish(&mut self) -> Result<Vec<Metric>>;
}
