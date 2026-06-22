//! Vendor raw PMU events (FP throughput, vectorization, memory data-source).
//!
//! **Fully data-driven.** The events are NOT hard-coded as raw config bytes —
//! each is named and resolved from the vendored pmu-events db at runtime via
//! `pmudb::resolve_config_in` → `(config, perf_type)`. One uniform path serves
//! AMD, Intel, and ARM, carries the correct PMU type (x86 `PERF_TYPE_RAW` vs
//! ARM's dynamic core-PMU type), and works across CPU generations without magic
//! numbers. A name that doesn't resolve for this model simply yields no metric.
//!
//! Like the generic collector, these run as per-thread hardware groups (see
//! `pmu`) so they stay accurate under PMU multiplexing on multithreaded
//! workloads. The per-vendor event set fits one ≤5-event group.
//!
//! Validated: AMD Zen 3–5 (the resolved configs reproduce the historical
//! hand-coded encodings — see `raw_pmu_policy_resolves` test) and ARM Neoverse V2
//! (Grace). Intel encodings follow the pmu-events data; unvalidated on real Intel.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use uaps_core::{Collector, Metric, MetricValue, Target};

use crate::cpu::{self, Vendor};
use crate::pmu::{EventCfg, ThreadGroups};

/// (role, pmu-events event name) per vendor. `role` is how `finish()` looks the
/// event up after resolution; the encoding comes entirely from the db.
fn policy(vendor: Vendor) -> &'static [(&'static str, &'static str)] {
    match vendor {
        // FpRetSseAvxOps already counts FLOPs; demand-fill sources by umask.
        Vendor::Amd => &[
            ("fp", "fp_ret_sse_avx_ops.all"),
            ("fills_all", "ls_dmnd_fills_from_sys.all"),
            ("fills_dram", "ls_dmnd_fills_from_sys.dram_io_all"),
            ("fills_remote", "ls_dmnd_fills_from_sys.far_all"),
        ],
        // FP_ARITH_INST_RETIRED per-width double-precision umasks → weighted FLOPs
        // + a scalar/vector split (vectorization %).
        Vendor::Intel => &[
            ("fp_scalar", "fp_arith_inst_retired.scalar_double"),
            ("fp_128", "fp_arith_inst_retired.128b_packed_double"),
            ("fp_256", "fp_arith_inst_retired.256b_packed_double"),
            ("fp_512", "fp_arith_inst_retired.512b_packed_double"),
        ],
        // FP element-ops (speculative — a proxy like AMD's) + last-level read
        // misses ≈ DRAM read fills.
        Vendor::Arm => &[
            ("fp_scale", "fp_scale_ops_spec"),
            ("fp_fixed", "fp_fixed_ops_spec"),
            ("fills_dram", "ll_cache_miss_rd"),
        ],
        Vendor::Other => &[],
    }
}

pub struct RawPmuCollector {
    vendor: Vendor,
    role_idx: HashMap<&'static str, usize>, // role → index into the perf group
    groups: ThreadGroups,
    started: Option<Instant>,
}

impl RawPmuCollector {
    pub fn new() -> Self {
        let vendor = cpu::detect().vendor;
        let mut role_idx = HashMap::new();
        let mut spec: Vec<EventCfg> = Vec::new();
        if let Some(db) = crate::pmudb::detect() {
            for &(role, name) in policy(vendor) {
                if let Some((config, etype)) = crate::pmudb::resolve_config_in(&db, name) {
                    role_idx.insert(role, spec.len());
                    spec.push(EventCfg { etype, config });
                }
            }
        }
        Self { vendor, role_idx, groups: ThreadGroups::new(vec![spec]), started: None }
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
        let role_idx = &self.role_idx;
        let get = |role: &str| -> Option<f64> {
            role_idx.get(role).and_then(|&i| sums.get(i).copied().flatten())
        };

        let mut out = Vec::new();
        match self.vendor {
            Vendor::Amd => {
                if let (Some(flops), true) = (get("fp"), elapsed > 0.0) {
                    out.push(gflops_metric(flops, elapsed));
                }
                for (role, key, label) in [
                    ("fills_all", "mem_fills_all", "Demand fills (all sources)"),
                    ("fills_dram", "mem_fills_dram", "Demand fills from DRAM"),
                    ("fills_remote", "mem_fills_remote", "Demand fills from remote node"),
                ] {
                    if let Some(v) = get(role) {
                        out.push(int_metric(key, label, v));
                    }
                }
            }
            Vendor::Intel => {
                // weights = double-precision elements per op.
                let scalar = get("fp_scalar").unwrap_or(0.0);
                let vector = get("fp_128").unwrap_or(0.0) * 2.0
                    + get("fp_256").unwrap_or(0.0) * 4.0
                    + get("fp_512").unwrap_or(0.0) * 8.0;
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
            Vendor::Arm => {
                // FP element-ops already count per element (NEON/SVE), so summing
                // FP_SCALE + FP_FIXED gives a width-aware FLOP proxy.
                let flops = get("fp_scale").unwrap_or(0.0) + get("fp_fixed").unwrap_or(0.0);
                if flops > 0.0 && elapsed > 0.0 {
                    out.push(gflops_metric(flops, elapsed));
                }
                // last-level read misses ≈ demand fills from DRAM; derive turns
                // this into DRAM bandwidth (× cache line) + the memory-bound model.
                if let Some(v) = get("fills_dram") {
                    out.push(int_metric("mem_fills_dram", "Demand fills from DRAM", v));
                }
            }
            Vendor::Other => {}
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

fn int_metric(key: &'static str, label: &str, v: f64) -> Metric {
    Metric { key, label: label.into(), value: MetricValue::Int { value: v as i64, unit: "" } }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The data-driven AMD policy must reproduce the historical hand-coded
    /// encodings (FP 0x0F03; demand-fill umasks all=0xFF, dram=0x48, remote=0x50
    /// over event 0x43). Runs only on an AMD host with sysfs (else skipped) —
    /// a guard that the rename from raw codes to event names didn't drift.
    #[test]
    fn raw_pmu_policy_resolves() {
        let Some(db) = crate::pmudb::detect() else { return };
        if cpu::detect().vendor != Vendor::Amd {
            return;
        }
        let expect = [
            ("fp_ret_sse_avx_ops.all", 0x0f03u64),
            ("ls_dmnd_fills_from_sys.all", 0xff43),
            ("ls_dmnd_fills_from_sys.dram_io_all", 0x4843),
            ("ls_dmnd_fills_from_sys.far_all", 0x5043),
        ];
        for (name, cfg) in expect {
            if let Some((got, _ty)) = crate::pmudb::resolve_config_in(&db, name) {
                assert_eq!(got, cfg, "{name} resolved to 0x{got:x}, expected 0x{cfg:x}");
            }
        }
    }
}
