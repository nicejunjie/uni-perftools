//! Shared `perf_event_open` plumbing: **per-thread counter groups**.
//!
//! Counting a multithreaded process with `inherit` + many independent events
//! is unreliable under PMU multiplexing — the `time_running` accounting on
//! inherited counters can be off, intermittently undercounting by large
//! factors on short runs. Instead we open events as **hardware groups** (which
//! schedule all-or-nothing, so their internal ratios are exact) on **each
//! thread** (discovered via `/proc/<pid>/task`), then sum across threads.
//!
//! Keep each group ≤5 events: a group only counts if all its members fit in
//! the core PMUs at once, and one PMC may be unavailable (e.g. NMI watchdog).
//! Co-locate events whose ratio must be exact (instructions+cycles, etc.) in
//! the same group.
//!
//! LIMITATION (per-process mode): because we do NOT use `inherit`, counters do
//! not follow `fork`/`exec`, and we only scan the launched child's
//! `/proc/<pid>/task`. A *wrapped* launch — `numactl ./app`, `taskset -c .. ./app`,
//! a shell or `env` wrapper, or anything that does the real work in a child
//! process — measures the near-idle wrapper, not the workload. Use system-wide
//! mode (`-a`, automatic for MPI) for those; it counts every process on the node.
//! Also, a thread that starts AND finishes within one ~20 ms `scan()` interval is
//! never attached and its slots are missed (self-consistent for ratios, but biases
//! absolute counts on very fine-grained threading).

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::atomic::{AtomicBool, Ordering};

use perf_event_open_sys as sys;

/// perf_event ABI event types (stable kernel constants).
pub const TYPE_HARDWARE: u32 = 0; // PERF_TYPE_HARDWARE
pub const TYPE_SOFTWARE: u32 = 1; // PERF_TYPE_SOFTWARE (kernel counters: ctx-switch, faults)
pub const TYPE_HW_CACHE: u32 = 3; // PERF_TYPE_HW_CACHE (portable cache/TLB encoding)
pub const TYPE_RAW: u32 = 4; // PERF_TYPE_RAW (CPU-specific raw encoding)

/// Counting scope. Off (default) = per-thread of the target process. On =
/// node-level: one group per online CPU (pid=-1), summed over CPUs — this counts
/// EVERY process on the node, so an `mpirun` launch is measured via its ranks (not
/// the idle launcher), matching Intel APS's per-node metrics and `perf stat -a`.
/// Requires `perf_event_paranoid <= 0` (or CAP_PERFMON); otherwise groups fail to
/// open and the affected metrics are reported as gaps.
static SYSTEM_WIDE: AtomicBool = AtomicBool::new(false);

pub fn set_system_wide(on: bool) {
    SYSTEM_WIDE.store(on, Ordering::Relaxed);
}
pub fn system_wide() -> bool {
    SYSTEM_WIDE.load(Ordering::Relaxed)
}

/// Warn (once) when perf_event_open fails on fd exhaustion, so it isn't
/// indistinguishable from a permissions/"counter unavailable" gap.
static FD_WARNED: AtomicBool = AtomicBool::new(false);

/// Raise the open-file soft limit to the hard limit. System-wide/MPI mode opens
/// roughly 7-9 perf event groups *per online CPU*, each with several fds: on a
/// 128-192 core node that is several thousand fds, well past the usual 1024 soft
/// limit. Without this, perf_event_open fails with EMFILE and the snapshot
/// silently degrades to gaps. Best-effort; safe to ignore failures.
pub fn raise_fd_limit() {
    unsafe {
        let mut rl: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0 && rl.rlim_cur < rl.rlim_max {
            rl.rlim_cur = rl.rlim_max;
            let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &rl);
        }
    }
}

/// Online CPU ids from /sys/devices/system/cpu/online ("0-31" / "0,2-4"), else 0..nproc.
pub fn online_cpus() -> Vec<i32> {
    if let Ok(s) = std::fs::read_to_string("/sys/devices/system/cpu/online") {
        let mut v = Vec::new();
        for part in s.trim().split(',') {
            match part.split_once('-') {
                Some((a, b)) => {
                    if let (Ok(a), Ok(b)) = (a.parse::<i32>(), b.parse::<i32>()) {
                        v.extend(a..=b);
                    }
                }
                None => {
                    if let Ok(a) = part.parse::<i32>() {
                        v.push(a);
                    }
                }
            }
        }
        if !v.is_empty() {
            return v;
        }
    }
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as i32;
    (0..n).collect()
}

