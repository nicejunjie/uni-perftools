//! Level-1 top-down (pipeline-slot) analysis for AMD Zen 5 via a dedicated
//! `perf_event` group.
//!
//! `perf`'s top-down is not special hardware support on AMD — it is arithmetic
//! over ordinary PMU events using vendor formulas shipped as open JSON in the
//! kernel tree. We compute the same thing directly through `perf_event_open`,
//! so there is no dependency on an installed `perf` binary.
//!
//! Crucially, the five events are opened as **one hardware group** on the
//! target pid. A group is scheduled all-or-nothing, so even when the PMU is
//! multiplexed against our other counters, the group's members always share
//! the same active window — making the top-down *ratios* exact rather than
//! noisy approximations of independently-scaled counters.
//!
//! Formulas (Zen 5, 8-wide dispatch), from the kernel `amdzen` pipeline-
//! utilization metric group:
//!   total_slots   = 8 * ls_not_halted_cyc
//!   frontend      = de_no_dispatch_per_slot.no_ops_from_frontend / total_slots
//!   backend       = de_no_dispatch_per_slot.backend_stalls        / total_slots
//!   retiring      = ex_ret_ops                                    / total_slots
//!   bad_spec      = (de_src_op_disp.all - ex_ret_ops)             / total_slots
//!
//! Limitations: a grouped read cannot use `inherit`, so this reflects the
//! target's initial thread (exact for single-threaded workloads; partial for
//! multi-threaded — full per-thread aggregation is future work). Enabled only
//! on AMD family 0x1A (Zen 5): the umasks are validated there and would be
//! wrong on other families.

use std::collections::HashSet;
use std::fs::File;
use std::os::fd::AsRawFd;

use anyhow::Result;
use perf_event_open_sys as sys;
use uaps_core::{Collector, Metric, MetricValue, Target};

use crate::cpu::{self, Vendor};
use crate::pmu::{self, TYPE_RAW};

// Raw PMU configs: event[7:0] | (umask << 8) | (event[11:8] << 32).
const CFG_CYCLES: u64 = 0x0076; // ls_not_halted_cyc
const CFG_FRONTEND: u64 = (1 << 32) | 0x01A0; // de_no_dispatch_per_slot.no_ops_from_frontend
const CFG_BACKEND: u64 = (1 << 32) | 0x1EA0; // de_no_dispatch_per_slot.backend_stalls (umask 0x1e)
const CFG_SMT: u64 = (1 << 32) | 0x60A0; // de_no_dispatch_per_slot.smt_contention (umask 0x60)
const CFG_RETIRED: u64 = 0x00C1; // ex_ret_ops
// Backend split (L2): of the cycles where retire is stalled on an incomplete
// op, how many are waiting on a load (memory) vs other execution (core).
const CFG_NOT_COMPLETE: u64 = 0x02D6; // ex_no_retire.not_complete (umask 0x02)
const CFG_LOAD_NOT_COMPLETE: u64 = 0xA2D6; // ex_no_retire.load_not_complete (umask 0xa2)

const SLOTS_PER_CYCLE: f64 = 8.0; // Zen 5 dispatch width

/// Per-thread groups: the 5-event L1 top-down group, plus an optional 2-event
/// backend-split group (`ex_no_retire`). They are separate groups because all
/// seven events cannot fit in AMD's 6 core PMCs at once (a group must schedule
/// as a unit). Each group's *internal* ratios stay exact; the cross-group
/// combination (backend × memory-fraction) is the standard AMD methodology.
struct ThreadGroup {
    main_leader: File,
    _main_members: Vec<File>,
    split_leader: Option<File>,
    _split_member: Option<File>,
    /// Last successful reads (cumulative counters), refreshed every sample so a
    /// thread that exits before `finish` still contributes its final values.
    main_last: Option<Vec<f64>>,
    split_last: Option<Vec<f64>>,
    /// Set once the main group's fd read fails (thread exited / ESRCH) after we
    /// captured at least one value: stop polling but keep the last good read.
    /// If the very first read fails the whole group is pruned in `scan_threads`.
    main_dead: bool,
    /// Same, for the best-effort split group's fd.
    split_dead: bool,
}

/// Open a grouped event set on thread `tid`: a leader plus members sharing its
/// group. Returns the leader and members, or `None` if any event fails.
fn open_grouped(pid: libc::pid_t, cpu: i32, leader_cfg: u64, member_cfgs: &[u64]) -> Option<(File, Vec<File>)> {
    let leader = pmu::open_event(TYPE_RAW, leader_cfg, pid, cpu, -1, true)?;
    let lfd = leader.as_raw_fd();
    let mut members = Vec::new();
    for &cfg in member_cfgs {
        members.push(pmu::open_event(TYPE_RAW, cfg, pid, cpu, lfd, false)?);
    }
    // SAFETY: enabling the leader with the GROUP flag starts the group atomically.
    unsafe {
        sys::ioctls::ENABLE(lfd, sys::bindings::PERF_IOC_FLAG_GROUP);
    }
    Some((leader, members))
}

