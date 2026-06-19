use std::time::Instant;

use anyhow::Result;
use uaps_core::{Collector, Metric, MetricValue, Target};

/// Measures wall-clock elapsed time between `start` and `finish`.
///
/// The simplest possible collector — it needs no privileges and works on any
/// platform, so it doubles as a smoke test for the collector harness.
#[derive(Default)]
pub struct ElapsedCollector {
    started: Option<Instant>,
}

impl ElapsedCollector {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Collector for ElapsedCollector {
    fn name(&self) -> &'static str {
        "elapsed"
    }

    fn start(&mut self, _target: &Target) -> Result<()> {
        self.started = Some(Instant::now());
        Ok(())
    }

    fn finish(&mut self) -> Result<Vec<Metric>> {
        let elapsed = self.started.map(|s| s.elapsed()).unwrap_or_default();
        Ok(vec![Metric {
            key: "elapsed_time",
            label: "Elapsed time".into(),
            value: MetricValue::Duration(elapsed),
        }])
    }
}
