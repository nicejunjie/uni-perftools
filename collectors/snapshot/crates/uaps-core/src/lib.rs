//! Core types shared across `uaps`: the normalized [`Metric`] model and the
//! [`Collector`] trait that every data-collection backend implements.
//!
//! Backends (in `uaps-collect`) emit raw or derived [`Metric`]s; the CLI
//! orchestrates them over a [`Target`] and hands the aggregated [`Snapshot`]
//! to the reporter (`uaps-report`). Keeping this contract small and
//! source-agnostic is what lets new platforms plug in without touching the
//! rest of the system.

mod collector;
mod derive;
mod metric;

pub use collector::{Collector, Target};
pub use derive::derive;
pub use metric::{Metric, MetricValue, Snapshot};
