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
/// runtime is loaded, the value of `OMP_WAIT_POLICY`, and whether threads are bound.
/// `passive` makes idle threads sleep (never a risk); `active` always spins; UNSET is
/// the subtle case — libgomp spins indefinitely only when threads are bound (a short,
/// sleeping spin otherwise), so an unset+unbound run still shows accurate imbalance and
/// must NOT be flagged. Pure + testable.
pub fn spin_masks_imbalance(omp_loaded: bool, wait_policy: Option<&str>, threads_bound: bool) -> bool {
    if !omp_loaded {
        return false;
    }
    match wait_policy.map(|p| p.trim().to_ascii_lowercase()).as_deref() {
        Some("passive") => false,
        Some("active") => true,
        _ => threads_bound, // unset (or other): indefinite spin only when bound
    }
}

#[cfg(test)]
mod tests {
    use super::spin_masks_imbalance;

    #[test]
    fn flags_active_or_bound_default_but_not_passive_or_unbound() {
        // not an OpenMP run → never flagged
        assert!(!spin_masks_imbalance(false, None, true));
        assert!(!spin_masks_imbalance(false, Some("active"), true));
        // passive → idle threads sleep → never a risk (regardless of binding)
        assert!(!spin_masks_imbalance(true, Some("passive"), true));
        assert!(!spin_masks_imbalance(true, Some("PASSIVE"), true));
        assert!(!spin_masks_imbalance(true, Some(" passive "), true));
        // explicit active → always spins
        assert!(spin_masks_imbalance(true, Some("active"), false));
        // UNSET: indefinite spin only when threads are BOUND
        assert!(spin_masks_imbalance(true, None, true));    // bound → flag
        assert!(!spin_masks_imbalance(true, None, false));  // unbound → accurate, don't flag
    }
}
