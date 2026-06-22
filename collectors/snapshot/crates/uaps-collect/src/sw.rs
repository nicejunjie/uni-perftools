//! Kernel software-counter collector (`PERF_TYPE_SOFTWARE`).
//!
//! Context switches, CPU migrations, and page faults — scheduling/OS pressure
//! signals that the hardware PMU doesn't provide. Software events always schedule
//! (no PMC limit, no multiplexing) and need no special privilege beyond what
//! per-process counting already requires, so this works even where the HWPC path
//! degrades. Uses the same per-thread / per-CPU group plumbing as `perf`.

use anyhow::Result;
use uaps_core::{Collector, Metric, MetricValue, Target};

use crate::pmu::{EventCfg, ThreadGroups, TYPE_SOFTWARE};

// PERF_COUNT_SW_* config values (stable kernel ABI).
const SW_PAGE_FAULTS: u64 = 2;
const SW_CONTEXT_SWITCHES: u64 = 3;
const SW_CPU_MIGRATIONS: u64 = 4;
const SW_PAGE_FAULTS_MAJ: u64 = 6;

/// Emitted metrics, in the flattened order of the group spec below.
const OUTPUTS: &[(&str, &str)] = &[
    ("ctx_switches", "Context switches"),
    ("cpu_migrations", "CPU migrations"),
    ("page_faults", "Page faults"),
    ("page_faults_maj", "Major page faults"),
];

pub struct SwCollector {
    groups: ThreadGroups,
}

impl SwCollector {
    pub fn new() -> Self {
        let sw = |config| EventCfg { etype: TYPE_SOFTWARE, config };
        // Software events don't compete for PMCs, so one group of all four is fine.
        let groups = vec![vec![
            sw(SW_CONTEXT_SWITCHES),
            sw(SW_CPU_MIGRATIONS),
            sw(SW_PAGE_FAULTS),
            sw(SW_PAGE_FAULTS_MAJ),
        ]];
        Self { groups: ThreadGroups::new(groups) }
    }
}

impl Default for SwCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for SwCollector {
    fn name(&self) -> &'static str {
        "sw"
    }

    fn start(&mut self, target: &Target) -> Result<()> {
        self.groups.start(target.pid);
        Ok(())
    }

    fn sample(&mut self) -> Result<()> {
        self.groups.scan();
        Ok(())
    }

    fn finish(&mut self) -> Result<Vec<Metric>> {
        let sums = self.groups.read_sums();
        let mut out = Vec::new();
        for (i, (key, label)) in OUTPUTS.iter().enumerate() {
            if let Some(Some(value)) = sums.get(i) {
                out.push(Metric {
                    key,
                    label: (*label).into(),
                    value: MetricValue::Int { value: *value as i64, unit: "" },
                });
            }
        }
        Ok(out)
    }
}