/// Open both top-down groups on one scope (a thread, or a CPU when system-wide).
/// The L1 group is required; the backend-split group is best-effort.
fn open_group(pid: libc::pid_t, cpu: i32) -> Option<ThreadGroup> {
    // 5-event group: the four no-dispatch / retire counters share the leader's
    // cycles. Bad speculation is derived as the remaining slots (the AMD L1 set —
    // retiring, frontend, backend, smt_contention, bad_spec — partitions all
    // slots), so it needs no separate dispatched-ops event and the group stays ≤5.
    let (main_leader, main_members) =
        open_grouped(pid, cpu, CFG_CYCLES, &[CFG_FRONTEND, CFG_BACKEND, CFG_SMT, CFG_RETIRED])?;
    let (split_leader, split_member) =
        match open_grouped(pid, cpu, CFG_NOT_COMPLETE, &[CFG_LOAD_NOT_COMPLETE]) {
            Some((l, mut m)) => (Some(l), m.pop()),
            None => (None, None),
        };
    Some(ThreadGroup {
        main_leader,
        _main_members: main_members,
        split_leader,
        _split_member: split_member,
        main_last: None,
        split_last: None,
        main_dead: false,
        split_dead: false,
    })
}

/// Read field 22 (`starttime`, clock ticks) of `/proc/<pid>/task/<tid>/stat`,
/// to disambiguate a recycled tid (pid_max wrap) from the thread it replaced.
/// Field 2 (`comm`) may contain spaces/parens, so split after the final ')'.
fn thread_starttime(pid: u32, tid: libc::pid_t) -> u64 {
    let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat")) else {
        return 0;
    };
    let Some(after) = s.rsplit_once(')').map(|(_, r)| r) else {
        return 0;
    };
    after.split_whitespace().nth(19).and_then(|f| f.parse().ok()).unwrap_or(0)
}

pub struct TopdownCollector {
    supported: bool,
    pid: u32,
    /// Scopes we've already opened a group for (avoid reopening each sample).
    /// Threads are keyed `(tid, starttime)` so a recycled tid is reopened as a
    /// new thread; system-wide CPUs are keyed `(cpu, 0)`.
    seen: HashSet<(i64, u64)>,
    groups: Vec<ThreadGroup>,
}

impl TopdownCollector {
    pub fn new() -> Self {
        let info = cpu::detect();
        // Zen 5 only: the umasks above are validated for family 0x1A.
        let supported = info.vendor == Vendor::Amd && info.family == 0x1A;
        Self { supported, pid: 0, seen: HashSet::new(), groups: Vec::new() }
    }

    /// Discover threads via `/proc/<pid>/task` and open a group for each new
    /// one. Called at start and on every sample, so threads created after
    /// launch (OpenMP pools, pthreads) are picked up while they run.
    fn scan_threads(&mut self) {
        if pmu::system_wide() {
            for cpu in pmu::online_cpus() {
                if !self.seen.insert((cpu as i64, 0)) {
                    continue;
                }
                if let Some(group) = open_group(-1, cpu) {
                    self.groups.push(group);
                }
            }
            return;
        }
        if let Ok(entries) = std::fs::read_dir(format!("/proc/{}/task", self.pid)) {
            for entry in entries.flatten() {
                let Ok(tid) = entry.file_name().to_string_lossy().parse::<libc::pid_t>() else {
                    continue;
                };
                let starttime = thread_starttime(self.pid, tid);
                if !self.seen.insert((tid as i64, starttime)) {
                    continue; // already handled (success or failure)
                }
                if let Some(group) = open_group(tid, -1) {
                    self.groups.push(group);
                }
            }
        }
        self.poll();
    }

    /// Cache every live group's counters. Counters are cumulative, so the latest
    /// read wins; an exited thread keeps its last poll (instead of dropping out
    /// of the sums in `finish`).
    fn poll(&mut self) {
        // Prune groups whose main fd died before we ever read it (a thread that
        // exited within the first sample interval) — they have no data and would
        // otherwise linger. Groups with a cached read are kept (their last good
        // cumulative values still count) but marked dead so we stop polling them.
        self.groups.retain(|g| !(g.main_dead && g.main_last.is_none()));
        for group in &mut self.groups {
            if !group.main_dead {
                match pmu::read_values(&mut group.main_leader, 5) {
                    Some(r) => group.main_last = Some(r.vals),
                    None => group.main_dead = true,
                }
            }
            if let Some(leader) = group.split_leader.as_mut() {
                if !group.split_dead {
                    match pmu::read_values(leader, 2) {
                        Some(r) => group.split_last = Some(r.vals),
                        None => group.split_dead = true,
                    }
                }
            }
        }
    }
}