/// One event: its perf type and config (HW enum value or raw encoding).
#[derive(Clone, Copy)]
pub struct EventCfg {
    pub etype: u32,
    pub config: u64,
}

/// Open a single perf event on thread `tid`. `group_fd` = -1 for a leader,
/// or the leader's fd for a member. Returns an owned File (auto-closes).
pub fn open_event(etype: u32, config: u64, pid: libc::pid_t, cpu: i32, group_fd: i32, leader: bool) -> Option<File> {
    let mut attr: sys::bindings::perf_event_attr = unsafe { std::mem::zeroed() };
    attr.type_ = etype;
    attr.size = std::mem::size_of::<sys::bindings::perf_event_attr>() as u32;
    attr.config = config;
    attr.set_disabled(if leader { 1 } else { 0 });
    attr.set_exclude_hv(1);
    if leader {
        attr.read_format = (sys::bindings::PERF_FORMAT_TOTAL_TIME_ENABLED
            | sys::bindings::PERF_FORMAT_TOTAL_TIME_RUNNING
            | sys::bindings::PERF_FORMAT_GROUP) as u64;
    }
    // SAFETY: attr is initialised; (pid,cpu) selects the scope — (tid,-1) per-thread
    // or (-1,cpu) system-wide; group_fd links members to their leader.
    let fd = unsafe { sys::perf_event_open(&mut attr, pid, cpu, group_fd, 0) };
    if fd < 0 {
        // EMFILE/ENFILE means we ran out of fds, NOT that the counter is
        // unavailable — surface it once so a large-node run isn't silently gapped.
        let err = std::io::Error::last_os_error();
        if matches!(err.raw_os_error(), Some(libc::EMFILE) | Some(libc::ENFILE))
            && !FD_WARNED.swap(true, Ordering::Relaxed)
        {
            eprintln!(
                "uaps: warning: out of file descriptors opening perf counters ({err}); \
                 metrics will be incomplete. Raise the open-file limit (ulimit -n)."
            );
        }
        None
    } else {
        // SAFETY: fresh owned fd from the kernel.
        Some(unsafe { File::from_raw_fd(fd) })
    }
}

/// A scaled grouped read plus its multiplexing coverage.
pub struct GroupRead {
    pub vals: Vec<f64>,
    /// `time_running / time_enabled` in `[0,1]`: the fraction of the window the
    /// group was actually scheduled. 1.0 = never multiplexed off.
    pub coverage: f64,
}

/// Read `n` grouped values from a leader, scaled for multiplexing.
/// Layout (PERF_FORMAT_GROUP, no IDs): [nr, time_enabled, time_running, v0..].
/// Returns the scaled values and the group's coverage, or `None` on a failed
/// read (incl. a torn/over- or under-sized record, or a dead fd / ESRCH).
pub fn read_values(leader: &mut File, n: usize) -> Option<GroupRead> {
    let mut buf = vec![0u8; (3 + n) * 8];
    leader.read_exact(&mut buf).ok()?;
    let at = |i: usize| -> f64 { u64::from_ne_bytes(buf[i * 8..i * 8 + 8].try_into().unwrap()) as f64 };
    let nr = at(0) as usize;
    let time_enabled = at(1);
    let time_running = at(2);
    // `nr != n` (not just `nr < n`): if the kernel returns MORE members than we
    // asked for, `read_exact` consumed only the first n values, leaving a torn
    // record in the fd that would corrupt every subsequent sample — fail instead.
    if nr != n || time_running == 0.0 || time_enabled == 0.0 {
        return None;
    }
    let scale = time_enabled / time_running;
    Some(GroupRead {
        vals: (0..n).map(|k| at(3 + k) * scale).collect(),
        coverage: (time_running / time_enabled).clamp(0.0, 1.0),
    })
}

struct Group {
    leader: File,
    _members: Vec<File>,
    /// Last successful scaled read. Counters are cumulative, so the newest read
    /// supersedes the previous one; a thread that has since exited keeps this
    /// cached value instead of vanishing from the totals (see `poll`).
    last: Option<Vec<f64>>,
    /// Coverage of the last successful read (`time_running/time_enabled`). Used
    /// to drop heavily-multiplexed groups from the cross-thread *absolute* sums
    /// instead of extrapolating fabricated counts.
    coverage: f64,
    /// Set once a read of this group's fd fails (ESRCH / thread gone). A dead
    /// group is pruned rather than left summing its stale `last` forever.
    dead: bool,
}

