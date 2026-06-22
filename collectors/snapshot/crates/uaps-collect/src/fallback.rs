//! Non-Linux placeholder collectors.
//!
//! uaps is Linux-first: the `/proc` and `perf_event_open` backends only exist
//! on Linux. On other platforms these stubs keep the binary building and the
//! cross-platform collectors (elapsed time, MPI aggregation) working, while
//! returning no metrics of their own. Real backends are roadmap Phase 6:
//! macOS via kperf / Instruments, Windows via ETW / Intel PCM.
//!
//! This module is compiled on every platform (so Linux builds verify it stays
//! valid) but only re-exported from `lib.rs` on non-Linux targets.

use anyhow::Result;
use uaps_core::{Collector, Metric, Target};

macro_rules! stub_collector {
    ($(#[$doc:meta])* $name:ident => $tag:literal) => {
        $(#[$doc])*
        #[derive(Default)]
        pub struct $name;

        impl $name {
            pub fn new() -> Self {
                Self
            }
        }

        impl Collector for $name {
            fn name(&self) -> &'static str {
                $tag
            }
            fn start(&mut self, _target: &Target) -> Result<()> {
                Ok(())
            }
            fn finish(&mut self) -> Result<Vec<Metric>> {
                Ok(Vec::new())
            }
        }
    };
}

stub_collector!(
    /// Placeholder for the `/proc` collector on non-Linux platforms.
    ProcCollector => "proc"
);
stub_collector!(
    /// Placeholder for the `perf_event_open` collector on non-Linux platforms.
    PerfCollector => "perf"
);
stub_collector!(
    /// Placeholder for the per-thread `/proc/<pid>/task` collector elsewhere.
    ThreadCollector => "threads"
);
stub_collector!(
    /// Placeholder for the vendor raw-PMU (FLOPS/vectorization) collector.
    RawPmuCollector => "raw_pmu"
);
stub_collector!(
    /// Placeholder for the top-down pipeline-slot collector.
    TopdownCollector => "topdown"
);
stub_collector!(
    /// Placeholder for the kernel software-event collector (ctx switches, faults).
    SwCollector => "sw"
);

#[cfg(test)]
mod tests {
    use super::*;

    // Verifies (on every platform, including Linux CI) that the stubs satisfy
    // the Collector contract and degrade to no metrics rather than erroring.
    #[test]
    fn stubs_implement_collector_and_yield_nothing() {
        let mut collectors: Vec<Box<dyn Collector>> = vec![
            Box::new(ProcCollector::new()),
            Box::new(PerfCollector::new()),
            Box::new(ThreadCollector::new()),
            Box::new(RawPmuCollector::new()),
            Box::new(TopdownCollector::new()),
        ];
        for c in &mut collectors {
            c.start(&Target { pid: 1 }).unwrap();
            assert!(c.finish().unwrap().is_empty());
        }
    }
}
