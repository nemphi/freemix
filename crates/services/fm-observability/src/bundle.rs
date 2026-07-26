use std::fmt::Write;

use fm_capabilities::{CapabilityRegistry, Health};

use crate::{
    EventLog, EventValue, HealthRegistry, MetricKind, MetricSeries, MetricStore, Redactor,
};

const TRUNCATION_MARKER: &str = "\n[TRUNCATED]\n";

/// Result of applying a strict byte cap to a support bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleExport {
    pub text: String,
    pub truncated: bool,
    pub uncapped_bytes: usize,
}

/// A borrowed, point-in-time view of caller-owned observability state.
#[derive(Clone, Copy, Debug)]
pub struct SupportBundle<'a> {
    events: &'a EventLog,
    metrics: &'a MetricStore,
    health: &'a HealthRegistry,
    capabilities: &'a CapabilityRegistry,
    redactor: Redactor,
}

impl<'a> SupportBundle<'a> {
    #[must_use]
    pub const fn new(
        events: &'a EventLog,
        metrics: &'a MetricStore,
        health: &'a HealthRegistry,
        capabilities: &'a CapabilityRegistry,
    ) -> Self {
        Self {
            events,
            metrics,
            health,
            capabilities,
            redactor: Redactor,
        }
    }

    #[must_use]
    pub const fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// Produces stable-order JSON-like text no larger than `maximum_bytes`.
    #[must_use]
    pub fn export(self, maximum_bytes: usize) -> BundleExport {
        let text = self.render();
        let uncapped_bytes = text.len();
        if uncapped_bytes <= maximum_bytes {
            return BundleExport {
                text,
                truncated: false,
                uncapped_bytes,
            };
        }

        let text = if maximum_bytes <= TRUNCATION_MARKER.len() {
            utf8_prefix(TRUNCATION_MARKER, maximum_bytes).to_owned()
        } else {
            let prefix_bytes = maximum_bytes - TRUNCATION_MARKER.len();
            let mut capped = utf8_prefix(&text, prefix_bytes).to_owned();
            capped.push_str(TRUNCATION_MARKER);
            capped
        };
        BundleExport {
            text,
            truncated: true,
            uncapped_bytes,
        }
    }

    fn render(self) -> String {
        let mut output = String::from("{\n  \"schema\":\"fm-support-v1\",\n");
        self.render_health(&mut output);
        self.render_capabilities(&mut output);
        self.render_metrics(&mut output);
        self.render_events(&mut output);
        output.push_str("}\n");
        output
    }

    fn render_health(self, output: &mut String) {
        let aggregate = self.health.aggregate();
        let _ = writeln!(
            output,
            "  \"health\":{{\"live\":{},\"ready\":{},\"degraded\":{},\"status\":\"{}\",\"checks\":[",
            aggregate.live,
            aggregate.ready,
            aggregate.degraded,
            aggregate.health.as_str()
        );
        for (index, check) in self.health.iter().enumerate() {
            if index != 0 {
                output.push_str(",\n");
            }
            output.push_str("    {\"component\":");
            push_redacted_string(output, &check.component, self.redactor);
            let _ = write!(
                output,
                ",\"live\":{},\"ready\":{},\"status\":\"{}\",\"detail\":",
                check.live,
                check.ready,
                check.health.as_str()
            );
            push_optional_redacted_string(output, check.detail.as_deref(), self.redactor);
            output.push('}');
        }
        output.push_str("\n  ]},\n");
    }

    fn render_capabilities(self, output: &mut String) {
        output.push_str("  \"capabilities\":[\n");
        for (index, (_, capability)) in self.capabilities.iter().enumerate() {
            if index != 0 {
                output.push_str(",\n");
            }
            output.push_str("    {\"key\":");
            push_redacted_string(output, capability.key.as_str(), self.redactor);
            output.push_str(",\"provider\":");
            push_redacted_string(output, capability.provider.id.as_str(), self.redactor);
            output.push_str(",\"version\":");
            push_redacted_string(output, capability.provider.version.as_str(), self.redactor);
            let (health, reason) = match &capability.health {
                Health::Healthy => ("healthy", None),
                Health::Degraded { reason } => ("degraded", Some(reason.as_str())),
                Health::Unhealthy { reason } => ("unhealthy", Some(reason.as_str())),
            };
            let _ = write!(output, ",\"health\":\"{health}\",\"reason\":");
            push_optional_redacted_string(output, reason, self.redactor);
            let _ = write!(
                output,
                ",\"limits\":{},\"formats\":{},\"memory_domains\":{},\"latency_modes\":{} }}",
                capability.limits.len(),
                capability.formats.len(),
                capability.memory_domains.len(),
                capability.latency_modes.len()
            );
        }
        output.push_str("\n  ],\n");
    }

