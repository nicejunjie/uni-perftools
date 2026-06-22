//! Generic hardware-counter collector (`PERF_TYPE_HARDWARE`).
//!
//! Counts portable PMU events (instructions, cycles, cache, branches) across
//! every thread of the target via per-thread hardware groups (see `pmu`).
//! Ratio pairs are co-located in the same group so IPC/CPI, cache-miss rate
//! and branch-mispredict rate are exact even under PMU multiplexing. Requires
//! `perf_event_paranoid` to permit measuring our own children; degrades to no
//! metrics rather than failing when counters are unavailable.

use anyhow::Result;
use uaps_core::{Collector, Metric, MetricValue, Target};

use crate::pmu::{EventCfg, ThreadGroups, TYPE_HARDWARE, TYPE_HW_CACHE};

// PERF_COUNT_HW_* config values (stable kernel ABI).
const HW_CPU_CYCLES: u64 = 0;
const HW_INSTRUCTIONS: u64 = 1;
const HW_CACHE_REFERENCES: u64 = 2;
const HW_CACHE_MISSES: u64 = 3;
const HW_BRANCH_INSTRUCTIONS: u64 = 4;
const HW_BRANCH_MISSES: u64 = 5;

// PERF_TYPE_HW_CACHE encoding: cache_id | (op << 8) | (result << 16).
// DTLB(3) / ITLB(4), op READ(0), result ACCESS(0) / MISS(1).
const DTLB_ACCESS: u64 = 3;
const DTLB_MISS: u64 = 3 | (1 << 16);
const ITLB_MISS: u64 = 4 | (1 << 16);

/// Emitted metrics, in the flattened order of the group spec below.
const OUTPUTS: &[(&str, &str)] = &[
    ("hw_instructions", "Instructions"),
    ("hw_cpu_cycles", "CPU cycles"),
    ("hw_cache_references", "Cache references"),
    ("hw_cache_misses", "Cache misses (LLC)"),
    ("hw_branch_instructions", "Branch instructions"),
    ("hw_branch_misses", "Branch misses"),
    ("dtlb_accesses", "dTLB load accesses"),
    ("dtlb_misses", "dTLB load misses"),
    ("itlb_misses", "iTLB misses"),
];

pub struct PerfCollector {
    groups: ThreadGroups,
}

impl PerfCollector {
    pub fn new() -> Self {
        let hw = |config| EventCfg { etype: TYPE_HARDWARE, config };
        let cache = |config| EventCfg { etype: TYPE_HW_CACHE, config };
        // Group A keeps instructions+cycles (IPC/CPI) and refs+misses
        // (miss rate, MPKI) together; group B the branch pair; group C the dTLB
        // access+miss pair (TLB miss rate). A group that can't schedule (e.g. dTLB
        // events unsupported) just yields no metrics for those keys.
        let groups = vec![
            vec![
                hw(HW_INSTRUCTIONS),
                hw(HW_CPU_CYCLES),
                hw(HW_CACHE_REFERENCES),
                hw(HW_CACHE_MISSES),
            ],
            vec![hw(HW_BRANCH_INSTRUCTIONS), hw(HW_BRANCH_MISSES)],
            vec![cache(DTLB_ACCESS), cache(DTLB_MISS), cache(ITLB_MISS)],
        ];
        Self { groups: ThreadGroups::new(groups) }
    }
}

impl Default for PerfCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for PerfCollector {
    fn name(&self) -> &'static str {
        "perf"
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
        if out.is_empty() {
            eprintln!(
                "uaps: no hardware counters available \
                 (check /proc/sys/kernel/perf_event_paranoid); reporting OS metrics only"
            );
        }
        Ok(out)
    }
}
