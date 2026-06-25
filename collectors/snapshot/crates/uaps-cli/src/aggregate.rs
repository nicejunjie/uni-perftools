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
/// (source_key, imbalance_key, label, floor). `floor` is the value below which the
/// metric is counter noise everywhere — reporting imbalance there is meaningless
/// (a ~zero max makes (max-avg)/max swing to ~100% on rounding dust), so we skip it.
const IMBALANCE: &[(&str, &str, &str, f64)] = &[
    ("elapsed_time", "elapsed_imbalance_pct", "Wall-time imbalance (across ranks)", 1e-3),
    ("cpu_time", "cpu_time_imbalance_pct", "CPU-time imbalance (across ranks)", 1e-3),
    ("gflops", "gflops_imbalance_pct", "FP-throughput imbalance (across ranks)", 0.1),
    ("ipc", "ipc_imbalance_pct", "IPC imbalance (across ranks)", 0.05),
    ("memory_bound", "memory_bound_imbalance_pct", "Memory-bound imbalance (across ranks)", 1.0),
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

/// One parsed rank file: its metrics plus the node it ran on (host/CPU-model tags,
/// absent in pre-tagging snapshots → `None`).
struct RankFile {
    metrics: RankMap,
    host: Option<String>,
    arch: Option<String>,
}

fn parse_rank(path: &Path) -> Option<RankFile> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let host = v.get("host").and_then(|x| x.as_str()).map(str::to_string);
    let arch = v.get("arch").and_then(|x| x.as_str()).map(str::to_string);
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
    Some(RankFile { metrics: m, host, arch })
}

