use std::time::Duration;

/// A single normalized measurement, independent of how it was collected.
///
/// `key` is a stable machine identifier (used by JSON export and tests);
/// `label` is the human-facing name shown in the terminal snapshot.
#[derive(Debug, Clone)]
pub struct Metric {
    pub key: &'static str,
    pub label: String,
    pub value: MetricValue,
}

/// The typed value of a [`Metric`]. The reporter knows how to format each
/// variant, so collectors never format numbers themselves.
#[derive(Debug, Clone)]
pub enum MetricValue {
    Duration(Duration),
    Float { value: f64, unit: &'static str },
    Int { value: i64, unit: &'static str },
    Percent(f64),
    Bytes(u64),
}

impl MetricValue {
    /// Numeric view of the value (seconds for durations, raw bytes for
    /// `Bytes`), used by the derivation engine to compute ratios.
    pub fn as_f64(&self) -> f64 {
        match self {
            MetricValue::Duration(d) => d.as_secs_f64(),
            MetricValue::Float { value, .. } => *value,
            MetricValue::Int { value, .. } => *value as f64,
            MetricValue::Percent(p) => *p,
            MetricValue::Bytes(b) => *b as f64,
        }
    }
}

/// The aggregated result of a profiling session: every metric produced by
/// every collector, in collection order.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub metrics: Vec<Metric>,
}

impl Snapshot {
    pub fn push(&mut self, metric: Metric) {
        self.metrics.push(metric);
    }

    pub fn extend(&mut self, metrics: impl IntoIterator<Item = Metric>) {
        self.metrics.extend(metrics);
    }

    /// Numeric value of the metric with this `key`, if present.
    pub fn numeric(&self, key: &str) -> Option<f64> {
        self.metrics.iter().find(|m| m.key == key).map(|m| m.value.as_f64())
    }
}
