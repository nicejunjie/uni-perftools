//! Best-effort detection of GPU offload from the CPU side.
//!
//! uaps measures only CPU hardware counters, so for a job that offloads its
//! compute to a GPU the CPU-side FP/roofline numbers are misleading — they see
//! near-zero FLOPs and call the program "idle" or "memory-bound" while the real
//! work runs on the device. We can't read GPU counters, but we CAN tell that a
//! process is driving a GPU by inspecting its `/proc/<pid>`:
//!   - an open GPU **device node** (`/dev/nvidia*`, `/dev/kfd`, `/dev/dri/renderD*`),
//!   - and, for the ambiguous render node, a mapped GPU **compute runtime**
//!     (CUDA / ROCm-HIP / Level-Zero / OpenCL).
//!
//! NVIDIA char devices and the AMD KFD node are compute-only, so an open fd to
//! either is an unambiguous signal. `/dev/dri/renderD*` is also used for graphics,
//! so we require a compute runtime to be mapped before trusting it (avoids flagging
//! a desktop GUI process). Detection is sticky in the caller: checked each sample
//! until found, then left alone.

/// What an open file descriptor's target tells us about GPU use.
enum FdHit {
    /// A compute-only device node — unambiguous (vendor named).
    Compute(&'static str),
    /// A DRM render node — GPU, but shared with graphics; needs corroboration.
    RenderNode,
}

/// Classify one `/proc/<pid>/fd/*` symlink target.
fn classify_fd(target: &str) -> Option<FdHit> {
    if target.contains("/dev/nvidia") {
        Some(FdHit::Compute("NVIDIA"))
    } else if target.contains("/dev/kfd") {
        Some(FdHit::Compute("AMD ROCm"))
    } else if target.contains("/dev/dri/renderD") {
        Some(FdHit::RenderNode)
    } else {
        None
    }
}

/// Identify a mapped GPU compute runtime in a `/proc/<pid>/maps` body, naming the
/// vendor. `None` when no known compute runtime is mapped (e.g. a graphics-only
/// renderD use).
fn vendor_from_maps(maps: &str) -> Option<&'static str> {
    for (needle, vendor) in [
        ("libcuda", "NVIDIA"),
        ("libcudart", "NVIDIA"),
        ("libnvidia", "NVIDIA"),
        ("libamdhip64", "AMD ROCm"),
        ("libhsa-runtime", "AMD ROCm"),
        ("libamdocl", "AMD ROCm"),
        ("libze_loader", "Intel oneAPI"),
        ("libze_intel", "Intel oneAPI"),
        ("libigdrcl", "Intel oneAPI"),
        ("libOpenCL", "OpenCL"),
    ] {
        if maps.contains(needle) {
            return Some(vendor);
        }
    }
    None
}

/// The GPU vendor/stack driving this process, or `None` if no GPU use is seen.
pub fn detect(pid: u32) -> Option<&'static str> {
    let mut render_node = false;
    if let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        for e in rd.flatten() {
            let Ok(target) = std::fs::read_link(e.path()) else { continue };
            match classify_fd(&target.to_string_lossy()) {
                Some(FdHit::Compute(vendor)) => return Some(vendor),
                Some(FdHit::RenderNode) => render_node = true,
                None => {}
            }
        }
    }
    // Render node open: trust it as GPU COMPUTE only if a compute runtime is also
    // mapped. A render node alone is shared with graphics (a desktop compositor/
    // browser opens it), so requiring the runtime avoids false-positives off the
    // cluster. CUDA/ROCm jobs hit the unambiguous nvidia/kfd nodes above regardless.
    if render_node {
        let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap_or_default();
        return vendor_from_maps(&maps);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_device_nodes_are_unambiguous() {
        assert!(matches!(classify_fd("/dev/nvidia0"), Some(FdHit::Compute("NVIDIA"))));
        assert!(matches!(classify_fd("/dev/nvidiactl"), Some(FdHit::Compute("NVIDIA"))));
        assert!(matches!(classify_fd("/dev/kfd"), Some(FdHit::Compute("AMD ROCm"))));
        assert!(matches!(classify_fd("/dev/dri/renderD128"), Some(FdHit::RenderNode)));
        // ordinary files are not GPU use
        assert!(classify_fd("/home/u/app.dat").is_none());
        assert!(classify_fd("/dev/dri/card0").is_none()); // primary node ≠ render node
    }

    #[test]
    fn render_node_needs_a_compute_runtime_to_name_a_vendor() {
        assert_eq!(vendor_from_maps("...libamdhip64.so.6..."), Some("AMD ROCm"));
        assert_eq!(vendor_from_maps("...libcudart.so.12..."), Some("NVIDIA"));
        assert_eq!(vendor_from_maps("...libze_loader.so.1..."), Some("Intel oneAPI"));
        // a graphics-only renderD use (Mesa) maps no compute runtime → no vendor
        assert_eq!(vendor_from_maps("...libGLX_mesa.so.0 libglapi.so.0..."), None);
    }
}
