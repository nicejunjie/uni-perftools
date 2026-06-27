//! `/proc/<pid>` sampling collector — the unprivileged baseline.
//!
//! Works on any Linux without special capabilities. Sampled periodically via
//! [`Collector::sample`] while the target runs; because `/proc/<pid>` vanishes
//! once the process exits, every sample is cached so [`finish`] can report the
//! last-seen values (and the running peaks).

use std::time::{Duration, Instant};

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
    /// Process state char from `/proc/<pid>/stat` (R/S/D/…). `D` = uninterruptible
    /// sleep = blocked on I/O — the NFS-aware I/O-wait signal (block-I/O delay
    /// accounting reads 0 for NFS, but the task still parks in D). NOTE: this is the
    /// thread-GROUP-LEADER's state; I/O done entirely on a worker/async thread while the
    /// leader runs/sleeps is missed (an under-count for that pattern). The process-wide
    /// `/proc/<pid>/io` byte volume still captures such I/O for the wrapper-note veto.
    state: u8,
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
    /// Node-wide busy ticks from /proc/stat at start / latest sample. Used for
    /// CPU utilization in system-wide mode (the per-pid stat would only see the
    /// launcher, not the ranks).
    node_base: u64,
    node_last: u64,
    /// Peak node memory in use (system-wide mode), bytes.
    node_peak_mem: u64,
    /// Node-wide block I/O (read,write) bytes at start / latest sample.
    node_disk_base: (u64, u64),
    node_disk_last: (u64, u64),
    /// Samples taken / samples where the process was in `D` (I/O-wait) state — gives a
    /// sampled I/O-wait FRACTION of wall time (NFS-aware; see `ProcSample::state`).
    samples: u64,
    blocked: u64,
}

/// Total non-idle CPU ticks across the whole node (sum over all CPUs), from the
/// aggregate `cpu` line of /proc/stat. `None` if it can't be parsed.
fn node_busy_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?; // "cpu  user nice system idle iowait irq softirq steal ..."
    let mut it = line.split_whitespace();
    if it.next()? != "cpu" {
        return None;
    }
    let vals: Vec<u64> = it.filter_map(|x| x.parse().ok()).collect();
    if vals.len() < 5 {
        return None;
    }
    let total: u64 = vals.iter().sum();
    Some(total.saturating_sub(vals[3] + vals[4])) // minus idle + iowait
}

/// Node memory in use (MemTotal - MemAvailable), in bytes, from /proc/meminfo.
/// `None` if either field is missing.
fn node_used_mem_bytes() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let (mut total, mut avail) = (None, None);
    for line in s.lines() {
        if let Some(r) = line.strip_prefix("MemTotal:") {
            total = Some(parse_kb_line(r));
        } else if let Some(r) = line.strip_prefix("MemAvailable:") {
            avail = Some(parse_kb_line(r));
        }
        if total.is_some() && avail.is_some() {
            break;
        }
    }
    Some(total?.saturating_sub(avail?))
}

