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
const CFG_DISPATCHED: u64 = 0x07AA; // de_src_op_disp.all (umask 0x07)
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
}

/// Open a grouped event set on thread `tid`: a leader plus members sharing its
/// group. Returns the leader and members, or `None` if any event fails.
fn open_grouped(tid: libc::pid_t, leader_cfg: u64, member_cfgs: &[u64]) -> Option<(File, Vec<File>)> {
    let leader = pmu::open_event(TYPE_RAW, leader_cfg, tid, -1, true)?;
    let lfd = leader.as_raw_fd();
    let mut members = Vec::new();
    for &cfg in member_cfgs {
        members.push(pmu::open_event(TYPE_RAW, cfg, tid, lfd, false)?);
    }
    // SAFETY: enabling the leader with the GROUP flag starts the group atomically.
    unsafe {
        sys::ioctls::ENABLE(lfd, sys::bindings::PERF_IOC_FLAG_GROUP);
    }
    Some((leader, members))
}

/// Open both top-down groups on a thread. The L1 group is required; the
/// backend-split group is best-effort.
fn open_group(tid: libc::pid_t) -> Option<ThreadGroup> {
    let (main_leader, main_members) =
        open_grouped(tid, CFG_CYCLES, &[CFG_FRONTEND, CFG_BACKEND, CFG_DISPATCHED, CFG_RETIRED])?;
    let (split_leader, split_member) =
        match open_grouped(tid, CFG_NOT_COMPLETE, &[CFG_LOAD_NOT_COMPLETE]) {
            Some((l, mut m)) => (Some(l), m.pop()),
            None => (None, None),
        };
    Some(ThreadGroup {
        main_leader,
        _main_members: main_members,
        split_leader,
        _split_member: split_member,
    })
}

pub struct TopdownCollector {
    supported: bool,
    pid: u32,
    /// Threads we've already opened a group for (avoid reopening each sample).
    seen: HashSet<libc::pid_t>,
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
        let dir = format!("/proc/{}/task", self.pid);
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        for entry in entries.flatten() {
            let Ok(tid) = entry.file_name().to_string_lossy().parse::<libc::pid_t>() else {
                continue;
            };
            if !self.seen.insert(tid) {
                continue; // already handled (success or failure)
            }
            if let Some(group) = open_group(tid) {
                self.groups.push(group);
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
        // Aggregate raw counts across all per-thread groups, then derive the
        // whole-process ratios from the sums.
        let (mut cycles, mut frontend, mut backend, mut dispatched, mut retired) =
            (0.0, 0.0, 0.0, 0.0, 0.0);
        let (mut not_complete, mut load_not_complete) = (0.0, 0.0);
        let mut threads = 0;

        for group in &mut self.groups {
            if let Some(v) = pmu::read_values(&mut group.main_leader, 5) {
                cycles += v[0];
                frontend += v[1];
                backend += v[2];
                dispatched += v[3];
                retired += v[4];
                threads += 1;
            }
            if let Some(leader) = group.split_leader.as_mut() {
                if let Some(v) = pmu::read_values(leader, 2) {
                    not_complete += v[0];
                    load_not_complete += v[1];
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
        ];

        // L2 backend split: of backend-bound slots, the fraction stalled on a
        // load (memory) vs other execution resources (core).
        if not_complete > 0.0 {
            let mem_frac = (load_not_complete / not_complete).clamp(0.0, 1.0);
            out.push(Metric {
                key: "topdown_backend_mem_pct",
                label: "  ↳ memory bound".into(),
                value: MetricValue::Percent(backend_pct * mem_frac),
            });
            out.push(Metric {
                key: "topdown_backend_core_pct",
                label: "  ↳ core bound".into(),
                value: MetricValue::Percent(backend_pct * (1.0 - mem_frac)),
            });
        }

        out.push(Metric {
            key: "topdown_badspec_pct",
            label: "Bad speculation".into(),
            value: MetricValue::Percent(pct((dispatched - retired).max(0.0))),
        });
        Ok(out)
    }
}
