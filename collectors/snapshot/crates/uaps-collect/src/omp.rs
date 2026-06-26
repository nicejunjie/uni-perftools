//! Detect when OpenMP **active-spin** can mask thread imbalance.
//!
//! uaps derives thread imbalance from per-thread CPU time (`/proc/<pid>/task/*/stat`),
//! which cannot tell a thread doing real work from one busy-WAITING. Under the active
//! wait policy (libgomp's default once threads are bound — the HPC norm — or an explicit
//! `OMP_WAIT_POLICY=active`), idle OpenMP threads spin at barriers instead of sleeping, so
//! every thread looks ~fully busy and the `(max-avg)/max` imbalance reads ~0 even when the
//! work is badly skewed. We can't fix the measurement (the spin IS CPU time), so we DETECT
//! the condition and flag it: the reported imbalance is then a LOWER BOUND, and the user
//! should re-run with `OMP_WAIT_POLICY=passive` (idle threads sleep) for the true value.

/// Whether an OpenMP runtime is mapped into the process (libgomp / LLVM libomp /
/// Intel libiomp5). Reads `/proc/<pid>/maps`; cheap, checked once (sticky) by the caller.
pub fn runtime_loaded(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/maps"))
        .map(|m| {
            m.contains("libgomp")
                || m.contains("libomp.so")
                || m.contains("libiomp5")
                || m.contains("libiomp.so")
        })
        .unwrap_or(false)
}

/// Decide whether active-spin may be masking thread imbalance, given that an OpenMP
/// runtime is loaded and the value of `OMP_WAIT_POLICY` (from the env uaps shares with
/// the child). Only an explicit `passive` policy makes idle threads sleep — anything
/// else (explicit `active`, or unset, which spins when threads are bound) is a risk.
/// Pure + testable.
pub fn spin_masks_imbalance(omp_loaded: bool, wait_policy: Option<&str>) -> bool {
    if !omp_loaded {
        return false;
    }
    !matches!(wait_policy, Some(p) if p.trim().eq_ignore_ascii_case("passive"))
}

#[cfg(test)]
mod tests {
    use super::spin_masks_imbalance;

    #[test]
    fn flags_active_or_default_but_not_passive() {
        // not an OpenMP run → never flagged
        assert!(!spin_masks_imbalance(false, None));
        assert!(!spin_masks_imbalance(false, Some("active")));
        // OpenMP loaded: passive is safe (idle threads sleep), everything else risks masking
        assert!(!spin_masks_imbalance(true, Some("passive")));
        assert!(!spin_masks_imbalance(true, Some("PASSIVE")));
        assert!(!spin_masks_imbalance(true, Some(" passive ")));
        assert!(spin_masks_imbalance(true, Some("active")));
        assert!(spin_masks_imbalance(true, None)); // default spins when bound (HPC norm)
    }
}