    fn render_metrics(self, output: &mut String) {
        output.push_str("  \"metrics\":[\n");
        for (index, (metric, series)) in self.metrics.iter().enumerate() {
            if index != 0 {
                output.push_str(",\n");
            }
            let _ = write!(
                output,
                "    {{\"name\":\"{}\",\"kind\":\"{}\",\"dropped\":{},\"summary\":",
                metric.as_str(),
                series.kind().as_str(),
                series.dropped()
            );
            render_metric_summary(output, series);
            output.push_str(",\"points\":[");
            for (point_index, point) in series.iter().enumerate() {
                if point_index != 0 {
                    output.push(',');
                }
                let _ = write!(
                    output,
                    "[{},{}]",
                    point.monotonic_millis,
                    JsonNumber(point.value)
                );
            }
            output.push_str("]}");
        }
        output.push_str("\n  ],\n");
    }

    fn render_events(self, output: &mut String) {
        let _ = writeln!(
            output,
            "  \"events\":{{\"capacity\":{},\"dropped\":{},\"records\":[",
            self.events.capacity(),
            self.events.dropped()
        );
        for (index, event) in self.events.iter().enumerate() {
            if index != 0 {
                output.push_str(",\n");
            }
            let _ = write!(
                output,
                "    {{\"sequence\":{},\"monotonic_ms\":{},\"severity\":\"{}\",\"category\":\"{}\",\"message\":",
                event.sequence,
                event.monotonic_millis,
                event.severity.as_str(),
                event.category.as_str()
            );
            push_redacted_string(output, &event.message, self.redactor);
            output.push_str(",\"fields\":{");
            for (field_index, field) in event.fields.iter().enumerate() {
                if field_index != 0 {
                    output.push(',');
                }
                push_redacted_string(output, &field.name, self.redactor);
                output.push(':');
                render_event_value(output, &field.value, &field.name, self.redactor);
            }
            output.push_str("}}");
        }
        output.push_str("\n  ]}\n");
    }
}

fn render_metric_summary(output: &mut String, series: &MetricSeries) {
    match series.kind() {
        MetricKind::Counter => {
            let summary = series.counter_summary().expect("counter kind");
            let _ = write!(
                output,
                "{{\"total\":{},\"updates\":{},\"retained\":{}}}",
                JsonNumber(summary.total),
                summary.updates,
                summary.retained_samples
            );
        }
        MetricKind::Gauge => {
            let summary = series.gauge_summary().expect("gauge kind");
            output.push_str("{\"current\":");
            push_optional_number(output, summary.current);
            output.push_str(",\"min\":");
            push_optional_number(output, summary.minimum);
            output.push_str(",\"max\":");
            push_optional_number(output, summary.maximum);
            let _ = write!(output, ",\"samples\":{}}}", summary.samples);
        }
        MetricKind::Histogram => {
            let summary = series.histogram_summary().expect("histogram kind");
            let _ = write!(output, "{{\"count\":{}", summary.count);
            for (name, value) in [
                ("min", summary.minimum),
                ("max", summary.maximum),
                ("mean", summary.mean),
                ("p50", summary.p50),
                ("p95", summary.p95),
                ("p99", summary.p99),
            ] {
                let _ = write!(output, ",\"{name}\":");
                push_optional_number(output, value);
            }
            output.push('}');
        }
    }
}

