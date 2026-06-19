//! Thread imbalance via `/proc/<pid>/task/<tid>/stat`.
//!
//! A runtime-agnostic alternative to OMPT: sample each thread's accumulated
//! CPU time and, at the end, measure how unevenly work was spread across the
//! threads that did meaningful work. Applies to OpenMP, pthreads, TBB — any
//! threading model — without instrumenting the runtime.
//!
//! This is a heuristic: it cannot isolate individual parallel regions the way
//! OMPT can, so it reflects whole-run thread balance. Threads doing <10% of
//! the busiest thread's work are treated as coordinators/idle and excluded.

use std::collections::HashMap;

use anyhow::Result;
use uaps_core::{Collector, Metric, MetricValue, Target};

pub struct ThreadCollector {
    pid: u32,
    clk_tck: u64,
    /// tid -> most recent (utime+stime) in clock ticks.
    last: HashMap<i32, u64>,
}

impl ThreadCollector {
    pub fn new() -> Self {
        let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        Self {
            pid: 0,
            clk_tck: if clk_tck > 0 { clk_tck as u64 } else { 100 },
            last: HashMap::new(),
        }
    }

    fn sample_threads(&mut self) {
        let dir = format!("/proc/{}/task", self.pid);
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let tid: i32 = match entry.file_name().to_string_lossy().parse() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let stat = match std::fs::read_to_string(entry.path().join("stat")) {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Fields after the final ')' start at `state` (field 3); utime is
            // field 14 (index 11), stime field 15 (index 12).
            if let Some(pos) = stat.rfind(')') {
                let f: Vec<&str> = stat[pos + 1..].split_whitespace().collect();
                if let (Some(u), Some(s)) = (f.get(11), f.get(12)) {
                    if let (Ok(u), Ok(s)) = (u.parse::<u64>(), s.parse::<u64>()) {
                        self.last.insert(tid, u + s);
                    }
                }
            }
        }
    }
}

impl Default for ThreadCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for ThreadCollector {
    fn name(&self) -> &'static str {
        "threads"
    }

    fn start(&mut self, target: &Target) -> Result<()> {
        self.pid = target.pid;
        self.sample_threads();
        Ok(())
    }

    fn sample(&mut self) -> Result<()> {
        self.sample_threads();
        Ok(())
    }

    fn finish(&mut self) -> Result<Vec<Metric>> {
        // Per-thread CPU seconds.
        let mut cpu: Vec<f64> = self
            .last
            .values()
            .map(|ticks| *ticks as f64 / self.clk_tck as f64)
            .collect();
        cpu.retain(|c| *c > 0.0);
        if cpu.len() < 2 {
            return Ok(Vec::new()); // not a meaningfully threaded run
        }

        let max = cpu.iter().cloned().fold(0.0_f64, f64::max);
        // Active workers: threads doing at least 10% of the busiest thread.
        let active: Vec<f64> = cpu.into_iter().filter(|c| *c >= 0.1 * max).collect();
        let mean = active.iter().sum::<f64>() / active.len() as f64;
        let imbalance = if max > 0.0 { (max - mean) / max * 100.0 } else { 0.0 };

        Ok(vec![
            Metric {
                key: "active_threads",
                label: "Active worker threads".into(),
                value: MetricValue::Int { value: active.len() as i64, unit: "" },
            },
            Metric {
                key: "thread_imbalance_pct",
                label: "Thread imbalance".into(),
                value: MetricValue::Percent(imbalance),
            },
        ])
    }
}