impl Default for TopdownCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for TopdownCollector {
    fn name(&self) -> &'static str {
        "topdown"
    }

    fn start(&mut self, target: &Target) -> Result<()> {
        if !self.supported {
            return Ok(());
        }
        self.pid = target.pid;
        self.scan_threads();
        Ok(())
    }

    fn sample(&mut self) -> Result<()> {
        if self.supported {
            self.scan_threads();
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Vec<Metric>> {
        // Capture any still-live threads, then aggregate raw counts across all
        // per-thread groups (using each group's last cached read so threads that
        // already exited still count), and derive whole-process ratios.
        self.poll();
        let (mut cycles, mut frontend, mut backend, mut smt, mut retired) =
            (0.0, 0.0, 0.0, 0.0, 0.0);
        let (mut not_complete, mut load_not_complete) = (0.0, 0.0);
        // Parallel sums restricted to threads whose best-effort L2 split group
        // also read: the memory/core fraction (over `not_complete`) and the
        // backend slots it multiplies must come from the *same* thread set, or
        // the product mixes two populations when the split fails to open on some
        // threads under PMC pressure.
        let (mut cycles_split, mut backend_split) = (0.0, 0.0);
        let mut threads = 0;
        let mut split_threads = 0;

        for group in &self.groups {
            if let Some(v) = &group.main_last {
                cycles += v[0];
                frontend += v[1];
                backend += v[2];
                smt += v[3];
                retired += v[4];
                threads += 1;

                // Fold this thread's L2 split (and the matching backend/cycles)
                // only when its split group also read, so the mem/core ratio and
                // the backend it scales share one thread population.
                if let Some(s) = &group.split_last {
                    not_complete += s[0];
                    load_not_complete += s[1];
                    cycles_split += v[0];
                    backend_split += v[2];
                    split_threads += 1;
                }
            }
        }

        let slots = SLOTS_PER_CYCLE * cycles;
        if threads == 0 || slots <= 0.0 {
            return Ok(Vec::new());
        }
        let pct = |x: f64| (x / slots * 100.0).clamp(0.0, 100.0);
        let backend_pct = pct(backend);

        let mut out = vec![
            Metric {
                key: "topdown_retiring_pct",
                label: "Retiring".into(),
                value: MetricValue::Percent(pct(retired)),
            },
            Metric {
                key: "topdown_frontend_pct",
                label: "Frontend bound".into(),
                value: MetricValue::Percent(pct(frontend)),
            },
            Metric {
                key: "topdown_backend_pct",
                label: "Backend bound".into(),
                value: MetricValue::Percent(backend_pct),
            },
            Metric {
                key: "topdown_smt_pct",
                label: "SMT contention".into(),
                value: MetricValue::Percent(pct(smt)),
            },
        ];

        // L2 backend split: of backend-bound slots, the fraction stalled on a
        // load (memory) vs other execution resources (core). Both the fraction
        // and the backend % it scales are taken over the SAME thread set (those
        // with a split read), and only when the split covered most main threads
        // — otherwise the split's population is too partial to represent the
        // whole-process backend, so we gap it rather than mislead.
        let split_slots = SLOTS_PER_CYCLE * cycles_split;
        if not_complete > 0.0
            && split_slots > 0.0
            && split_threads * 2 >= threads
        {
            let mem_frac = (load_not_complete / not_complete).clamp(0.0, 1.0);
            // backend % computed over the split population, not the whole one.
            let backend_split_pct = (backend_split / split_slots * 100.0).clamp(0.0, 100.0);
            out.push(Metric {
                key: "topdown_backend_mem_pct",
                label: "  ↳ memory bound".into(),
                value: MetricValue::Percent(backend_split_pct * mem_frac),
            });
            out.push(Metric {
                key: "topdown_backend_core_pct",
                label: "  ↳ core bound".into(),
                value: MetricValue::Percent(backend_split_pct * (1.0 - mem_frac)),
            });
        }

        // Bad speculation as a REMAINDER of the other four categories. The four
        // buckets nominally partition the non-bad-spec slots, but they are
        // independent counters and retired is op-granular while slots are
        // dispatch-granular, so this remainder absorbs all of their measurement
        // skew — it can read high on a workload with little real bad speculation.
        // The canonical directly-measured formula (de_src_op_disp.all - ex_ret_ops)
        // needs a 5th co-grouped event, exceeding the PMC budget here; the
        // data-driven HwpcCollector uses it and supersedes this fallback whenever
        // the CPU model resolves. Clamped so counter skew can't yield a negative.
        let badspec = (slots - frontend - backend - smt - retired).max(0.0);
        out.push(Metric {
            key: "topdown_badspec_pct",
            label: "Bad speculation".into(),
            value: MetricValue::Percent(pct(badspec)),
        });
        Ok(out)
    }
}
