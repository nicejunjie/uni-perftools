//! Insight engine: interprets derived metrics into ranked, human-readable
//! findings — the "your application is X bound" headline plus advice that
//! Intel APS is known for. Rules fire only when the relevant metrics exist,
//! so a report degrades to fewer insights rather than wrong ones.

use crate::metric::Snapshot;

/// A single finding. The first element of [`insights`]'s result is the
/// headline bottleneck.
#[derive(Debug, Clone)]
pub struct Insight {
    pub headline: String,
    pub detail: String,
}

const MIB: f64 = 1024.0 * 1024.0;

/// Rank findings for a snapshot, most significant first.
pub fn insights(snapshot: &Snapshot) -> Vec<Insight> {
    let mut out = Vec::new();

    let mpki = snapshot.numeric("llc_mpki");
    let dram_dpki = snapshot.numeric("dram_dpki");
    let numa_remote = snapshot.numeric("numa_remote_pct");
    let ipc = snapshot.numeric("ipc");
    let bmr = snapshot.numeric("branch_mispredict_rate");
    let cores = snapshot.numeric("cpu_cores_used");
    let cpu_time = snapshot.numeric("cpu_time");
    let elapsed = snapshot.numeric("elapsed_time").unwrap_or(0.0);
    let disk = snapshot.numeric("disk_read").unwrap_or(0.0)
        + snapshot.numeric("disk_write").unwrap_or(0.0);
    let mpi_pct = snapshot.numeric("mpi_time_pct");
    let thread_imb = snapshot.numeric("thread_imbalance_pct");

    // MPI communication bound (HPC) — highest priority when present.
    if let Some(p) = mpi_pct {
        if p >= 15.0 {
            out.push(Insight {
                headline: "MPI-bound".into(),
                detail: format!(
                    "{p:.1}% of time spent in MPI. Reduce communication/wait: balance \
                     work across ranks, overlap comm with compute, or aggregate messages."
                ),
            });
        }
    }
    // Thread imbalance (OpenMP/pthreads) — only meaningful when multithreaded.
    if let Some(imb) = thread_imb {
        if imb >= 20.0 {
            out.push(Insight {
                headline: "Thread imbalance".into(),
                detail: format!(
                    "{imb:.1}% gap between the busiest thread and the average worker. \
                     Rebalance work (dynamic/guided OpenMP scheduling) or split hot threads."
                ),
            });
        }
    }
    // I/O bound: CPU idle most of wall time while moving real disk bytes.
    // Checked before memory: an I/O workload's kernel page-copies inflate
    // cache-miss counts, but a CPU-idle process is genuinely I/O-bound. A
    // truly memory-bound workload runs CPU-hot and fails this guard.
    if let Some(ct) = cpu_time {
        if elapsed > 0.0 && ct / elapsed < 0.5 && disk > 16.0 * MIB {
            out.push(Insight {
                headline: "I/O-bound".into(),
                detail: format!(
                    "CPU busy only {:.0}% of wall time while moving {:.0} MiB of disk I/O. \
                     Overlap I/O with compute, batch requests, or buffer more aggressively.",
                    ct / elapsed * 100.0,
                    disk / MIB
                ),
            });
        }
    }
    // Memory bound. Prefer the measured DRAM-fill rate (AMD) over generic
    // LLC misses when available — it counts only fills that actually hit DRAM.
    if let Some(d) = dram_dpki {
        if d >= 5.0 {
            out.push(Insight {
                headline: "Memory-bound".into(),
                detail: format!(
                    "{d:.1} DRAM fills per 1K instructions (measured). Improve data \
                     locality (cache blocking), shrink the working set, or prefetch."
                ),
            });
        }
    } else if let Some(m) = mpki {
        if m >= 5.0 {
            out.push(Insight {
                headline: "Memory-bound".into(),
                detail: format!(
                    "{m:.1} LLC misses per 1K instructions. Improve data locality \
                     (cache blocking), check NUMA placement, shrink the working set."
                ),
            });
        }
    }
    // NUMA: significant share of demand fills coming from a remote node.
    if let Some(r) = numa_remote {
        if r >= 15.0 {
            out.push(Insight {
                headline: "NUMA remote access".into(),
                detail: format!(
                    "{r:.1}% of demand fills came from a remote NUMA node. Pin threads \
                     and memory to the same node (numactl/first-touch allocation)."
                ),
            });
        }
    }
    // Frontend bound (instruction supply: i-cache/iTLB misses, decode, fetch).
    // Surfaced here because no other rule covers it; backend-bound cases are
    // already captured by the memory-bound rule above.
    if let Some(fe) = snapshot.numeric("topdown_frontend_pct") {
        if fe >= 25.0 {
            out.push(Insight {
                headline: "Frontend-bound".into(),
                detail: format!(
                    "{fe:.1}% of pipeline slots stalled on instruction supply. Reduce \
                     code footprint / I-cache pressure; check for excessive branching \
                     or large unrolled hot paths."
                ),
            });
        }
    }
    // Branch misprediction.
    if let Some(b) = bmr {
        if b >= 3.0 {
            out.push(Insight {
                headline: "Branch misprediction".into(),
                detail: format!(
                    "{b:.1}% of branches mispredicted. Make hot branches more predictable \
                     or restructure them to be branchless."
                ),
            });
        }
    }
    // Under-parallelized: little cache pressure, but using ~1 core for a while.
    // Skipped under MPI, where per-process /proc metrics describe only the
    // launcher, not the ranks (so "cores used" is not the workload's).
    if let (Some(c), Some(m)) = (cores, mpki) {
        if mpi_pct.is_none() && c < 1.5 && m < 5.0 && elapsed > 0.5 {
            out.push(Insight {
                headline: "Mostly single-threaded".into(),
                detail: format!(
                    "Averaged {c:.1} cores. If the work is parallelizable, threading \
                     could reduce wall-clock time substantially."
                ),
            });
        }
    }
    // Healthy compute-bound: only surface if nothing else fired.
    if out.is_empty() {
        if let (Some(i), Some(m)) = (ipc, mpki) {
            if i >= 1.5 && m < 1.0 {
                out.push(Insight {
                    headline: "Compute-bound".into(),
                    detail: format!(
                        "High IPC ({i:.2}) with low cache pressure — cores are well used. \
                         Look at vectorization (SIMD) and algorithmic improvements next."
                    ),
                });
            }
        }
    }

    out
}
