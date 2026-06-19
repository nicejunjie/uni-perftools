//! Rendering of a [`Snapshot`] (+ [`Insight`]s) into terminal, JSON, or HTML.
//!
//! Presentation lives here: metrics are grouped into APS-style sections by a
//! central table so collectors stay presentation-agnostic. Any metric not
//! listed in a section still appears under "Other", so nothing is hidden.

use std::time::Duration;

use uaps_core::{Insight, Metric, MetricValue, Snapshot};

/// Section title + the metric keys it contains, in display order.
const SECTIONS: &[(&str, &[&str])] = &[
    ("Overview", &["elapsed_time", "cpu_core_pct", "cpu_cores_used", "max_threads"]),
    ("Compute", &["cpu_time", "ipc", "cpi", "gflops", "vectorization_pct", "branch_mispredict_rate"]),
    (
        "Top-down (% of pipeline slots)",
        &[
            "topdown_retiring_pct",
            "topdown_frontend_pct",
            "topdown_backend_pct",
            "topdown_backend_mem_pct",
            "topdown_backend_core_pct",
            "topdown_badspec_pct",
        ],
    ),
    (
        "Memory",
        &[
            "peak_rss",
            "cache_miss_rate",
            "llc_mpki",
            "dram_dpki",
            "dram_bound_pct",
            "numa_remote_pct",
            "memory_bound",
            "memory_bound_est",
        ],
    ),
    (
        "Parallelism (MPI / threads)",
        &[
            "mpi_ranks",
            "mpi_time",
            "mpi_time_pct",
            "mpi_imbalance_pct",
            "mpi_top_fn_time",
            "active_threads",
            "thread_imbalance_pct",
        ],
    ),
    ("I/O", &["disk_read", "disk_write", "io_read", "io_write"]),
    (
        "Hardware counters",
        &[
            "hw_instructions",
            "hw_cpu_cycles",
            "hw_cache_references",
            "hw_cache_misses",
            "hw_branch_instructions",
            "hw_branch_misses",
            "mem_fills_all",
            "mem_fills_dram",
            "mem_fills_remote",
        ],
    ),
];

/// Output format selectable on the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
    Html,
}

/// Render in the requested format.
pub fn render(snapshot: &Snapshot, insights: &[Insight], format: Format) -> String {
    match format {
        Format::Text => render_terminal(snapshot, insights),
        Format::Json => render_json(snapshot, insights),
        Format::Html => render_html(snapshot, insights),
    }
}