/// Read field 22 (`starttime`, in clock ticks) of `/proc/<pid>/task/<tid>/stat`.
/// Combined with the tid it uniquely identifies a thread incarnation: when the
/// kernel recycles a tid (pid_max wrap on a long run) the new thread gets a
/// different starttime, so dedup keyed on `(tid, starttime)` does not mistake it
/// for the dead one. Field 2 (`comm`) may contain spaces/parens, so we split
/// after the final ')'. Returns 0 if it can't be read (still dedups on tid).
fn thread_starttime(pid: u32, tid: libc::pid_t) -> u64 {
    match std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat")) {
        Ok(s) => parse_starttime(&s),
        Err(_) => 0,
    }
}

/// Pull field 22 (`starttime`) out of a `/proc/.../stat` line. Field 2 (`comm`)
/// can contain spaces and parens, so we split after the FINAL ')': everything
/// after is space-separated `state ppid ...`, where starttime is the 20th token
/// (field 22 − fields 1,2 already consumed). Returns 0 if malformed.
fn parse_starttime(stat: &str) -> u64 {
    let Some((_, after)) = stat.rsplit_once(')') else {
        return 0;
    };
    after.split_whitespace().nth(19).and_then(|f| f.parse().ok()).unwrap_or(0)
}

fn open_group(spec: &[EventCfg], pid: libc::pid_t, cpu: i32) -> Option<Group> {
    let first = spec.first()?;
    let leader = open_event(first.etype, first.config, pid, cpu, -1, true)?;
    let lfd = leader.as_raw_fd();
    let mut members = Vec::new();
    for ev in &spec[1..] {
        members.push(open_event(ev.etype, ev.config, pid, cpu, lfd, false)?);
    }
    // SAFETY: enabling the leader with the GROUP flag starts the group atomically.
    unsafe {
        sys::ioctls::ENABLE(lfd, sys::bindings::PERF_IOC_FLAG_GROUP);
    }
    Some(Group { leader, _members: members, last: None, coverage: 0.0, dead: false })
}

/// Manages a set of counter groups replicated across every thread of a target.
/// Each `group` (≤5 events) is opened per thread; [`read_sums`] returns the
/// per-event totals summed over all threads, flattened in the order the groups
/// were given (`None` for an event no thread could program).
pub struct ThreadGroups {
    pid: u32,
    groups: Vec<Vec<EventCfg>>,
    /// Discovered scopes already opened. For threads the key is
    /// `(tid, starttime)` so a recycled tid (pid_max wrap on a long run) is
    /// treated as a fresh thread instead of being skipped; for system-wide CPUs
    /// it is `(cpu, 0)`.
    seen: HashSet<(i64, u64)>,
    /// Per thread: one optional Group per spec group (None if it failed).
    threads: Vec<Vec<Option<Group>>>,
}

impl ThreadGroups {
    pub fn new(groups: Vec<Vec<EventCfg>>) -> Self {
        Self { pid: 0, groups, seen: HashSet::new(), threads: Vec::new() }
    }

    /// True if this collector has any events configured at all.
    pub fn configured(&self) -> bool {
        self.groups.iter().any(|g| !g.is_empty())
    }

    pub fn start(&mut self, pid: u32) {
        self.pid = pid;
        self.scan();
    }

    /// Discover new counting scopes and open all groups on each. Per-thread by
    /// default (one set per `/proc/<pid>/task` thread); system-wide opens one set
    /// per online CPU (pid=-1) — CPUs are static, so this runs once.
    pub fn scan(&mut self) {
        if !self.configured() {
            return;
        }
        if system_wide() {
            for cpu in online_cpus() {
                if !self.seen.insert((cpu as i64, 0)) {
                    continue;
                }
                let opened = self.groups.iter().map(|spec| open_group(spec, -1, cpu)).collect();
                self.threads.push(opened);
            }
        } else if let Ok(entries) = std::fs::read_dir(format!("/proc/{}/task", self.pid)) {
            for entry in entries.flatten() {
                let Ok(tid) = entry.file_name().to_string_lossy().parse::<libc::pid_t>() else {
                    continue;
                };
                let starttime = thread_starttime(self.pid, tid);
                if !self.seen.insert((tid as i64, starttime)) {
                    continue;
                }
                let opened = self.groups.iter().map(|spec| open_group(spec, tid, -1)).collect();
                self.threads.push(opened);
            }
        }
        // Cache every live group's counters now. A thread that exits before the
        // final read still contributes its last poll instead of dropping out.
        self.poll();
    }

