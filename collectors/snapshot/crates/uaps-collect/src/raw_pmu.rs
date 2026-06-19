//! Vendor-specific raw PMU events (`PERF_TYPE_RAW`): FP throughput,
//! vectorization, and memory data-source counts.
//!
//! Like the generic collector, these run as per-thread hardware groups (see
//! `pmu`) instead of inherited individual counters, so they stay accurate
//! under PMU multiplexing on multithreaded workloads. All events fit one
//! ≤5-event group per vendor.
//!
//! Validated on AMD Zen 5 (Ryzen 9 9950X). Intel encodings follow the SDM but
//! are not validated on this host. Any unsupported event simply yields no
//! metric for that quantity.

use std::time::Instant;

use anyhow::Result;
use uaps_core::{Collector, Metric, MetricValue, Target};

use crate::cpu::{self, Vendor};
use crate::pmu::{EventCfg, ThreadGroups, TYPE_RAW};

fn raw(config: u64) -> EventCfg {
    EventCfg { etype: TYPE_RAW, config }
}

enum Kind {
    /// AMD: [FpRetSseAvxOps, fills_all, fills_dram, fills_remote].
    Amd,
    /// Intel: [scalar_f64, 128b_f64, 256b_f64, 512b_f64].
    Intel,
    None,
}

pub struct RawPmuCollector {
    kind: Kind,
    groups: ThreadGroups,
    started: Option<Instant>,
}

impl RawPmuCollector {
    pub fn new() -> Self {
        let info = cpu::detect();
        let (kind, spec) = match info.vendor {
            // FpRetSseAvxOps (0x0F03) already counts FLOPs; demand-fill sources
            // 0x43 with umasks all/local+remote-DRAM/remote-node.
            Vendor::Amd => (
                Kind::Amd,
                vec![raw(0x0F03), raw(0xFF43), raw(0x4843), raw(0x5043)],
            ),
            // FP_ARITH_INST_RETIRED (0xC7) per-width double-precision umasks.
            Vendor::Intel => (
                Kind::Intel,
                vec![raw(0x01C7), raw(0x04C7), raw(0x10C7), raw(0x40C7)],
            ),
            Vendor::Other => (Kind::None, vec![]),
        };
        Self { kind, groups: ThreadGroups::new(vec![spec]), started: None }
    }
}

impl Default for RawPmuCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for RawPmuCollector {
    fn name(&self) -> &'static str {
        "raw_pmu"
    }

    fn start(&mut self, target: &Target) -> Result<()> {
        self.started = Some(Instant::now());
        self.groups.start(target.pid);
        Ok(())
    }

    fn sample(&mut self) -> Result<()> {
        self.groups.scan();
        Ok(())
    }

    fn finish(&mut self) -> Result<Vec<Metric>> {
        let elapsed = self.started.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
        let sums = self.groups.read_sums();
        let get = |i: usize| sums.get(i).copied().flatten();

        let mut out = Vec::new();
        match self.kind {
            Kind::Amd => {
                if let Some(flops) = get(0) {
                    if elapsed > 0.0 {
                        out.push(gflops_metric(flops, elapsed));
                    }
                }
                for (i, key, label) in [
                    (1, "mem_fills_all", "Demand fills (all sources)"),
                    (2, "mem_fills_dram", "Demand fills from DRAM"),
                    (3, "mem_fills_remote", "Demand fills from remote node"),
                ] {
                    if let Some(v) = get(i) {
                        out.push(Metric {
                            key,
                            label: label.into(),
                            value: MetricValue::Int { value: v as i64, unit: "" },
                        });
                    }
                }
            }
            Kind::Intel => {
                // weights = double-precision elements per op.
                let scalar = get(0).unwrap_or(0.0);
                let p128 = get(1).unwrap_or(0.0);
                let p256 = get(2).unwrap_or(0.0);
                let p512 = get(3).unwrap_or(0.0);
                let vector = p128 * 2.0 + p256 * 4.0 + p512 * 8.0;
                let total = scalar + vector;
                if total > 0.0 && elapsed > 0.0 {
                    out.push(gflops_metric(total, elapsed));
                    out.push(Metric {
                        key: "vectorization_pct",
                        label: "Vectorization".into(),
                        value: MetricValue::Percent(vector / total * 100.0),
                    });
                }
            }
            Kind::None => {}
        }
        Ok(out)
    }
}

fn gflops_metric(flops: f64, elapsed: f64) -> Metric {
    Metric {
        key: "gflops",
        label: "FP throughput".into(),
        value: MetricValue::Float { value: flops / elapsed / 1e9, unit: "GFLOP/s" },
    }
}
