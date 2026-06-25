//! Cross-rank aggregation of per-rank snapshots (APS-style).
//!
//! In per-rank mode each rank counts ONLY its own process on its OWN node and
//! writes `snap.<rank>.json`. This reduces those into one job-level [`Snapshot`]:
//!   - counts / throughput / volume  → SUM   (hw_*, mem_*, fp_*, gflops, bytes…)
//!   - wall time (`elapsed_time`)     → MAX   (the job's wall is the slowest rank)
//!   - percentages                    → MEAN  (per-rank %, averaged over ranks)
//!   - derived ratios (IPC, CPI, BW…) → RECOMPUTED from the summed raws via the
//!     same `derive()` the single-process path uses, so aggregate IPC is
//!     sum(instr)/sum(cycles), not a mean of per-rank ratios.
//!
//! It also adds a per-rank HW imbalance `(max-avg)/max` for the headline metrics —
//! the per-rank microarchitectural spread APS exposes and the old node-level
//! uaps could not produce.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};
use uaps_core::{Metric, MetricValue, Snapshot};

/// Ratio metrics produced by `uaps_core::derive` — excluded from the raw copy and
/// regenerated from the summed raws so they are exact at job level.
const DERIVED: &[&str] = &[
    "branch_mispredict_rate", "cache_miss_rate", "cpi", "cpu_freq_ghz",
    "dram_bandwidth_gbs", "dram_bound_pct", "dram_dpki", "dtlb_miss_rate",
    "dtlb_mpki", "ipc", "itlb_mpki", "llc_mpki", "memory_bound",
    "memory_bound_est", "numa_remote_pct",
];

/// Headline metrics that get a per-rank imbalance companion `(max-avg)/max`.
/// (source_key, imbalance_key, label)
const IMBALANCE: &[(&str, &str, &str)] = &[
    ("elapsed_time", "elapsed_imbalance_pct", "Wall-time imbalance (across ranks)"),
    ("cpu_time", "cpu_time_imbalance_pct", "CPU-time imbalance (across ranks)"),
    ("gflops", "gflops_imbalance_pct", "FP-throughput imbalance (across ranks)"),
    ("ipc", "ipc_imbalance_pct", "IPC imbalance (across ranks)"),
    ("memory_bound", "memory_bound_imbalance_pct", "Memory-bound imbalance (across ranks)"),
];

struct RankMetric {
    value: f64,
    unit: String,
    label: String,
}
type RankMap = BTreeMap<String, RankMetric>;

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// Reconstruct a typed value from the JSON (value, unit) so the renderer formats
/// it like a freshly-collected metric.
fn typed(value: f64, unit: &str) -> MetricValue {
    match unit {
        "s" => MetricValue::Duration(Duration::from_secs_f64(value.max(0.0))),
        "%" => MetricValue::Percent(value),
        "bytes" => MetricValue::Bytes(value.max(0.0) as u64),
        "" => {
            if value.is_finite() && value.fract() == 0.0 && value.abs() < 9e18 {
                MetricValue::Int { value: value as i64, unit: "" }
            } else {
                MetricValue::Float { value, unit: "" }
            }
        }
        u => MetricValue::Float { value, unit: leak(u) },
    }
}

fn parse_rank(path: &Path) -> Option<RankMap> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let arr = v.get("metrics")?.as_array()?;
    let mut m = RankMap::new();
    for it in arr {
        let key = match it.get("key").and_then(|x| x.as_str()) {
            Some(k) => k.to_string(),
            None => continue,
        };
        // value may be JSON null for non-finite — skip those rows.
        let value = match it.get("value").and_then(|x| x.as_f64()) {
            Some(x) if x.is_finite() => x,
            _ => continue,
        };
        let unit = it.get("unit").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let label = it.get("label").and_then(|x| x.as_str()).unwrap_or(&key).to_string();
        m.insert(key, RankMetric { value, unit, label });
    }
    Some(m)
}