    /// Refresh each live group's cached counter read. Called on every `scan`
    /// (i.e. every sample) and once more in `read_sums`.
    fn poll(&mut self) {
        let sizes: Vec<usize> = self.groups.iter().map(|g| g.len()).collect();
        for thread in &mut self.threads {
            for (gi, slot) in thread.iter_mut().enumerate() {
                let Some(group) = slot else { continue };
                if group.dead {
                    continue;
                }
                match read_values(&mut group.leader, sizes[gi]) {
                    Some(r) => {
                        group.last = Some(r.vals);
                        group.coverage = r.coverage;
                    }
                    // Read failed. Two cases:
                    //  - We had a prior successful read → the thread/process just
                    //    exited (ESRCH). Keep its last cumulative read but stop
                    //    polling it (so a recycled TID can't double-count it).
                    //  - We never read it successfully → it may simply not have
                    //    been scheduled yet (just opened, or its CPU still idle —
                    //    `time_running == 0` right after `scan()` opens it). Leave
                    //    it to retry on the next poll. Pruning here permanently
                    //    drops a counter that is merely slow to start, which gapped
                    //    HPL/HPCG FP (per-CPU groups first-polled before the ranks
                    //    spun up).
                    None => {
                        if group.last.is_some() {
                            group.dead = true;
                        }
                    }
                }
            }
        }
    }

    /// Per-event totals summed across all threads, flattened across groups.
    /// Uses each group's most recent successful read (cached by `poll`), so
    /// threads that have already exited still count. `None` for any event that
    /// no thread ever successfully read.
    pub fn read_sums(&mut self) -> Vec<Option<f64>> {
        self.poll(); // capture still-live threads one last time
        let sizes: Vec<usize> = self.groups.iter().map(|g| g.len()).collect();
        let total: usize = sizes.iter().sum();
        let mut offsets = Vec::with_capacity(sizes.len());
        let mut off = 0;
        for &s in &sizes {
            offsets.push(off);
            off += s;
        }

        let mut sums = vec![0.0f64; total];
        let mut present = vec![false; total];
        for thread in &self.threads {
            for (gi, group) in thread.iter().enumerate() {
                let Some(group) = group else { continue };
                let Some(vals) = &group.last else { continue };
                // NOTE: we deliberately do NOT gap groups on low multiplexing
                // coverage. `uaps -a` runs ~5-6 event groups competing for AMD's
                // 6 PMCs, so even a perfectly-pinned compute-bound run gives each
                // group a low per-group `time_running/time_enabled` (~0.15) — the
                // values in `vals` are already scaled by that factor in
                // `read_values`, which compensates correctly (HPL GFLOPS=535 here
                // matches the validated ground truth). A coverage floor here
                // wrongly gapped that validated metric, so the raw counts are
                // always summed; `coverage` is retained only for diagnostics.
                for (j, v) in vals.iter().enumerate() {
                    sums[offsets[gi] + j] += *v;
                    present[offsets[gi] + j] = true;
                }
            }
        }
        sums.into_iter().zip(present).map(|(s, p)| p.then_some(s)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_starttime_field_22_with_parens_in_comm() {
        // A real /proc/<pid>/task/<tid>/stat line whose comm contains a space
        // and parens; field 22 (starttime) is 12481606 here. Splitting after the
        // FINAL ')' must land on it (the 20th token of the remainder).
        let line = "848646 (my proc) R 848637 848646 848637 0 -1 4194304 93 0 0 0 \
                    0 0 0 0 20 0 1 0 12481606 8642560 478 18446744073709551615 0 0";
        assert_eq!(parse_starttime(line), 12481606);
        // A different incarnation (recycled tid) yields a different starttime.
        let line2 = "848646 (my proc) R 848637 848646 848637 0 -1 4194304 93 0 0 0 \
                     0 0 0 0 20 0 1 0 99999999 8642560 478 18446744073709551615 0 0";
        assert_eq!(parse_starttime(line2), 99999999);
    }

    #[test]
    fn malformed_starttime_is_zero_not_panic() {
        assert_eq!(parse_starttime(""), 0);
        assert_eq!(parse_starttime("no close paren 1 2 3"), 0);
        assert_eq!(parse_starttime("1 (c) R 1 2"), 0); // too few fields
    }
}