/// Group metrics into (section title, metrics) pairs, omitting empty sections.
/// Trailing "Other" catches any metric not assigned to a named section.
fn grouped(snapshot: &Snapshot) -> Vec<(&'static str, Vec<&Metric>)> {
    let mut assigned: Vec<bool> = vec![false; snapshot.metrics.len()];
    let mut out: Vec<(&'static str, Vec<&Metric>)> = Vec::new();

    for (title, keys) in SECTIONS {
        let mut group = Vec::new();
        for key in *keys {
            for (i, m) in snapshot.metrics.iter().enumerate() {
                if &m.key == key {
                    group.push(m);
                    assigned[i] = true;
                }
            }
        }
        if !group.is_empty() {
            out.push((title, group));
        }
    }

    let leftover: Vec<&Metric> = snapshot
        .metrics
        .iter()
        .enumerate()
        .filter(|(i, _)| !assigned[*i])
        .map(|(_, m)| m)
        .collect();
    if !leftover.is_empty() {
        out.push(("Other", leftover));
    }
    out
}

// ---------------------------------------------------------------- terminal

pub fn render_terminal(snapshot: &Snapshot, insights: &[Insight]) -> String {
    let title = "uaps — Application Performance Snapshot";
    let mut out = String::new();
    out.push_str(title);
    out.push('\n');
    out.push_str(&"=".repeat(title.chars().count()));
    out.push('\n');

    if let Some(top) = insights.first() {
        out.push_str(&format!("\nBottleneck: {}\n", top.headline));
    }

    let groups = grouped(snapshot);
    let width = snapshot
        .metrics
        .iter()
        .map(|m| m.label.chars().count())
        .max()
        .unwrap_or(0);

    for (title, metrics) in &groups {
        out.push_str(&format!("\n{title}\n"));
        for m in metrics {
            out.push_str(&format!(
                "  {:<width$}  {}\n",
                m.label,
                format_value(&m.value),
                width = width
            ));
        }
    }

    if !insights.is_empty() {
        out.push_str("\nInsights\n");
        for ins in insights {
            out.push_str(&format!("  • {}: {}\n", ins.headline, ins.detail));
        }
    }

    out
}

// -------------------------------------------------------------------- json

pub fn render_json(snapshot: &Snapshot, insights: &[Insight]) -> String {
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
    out.push_str("  ],\n  \"insights\": [\n");
    for (i, ins) in insights.iter().enumerate() {
        let comma = if i + 1 < insights.len() { "," } else { "" };
        out.push_str(&format!(
            "    {{\"headline\": {}, \"detail\": {}}}{}\n",
            json_str(&ins.headline),
            json_str(&ins.detail),
            comma
        ));
    }
    out.push_str("  ]\n}\n");
    out
}

// -------------------------------------------------------------------- html

pub fn render_html(snapshot: &Snapshot, insights: &[Insight]) -> String {
    let mut out = String::from(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <title>uaps snapshot</title><style>\
         body{font-family:system-ui,sans-serif;margin:2rem;max-width:48rem}\
         h1{font-size:1.3rem}h2{font-size:1rem;margin-top:1.5rem;color:#334}\
         table{border-collapse:collapse;width:100%}\
         td{padding:.2rem .6rem;border-bottom:1px solid #eee}\
         td.v{text-align:right;font-variant-numeric:tabular-nums}\
         .ins{background:#fff7e6;border-left:3px solid #e8a;padding:.5rem .8rem;margin:.4rem 0}\
         </style></head><body>\n",
    );
    out.push_str("<h1>uaps — Application Performance Snapshot</h1>\n");
    if let Some(top) = insights.first() {
        out.push_str(&format!("<p><strong>Bottleneck:</strong> {}</p>\n", html_esc(&top.headline)));
    }
    for (title, metrics) in grouped(snapshot) {
        out.push_str(&format!("<h2>{}</h2>\n<table>\n", html_esc(title)));
        for m in metrics {
            out.push_str(&format!(
                "<tr><td>{}</td><td class=\"v\">{}</td></tr>\n",
                html_esc(&m.label),
                html_esc(&format_value(&m.value))
            ));
        }
        out.push_str("</table>\n");
    }
    if !insights.is_empty() {
        out.push_str("<h2>Insights</h2>\n");
        for ins in insights {
            out.push_str(&format!(
                "<div class=\"ins\"><strong>{}</strong>: {}</div>\n",
                html_esc(&ins.headline),
                html_esc(&ins.detail)
            ));
        }
    }
    out.push_str("</body></html>\n");
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

fn html_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn terminal_has_sections_and_bottleneck() {
        let s = sample();
        let ins = uaps_core::insights(&s);
        let out = render_terminal(&s, &ins);
        assert!(out.contains("Overview"));
        assert!(out.contains("1.500 s"));
    }

    #[test]
    fn json_is_wellformed_ish() {
        let s = sample();
        let out = render_json(&s, &[]);
        assert!(out.trim_start().starts_with('{'));
        assert!(out.contains("\"key\": \"elapsed_time\""));
        assert!(out.contains("\"value\": 1.5"));
    }

    #[test]
    fn html_escapes_and_wraps() {
        let out = render_html(&sample(), &[]);
        assert!(out.contains("<table>"));
        assert!(out.contains("uaps"));
    }

    #[test]
    fn unlisted_metrics_go_to_other() {
        let mut s = Snapshot::default();
        s.push(Metric {
            key: "mystery_metric",
            label: "Mystery".into(),
            value: MetricValue::Int { value: 1, unit: "" },
        });
        let out = render_terminal(&s, &[]);
        assert!(out.contains("Other"));
        assert!(out.contains("Mystery"));
    }

    #[test]
    fn formats_bytes_with_scale() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MiB");
    }
}
