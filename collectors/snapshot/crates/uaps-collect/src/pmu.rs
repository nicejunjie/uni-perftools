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

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};

use perf_event_open_sys as sys;

/// perf_event ABI event types (stable kernel constants).
pub const TYPE_HARDWARE: u32 = 0; // PERF_TYPE_HARDWARE
pub const TYPE_RAW: u32 = 4; // PERF_TYPE_RAW (CPU-specific raw encoding)

/// One event: its perf type and config (HW enum value or raw encoding).
#[derive(Clone, Copy)]
pub struct EventCfg {
    pub etype: u32,
    pub config: u64,
}

/// Open a single perf event on thread `tid`. `group_fd` = -1 for a leader,
/// or the leader's fd for a member. Returns an owned File (auto-closes).
pub fn open_event(etype: u32, config: u64, tid: libc::pid_t, group_fd: i32, leader: bool) -> Option<File> {
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
    // SAFETY: attr is initialised; cpu = -1 (any), group_fd links to leader.
    let fd = unsafe { sys::perf_event_open(&mut attr, tid, -1, group_fd, 0) };
    if fd < 0 {
        None
    } else {
        // SAFETY: fresh owned fd from the kernel.
        Some(unsafe { File::from_raw_fd(fd) })
    }
}

/// Read `n` grouped values from a leader, scaled for multiplexing.
/// Layout (PERF_FORMAT_GROUP, no IDs): [nr, time_enabled, time_running, v0..].
pub fn read_values(leader: &mut File, n: usize) -> Option<Vec<f64>> {
    let mut buf = vec![0u8; (3 + n) * 8];
    leader.read_exact(&mut buf).ok()?;
    let at = |i: usize| -> f64 { u64::from_ne_bytes(buf[i * 8..i * 8 + 8].try_into().unwrap()) as f64 };
    let nr = at(0) as usize;
    let time_enabled = at(1);
    let time_running = at(2);
    if nr < n || time_running == 0.0 {
        return None;
    }
    let scale = time_enabled / time_running;
    Some((0..n).map(|k| at(3 + k) * scale).collect())
}

struct Group {
    leader: File,
    _members: Vec<File>,
}

fn open_group(spec: &[EventCfg], tid: libc::pid_t) -> Option<Group> {
    let first = spec.first()?;
    let leader = open_event(first.etype, first.config, tid, -1, true)?;
    let lfd = leader.as_raw_fd();
    let mut members = Vec::new();
    for ev in &spec[1..] {
        members.push(open_event(ev.etype, ev.config, tid, lfd, false)?);
    }
    // SAFETY: enabling the leader with the GROUP flag starts the group atomically.
    unsafe {
        sys::ioctls::ENABLE(lfd, sys::bindings::PERF_IOC_FLAG_GROUP);
    }
    Some(Group { leader, _members: members })
}

/// Manages a set of counter groups replicated across every thread of a target.
/// Each `group` (≤5 events) is opened per thread; [`read_sums`] returns the
/// per-event totals summed over all threads, flattened in the order the groups
/// were given (`None` for an event no thread could program).
pub struct ThreadGroups {
    pid: u32,
    groups: Vec<Vec<EventCfg>>,
    seen: HashSet<libc::pid_t>,
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

    /// Discover new threads and open all groups on each.
    pub fn scan(&mut self) {
        if !self.configured() {
            return;
        }
        let dir = format!("/proc/{}/task", self.pid);
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        for entry in entries.flatten() {
            let Ok(tid) = entry.file_name().to_string_lossy().parse::<libc::pid_t>() else {
                continue;
            };
            if !self.seen.insert(tid) {
                continue;
            }
            let opened = self.groups.iter().map(|spec| open_group(spec, tid)).collect();
            self.threads.push(opened);
        }
    }

    /// Per-event totals summed across all threads, flattened across groups.
    /// `None` for any event that no thread successfully programmed.
    pub fn read_sums(&mut self) -> Vec<Option<f64>> {
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
        for thread in &mut self.threads {
            for (gi, group) in thread.iter_mut().enumerate() {
                let Some(group) = group else { continue };
                if let Some(vals) = read_values(&mut group.leader, sizes[gi]) {
                    for (j, v) in vals.into_iter().enumerate() {
                        sums[offsets[gi] + j] += v;
                        present[offsets[gi] + j] = true;
                    }
                }
            }
        }
        sums.into_iter().zip(present).map(|(s, p)| p.then_some(s)).collect()
    }
}
