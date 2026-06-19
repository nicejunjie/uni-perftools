//! `/proc/<pid>` sampling collector — the unprivileged baseline.
//!
//! Works on any Linux without special capabilities. Sampled periodically via
//! [`Collector::sample`] while the target runs; because `/proc/<pid>` vanishes
//! once the process exits, every sample is cached so [`finish`] can report the
//! last-seen values (and the running peaks).

use std::time::Instant;

use anyhow::Result;
use uaps_core::{Collector, Metric, MetricValue, Target};

/// One read of the per-process `/proc` files we care about.
#[derive(Debug, Clone, Copy, Default)]
struct ProcSample {
    /// utime + stime, in clock ticks.
    cpu_ticks: u64,
    rss_bytes: u64,
    threads: i64,
    /// `/proc/<pid>/io` read_bytes / write_bytes (actual block-layer I/O).
    disk_read: u64,
    disk_write: u64,
    /// `/proc/<pid>/io` rchar / wchar (logical bytes through read/write).
    io_rchar: u64,
    io_wchar: u64,
}

pub struct ProcCollector {
    pid: u32,
    started: Option<Instant>,
    clk_tck: u64,
    ncpu: u64,
    /// Most recent successful sample (used at finish, since /proc disappears).
    last: Option<ProcSample>,
    peak_rss: u64,
    max_threads: i64,
}

impl ProcCollector {
    pub fn new() -> Self {
        // SAFETY: sysconf with these standard names has no side effects.
        let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let ncpu = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        Self {
            pid: 0,
            started: None,
            clk_tck: if clk_tck > 0 { clk_tck as u64 } else { 100 },
            ncpu: if ncpu > 0 { ncpu as u64 } else { 1 },
            last: None,
            peak_rss: 0,
            max_threads: 0,
        }
    }

    fn read_sample(&self) -> Option<ProcSample> {
        let mut s = ProcSample::default();

        // /proc/<pid>/stat — comm (field 2) may contain spaces/parens, so we
        // parse everything after the final ')'. Fields then start at `state`
        // (field 3), i.e. field N is at index N-3.
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.pid)).ok()?;
        let after = &stat[stat.rfind(')')? + 1..];
        let f: Vec<&str> = after.split_whitespace().collect();
        let utime: u64 = f.get(11)?.parse().ok()?;
        let stime: u64 = f.get(12)?.parse().ok()?;
        s.cpu_ticks = utime + stime;
        s.threads = f.get(17)?.parse().unwrap_or(0);

        // /proc/<pid>/status — VmRSS (current resident set).
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", self.pid)) {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    s.rss_bytes = parse_kb_line(rest);
                }
            }
        }

        // /proc/<pid>/io — may be unreadable for foreign processes; best-effort.
        if let Ok(io) = std::fs::read_to_string(format!("/proc/{}/io", self.pid)) {
            for line in io.lines() {
                let (k, v) = match line.split_once(':') {
                    Some(kv) => kv,
                    None => continue,
                };
                let val: u64 = v.trim().parse().unwrap_or(0);
                match k {
                    "read_bytes" => s.disk_read = val,
                    "write_bytes" => s.disk_write = val,
                    "rchar" => s.io_rchar = val,
                    "wchar" => s.io_wchar = val,
                    _ => {}
                }
            }
        }

        Some(s)
    }
}

impl Default for ProcCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for ProcCollector {
    fn name(&self) -> &'static str {
        "proc"
    }

    fn start(&mut self, target: &Target) -> Result<()> {
        self.pid = target.pid;
        self.started = Some(Instant::now());
        // Take an immediate baseline sample so very short runs still report.
        if let Some(s) = self.read_sample() {
            self.peak_rss = self.peak_rss.max(s.rss_bytes);
            self.max_threads = self.max_threads.max(s.threads);
            self.last = Some(s);
        }
        Ok(())
    }

    fn sample(&mut self) -> Result<()> {
        if let Some(s) = self.read_sample() {
            self.peak_rss = self.peak_rss.max(s.rss_bytes);
            self.max_threads = self.max_threads.max(s.threads);
            self.last = Some(s);
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Vec<Metric>> {
        let last = match self.last {
            Some(s) => s,
            // Never managed to read /proc (e.g. instant exit): nothing to report.
            None => return Ok(Vec::new()),
        };
        let wall = self.started.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
        let cpu_secs = last.cpu_ticks as f64 / self.clk_tck as f64;
        let cores_used = if wall > 0.0 { cpu_secs / wall } else { 0.0 };
        let core_pct = cores_used / self.ncpu as f64 * 100.0;

        Ok(vec![
            Metric {
                key: "cpu_time",
                label: "CPU time".into(),
                value: MetricValue::Float { value: cpu_secs, unit: "s" },
            },
            Metric {
                key: "cpu_cores_used",
                label: "CPU utilization".into(),
                value: MetricValue::Float { value: cores_used, unit: "cores" },
            },
            Metric {
                key: "cpu_core_pct",
                label: "Physical core utilization".into(),
                value: MetricValue::Percent(core_pct),
            },
            Metric {
                key: "peak_rss",
                label: "Memory footprint (peak RSS)".into(),
                value: MetricValue::Bytes(self.peak_rss),
            },
            Metric {
                key: "max_threads",
                label: "Threads (peak)".into(),
                value: MetricValue::Int { value: self.max_threads, unit: "" },
            },
            Metric {
                key: "disk_read",
                label: "Disk read".into(),
                value: MetricValue::Bytes(last.disk_read),
            },
            Metric {
                key: "disk_write",
                label: "Disk write".into(),
                value: MetricValue::Bytes(last.disk_write),
            },
            Metric {
                key: "io_read",
                label: "I/O read (logical)".into(),
                value: MetricValue::Bytes(last.io_rchar),
            },
            Metric {
                key: "io_write",
                label: "I/O write (logical)".into(),
                value: MetricValue::Bytes(last.io_wchar),
            },
        ])
    }
}

/// Parse a `/proc/<pid>/status` value line like `   1234 kB` into bytes.
fn parse_kb_line(rest: &str) -> u64 {
    rest.split_whitespace()
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kb_lines() {
        assert_eq!(parse_kb_line("   2048 kB"), 2048 * 1024);
        assert_eq!(parse_kb_line("0 kB"), 0);
    }

    #[test]
    fn samples_self() {
        // Profiling our own pid should always yield a readable sample.
        let mut c = ProcCollector::new();
        let me = std::process::id();
        c.start(&Target { pid: me }).unwrap();
        c.sample().unwrap();
        let metrics = c.finish().unwrap();
        assert!(metrics.iter().any(|m| m.key == "peak_rss"));
        assert!(metrics.iter().any(|m| m.key == "max_threads"));
    }
}