/// Read every `snap.<rank>.json` in `dir`, reduce to one job-level snapshot, and
/// return it with the number of ranks aggregated.
pub fn aggregate(dir: &Path) -> Result<(Snapshot, usize)> {
    let mut ranks: Vec<RankMap> = Vec::new();
    // (host, arch) per rank — for per-node participation + mixed-arch roofline warning.
    let mut nodes: Vec<(Option<String>, Option<String>)> = Vec::new();
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
            if let Some(rf) = parse_rank(&p) {
                nodes.push((rf.host, rf.arch));
                ranks.push(rf.metrics);
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
    // `gpu_offload` is a per-rank boolean (1 = this rank drove a GPU); MAX makes the
    // aggregate flag the job whenever ANY rank offloaded to a GPU.
    const MAX_KEYS: &[&str] = &["elapsed_time", "max_threads", "mpi_world_size", "gpu_offload"];

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
    // (max-avg)/max convention. Skipped when the metric is ~zero everywhere (below
    // its floor) — that "imbalance" would be counter noise, not real work skew.
    for (src, imb_key, label, floor) in IMBALANCE {
        let vals: Vec<f64> = ranks.iter().filter_map(|r| r.get(*src)).map(|m| m.value).collect();
        if let Some(imb) = imbalance_pct(&vals, n, *floor) {
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

    // Detect PARTIAL / MISSING vendor HWPC: ranks that reported but have no vendor
    // counters (FP/roofline/top-down) — summed gflops/DRAM/etc. then silently
    // undercount those ranks. Almost always the pmu-events DB wasn't found on some
    // nodes (a staged binary without its DB), or perf is disabled there.
    let hwpc_ranks = ranks.iter().filter(|r| r.contains_key("gflops")).count();
    if let Some(w) = partial_hwpc_warning(hwpc_ranks, n) {
        eprintln!("{w}");
    }

    // GPU offload: a CPU-only roofline misrepresents a GPU-offloaded job — warn loudly
    // (the renderer suppresses the roofline itself on this same `gpu_offload` flag).
    let gpu_ranks = ranks.iter().filter(|r| r.contains_key("gpu_offload")).count();
    if let Some(w) = gpu_offload_warning(gpu_ranks, n) {
        eprintln!("{w}");
    }

    // Per-node participation + mixed-arch roofline caveat (only meaningful if some
    // rank actually produced vendor HWPC / a roofline point).
    for line in node_participation(&nodes, hwpc_ranks > 0) {
        eprintln!("{line}");
    }
    let _ = dir; // retained for the file-based explicit (--rank-dir) path

    Ok((agg, n))
}

/// Diagnostics about which nodes/CPU-models the ranks ran on. Emits:
///   - an info line when the ranks span more than one node (so a multi-node run is
///     visibly multi-node, with the per-arch rank split);
///   - a WARNING when they span more than one CPU model AND a roofline exists — the
///     aggregated roofline/GFLOPS/top-down then mix heterogeneous FLOP+bandwidth
///     ceilings, so the single job-level point is not physically meaningful.
/// Empty when every rank is one node+arch, or the snapshots predate node tagging
/// (all `None`). Pure + testable; the caller prints each line.
fn node_participation(
    nodes: &[(Option<String>, Option<String>)],
    has_roofline: bool,
) -> Vec<String> {
    use std::collections::{BTreeMap, BTreeSet};
    let hosts: BTreeSet<&str> = nodes.iter().filter_map(|(h, _)| h.as_deref()).collect();
    let mut arch_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, a) in nodes {
        if let Some(a) = a.as_deref() {
            *arch_counts.entry(a).or_insert(0) += 1;
        }
    }
    let mut out = Vec::new();
    if hosts.len() > 1 {
        let split: Vec<String> =
            arch_counts.iter().map(|(a, c)| format!("{a} ({c} ranks)")).collect();
        out.push(format!(
            "uaps: ranks span {} nodes, {} CPU model(s): {}",
            hosts.len(),
            arch_counts.len(),
            split.join(", ")
        ));
    }
    if arch_counts.len() > 1 && has_roofline {
        out.push(format!(
            "uaps: WARNING: ranks span {} different CPU models ({}) — the aggregated roofline / \
             GFLOPS / top-down MIX heterogeneous FLOP+bandwidth ceilings, so the single job-level \
             roofline point is not physically meaningful. Group ranks by node type and compare \
             per-arch instead.",
            arch_counts.len(),
            arch_counts.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    out
}

/// Warning when vendor HWPC is missing on some/all ranks (the rest contribute no
/// FP/roofline/top-down, so the job totals undercount). `None` when every rank has
/// it. Pure + testable; the caller prints it.
fn partial_hwpc_warning(hwpc_ranks: usize, n: usize) -> Option<String> {
    if n == 0 || hwpc_ranks >= n {
        return None;
    }
    if hwpc_ranks == 0 {
        Some(format!(
            "uaps: WARNING: no vendor HW counters on ANY of the {n} ranks (FP/roofline/top-down \
             absent). The pmu-events DB was not found, or perf is disabled. Stage the pmu-events \
             tree alongside the uaps binary (or set UAPS_PMU_EVENTS), and ensure \
             perf_event_paranoid<=1."
        ))
    } else {
        Some(format!(
            "uaps: WARNING: vendor HW counters present on only {hwpc_ranks} of {n} ranks — the \
             other {} contribute NO FP/roofline/top-down, so those job totals UNDERCOUNT. Likely \
             the pmu-events DB is missing on some nodes (a staged binary without its DB): stage \
             pmu-events alongside the binary, or set UAPS_PMU_EVENTS.",
            n - hwpc_ranks
        ))
    }
}

/// Per-rank imbalance `(max-avg)/max` as a percent (0-100), averaging over ALL `n`
/// ranks so a rank that did none of this work still counts. `None` when fewer than 2
/// ranks reported the metric, or its max is below `floor` (effectively zero
/// everywhere — the ratio would otherwise amplify counter dust into a bogus ~100%).
/// Pure + testable.
fn imbalance_pct(vals: &[f64], n: usize, floor: f64) -> Option<f64> {
    if vals.len() < 2 {
        return None;
    }
    let mx = vals.iter().cloned().fold(f64::MIN, f64::max);
    if mx < floor {
        return None;
    }
    let avg = vals.iter().sum::<f64>() / n as f64;
    Some(((mx - avg) / mx * 100.0).max(0.0))
}

/// Warning when GPU offload was detected on some/all ranks. uaps reads only CPU
/// counters, so for a GPU-offloaded job the FP/roofline numbers reflect CPU work
/// alone and miss all device compute — hence the roofline is suppressed. `None`
/// when no rank used a GPU. Pure + testable; the caller prints it.
fn gpu_offload_warning(gpu_ranks: usize, n: usize) -> Option<String> {
    if gpu_ranks == 0 || n == 0 {
        return None;
    }
    let scope = if gpu_ranks == n {
        format!("all {n} ranks")
    } else {
        format!("{gpu_ranks} of {n} ranks")
    };
    Some(format!(
        "uaps: WARNING: GPU offload detected on {scope} — uaps measures only CPU counters, so the \
         FP / GFLOPS / roofline numbers reflect CPU work alone and MISS all GPU compute. The \
         whole-program roofline is SUPPRESSED (a CPU-only roofline would misrepresent a \
         GPU-offloaded job). Profile device kernels with a GPU tool (nsight / rocprof / VTune)."
    ))
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
    fn imbalance_pct_floors_out_near_zero_noise() {
        // real skew: gflops [10, 30] over 2 ranks → (30-20)/30 = 33.3%
        let v = imbalance_pct(&[10.0, 30.0], 2, 0.1).expect("real skew");
        assert!((v - 100.0 / 3.0).abs() < 1e-6, "{v}");
        // counter dust: both ranks ~0 GFLOP/s, one a hair higher → must NOT report
        // a huge imbalance. Below the 0.1 GFLOP/s floor → None.
        assert!(imbalance_pct(&[0.0008, 0.0011], 2, 0.1).is_none(), "near-zero noise suppressed");
        // only one rank reported the metric → no cross-rank imbalance
        assert!(imbalance_pct(&[42.0], 2, 0.1).is_none());
        // a rank that did none of the work still counts toward the average (sum/n):
        // vals [0, 4] over n=2 → avg=2, (4-2)/4 = 50%
        let v2 = imbalance_pct(&[0.0, 4.0], 2, 0.1).expect("two ranks, one idle");
        assert!((v2 - 50.0).abs() < 1e-9, "{v2}");
    }

    #[test]
    fn gpu_offload_warning_fires_when_any_rank_used_a_gpu() {
        assert!(gpu_offload_warning(0, 4).is_none(), "no GPU rank → silent");
        assert!(gpu_offload_warning(0, 0).is_none());
        let all = gpu_offload_warning(4, 4).expect("should warn");
        assert!(all.contains("all 4 ranks") && all.contains("SUPPRESSED"), "{all}");
        let some = gpu_offload_warning(1, 4).expect("should warn");
        assert!(some.contains("1 of 4 ranks"), "{some}");
    }

    #[test]
    fn node_participation_flags_multinode_and_mixed_arch() {
        let some = |h: &str, a: &str| (Some(h.to_string()), Some(a.to_string()));
        // single node, single arch → silent
        assert!(node_participation(&[some("n0", "amdzen5"), some("n0", "amdzen5")], true).is_empty());
        // two nodes, same arch → an info line, no mixed-arch warning
        let two = node_participation(&[some("z20", "amdzen5"), some("legion", "amdzen5")], true);
        assert_eq!(two.len(), 1, "{two:?}");
        assert!(two[0].contains("2 nodes"), "{:?}", two[0]);
        assert!(two[0].contains("1 CPU model"), "{:?}", two[0]);
        // two nodes, two arches, roofline present → info + mixed-arch WARNING
        let mix = node_participation(&[some("z20", "amdzen5"), some("vista", "arm/neoverse-v2")], true);
        assert_eq!(mix.len(), 2, "{mix:?}");
        assert!(mix[1].contains("WARNING") && mix[1].contains("different CPU models"), "{:?}", mix[1]);
        // mixed arch but NO roofline (no HWPC) → no roofline warning
        let no_rl = node_participation(&[some("z20", "amdzen5"), some("vista", "arm/neoverse-v2")], false);
        assert_eq!(no_rl.len(), 1, "no roofline → only the node-span info line: {no_rl:?}");
        // untagged (old) snapshots → silent
        assert!(node_participation(&[(None, None), (None, None)], true).is_empty());
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

    #[test]
    fn partial_hwpc_warning_flags_ranks_missing_vendor_counters() {
        assert!(partial_hwpc_warning(4, 4).is_none(), "complete → no warning");
        assert!(partial_hwpc_warning(0, 0).is_none());
        // some ranks have HWPC, some don't (staged binary without DB on some nodes)
        let w = partial_hwpc_warning(2, 4).expect("should warn");
        assert!(w.contains("only 2 of 4 ranks"), "{w}");
        assert!(w.contains("UNDERCOUNT"), "{w}");
        // none have it
        let w0 = partial_hwpc_warning(0, 4).expect("should warn");
        assert!(w0.contains("no vendor HW counters on ANY"), "{w0}");
    }
}
