//! Minimal CPU identification, used to pick vendor-specific raw PMU events.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Amd,
    Intel,
    Other,
}

#[derive(Debug, Clone, Copy)]
pub struct CpuInfo {
    pub vendor: Vendor,
    pub family: u32,
    pub model: u32,
}

/// Read vendor / family / model from `/proc/cpuinfo` (first processor block;
/// all cores are identical on the machines we target).
pub fn detect() -> CpuInfo {
    let text = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let mut info = CpuInfo { vendor: Vendor::Other, family: 0, model: 0 };
    for line in text.lines() {
        let Some((key, val)) = line.split_once(':') else { continue };
        match key.trim() {
            "vendor_id" => {
                info.vendor = match val.trim() {
                    "AuthenticAMD" => Vendor::Amd,
                    "GenuineIntel" => Vendor::Intel,
                    _ => Vendor::Other,
                };
            }
            "cpu family" => info.family = val.trim().parse().unwrap_or(0),
            "model" => info.model = val.trim().parse().unwrap_or(0),
            // Stop at the end of the first processor's block.
            "" if info.family != 0 => break,
            _ => {}
        }
    }
    info
}