/// Node-wide block I/O (read_bytes, write_bytes) summed over whole-disk devices
/// from /sys/block/<dev>/stat (sectors_read=field 2, sectors_written=field 6, 512 B
/// each). Top-level /sys/block entries are whole disks, so partitions aren't double
/// counted; pseudo devices (loop/ram/zram/sr) are skipped. `None` if none readable.
fn node_disk_bytes() -> Option<(u64, u64)> {
    let (mut rd, mut wr, mut any) = (0u64, 0u64, false);
    for entry in std::fs::read_dir("/sys/block").ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if ["loop", "ram", "zram", "sr", "fd", "dm-"].iter().any(|p| name.starts_with(p)) {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else { continue };
        let f: Vec<u64> = stat.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if f.len() >= 7 {
            rd += f[2];
            wr += f[6];
            any = true;
        }
    }
    any.then_some((rd * 512, wr * 512))
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
            node_base: 0,
            node_last: 0,
            node_peak_mem: 0,
            node_disk_base: (0, 0),
            node_disk_last: (0, 0),
            samples: 0,
            blocked: 0,
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
        // field 3 (state) is the first token after the ')'; index N-3 → index 0.
        s.state = f.first().and_then(|t| t.bytes().next()).unwrap_or(0);
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
        if crate::pmu::system_wide() {
            let b = node_busy_ticks().unwrap_or(0);
            self.node_base = b;
            self.node_last = b;
            self.node_peak_mem = self.node_peak_mem.max(node_used_mem_bytes().unwrap_or(0));
            let d = node_disk_bytes().unwrap_or((0, 0));
            self.node_disk_base = d;
            self.node_disk_last = d;
        }
        // Take an immediate baseline sample so very short runs still report.
        if let Some(s) = self.read_sample() {
            self.peak_rss = self.peak_rss.max(s.rss_bytes);
            self.max_threads = self.max_threads.max(s.threads);
            self.samples += 1;
            if s.state == b'D' {
                self.blocked += 1; // uninterruptible sleep = blocked on I/O
            }
            self.last = Some(s);
        }
        Ok(())
    }

    fn sample(&mut self) -> Result<()> {
        if crate::pmu::system_wide() {
            self.node_last = node_busy_ticks().unwrap_or(self.node_last);
            self.node_peak_mem = self.node_peak_mem.max(node_used_mem_bytes().unwrap_or(0));
            self.node_disk_last = node_disk_bytes().unwrap_or(self.node_disk_last);
        }
        if let Some(s) = self.read_sample() {
            self.peak_rss = self.peak_rss.max(s.rss_bytes);
            self.max_threads = self.max_threads.max(s.threads);
            self.samples += 1;
            if s.state == b'D' {
                self.blocked += 1; // uninterruptible sleep = blocked on I/O
            }
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
        // System-wide: CPU busy time is the node-wide delta from /proc/stat (counts
        // every rank); per-process: the launched process's own utime+stime.
        let cpu_ticks = if crate::pmu::system_wide() {
            self.node_last.saturating_sub(self.node_base)
        } else {
            last.cpu_ticks
        };
        let cpu_secs = cpu_ticks as f64 / self.clk_tck as f64;
        let cores_used = if wall > 0.0 { cpu_secs / wall } else { 0.0 };
        let core_pct = cores_used / self.ncpu as f64 * 100.0;

        // Memory: node-wide used memory (peak) in system-wide mode, else the
        // process's own peak RSS.
        let sysw = crate::pmu::system_wide();
        let (mem_label, mem_bytes) = if sysw {
            ("Node memory used (peak)", self.node_peak_mem)
        } else {
            ("Memory footprint (peak RSS)", self.peak_rss)
        };
        // Disk I/O: node-wide block-device delta in system-wide mode, else the
        // process's own /proc/<pid>/io block counters.
        let (disk_rd, disk_wr) = if sysw {
            (
                self.node_disk_last.0.saturating_sub(self.node_disk_base.0),
                self.node_disk_last.1.saturating_sub(self.node_disk_base.1),
            )
        } else {
            (last.disk_read, last.disk_write)
        };

        let mut out = vec![
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
                label: mem_label.into(),
                value: MetricValue::Bytes(mem_bytes),
            },
            Metric {
                key: "max_threads",
                label: "Threads (peak)".into(),
                value: MetricValue::Int { value: self.max_threads, unit: "" },
            },
            Metric {
                key: "disk_read",
                label: "Disk read".into(),
                value: MetricValue::Bytes(disk_rd),
            },
            Metric {
                key: "disk_write",
                label: "Disk write".into(),
                value: MetricValue::Bytes(disk_wr),
            },
        ];
        // Logical (rchar/wchar) I/O is inherently per-process — only meaningful in
        // per-process mode; omit it for a node-level snapshot.
        if !sysw {
            out.push(Metric {
                key: "io_read",
                label: "I/O read (logical)".into(),
                value: MetricValue::Bytes(last.io_rchar),
            });
            out.push(Metric {
                key: "io_write",
                label: "I/O write (logical)".into(),
                value: MetricValue::Bytes(last.io_wchar),
            });
            // Sampled I/O-wait time: fraction of samples the process was parked in `D`
            // (blocked on I/O) × wall. NFS-aware where block-I/O delay accounting is 0.
            // Coarse for very short runs (few samples); an estimate, labelled as such.
            if self.samples > 0 && self.blocked > 0 {
                let frac = self.blocked as f64 / self.samples as f64;
                out.push(Metric {
                    key: "io_wait",
                    label: "I/O wait (sampled est.)".into(),
                    value: MetricValue::Duration(Duration::from_secs_f64(frac * wall)),
                });
                // Sample count behind the estimate, so the report can show a confidence
                // range (binomial error ∝ 1/√N) and flag a small-N / low-confidence run.
                out.push(Metric {
                    key: "io_wait_samples",
                    label: "I/O-wait samples".into(),
                    value: MetricValue::Int { value: self.samples as i64, unit: "" },
                });
            }
        }

        // SMT enabled? Lets the report attribute the AMD top-down residual to
        // SMT contention (siblings stealing dispatch slots), or show NA when SMT
        // is off / the CPU has no SMT. 1 only when /sys reports it active.
        let smt_active = std::fs::read_to_string("/sys/devices/system/cpu/smt/active")
            .ok()
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
        out.push(Metric {
            key: "smt_active",
            label: "SMT active".into(),
            value: MetricValue::Int { value: smt_active as i64, unit: "" },
        });
        // Flag node-level scope so the report can skip per-process views (e.g.
        // thread parallel-efficiency) whose inputs are the launcher, not the ranks.
        out.push(Metric {
            key: "system_wide",
            label: "System-wide".into(),
            value: MetricValue::Int { value: sysw as i64, unit: "" },
        });

        Ok(out)
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
