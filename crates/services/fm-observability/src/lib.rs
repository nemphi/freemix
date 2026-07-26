//! Bounded operational events, metrics, health, alerts, and support bundles.
//!
//! All storage is caller-owned. The crate does not install a global recorder or
//! spawn background work, which keeps it suitable for engine, service, and test
//! composition roots alike.

mod alert;
mod bundle;
mod event;
mod health;
mod metric;
mod redact;

pub use alert::{
    AlertPolicy, AlertPolicyError, AlertState, AlertTransition, ThresholdAlert, ThresholdDirection,
};
pub use bundle::{BundleExport, SupportBundle};
pub use event::{
    Category, EventField, EventLog, EventRecord, EventValue, SequenceExhausted, Severity,
};
pub use health::{ComponentHealth, HealthAggregate, HealthCheck, HealthRegistry};
pub use metric::{
    CounterSummary, GaugeSummary, HistogramSummary, Metric, MetricError, MetricKind, MetricPoint,
    MetricSeries, MetricStore,
};
pub use redact::Redactor;