fn render_event_value(output: &mut String, value: &EventValue, name: &str, redactor: Redactor) {
    if redactor.is_secret_name(name) {
        push_json_string(output, Redactor::SECRET_MARKER);
        return;
    }
    match value {
        EventValue::Boolean(value) => output.push_str(if *value { "true" } else { "false" }),
        EventValue::Integer(value) => {
            let _ = write!(output, "{value}");
        }
        EventValue::Unsigned(value) => {
            let _ = write!(output, "{value}");
        }
        EventValue::Float(value) => {
            let _ = write!(output, "{}", JsonNumber(*value));
        }
        EventValue::Text(value) => push_redacted_string(output, value, redactor),
    }
}

fn push_optional_number(output: &mut String, value: Option<f64>) {
    if let Some(value) = value {
        let _ = write!(output, "{}", JsonNumber(value));
    } else {
        output.push_str("null");
    }
}

fn push_optional_redacted_string(output: &mut String, value: Option<&str>, redactor: Redactor) {
    if let Some(value) = value {
        push_redacted_string(output, value, redactor);
    } else {
        output.push_str("null");
    }
}

fn push_redacted_string(output: &mut String, value: &str, redactor: Redactor) {
    push_json_string(output, &redactor.redact(value));
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                let _ = write!(output, "\\u{:04x}", u32::from(character));
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

fn utf8_prefix(value: &str, maximum_bytes: usize) -> &str {
    let mut end = maximum_bytes.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

struct JsonNumber(f64);

impl std::fmt::Display for JsonNumber {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_finite() {
            self.0.fmt(formatter)
        } else {
            formatter.write_str("null")
        }
    }
}

#[cfg(test)]
mod tests {
    use fm_capabilities::{Capability, CapabilityKey, Provider, ProviderVersion, StableId};

    use crate::{Category, ComponentHealth, EventField, HealthCheck, Metric, Severity};

    use super::*;

    fn fixture() -> (EventLog, MetricStore, HealthRegistry, CapabilityRegistry) {
        let mut events = EventLog::new(4);
        events
            .record(
                7,
                Severity::Warning,
                Category::Network,
                "peer=10.0.0.8 token=do-not-export",
                [EventField::new("path", "/Users/alice/show.mov")],
            )
            .unwrap();
        let mut metrics = MetricStore::new(2);
        metrics
            .set_gauge(Metric::CpuUtilizationPercent, 7, 42.5)
            .unwrap();
        let mut health = HealthRegistry::new();
        health.update(
            HealthCheck::new("engine", true, true, ComponentHealth::Healthy)
                .with_detail("listening on 127.0.0.1"),
        );
        let mut capabilities = CapabilityRegistry::new();
        for (key, provider) in [("video.raw", "zeta"), ("audio.raw", "alpha")] {
            capabilities
                .register(Capability::new(
                    CapabilityKey::new(key).unwrap(),
                    Provider::new(
                        StableId::new(provider).unwrap(),
                        ProviderVersion::new("1").unwrap(),
                    ),
                ))
                .unwrap();
        }
        (events, metrics, health, capabilities)
    }

    #[test]
    fn support_bundle_is_deterministic_sorted_and_redacted() {
        let (events, metrics, health, capabilities) = fixture();
        let bundle = SupportBundle::new(&events, &metrics, &health, &capabilities);
        let first = bundle.export(usize::MAX);
        let second = bundle.export(usize::MAX);

        assert_eq!(first, second);
        assert!(!first.truncated);
        assert!(first.text.find("audio.raw") < first.text.find("video.raw"));
        for leaked in ["10.0.0.8", "do-not-export", "/Users/alice", "127.0.0.1"] {
            assert!(!first.text.contains(leaked));
        }
    }

    #[test]
    fn support_bundle_never_exceeds_size_cap() {
        let (events, metrics, health, capabilities) = fixture();
        let bundle = SupportBundle::new(&events, &metrics, &health, &capabilities);

        for cap in [0, 1, TRUNCATION_MARKER.len(), 64, 257] {
            let export = bundle.export(cap);
            assert!(export.truncated);
            assert!(export.text.len() <= cap);
            if cap > TRUNCATION_MARKER.len() {
                assert!(export.text.ends_with(TRUNCATION_MARKER));
            }
        }
    }
}
