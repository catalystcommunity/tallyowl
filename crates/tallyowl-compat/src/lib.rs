//! Compatibility receivers.
//!
//! `AGENTS.md` permits exactly two of these, and this crate holds both:
//!
//! - a **Prometheus and OpenMetrics scrape**, which is outbound, over HTTP,
//!   against the targets an operator names;
//! - an **OpenTelemetry metric and trace push**, which listens on the transport
//!   existing exporters already use.
//!
//! Both normalize at the collector. Nothing that arrives here travels further
//! in its original form: a scrape becomes native metric points, an
//! OpenTelemetry push becomes native metric points and spans, and every later
//! hop is CSIL.
//!
//! **OpenTelemetry logs are out of scope**, and the receiver says so rather
//! than accepting them quietly. See D12.
//!
//! # Neither one starts by itself
//!
//! D12: "A compatibility edge must be a choice that an operator makes, never a
//! port that appears because the binary contains the feature." So the scraper
//! has no default target and the receiver has no default listener. The
//! collector starts each one only when configuration asks for it.

pub mod exposition;
pub mod otlp;
pub mod protobuf;
pub mod receiver;
pub mod scrape;

use tallyowl_collector_api::types::{MetricKind, MetricPointPayload, Property};
use tallyowl_wire::{collector as wire, Value};

/// The identity of one metric series: the kind, and every label.
///
/// It lives here rather than beside the collector's budget because two places
/// need one rule. The budget counts a series and the scraper watches one for a
/// restart, and two rules that had to agree would drift the first time somebody
/// changed one of them.
///
/// The temporality is deliberately not part of it. A producer that changes
/// temporality mid run is reporting the same series a different way, and
/// counting it twice would double what an operator is charged for it.
pub fn series_key(point: &MetricPointPayload) -> String {
    let mut labels: Vec<(String, String)> = point
        .labels
        .iter()
        .map(|label| (label.key.clone(), label_text(label)))
        .collect();
    labels.sort();
    let mut key = String::with_capacity(64);
    key.push_str(match point.metric_kind {
        MetricKind::Counter => "counter",
        MetricKind::Gauge => "gauge",
        MetricKind::Histogram => "histogram",
    });
    for (name, value) in labels {
        key.push('\u{1}');
        key.push_str(&name);
        key.push('\u{2}');
        key.push_str(&value);
    }
    key
}

/// One label's value as text, for a series key and for a byte budget.
pub fn label_text(label: &Property) -> String {
    match wire::read(&label.value) {
        Ok(Value::Text(text)) => text,
        Ok(value) => value.to_display(),
        // A label this build cannot read still needs a stable identity, or two
        // unreadable labels would look like one series.
        Err(_) => format!("{:?}", label.value.kind),
    }
}
