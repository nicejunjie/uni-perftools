//! Serialization of a [`Snapshot`] to the on-disk JSON **contract**.
//!
//! This crate used to also render terminal/HTML reports, but that duplicated the
//! shared `core/` renderer (roofline, viewpoints, insights). The human report for
//! BOTH tiers now comes from `core/` — `uaps run` stages this JSON and hands it to
//! `core/cli/upat report --collector uaps`. So all that lives here is the metric
//! JSON every downstream tool reads, plus the value formatting it needs.

use std::time::Duration;

use uaps_core::{MetricValue, Snapshot};

/// Output format selectable on the CLI. Text/HTML are produced by the shared core
/// renderer; only JSON is emitted here (the contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
    Html,
}

// -------------------------------------------------------------------- json

/// Render the snapshot to the JSON contract: one row per metric with key, label,
/// numeric value, unit, and a pre-formatted display string. Insights and the
/// human layout are the shared core's job, so they are not emitted here.
pub fn render_json(snapshot: &Snapshot) -> String {
    let mut out = String::from("{\n  \"metrics\": [\n");
    for (i, m) in snapshot.metrics.iter().enumerate() {
        let comma = if i + 1 < snapshot.metrics.len() { "," } else { "" };
        out.push_str(&format!(
            "    {{\"key\": \"{}\", \"label\": {}, \"value\": {}, \"unit\": {}, \"display\": {}}}{}\n",
            m.key,
            json_str(&m.label),
            json_number(&m.value),
            json_str(unit_of(&m.value)),
            json_str(&format_value(&m.value)),
            comma
        ));
    }
    out.push_str("  ]\n}\n");
    out
}

// ------------------------------------------------------------- formatting

fn format_value(value: &MetricValue) -> String {
    match value {
        MetricValue::Duration(d) => format_duration(*d),
        MetricValue::Float { value, unit: "" } => format!("{value:.2}"),
        MetricValue::Float { value, unit } => format!("{value:.2} {unit}"),
        MetricValue::Int { value, unit: "" } => format!("{value}"),
        MetricValue::Int { value, unit } => format!("{value} {unit}"),
        MetricValue::Percent(p) => format!("{p:.1}%"),
        MetricValue::Bytes(b) => format_bytes(*b),
    }
}

fn unit_of(value: &MetricValue) -> &'static str {
    match value {
        MetricValue::Duration(_) => "s",
        MetricValue::Float { unit, .. } | MetricValue::Int { unit, .. } => unit,
        MetricValue::Percent(_) => "%",
        MetricValue::Bytes(_) => "bytes",
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{:.1} ms", secs * 1000.0)
    } else {
        format!("{secs:.3} s")
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn json_number(value: &MetricValue) -> String {
    let n = value.as_f64();
    if n.is_finite() {
        format!("{n}")
    } else {
        "null".into()
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use uaps_core::Metric;

    fn sample() -> Snapshot {
        let mut s = Snapshot::default();
        s.push(Metric {
            key: "elapsed_time",
            label: "Elapsed time".into(),
            value: MetricValue::Duration(Duration::from_millis(1500)),
        });
        s.push(Metric {
            key: "llc_mpki",
            label: "LLC misses / 1K instr".into(),
            value: MetricValue::Float { value: 20.0, unit: "MPKI" },
        });
        s
    }

    #[test]
    fn json_is_wellformed_ish() {
        let out = render_json(&sample());
        assert!(out.trim_start().starts_with('{'));
        assert!(out.contains("\"key\": \"elapsed_time\""));
        assert!(out.contains("\"value\": 1.5"));
        assert!(out.contains("\"display\": \"1.500 s\""));
    }

    #[test]
    fn formats_bytes_with_scale() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MiB");
    }
}