/// Read every `snap.<rank>.json` in `dir`, reduce to one job-level snapshot, and
/// return it with the number of ranks aggregated.
pub fn aggregate(dir: &Path) -> Result<(Snapshot, usize)> {
    let mut ranks: Vec<RankMap> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        let name = match p.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // snap.<rank>.json, but not the aggregate snap.json itself.
        if name.starts_with("snap.") && name.ends_with(".json") && name != "snap.json" {
            // A rank killed mid-run leaves a truncated file — skip it, don't abort
            // the whole job's report (the normal failure mode at scale).
            if let Some(m) = parse_rank(&p) {
                ranks.push(m);
            }
        }
    }
    if ranks.is_empty() {
        bail!("no readable per-rank snapshots (snap.<rank>.json) in {}", dir.display());
    }
    let n = ranks.len();

    // Union of keys, remembering each key's (unit, label) from its first sighting.
    let mut keys: BTreeMap<String, (String, String)> = BTreeMap::new();
    for r in &ranks {
        for (k, rm) in r {
            keys.entry(k.clone())
                .or_insert_with(|| (rm.unit.clone(), rm.label.clone()));
        }
    }

    // Keys that are per-rank representative, not additive: take the MAX across
    // ranks (job wall = slowest rank; threads/rank = busiest rank; world size is
    // identical on every rank), not the sum.
    const MAX_KEYS: &[&str] = &["elapsed_time", "max_threads", "mpi_world_size"];

    let mut agg = Snapshot::default();
    // Rank count is always available (the imbalance denominator and the report's
    // "ranks N"); emit it up front so it survives even when no MPI shim ran.
    agg.push(Metric {
        key: "nranks",
        label: "MPI ranks".into(),
        value: MetricValue::Int { value: n as i64, unit: "" },
    });
    for (k, (unit, label)) in &keys {
        if DERIVED.contains(&k.as_str()) {
            continue; // regenerated from summed raws below
        }
        let vals: Vec<f64> = ranks.iter().filter_map(|r| r.get(k)).map(|m| m.value).collect();
        if vals.is_empty() {
            continue;
        }
        let reduced = if MAX_KEYS.contains(&k.as_str()) {
            vals.iter().cloned().fold(f64::MIN, f64::max)
        } else if unit == "%" {
            vals.iter().sum::<f64>() / vals.len() as f64
        } else {
            vals.iter().sum()
        };
        agg.push(Metric { key: leak(k), label: label.clone(), value: typed(reduced, unit) });
    }

    // Recompute the ratios (IPC, CPI, bandwidth, …) from the SUMMED raws — exact at
    // job level, not a mean of per-rank ratios.
    uaps_core::derive(&mut agg);

    // Per-rank HW imbalance for the headline metrics. Average over ALL ranks (a
    // rank that did no FP still counts toward FP imbalance), matching the suite's
    // (max-avg)/max convention.
    for (src, imb_key, label) in IMBALANCE {
        let vals: Vec<f64> = ranks.iter().filter_map(|r| r.get(*src)).map(|m| m.value).collect();
        if vals.len() < 2 {
            continue;
        }
        let sum: f64 = vals.iter().sum();
        let mx = vals.iter().cloned().fold(f64::MIN, f64::max);
        if mx > 0.0 {
            let avg = sum / n as f64;
            let imb = ((mx - avg) / mx * 100.0).max(0.0);
            agg.push(Metric {
                key: imb_key,
                label: (*label).to_string(),
                value: MetricValue::Percent(imb),
            });
        }
    }

    // Warn (don't silently undercount) if fewer ranks reported than the job had.
    let expected = ranks
        .iter()
        .filter_map(|r| r.get("mpi_world_size"))
        .map(|m| m.value as i64)
        .max();
    if let Some(w) = short_count_warning(n, expected) {
        eprintln!("{w}");
    }
    let _ = dir; // retained for the file-based explicit (--rank-dir) path

    Ok((agg, n))
}

