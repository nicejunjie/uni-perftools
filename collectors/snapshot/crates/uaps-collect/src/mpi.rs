//! MPI metrics: aggregates the per-rank files written by the PMPI shim
//! (`shim/mpi/uaps_mpi.c`, LD_PRELOAD-ed by the CLI under `--mpi`).
//!
//! The shim writes `$UAPS_MPI_OUTDIR/rank_<n>.txt` at MPI_Finalize. This
//! collector reads them all at [`finish`], aggregates across ranks, then
//! removes the directory. If no rank files are present (MPI didn't run, or the
//! preload didn't reach the ranks) it reports nothing rather than failing.

use std::path::PathBuf;

use anyhow::Result;
use uaps_core::{Collector, Metric, MetricValue, Target};

#[derive(Default)]
struct RankStat {
    wall: f64,
    mpi_time: f64,
    /// (function name, seconds, calls)
    fns: Vec<(String, f64, i64)>,
}

pub struct MpiCollector {
    outdir: PathBuf,
}

impl MpiCollector {
    pub fn new(outdir: PathBuf) -> Self {
        Self { outdir }
    }

    fn read_ranks(&self) -> Vec<RankStat> {
        let mut ranks = Vec::new();
        let entries = match std::fs::read_dir(&self.outdir) {
            Ok(e) => e,
            Err(_) => return ranks,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("rank_") && name.ends_with(".txt")) {
                continue;
            }
            let body = match std::fs::read_to_string(entry.path()) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let mut rs = RankStat::default();
            for line in body.lines() {
                if let Some(v) = line.strip_prefix("wall=") {
                    rs.wall = v.parse().unwrap_or(0.0);
                } else if let Some(v) = line.strip_prefix("mpi_time=") {
                    rs.mpi_time = v.parse().unwrap_or(0.0);
                } else if let Some(v) = line.strip_prefix("fn=") {
                    // "fn=MPI_Send 0.0002 5000"
                    let mut it = v.split_whitespace();
                    if let (Some(n), Some(t), Some(c)) = (it.next(), it.next(), it.next()) {
                        rs.fns.push((
                            n.to_string(),
                            t.parse().unwrap_or(0.0),
                            c.parse().unwrap_or(0),
                        ));
                    }
                }
            }
            ranks.push(rs);
        }
        ranks
    }
}

impl Collector for MpiCollector {
    fn name(&self) -> &'static str {
        "mpi"
    }

    fn start(&mut self, _target: &Target) -> Result<()> {
        Ok(())
    }

    fn finish(&mut self) -> Result<Vec<Metric>> {
        let ranks = self.read_ranks();
        // Best-effort cleanup of the temp directory.
        let _ = std::fs::remove_dir_all(&self.outdir);

        let n = ranks.len();
        if n == 0 {
            eprintln!(
                "uaps: --mpi set but no per-rank data was produced \
                 (did LD_PRELOAD reach the ranks? for some launchers use `mpirun -x LD_PRELOAD`)"
            );
            return Ok(Vec::new());
        }

        let avg_wall = ranks.iter().map(|r| r.wall).sum::<f64>() / n as f64;
        let avg_mpi = ranks.iter().map(|r| r.mpi_time).sum::<f64>() / n as f64;
        let max_mpi = ranks.iter().map(|r| r.mpi_time).fold(0.0_f64, f64::max);

        let mpi_time_pct = if avg_wall > 0.0 { avg_mpi / avg_wall * 100.0 } else { 0.0 };
        // Imbalance = (max-avg)/max: the recoverable fraction of the busiest
        // rank's MPI time. Suite-wide definition (matches the profile collector
        // and core/contract); bounded 0-100.
        let imbalance_pct = if max_mpi > 0.0 {
            ((max_mpi - avg_mpi) / max_mpi * 100.0).max(0.0)
        } else {
            0.0
        };

        // Aggregate per-function time across ranks; surface the top 5 (APS-style).
        let mut fn_totals: Vec<(String, f64)> = Vec::new();
        for r in &ranks {
            for (name, t, _c) in &r.fns {
                match fn_totals.iter_mut().find(|(n, _)| n == name) {
                    Some(entry) => entry.1 += t,
                    None => fn_totals.push((name.clone(), *t)),
                }
            }
        }
        fn_totals.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let fn_sum: f64 = fn_totals.iter().map(|(_, t)| t).sum();

        let mut out = vec![
            Metric {
                key: "mpi_ranks",
                label: "MPI ranks".into(),
                value: MetricValue::Int { value: n as i64, unit: "" },
            },
            Metric {
                key: "mpi_time",
                label: "MPI time (avg/rank)".into(),
                value: MetricValue::Float { value: avg_mpi, unit: "s" },
            },
            Metric {
                key: "mpi_time_pct",
                label: "MPI time".into(),
                value: MetricValue::Percent(mpi_time_pct),
            },
            Metric {
                key: "mpi_imbalance_pct",
                label: "MPI imbalance".into(),
                value: MetricValue::Percent(imbalance_pct),
            },
        ];
        // Top 5 MPI functions by aggregate time (the keys mpi_top1..5 are listed
        // in the report's Parallelism section). Label carries name + %-of-MPI;
        // value is total seconds across ranks.
        const TOPKEYS: [&str; 5] = ["mpi_top1", "mpi_top2", "mpi_top3", "mpi_top4", "mpi_top5"];
        for (i, (name, t)) in fn_totals.iter().take(5).enumerate() {
            let pct = if fn_sum > 0.0 { t / fn_sum * 100.0 } else { 0.0 };
            out.push(Metric {
                key: TOPKEYS[i],
                label: format!("  {name}  ({pct:.0}% of MPI)"),
                value: MetricValue::Float { value: *t, unit: "s" },
            });
        }
        Ok(out)
    }
}
