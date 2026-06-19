//! Derivation engine: turns raw collected metrics into APS-style derived
//! metrics, independent of how the raw data was gathered.
//!
//! This is the layer that makes the profiler portable — backends emit raw,
//! vendor-specific counts; the stable derived metrics are computed here from
//! whatever raw inputs are present. Each derived metric is emitted only when
//! its inputs exist and denominators are non-zero, so partial data (e.g. no
//! PMU access) still yields a partial but correct report.
//!
//! Fidelity note: CPI/IPC, cache-miss rate, MPKI and branch-mispredict rate
//! are computed directly from measured counts and are exact. The memory-bound
//! percentage is an *estimate* — real Intel APS measures pipeline-slot stalls
//! via vendor-specific events; with only generic counters we model it from LLC
//! misses. Vectorization % and GFLOPS likewise require vendor-specific FP/SIMD
//! events and are intentionally not fabricated here.

use crate::metric::{Metric, MetricValue, Snapshot};

/// Approximate cycles lost per last-level-cache miss that reaches DRAM. Only
/// feeds the *estimated* memory-bound percentage. A real implementation reads
/// measured stall counters; this constant is a deliberately conservative model.
const LLC_MISS_PENALTY_CYCLES: f64 = 130.0;

/// Approximate cycles a demand DRAM fill stalls the pipeline. Used for the
/// memory-bound % when measured DRAM-fill counts are available (AMD), which is
/// a better-grounded signal than counting all last-level-cache misses.
const DRAM_FILL_PENALTY_CYCLES: f64 = 300.0;

/// Minimum demand fills before fill-source *ratios* are trustworthy (avoids
/// dividing a tiny number by a tiny number for non-memory workloads).
const MIN_FILLS_FOR_RATIO: f64 = 10_000.0;

/// Append derived metrics to `snapshot`, in place.
pub fn derive(snapshot: &mut Snapshot) {
    let insns = snapshot.numeric("hw_instructions");
    let cycles = snapshot.numeric("hw_cpu_cycles");
    let cache_refs = snapshot.numeric("hw_cache_references");
    let cache_miss = snapshot.numeric("hw_cache_misses");
    let branches = snapshot.numeric("hw_branch_instructions");
    let branch_miss = snapshot.numeric("hw_branch_misses");
    // Measured demand-fill data sources (AMD ls_dmnd_fills_from_sys).
    let fills_all = snapshot.numeric("mem_fills_all");
    let fills_dram = snapshot.numeric("mem_fills_dram");
    let fills_remote = snapshot.numeric("mem_fills_remote");

    let mut derived = Vec::new();

    if let (Some(i), Some(c)) = (insns, cycles) {
        if i > 0.0 {
            derived.push(float("cpi", "CPI (cycles/instruction)", c / i, ""));
        }
        if c > 0.0 {
            derived.push(float("ipc", "IPC (instructions/cycle)", i / c, ""));
        }
    }
    if let (Some(refs), Some(miss)) = (cache_refs, cache_miss) {
        if refs > 0.0 {
            derived.push(pct("cache_miss_rate", "Cache miss rate", miss / refs * 100.0));
        }
    }
    if let (Some(i), Some(miss)) = (insns, cache_miss) {
        if i > 0.0 {
            derived.push(float("llc_mpki", "LLC misses / 1K instr", miss / i * 1000.0, "MPKI"));
        }
    }
    if let (Some(b), Some(bm)) = (branches, branch_miss) {
        if b > 0.0 {
            derived.push(pct("branch_mispredict_rate", "Branch mispredict rate", bm / b * 100.0));
        }
    }
    // Measured memory hierarchy (AMD demand-fill data sources).
    if let (Some(i), Some(dram)) = (insns, fills_dram) {
        if i > 0.0 {
            derived.push(float("dram_dpki", "DRAM fills / 1K instr", dram / i * 1000.0, "DPKI"));
        }
    }
    if let (Some(all), Some(dram)) = (fills_all, fills_dram) {
        if all >= MIN_FILLS_FOR_RATIO {
            derived.push(pct("dram_bound_pct", "DRAM-bound (of demand fills)", dram / all * 100.0));
        }
    }
    if let (Some(all), Some(remote)) = (fills_all, fills_remote) {
        if all >= MIN_FILLS_FOR_RATIO {
            derived.push(pct("numa_remote_pct", "NUMA remote access", remote / all * 100.0));
        }
    }

    // Memory bound: prefer measured DRAM fills × DRAM latency; otherwise fall
    // back to the all-LLC-miss penalty estimate.
    if let (Some(c), Some(dram)) = (cycles, fills_dram) {
        if c > 0.0 {
            let mb = (dram * DRAM_FILL_PENALTY_CYCLES / c * 100.0).clamp(0.0, 100.0);
            derived.push(pct("memory_bound", "Memory bound", mb));
        }
    } else if let (Some(c), Some(miss)) = (cycles, cache_miss) {
        if c > 0.0 {
            let est = (miss * LLC_MISS_PENALTY_CYCLES / c * 100.0).clamp(0.0, 100.0);
            derived.push(pct("memory_bound_est", "Memory bound (est.)", est));
        }
    }

    snapshot.extend(derived);
}

fn float(key: &'static str, label: &str, value: f64, unit: &'static str) -> Metric {
    Metric { key, label: label.into(), value: MetricValue::Float { value, unit } }
}

fn pct(key: &'static str, label: &str, value: f64) -> Metric {
    Metric { key, label: label.into(), value: MetricValue::Percent(value) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(key: &'static str, value: i64) -> Metric {
        Metric { key, label: key.into(), value: MetricValue::Int { value, unit: "" } }
    }

    #[test]
    fn derives_cpi_ipc_and_rates() {
        let mut s = Snapshot::default();
        s.push(raw("hw_instructions", 2_000_000));
        s.push(raw("hw_cpu_cycles", 1_000_000));
        s.push(raw("hw_cache_references", 1000));
        s.push(raw("hw_cache_misses", 250));
        derive(&mut s);

        assert!((s.numeric("ipc").unwrap() - 2.0).abs() < 1e-9);
        assert!((s.numeric("cpi").unwrap() - 0.5).abs() < 1e-9);
        assert!((s.numeric("cache_miss_rate").unwrap() - 25.0).abs() < 1e-9);
    }

    #[test]
    fn skips_derived_metrics_without_inputs() {
        // Only OS metrics present — no counters, so no derived counter metrics.
        let mut s = Snapshot::default();
        s.push(Metric {
            key: "peak_rss",
            label: "rss".into(),
            value: MetricValue::Bytes(1024),
        });
        derive(&mut s);
        assert!(s.numeric("cpi").is_none());
        assert!(s.numeric("ipc").is_none());
    }

    #[test]
    fn memory_bound_estimate_is_clamped() {
        let mut s = Snapshot::default();
        s.push(raw("hw_cpu_cycles", 1_000_000));
        s.push(raw("hw_cache_misses", 1_000_000)); // absurdly high → clamps to 100%
        derive(&mut s);
        assert_eq!(s.numeric("memory_bound_est").unwrap(), 100.0);
    }
}