/// Warning when only `found` of `expected` ranks reported back — the rest crashed/
/// were killed before finishing, or couldn't reach the launch-node TCP collector (a
/// firewall). `None` when every rank reported (or the world size is unknown), so a
/// complete run is never flagged. Pure + testable; the caller prints it.
fn short_count_warning(found: usize, expected: Option<i64>) -> Option<String> {
    let exp = expected? as usize;
    if exp <= found {
        return None;
    }
    Some(format!(
        "uaps: WARNING: aggregated {found} of {exp} ranks — {} never reported (crashed, or \
         could not reach the launch-node collector). The job-level metrics below reflect \
         only the {found} ranks that completed.",
        exp - found
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// A scratch dir under the crate (never /tmp), unique per test, auto-removed.
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            let id = SEQ.fetch_add(1, Ordering::Relaxed);
            let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join(format!("agg_test_{}_{}", std::process::id(), id));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Scratch(p)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_rank(dir: &Path, rank: usize, rows: &[(&str, f64, &str)]) {
        let body: Vec<String> = rows
            .iter()
            .map(|(k, v, u)| {
                format!(
                    "    {{\"key\": \"{k}\", \"label\": \"{k}\", \"value\": {v}, \"unit\": \"{u}\", \"display\": \"x\"}}"
                )
            })
            .collect();
        let json = format!("{{\n  \"metrics\": [\n{}\n  ]\n}}\n", body.join(",\n"));
        std::fs::write(dir.join(format!("snap.{rank}.json")), json).unwrap();
    }

    fn get(s: &Snapshot, key: &str) -> f64 {
        s.numeric(key).unwrap_or_else(|| panic!("missing metric {key}"))
    }

    #[test]
    fn reduces_sum_max_mean_and_recomputes_ratios() {
        let sc = Scratch::new();
        // rank 0: ipc would be 1.0 (100/100); rank 1: 3.0 (300/100).
        write_rank(&sc.0, 0, &[
            ("hw_instructions", 100.0, ""), ("hw_cpu_cycles", 100.0, ""),
            ("gflops", 10.0, "GFLOP/s"), ("elapsed_time", 2.0, "s"), ("cpu_time", 2.0, "s"),
            ("vectorization_pct", 40.0, "%"),
        ]);
        write_rank(&sc.0, 1, &[
            ("hw_instructions", 300.0, ""), ("hw_cpu_cycles", 100.0, ""),
            ("gflops", 30.0, "GFLOP/s"), ("elapsed_time", 4.0, "s"), ("cpu_time", 4.0, "s"),
            ("vectorization_pct", 60.0, "%"),
        ]);

        let (agg, n) = aggregate(&sc.0).unwrap();
        assert_eq!(n, 2);
        assert_eq!(get(&agg, "nranks"), 2.0);
        // SUM counts/throughput:
        assert!((get(&agg, "gflops") - 40.0).abs() < 1e-9, "gflops sum");
        assert!((get(&agg, "hw_instructions") - 400.0).abs() < 1e-9);
        assert!((get(&agg, "cpu_time") - 6.0).abs() < 1e-9, "cpu_time sum");
        // MAX wall:
        assert!((get(&agg, "elapsed_time") - 4.0).abs() < 1e-9, "elapsed max");
        // MEAN percentages:
        assert!((get(&agg, "vectorization_pct") - 50.0).abs() < 1e-9, "vec mean");
        // RECOMPUTED ratio from SUMMED raws: ipc = 400/200 = 2.0 (not mean(1,3)=2 here
        // by luck — use cpi to disambiguate: 200/400 = 0.5, while mean(1, 0.333) = 0.667).
        assert!((get(&agg, "ipc") - 2.0).abs() < 1e-9, "ipc from summed raws");
        assert!((get(&agg, "cpi") - 0.5).abs() < 1e-9, "cpi from summed raws (not mean)");
        // Imbalance (max-avg)/max over ranks:
        // gflops [10,30]: (30-20)/30 = 33.33%
        assert!((get(&agg, "gflops_imbalance_pct") - 100.0 / 3.0).abs() < 1e-6, "gflops imb");
        // cpu_time [2,4]: (4-3)/4 = 25%
        assert!((get(&agg, "cpu_time_imbalance_pct") - 25.0).abs() < 1e-9, "cpu_time imb");
    }

    #[test]
    fn skips_truncated_rank_files_without_aborting() {
        let sc = Scratch::new();
        write_rank(&sc.0, 0, &[("gflops", 12.0, "GFLOP/s"), ("elapsed_time", 1.0, "s")]);
        // a rank killed mid-write leaves a truncated file:
        std::fs::write(sc.0.join("snap.1.json"), "{\"metrics\": [ {\"key\": \"gfl").unwrap();
        // and a stray non-snap file must be ignored:
        std::fs::write(sc.0.join("rank_2.txt"), "rank=2\n").unwrap();

        let (agg, n) = aggregate(&sc.0).unwrap();
        assert_eq!(n, 1, "only the one good rank counted");
        assert!((get(&agg, "gflops") - 12.0).abs() < 1e-9);
    }

    #[test]
    fn empty_dir_is_an_error_not_a_panic() {
        let sc = Scratch::new();
        assert!(aggregate(&sc.0).is_err());
    }

    #[test]
    fn short_aggregate_surfaces_world_size_for_undercount_detection() {
        // Job had 4 ranks (each records mpi_world_size=4) but only 2 snaps reached
        // this (node-local) dir. The aggregate must carry the real world size (maxed,
        // not summed) next to the found count so the gap is visible + warned on.
        let sc = Scratch::new();
        write_rank(&sc.0, 0, &[("gflops", 5.0, "GFLOP/s"), ("mpi_world_size", 4.0, "")]);
        write_rank(&sc.0, 1, &[("gflops", 5.0, "GFLOP/s"), ("mpi_world_size", 4.0, "")]);
        let (agg, n) = aggregate(&sc.0).unwrap();
        assert_eq!(n, 2, "only 2 of the job's ranks reported back");
        assert_eq!(get(&agg, "nranks"), 2.0);
        assert_eq!(get(&agg, "mpi_world_size"), 4.0, "world size maxed, not summed (would be 8)");
    }

    #[test]
    fn short_count_warning_fires_only_when_ranks_are_missing() {
        // complete run (or unknown world size) → no warning
        assert!(short_count_warning(4, Some(4)).is_none());
        assert!(short_count_warning(8, None).is_none());
        assert!(short_count_warning(5, Some(4)).is_none()); // never negative
        // short run → an actionable warning naming the gap
        let w = short_count_warning(2, Some(4)).expect("should warn");
        assert!(w.contains("aggregated 2 of 4 ranks"), "{w}");
        assert!(w.contains("never reported"), "{w}");
    }
}
