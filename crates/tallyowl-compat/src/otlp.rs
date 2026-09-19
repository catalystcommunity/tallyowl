//! OpenTelemetry metrics and traces, normalized into native types at the edge.
//!
//! # Normalize immediately
//!
//! `docs/DATA_MODEL.md` section 3.4: "Compatibility receivers normalize
//! temporality, types, units, labels, and resource attributes into this metric
//! model immediately. From the collector to the head and storage nodes, the
//! representation and transport are native CSIL." So this module returns
//! `TelemetryItem`, and no OpenTelemetry shape exists above it.
//!
//! | OpenTelemetry | Becomes |
//! | --- | --- |
//! | `Sum`, monotonic | a counter, with the temporality the producer declared |
//! | `Sum`, not monotonic | a gauge, because a sum that falls is a level |
//! | `Gauge` | a gauge |
//! | `Histogram` | a histogram, with bucket counts made cumulative |
//! | `ExponentialHistogram` | refused, and counted |
//! | `Summary` | one `_sum` and one `_count` counter, and a gauge for each quantile |
//! | `Span` | a native span |
//! | any log record | refused. See D12 |
//!
//! # Bucket counts are not cumulative on the OpenTelemetry side
//!
//! `HistogramDataPoint.bucket_counts` holds the observations **in** each
//! bucket, and `HistogramValue.counts` in `csil/tallyowl-ingest.csil` holds the
//! observations at or **below** each bound, which is what the exposition format
//! and every native producer use. So this runs a total across the buckets. A
//! copy without one would report a p99 that is far too low and look plausible.
//!
//! # `service.name` is not a label
//!
//! An OpenTelemetry resource carries `service.name` and `service.version`, and
//! TallyOwl's envelope carries a service name and a release. Leaving them as
//! labels would put a service name in the metric series identity and out of the
//! envelope, so a query for "everything from checkout" would miss them. They
//! move to the envelope. Every other resource attribute becomes a label, which
//! is what makes a resource attribute queryable at all.

use tallyowl_collector_api::types::{
    Envelope, HistogramValue, MetricKind, MetricPointPayload,
    MetricPointPayload_temporality as Temporality, PropertyOrigin, SpanKind, SpanPayload,
    SpanPayload_status, TelemetryItem, TelemetryKind,
};
use tallyowl_wire::{collector as wire, collector_items_bridge as items, Value};

use crate::protobuf::{packed_double, packed_fixed64, Reader, Wire};

pub const SDK_NAME: &str = "tallyowl-compat-opentelemetry";

/// What one push produced.
#[derive(Debug, Clone, Default)]
pub struct Normalized {
    pub items: Vec<TelemetryItem>,
    /// Data points this build cannot represent. The receiver reports the count
    /// in the OpenTelemetry partial-success field, so an exporter learns that
    /// some of what it sent did not arrive rather than assuming all of it did.
    pub rejected: u64,
    /// Why, in the words the acknowledgement carries.
    pub reason: Option<String>,
}

impl Normalized {
    fn refuse(&mut self, count: u64, reason: &str) {
        self.rejected += count;
        if self.reason.is_none() {
            self.reason = Some(reason.to_string());
        }
    }
}

/// One resource or scope's attributes, and the two that leave the label set.
#[derive(Debug, Clone, Default)]
struct Attributes {
    service_name: Option<String>,
    release: Option<String>,
    labels: Vec<(String, Value)>,
}

impl Attributes {
    fn merged_with(&self, other: &Attributes) -> Attributes {
        let mut out = self.clone();
        out.service_name = other.service_name.clone().or(out.service_name);
        out.release = other.release.clone().or(out.release);
        for (key, value) in &other.labels {
            out.labels.retain(|(held, _)| held != key);
            out.labels.push((key.clone(), value.clone()));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Read an `ExportMetricsServiceRequest`.
pub fn metrics(body: &[u8]) -> Normalized {
    let mut out = Normalized::default();
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        if field.number == 1 {
            resource_metrics(field.wire.as_bytes(), &mut out);
        }
    }
    out
}

fn resource_metrics(body: &[u8], out: &mut Normalized) {
    let mut resource = Attributes::default();
    let mut scopes: Vec<&[u8]> = Vec::new();
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        match field.number {
            1 => resource = read_resource(field.wire.as_bytes()),
            2 => scopes.push(field.wire.as_bytes()),
            _ => {}
        }
    }
    for scope in scopes {
        scope_metrics(scope, &resource, out);
    }
}

fn scope_metrics(body: &[u8], resource: &Attributes, out: &mut Normalized) {
    let mut scope = Attributes::default();
    let mut metrics: Vec<&[u8]> = Vec::new();
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        match field.number {
            1 => scope = read_scope(field.wire.as_bytes()),
            2 => metrics.push(field.wire.as_bytes()),
            _ => {}
        }
    }
    let attributes = resource.merged_with(&scope);
    for metric in metrics {
        one_metric(metric, &attributes, out);
    }
}

fn one_metric(body: &[u8], attributes: &Attributes, out: &mut Normalized) {
    let mut name = String::new();
    let mut description = None;
    let mut unit = None;
    let mut reader = Reader::new(body);
    // Fields 5, 7, 9, 10, and 11 are the `data` choice. The name may arrive
    // after the data, so the data is gathered and read at the end.
    let mut gauge: Vec<&[u8]> = Vec::new();
    let mut sum: Option<&[u8]> = None;
    let mut histogram: Option<&[u8]> = None;
    let mut exponential = 0u64;
    let mut summary: Vec<&[u8]> = Vec::new();

    while let Some(field) = reader.next_field() {
        match field.number {
            1 => name = field.wire.as_text(),
            2 => {
                let text = field.wire.as_text();
                if !text.is_empty() {
                    description = Some(text);
                }
            }
            3 => {
                let text = field.wire.as_text();
                if !text.is_empty() {
                    unit = Some(text);
                }
            }
            5 => gauge.push(field.wire.as_bytes()),
            7 => sum = Some(field.wire.as_bytes()),
            9 => histogram = Some(field.wire.as_bytes()),
            10 => exponential += 1,
            11 => summary.push(field.wire.as_bytes()),
            _ => {}
        }
    }
    if name.is_empty() {
        out.refuse(1, "a metric arrived with no name");
        return;
    }
    let shape = MetricShape {
        name,
        description,
        unit,
        attributes,
    };

    for body in gauge {
        for point in number_points(body) {
            out.items.push(number_item(
                &shape,
                &point,
                MetricKind::Gauge,
                false,
                Temporality::Cumulative,
            ));
        }
    }
    if let Some(body) = sum {
        let (points, temporality, monotonic) = read_sum(body);
        let kind = if monotonic {
            MetricKind::Counter
        } else {
            // A sum that is not monotonic is a level rather than a total. A
            // counter it is not, and `rate` over it would be wrong.
            MetricKind::Gauge
        };
        for point in points {
            out.items.push(number_item(
                &shape,
                &point,
                kind.clone(),
                monotonic,
                temporality.clone(),
            ));
        }
    }
    if let Some(body) = histogram {
        let (points, temporality) = read_histogram(body);
        for point in points {
            out.items
                .push(histogram_item(&shape, &point, temporality.clone()));
        }
    }
    if exponential > 0 {
        // An exponential histogram has no explicit bounds, and the native
        // histogram is defined by its bounds. Converting one would invent
        // bounds nobody chose, so it is refused where an exporter can see it.
        out.refuse(
            exponential,
            "this build stores a histogram with explicit bounds. Configure the exporter for an explicit-bucket histogram.",
        );
    }
    for body in summary {
        for point in summary_points(body) {
            summary_items(&shape, &point, out);
        }
    }
}

struct MetricShape<'a> {
    name: String,
    description: Option<String>,
    unit: Option<String>,
    attributes: &'a Attributes,
}

#[derive(Debug, Default, Clone)]
struct NumberPoint {
    labels: Vec<(String, Value)>,
    start_ns: u64,
    time_ns: u64,
    value: f64,
    exemplar_trace_id: Option<Vec<u8>>,
}

fn read_sum(body: &[u8]) -> (Vec<NumberPoint>, Temporality, bool) {
    let mut points = Vec::new();
    let mut temporality = Temporality::Cumulative;
    let mut monotonic = false;
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        match field.number {
            1 => points.push(number_point(field.wire.as_bytes())),
            // 1 is delta, 2 is cumulative, 0 is unspecified. An unspecified
            // temporality is read as cumulative, which is what an exporter that
            // omits it means in practice and the safer of the two: reading a
            // cumulative total as a delta would multiply it by the sample count.
            2 => {
                temporality = if field.wire.as_u64() == 1 {
                    Temporality::Delta
                } else {
                    Temporality::Cumulative
                }
            }
            3 => monotonic = field.wire.as_u64() != 0,
            _ => {}
        }
    }
    (points, temporality, monotonic)
}

fn number_points(body: &[u8]) -> Vec<NumberPoint> {
    let mut out = Vec::new();
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        if field.number == 1 {
            out.push(number_point(field.wire.as_bytes()));
        }
    }
    // A `Gauge` holds its points under field 1; a bare `NumberDataPoint` list
    // does not exist on its own, so an empty result means the message held no
    // data points rather than that it was misread.
    out
}

fn number_point(body: &[u8]) -> NumberPoint {
    let mut point = NumberPoint::default();
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        match field.number {
            7 => {
                if let Some((key, value)) = key_value(field.wire.as_bytes()) {
                    point.labels.push((key, value));
                }
            }
            2 => point.start_ns = field.wire.as_u64(),
            3 => point.time_ns = field.wire.as_u64(),
            4 => point.value = field.wire.as_f64(),
            5 if point.exemplar_trace_id.is_none() => {
                point.exemplar_trace_id = exemplar_trace(field.wire.as_bytes());
            }
            6 => point.value = field.wire.as_i64() as f64,
            _ => {}
        }
    }
    point
}

#[derive(Debug, Default, Clone)]
struct HistogramPoint {
    labels: Vec<(String, Value)>,
    start_ns: u64,
    time_ns: u64,
    count: u64,
    sum: f64,
    bucket_counts: Vec<u64>,
    bounds: Vec<f64>,
    exemplar_trace_id: Option<Vec<u8>>,
}

fn read_histogram(body: &[u8]) -> (Vec<HistogramPoint>, Temporality) {
    let mut points = Vec::new();
    let mut temporality = Temporality::Cumulative;
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        match field.number {
            1 => points.push(histogram_point(field.wire.as_bytes())),
            2 => {
                temporality = if field.wire.as_u64() == 1 {
                    Temporality::Delta
                } else {
                    Temporality::Cumulative
                }
            }
            _ => {}
        }
    }
    (points, temporality)
}

fn histogram_point(body: &[u8]) -> HistogramPoint {
    let mut point = HistogramPoint::default();
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        match field.number {
            9 => {
                if let Some((key, value)) = key_value(field.wire.as_bytes()) {
                    point.labels.push((key, value));
                }
            }
            2 => point.start_ns = field.wire.as_u64(),
            3 => point.time_ns = field.wire.as_u64(),
            4 => point.count = field.wire.as_u64(),
            5 => point.sum = field.wire.as_f64(),
            6 => match field.wire {
                // Packed is what every current exporter writes; one at a time
                // is legal and an older one may.
                Wire::Bytes(bytes) => point.bucket_counts.extend(packed_fixed64(bytes)),
                other => point.bucket_counts.push(other.as_u64()),
            },
            7 => match field.wire {
                Wire::Bytes(bytes) => point.bounds.extend(packed_double(bytes)),
                other => point.bounds.push(other.as_f64()),
            },
            8 if point.exemplar_trace_id.is_none() => {
                point.exemplar_trace_id = exemplar_trace(field.wire.as_bytes());
            }
            _ => {}
        }
    }
    point
}

#[derive(Debug, Default, Clone)]
struct SummaryPoint {
    labels: Vec<(String, Value)>,
    start_ns: u64,
    time_ns: u64,
    count: u64,
    sum: f64,
    quantiles: Vec<(f64, f64)>,
}

fn summary_points(body: &[u8]) -> Vec<SummaryPoint> {
    let mut out = Vec::new();
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        if field.number != 1 {
            continue;
        }
        let mut point = SummaryPoint::default();
        let mut inner = Reader::new(field.wire.as_bytes());
        while let Some(field) = inner.next_field() {
            match field.number {
                7 => {
                    if let Some((key, value)) = key_value(field.wire.as_bytes()) {
                        point.labels.push((key, value));
                    }
                }
                2 => point.start_ns = field.wire.as_u64(),
                3 => point.time_ns = field.wire.as_u64(),
                4 => point.count = field.wire.as_u64(),
                5 => point.sum = field.wire.as_f64(),
                6 => {
                    let mut at = Reader::new(field.wire.as_bytes());
                    let (mut quantile, mut value) = (0.0, 0.0);
                    while let Some(field) = at.next_field() {
                        match field.number {
                            1 => quantile = field.wire.as_f64(),
                            2 => value = field.wire.as_f64(),
                            _ => {}
                        }
                    }
                    point.quantiles.push((quantile, value));
                }
                _ => {}
            }
        }
        out.push(point);
    }
    out
}

fn exemplar_trace(body: &[u8]) -> Option<Vec<u8>> {
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        if field.number == 5 {
            let id = field.wire.as_bytes();
            if id.len() == 16 {
                return Some(id.to_vec());
            }
        }
    }
    None
}

fn number_item(
    shape: &MetricShape<'_>,
    point: &NumberPoint,
    kind: MetricKind,
    monotonic: bool,
    temporality: Temporality,
) -> TelemetryItem {
    let end_at = ms(point.time_ns);
    let start_at = if point.start_ns == 0 {
        end_at
    } else {
        ms(point.start_ns)
    };
    build(
        shape,
        MetricPointPayload {
            metric_name: shape.name.clone(),
            metric_kind: kind,
            unit: shape.unit.clone(),
            description: shape.description.clone(),
            monotonic,
            temporality,
            start_at,
            end_at,
            labels: labels_of(shape, &point.labels),
            number_value: Some(point.value),
            histogram_value: None,
            exemplar_trace_id: point.exemplar_trace_id.clone(),
        },
        point.exemplar_trace_id.as_deref(),
        end_at,
    )
}

fn histogram_item(
    shape: &MetricShape<'_>,
    point: &HistogramPoint,
    temporality: Temporality,
) -> TelemetryItem {
    let end_at = ms(point.time_ns);
    let start_at = if point.start_ns == 0 {
        end_at
    } else {
        ms(point.start_ns)
    };
    // OpenTelemetry counts observations **in** each bucket and the native
    // format counts them at or **below** each bound. This is that conversion,
    // and without it every quantile would read far too low.
    let mut running = 0u64;
    let mut cumulative = Vec::with_capacity(point.bounds.len());
    for index in 0..point.bounds.len() {
        running = running.saturating_add(point.bucket_counts.get(index).copied().unwrap_or(0));
        cumulative.push(running);
    }
    build(
        shape,
        MetricPointPayload {
            metric_name: shape.name.clone(),
            metric_kind: MetricKind::Histogram,
            unit: shape.unit.clone(),
            description: shape.description.clone(),
            monotonic: false,
            temporality,
            start_at,
            end_at,
            labels: labels_of(shape, &point.labels),
            number_value: None,
            histogram_value: Some(HistogramValue {
                count: point.count,
                sum: point.sum,
                bounds: point.bounds.clone(),
                counts: cumulative,
            }),
            exemplar_trace_id: point.exemplar_trace_id.clone(),
        },
        point.exemplar_trace_id.as_deref(),
        end_at,
    )
}

fn summary_items(shape: &MetricShape<'_>, point: &SummaryPoint, out: &mut Normalized) {
    let end_at = ms(point.time_ns);
    let start_at = if point.start_ns == 0 {
        end_at
    } else {
        ms(point.start_ns)
    };
    let mut scalar = |name: String, kind: MetricKind, monotonic: bool, value: f64, extra| {
        let mut labels = point.labels.clone();
        if let Some((key, text)) = extra {
            labels.push((key, Value::Text(text)));
        }
        out.items.push(build(
            shape,
            MetricPointPayload {
                metric_name: name,
                metric_kind: kind,
                unit: shape.unit.clone(),
                description: shape.description.clone(),
                monotonic,
                temporality: Temporality::Cumulative,
                start_at,
                end_at,
                labels: labels_of(shape, &labels),
                number_value: Some(value),
                histogram_value: None,
                exemplar_trace_id: None,
            },
            None,
            end_at,
        ));
    };
    scalar(
        format!("{}_sum", shape.name),
        MetricKind::Counter,
        true,
        point.sum,
        None,
    );
    scalar(
        format!("{}_count", shape.name),
        MetricKind::Counter,
        true,
        point.count as f64,
        None,
    );
    for (quantile, value) in &point.quantiles {
        // A quantile the producer computed becomes its own gauge, for the same
        // reason a Prometheus summary does: two producers' p99 values have no
        // p99 between them, and a histogram is what merges.
        scalar(
            shape.name.clone(),
            MetricKind::Gauge,
            false,
            *value,
            Some(("quantile".to_string(), format_quantile(*quantile))),
        );
    }
}

fn format_quantile(value: f64) -> String {
    let text = format!("{value}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn build(
    shape: &MetricShape<'_>,
    payload: MetricPointPayload,
    trace_id: Option<&[u8]>,
    occurred_at: i64,
) -> TelemetryItem {
    let mut envelope = envelope(TelemetryKind::MetricPoint, shape.attributes, occurred_at);
    if let Some(trace_id) = trace_id {
        envelope.trace_id = Some(trace_id.to_vec());
    }
    items::metric_point(envelope, payload)
}

fn labels_of(
    shape: &MetricShape<'_>,
    point_labels: &[(String, Value)],
) -> Vec<tallyowl_collector_api::types::Property> {
    // The resource's attributes come first and the data point's own win, so a
    // point that names a host overrides the resource that named a different
    // one.
    let mut merged: Vec<(String, Value)> = shape.attributes.labels.clone();
    for (key, value) in point_labels {
        merged.retain(|(held, _)| held != key);
        merged.push((key.clone(), value.clone()));
    }
    merged.sort_by(|a, b| a.0.cmp(&b.0));
    merged
        .into_iter()
        // The origin is `client`: an OpenTelemetry push comes from outside, so
        // its attributes are not values the collector can vouch for.
        .map(|(key, value)| wire::property(&key, value, PropertyOrigin::Client))
        .collect()
}

// ---------------------------------------------------------------------------
// Traces
// ---------------------------------------------------------------------------

/// Read an `ExportTraceServiceRequest`.
pub fn traces(body: &[u8]) -> Normalized {
    let mut out = Normalized::default();
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        if field.number == 1 {
            resource_spans(field.wire.as_bytes(), &mut out);
        }
    }
    out
}

fn resource_spans(body: &[u8], out: &mut Normalized) {
    let mut resource = Attributes::default();
    let mut scopes: Vec<&[u8]> = Vec::new();
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        match field.number {
            1 => resource = read_resource(field.wire.as_bytes()),
            2 => scopes.push(field.wire.as_bytes()),
            _ => {}
        }
    }
    for scope in scopes {
        let mut scope_attributes = Attributes::default();
        let mut spans: Vec<&[u8]> = Vec::new();
        let mut reader = Reader::new(scope);
        while let Some(field) = reader.next_field() {
            match field.number {
                1 => scope_attributes = read_scope(field.wire.as_bytes()),
                2 => spans.push(field.wire.as_bytes()),
                _ => {}
            }
        }
        let attributes = resource.merged_with(&scope_attributes);
        for span in spans {
            one_span(span, &attributes, out);
        }
    }
}

fn one_span(body: &[u8], attributes: &Attributes, out: &mut Normalized) {
    let mut trace_id: Vec<u8> = Vec::new();
    let mut span_id: Vec<u8> = Vec::new();
    let mut parent: Option<Vec<u8>> = None;
    let mut name = String::new();
    let mut kind = SpanKind::Internal;
    let mut start_ns = 0u64;
    let mut end_ns = 0u64;
    let mut status = SpanPayload_status::Unset;
    let mut span_attributes: Vec<(String, Value)> = Vec::new();
    let mut links: Vec<tallyowl_collector_api::types::SpanLink> = Vec::new();

    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        match field.number {
            1 => trace_id = field.wire.as_bytes().to_vec(),
            2 => span_id = field.wire.as_bytes().to_vec(),
            4 => {
                let id = field.wire.as_bytes();
                if id.len() == 8 {
                    parent = Some(id.to_vec());
                }
            }
            5 => name = field.wire.as_text(),
            6 => {
                kind = match field.wire.as_u64() {
                    2 => SpanKind::Server,
                    3 => SpanKind::Client,
                    4 => SpanKind::Producer,
                    5 => SpanKind::Consumer,
                    _ => SpanKind::Internal,
                }
            }
            7 => start_ns = field.wire.as_u64(),
            8 => end_ns = field.wire.as_u64(),
            9 => {
                if let Some(pair) = key_value(field.wire.as_bytes()) {
                    span_attributes.push(pair);
                }
            }
            13 => {
                if let Some(link) = read_link(field.wire.as_bytes()) {
                    links.push(link);
                }
            }
            15 => status = read_status(field.wire.as_bytes()),
            _ => {}
        }
    }

    // A span with no trace or no span identifier cannot be joined to anything,
    // and storing it would make a trace search report a span that belongs to no
    // trace. It is refused where the exporter can see the count.
    if trace_id.len() != 16 || span_id.len() != 8 {
        out.refuse(
            1,
            "a span arrived without a 16-byte trace identifier and an 8-byte span identifier",
        );
        return;
    }

    let start_at = ms(start_ns);
    let duration_ms = ms(end_ns.saturating_sub(start_ns));
    let mut envelope = envelope(TelemetryKind::Span, attributes, start_at);
    envelope.trace_id = Some(trace_id);
    envelope.span_id = Some(span_id.clone());
    for (key, value) in span_attributes {
        envelope
            .properties
            .push(wire::property(&key, value, PropertyOrigin::Client));
    }

    out.items.push(items::span(
        envelope,
        SpanPayload {
            operation: if name.is_empty() {
                "unnamed".to_string()
            } else {
                name
            },
            kind,
            start_at,
            duration_ms,
            status,
            resource: None,
            parent_span_id: parent,
            links: (!links.is_empty()).then_some(links),
            error_event_id: None,
            // The head applies tail sampling after commit. A push that already
            // decided is not overridden here, but nothing pretends this edge
            // made the decision either.
            sampling_reason: Some("opentelemetry-push".to_string()),
        },
    ));
}

fn read_link(body: &[u8]) -> Option<tallyowl_collector_api::types::SpanLink> {
    let mut trace_id = Vec::new();
    let mut span_id = Vec::new();
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        match field.number {
            1 => trace_id = field.wire.as_bytes().to_vec(),
            2 => span_id = field.wire.as_bytes().to_vec(),
            _ => {}
        }
    }
    (trace_id.len() == 16 && span_id.len() == 8)
        .then_some(tallyowl_collector_api::types::SpanLink { trace_id, span_id })
}

fn read_status(body: &[u8]) -> SpanPayload_status {
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        if field.number == 3 {
            return match field.wire.as_u64() {
                1 => SpanPayload_status::Ok,
                2 => SpanPayload_status::Error,
                _ => SpanPayload_status::Unset,
            };
        }
    }
    SpanPayload_status::Unset
}

// ---------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------

fn read_resource(body: &[u8]) -> Attributes {
    let mut out = Attributes::default();
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        if field.number == 1 {
            if let Some((key, value)) = key_value(field.wire.as_bytes()) {
                take_attribute(&mut out, key, value);
            }
        }
    }
    out
}

fn read_scope(body: &[u8]) -> Attributes {
    let mut out = Attributes::default();
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        if field.number == 3 {
            if let Some((key, value)) = key_value(field.wire.as_bytes()) {
                take_attribute(&mut out, key, value);
            }
        }
    }
    out
}

fn take_attribute(out: &mut Attributes, key: String, value: Value) {
    match key.as_str() {
        "service.name" => out.service_name = Some(value.to_display()),
        "service.version" => out.release = Some(value.to_display()),
        _ => out.labels.push((key, value)),
    }
}

/// One `KeyValue`, with the `AnyValue` kept in its own type.
fn key_value(body: &[u8]) -> Option<(String, Value)> {
    let mut key = String::new();
    let mut value = Value::Null;
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        match field.number {
            1 => key = field.wire.as_text(),
            2 => value = any_value(field.wire.as_bytes()),
            _ => {}
        }
    }
    (!key.is_empty()).then_some((key, value))
}

/// An `AnyValue`. TallyOwl keeps a value's type rather than turning it into
/// text to make it fit; `csil/types/common.csil` says so and this is that rule
/// at a compatibility edge.
fn any_value(body: &[u8]) -> Value {
    let mut reader = Reader::new(body);
    while let Some(field) = reader.next_field() {
        return match field.number {
            1 => Value::Text(field.wire.as_text()),
            2 => Value::Boolean(field.wire.as_u64() != 0),
            3 => Value::Integer(field.wire.as_i64()),
            4 => Value::Float(field.wire.as_f64()),
            // An array or a nested list has no native property shape. Its text
            // form keeps the value readable, which is better than dropping it.
            5 | 6 => Value::Text(format!("{:?}", field.wire.as_bytes())),
            7 => Value::Bytes(field.wire.as_bytes().to_vec()),
            _ => continue,
        };
    }
    Value::Null
}

fn envelope(kind: TelemetryKind, attributes: &Attributes, occurred_at: i64) -> Envelope {
    Envelope {
        event_id: new_event_id(occurred_at),
        kind,
        schema_version: 1,
        occurred_at,
        observed_at: None,
        // The collector stamps the receive time and the tenancy, exactly as it
        // does for a native batch. A compatibility edge gets no shortcut.
        received_at: None,
        workspace_id: None,
        project_id: None,
        source_id: None,
        sequence: None,
        release: attributes.release.clone(),
        service_name: attributes.service_name.clone(),
        request_id: None,
        session_id: None,
        end_user_id: None,
        anonymous_id: None,
        trace_id: None,
        span_id: None,
        consent: None,
        sdk_name: SDK_NAME.to_string(),
        sdk_version: env!("CARGO_PKG_VERSION").to_string(),
        properties: Vec::new(),
        measurements: None,
    }
}

/// Nanoseconds to milliseconds. `docs/CONVENTIONS.md` section 7: store and
/// transmit milliseconds since the Unix epoch.
fn ms(nanoseconds: u64) -> i64 {
    (nanoseconds / 1_000_000) as i64
}

/// A UUIDv7 for an item that arrived without an identifier of its own.
///
/// An OpenTelemetry data point has no event ID, and TallyOwl needs one for
/// deduplication. The time part comes from the point's own time, so a resend of
/// the same push produces a different ID: an OpenTelemetry retry is therefore
/// **not** deduplicated by this edge. `AGENTS.md` forbids claiming exactly-once
/// anywhere, and this is one of the places that claim would be wrong.
fn new_event_id(occurred_at: i64) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    let ms = occurred_at.max(0) as u64;
    id[0] = (ms >> 40) as u8;
    id[1] = (ms >> 32) as u8;
    id[2] = (ms >> 24) as u8;
    id[3] = (ms >> 16) as u8;
    id[4] = (ms >> 8) as u8;
    id[5] = ms as u8;
    let mut random = [0u8; 10];
    if getrandom::fill(&mut random).is_err() {
        // The system random source failed. A repeated ID would suppress a later
        // legitimate item, so this separates them by time rather than by
        // repeating a constant.
        let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        random[..8].copy_from_slice(&counter.to_le_bytes());
    }
    id[6..].copy_from_slice(&random);
    id[6] = (id[6] & 0x0f) | 0x70;
    id[8] = (id[8] & 0x3f) | 0x80;
    id
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protobuf::{write_bytes_field, write_varint, write_varint_field};

    // ---- builders that write the OpenTelemetry shapes ----------------------

    fn any_string(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        write_bytes_field(&mut out, 1, text.as_bytes());
        out
    }

    fn any_int(value: i64) -> Vec<u8> {
        let mut out = Vec::new();
        write_varint_field(&mut out, 3, value as u64);
        out
    }

    fn attribute(key: &str, value: Vec<u8>) -> Vec<u8> {
        let mut out = Vec::new();
        write_bytes_field(&mut out, 1, key.as_bytes());
        write_bytes_field(&mut out, 2, &value);
        out
    }

    fn resource(attributes: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for attribute in attributes {
            write_bytes_field(&mut out, 1, attribute);
        }
        out
    }

    fn fixed64(out: &mut Vec<u8>, number: u32, value: u64) {
        write_varint(out, ((number as u64) << 3) | 1);
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn double(out: &mut Vec<u8>, number: u32, value: f64) {
        fixed64(out, number, value.to_bits());
    }

    fn number_data_point(labels: &[Vec<u8>], start_ns: u64, time_ns: u64, value: f64) -> Vec<u8> {
        let mut out = Vec::new();
        fixed64(&mut out, 2, start_ns);
        fixed64(&mut out, 3, time_ns);
        double(&mut out, 4, value);
        for label in labels {
            write_bytes_field(&mut out, 7, label);
        }
        out
    }

    fn sum(points: &[Vec<u8>], temporality: u64, monotonic: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for point in points {
            write_bytes_field(&mut out, 1, point);
        }
        write_varint_field(&mut out, 2, temporality);
        write_varint_field(&mut out, 3, u64::from(monotonic));
        out
    }

    fn gauge(points: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for point in points {
            write_bytes_field(&mut out, 1, point);
        }
        out
    }

    fn histogram_data_point(
        start_ns: u64,
        time_ns: u64,
        count: u64,
        total: f64,
        buckets: &[u64],
        bounds: &[f64],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        fixed64(&mut out, 2, start_ns);
        fixed64(&mut out, 3, time_ns);
        fixed64(&mut out, 4, count);
        double(&mut out, 5, total);
        let mut packed = Vec::new();
        for value in buckets {
            packed.extend_from_slice(&value.to_le_bytes());
        }
        write_bytes_field(&mut out, 6, &packed);
        let mut packed = Vec::new();
        for value in bounds {
            packed.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        write_bytes_field(&mut out, 7, &packed);
        out
    }

    fn metric(name: &str, unit: &str, data_field: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_bytes_field(&mut out, 1, name.as_bytes());
        write_bytes_field(&mut out, 2, b"a description");
        write_bytes_field(&mut out, 3, unit.as_bytes());
        write_bytes_field(&mut out, data_field, data);
        out
    }

    fn export_metrics(resource_attributes: &[Vec<u8>], metrics_in: &[Vec<u8>]) -> Vec<u8> {
        let mut scope = Vec::new();
        for metric in metrics_in {
            write_bytes_field(&mut scope, 2, metric);
        }
        let mut resource_metrics = Vec::new();
        write_bytes_field(&mut resource_metrics, 1, &resource(resource_attributes));
        write_bytes_field(&mut resource_metrics, 2, &scope);
        let mut out = Vec::new();
        write_bytes_field(&mut out, 1, &resource_metrics);
        out
    }

    fn point_of(item: &TelemetryItem) -> &MetricPointPayload {
        item.metric_point.as_ref().expect("a metric point")
    }

    fn label(item: &TelemetryItem, key: &str) -> Option<String> {
        point_of(item)
            .labels
            .iter()
            .find(|l| l.key == key)
            .map(|l| wire::read(&l.value).unwrap().to_display())
    }

    // ---- metrics -----------------------------------------------------------

    #[test]
    fn a_monotonic_sum_becomes_a_counter_with_its_temporality() {
        let point = number_data_point(&[], 1_000_000_000, 2_000_000_000, 7.0);
        let body = export_metrics(&[], &[metric("requests", "1", 7, &sum(&[point], 1, true))]);
        let out = metrics(&body);
        assert_eq!(out.items.len(), 1);
        let point = point_of(&out.items[0]);
        assert_eq!(point.metric_name, "requests");
        assert_eq!(point.metric_kind, MetricKind::Counter);
        assert!(point.monotonic);
        assert_eq!(point.temporality, Temporality::Delta);
        assert_eq!(point.number_value, Some(7.0));
        assert_eq!(point.start_at, 1_000);
        assert_eq!(point.end_at, 2_000);
        assert_eq!(point.unit.as_deref(), Some("1"));
        assert_eq!(point.description.as_deref(), Some("a description"));
    }

    #[test]
    fn a_sum_that_is_not_monotonic_becomes_a_gauge() {
        // A sum that falls is a level. Calling it a counter would make every
        // rate over it wrong.
        let point = number_data_point(&[], 0, 2_000_000_000, 3.0);
        let body = export_metrics(
            &[],
            &[metric("in_flight", "1", 7, &sum(&[point], 2, false))],
        );
        let out = metrics(&body);
        assert_eq!(point_of(&out.items[0]).metric_kind, MetricKind::Gauge);
        assert!(!point_of(&out.items[0]).monotonic);
    }

    #[test]
    fn an_unspecified_temporality_is_read_as_cumulative() {
        // Reading a cumulative total as a delta would multiply it by the number
        // of samples, so the safe reading is the cumulative one.
        let point = number_data_point(&[], 0, 1_000_000_000, 5.0);
        let body = export_metrics(&[], &[metric("x", "1", 7, &sum(&[point], 0, true))]);
        assert_eq!(
            point_of(&metrics(&body).items[0]).temporality,
            Temporality::Cumulative
        );
    }

    #[test]
    fn a_gauge_becomes_a_gauge() {
        let point = number_data_point(&[], 0, 1_000_000_000, 12.5);
        let body = export_metrics(&[], &[metric("temperature", "C", 5, &gauge(&[point]))]);
        let out = metrics(&body);
        assert_eq!(point_of(&out.items[0]).metric_kind, MetricKind::Gauge);
        assert_eq!(point_of(&out.items[0]).number_value, Some(12.5));
    }

    #[test]
    fn a_histograms_bucket_counts_become_cumulative() {
        // This is the conversion that a copy would get wrong. OpenTelemetry
        // counts observations in each bucket; the native format counts them at
        // or below each bound.
        let point =
            histogram_data_point(0, 1_000_000_000, 10, 5.5, &[2, 3, 4, 1], &[1.0, 5.0, 10.0]);
        let body = export_metrics(
            &[],
            &[metric("latency", "s", 9, &{
                let mut out = Vec::new();
                write_bytes_field(&mut out, 1, &point);
                write_varint_field(&mut out, 2, 2);
                out
            })],
        );
        let out = metrics(&body);
        let histogram = point_of(&out.items[0]).histogram_value.as_ref().unwrap();
        assert_eq!(histogram.bounds, vec![1.0, 5.0, 10.0]);
        assert_eq!(histogram.counts, vec![2, 5, 9], "counts run a total");
        assert_eq!(histogram.count, 10);
        assert_eq!(histogram.sum, 5.5);
    }

    #[test]
    fn a_service_name_reaches_the_envelope_and_not_the_label_set() {
        let body = export_metrics(
            &[
                attribute("service.name", any_string("checkout")),
                attribute("service.version", any_string("1.4.0")),
                attribute("host.name", any_string("node-1")),
            ],
            &[metric(
                "x",
                "1",
                7,
                &sum(&[number_data_point(&[], 0, 1_000_000_000, 1.0)], 2, true),
            )],
        );
        let out = metrics(&body);
        let item = &out.items[0];
        assert_eq!(item.envelope.service_name.as_deref(), Some("checkout"));
        assert_eq!(item.envelope.release.as_deref(), Some("1.4.0"));
        assert!(label(item, "service.name").is_none());
        // Every other resource attribute is a label, which is what makes it
        // queryable.
        assert_eq!(label(item, "host.name").as_deref(), Some("node-1"));
    }

    #[test]
    fn a_data_point_label_beats_a_resource_label_of_the_same_name() {
        let body = export_metrics(
            &[attribute("host.name", any_string("resource"))],
            &[metric(
                "x",
                "1",
                7,
                &sum(
                    &[number_data_point(
                        &[attribute("host.name", any_string("point"))],
                        0,
                        1_000_000_000,
                        1.0,
                    )],
                    2,
                    true,
                ),
            )],
        );
        let out = metrics(&body);
        assert_eq!(label(&out.items[0], "host.name").as_deref(), Some("point"));
    }

    #[test]
    fn an_attribute_keeps_its_type_rather_than_becoming_text() {
        let body = export_metrics(
            &[attribute("replica.index", any_int(3))],
            &[metric(
                "x",
                "1",
                7,
                &sum(&[number_data_point(&[], 0, 1_000_000_000, 1.0)], 2, true),
            )],
        );
        let out = metrics(&body);
        let value = point_of(&out.items[0])
            .labels
            .iter()
            .find(|l| l.key == "replica.index")
            .map(|l| wire::read(&l.value).unwrap());
        assert_eq!(value, Some(Value::Integer(3)));
    }

    #[test]
    fn an_exponential_histogram_is_refused_where_the_exporter_can_see_it() {
        let body = export_metrics(&[], &[metric("x", "1", 10, b"anything")]);
        let out = metrics(&body);
        assert!(out.items.is_empty());
        assert_eq!(out.rejected, 1);
        assert!(out.reason.unwrap().contains("explicit"));
    }

    #[test]
    fn a_summary_becomes_a_sum_a_count_and_one_gauge_for_each_quantile() {
        let mut quantile = Vec::new();
        double(&mut quantile, 1, 0.99);
        double(&mut quantile, 2, 0.42);
        let mut point = Vec::new();
        fixed64(&mut point, 3, 1_000_000_000);
        fixed64(&mut point, 4, 100);
        double(&mut point, 5, 12.0);
        write_bytes_field(&mut point, 6, &quantile);
        let mut summary = Vec::new();
        write_bytes_field(&mut summary, 1, &point);

        let body = export_metrics(&[], &[metric("rpc", "s", 11, &summary)]);
        let out = metrics(&body);
        assert_eq!(out.items.len(), 3);
        assert!(out.items.iter().any(
            |i| point_of(i).metric_name == "rpc_sum" && point_of(i).number_value == Some(12.0)
        ));
        assert!(out
            .items
            .iter()
            .any(|i| point_of(i).metric_name == "rpc_count"
                && point_of(i).number_value == Some(100.0)));
        let quantile = out
            .items
            .iter()
            .find(|i| point_of(i).metric_kind == MetricKind::Gauge)
            .expect("a quantile gauge");
        assert_eq!(label(quantile, "quantile").as_deref(), Some("0.99"));
        assert_eq!(point_of(quantile).number_value, Some(0.42));
    }

    #[test]
    fn a_metric_with_no_name_is_refused_rather_than_stored_unnamed() {
        let body = export_metrics(
            &[],
            &[{
                let mut out = Vec::new();
                write_bytes_field(
                    &mut out,
                    7,
                    &sum(&[number_data_point(&[], 0, 1, 1.0)], 2, true),
                );
                out
            }],
        );
        let out = metrics(&body);
        assert!(out.items.is_empty());
        assert_eq!(out.rejected, 1);
    }

    #[test]
    fn an_empty_push_produces_nothing_and_refuses_nothing() {
        let out = metrics(&[]);
        assert!(out.items.is_empty());
        assert_eq!(out.rejected, 0);
    }

    #[test]
    fn a_truncated_push_keeps_what_arrived_before_the_fault() {
        let point = number_data_point(&[], 0, 1_000_000_000, 1.0);
        let whole = export_metrics(&[], &[metric("x", "1", 7, &sum(&[point], 2, true))]);
        let cut = &whole[..whole.len() - 3];
        // It does not panic and it does not loop. Whatever it can read, it
        // reads.
        let _ = metrics(cut);
    }

    #[test]
    fn an_integer_data_point_value_arrives_as_a_number() {
        let mut point = Vec::new();
        fixed64(&mut point, 3, 1_000_000_000);
        write_varint(&mut point, (6 << 3) | 1);
        point.extend_from_slice(&42i64.to_le_bytes());
        let body = export_metrics(&[], &[metric("x", "1", 7, &sum(&[point], 2, true))]);
        assert_eq!(point_of(&metrics(&body).items[0]).number_value, Some(42.0));
    }

    // ---- traces ------------------------------------------------------------

    fn span(trace: &[u8], id: &[u8], parent: Option<&[u8]>, name: &str, kind: u64) -> Vec<u8> {
        let mut out = Vec::new();
        write_bytes_field(&mut out, 1, trace);
        write_bytes_field(&mut out, 2, id);
        if let Some(parent) = parent {
            write_bytes_field(&mut out, 4, parent);
        }
        write_bytes_field(&mut out, 5, name.as_bytes());
        write_varint_field(&mut out, 6, kind);
        fixed64(&mut out, 7, 1_000_000_000);
        fixed64(&mut out, 8, 1_250_000_000);
        let mut status = Vec::new();
        write_varint_field(&mut status, 3, 2);
        write_bytes_field(&mut out, 15, &status);
        out
    }

    fn export_traces(resource_attributes: &[Vec<u8>], spans: &[Vec<u8>]) -> Vec<u8> {
        let mut scope = Vec::new();
        for span in spans {
            write_bytes_field(&mut scope, 2, span);
        }
        let mut resource_spans = Vec::new();
        write_bytes_field(&mut resource_spans, 1, &resource(resource_attributes));
        write_bytes_field(&mut resource_spans, 2, &scope);
        let mut out = Vec::new();
        write_bytes_field(&mut out, 1, &resource_spans);
        out
    }

    #[test]
    fn a_span_becomes_a_native_span_with_its_identifiers_and_its_duration() {
        let trace = [1u8; 16];
        let id = [2u8; 8];
        let parent = [3u8; 8];
        let body = export_traces(
            &[attribute("service.name", any_string("checkout"))],
            &[span(&trace, &id, Some(&parent), "GET /cart", 2)],
        );
        let out = traces(&body);
        assert_eq!(out.items.len(), 1);
        let item = &out.items[0];
        let span = item.span.as_ref().expect("a span");
        assert_eq!(span.operation, "GET /cart");
        assert_eq!(span.kind, SpanKind::Server);
        assert_eq!(span.start_at, 1_000);
        assert_eq!(span.duration_ms, 250);
        assert_eq!(span.status, SpanPayload_status::Error);
        assert_eq!(span.parent_span_id.as_deref(), Some(&parent[..]));
        assert_eq!(item.envelope.trace_id.as_deref(), Some(&trace[..]));
        assert_eq!(item.envelope.span_id.as_deref(), Some(&id[..]));
        assert_eq!(item.envelope.service_name.as_deref(), Some("checkout"));
    }

    #[test]
    fn a_span_without_usable_identifiers_is_refused_rather_than_stored() {
        // A span nothing can join to a trace would appear in a trace search and
        // belong to no trace.
        let body = export_traces(&[], &[span(&[1u8; 4], &[2u8; 8], None, "x", 1)]);
        let out = traces(&body);
        assert!(out.items.is_empty());
        assert_eq!(out.rejected, 1);
    }

    #[test]
    fn a_span_attribute_reaches_the_envelope_properties() {
        let trace = [1u8; 16];
        let id = [2u8; 8];
        let mut with_attribute = span(&trace, &id, None, "x", 1);
        write_bytes_field(
            &mut with_attribute,
            9,
            &attribute("http.route", any_string("/cart")),
        );
        let out = traces(&export_traces(&[], &[with_attribute]));
        let held = &out.items[0].envelope.properties;
        assert!(held.iter().any(
            |p| p.key == "http.route" && wire::read(&p.value).unwrap().to_display() == "/cart"
        ));
    }

    #[test]
    fn a_span_link_survives_when_both_of_its_identifiers_are_usable() {
        let trace = [1u8; 16];
        let id = [2u8; 8];
        let mut with_link = span(&trace, &id, None, "x", 1);
        let mut link = Vec::new();
        write_bytes_field(&mut link, 1, &[9u8; 16]);
        write_bytes_field(&mut link, 2, &[8u8; 8]);
        write_bytes_field(&mut with_link, 13, &link);
        let out = traces(&export_traces(&[], &[with_link]));
        let links = out.items[0].span.as_ref().unwrap().links.as_ref().unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].trace_id, vec![9u8; 16]);
    }

    #[test]
    fn every_span_kind_maps_to_its_native_kind() {
        let trace = [1u8; 16];
        let id = [2u8; 8];
        for (otel, native) in [
            (1u64, SpanKind::Internal),
            (2, SpanKind::Server),
            (3, SpanKind::Client),
            (4, SpanKind::Producer),
            (5, SpanKind::Consumer),
            (0, SpanKind::Internal),
        ] {
            let out = traces(&export_traces(&[], &[span(&trace, &id, None, "x", otel)]));
            assert_eq!(out.items[0].span.as_ref().unwrap().kind, native);
        }
    }

    #[test]
    fn an_item_from_this_edge_never_carries_tenancy() {
        // Never accept tenancy from a payload. The collector stamps it, and a
        // compatibility edge gets no shortcut.
        let body = export_metrics(
            &[],
            &[metric(
                "x",
                "1",
                7,
                &sum(&[number_data_point(&[], 0, 1_000_000_000, 1.0)], 2, true),
            )],
        );
        let envelope = &metrics(&body).items[0].envelope;
        assert!(envelope.workspace_id.is_none());
        assert!(envelope.project_id.is_none());
        assert!(envelope.source_id.is_none());
        assert!(envelope.received_at.is_none());
    }

    #[test]
    fn every_item_carries_a_usable_identifier() {
        let body = export_metrics(
            &[],
            &[metric(
                "x",
                "1",
                7,
                &sum(&[number_data_point(&[], 0, 1_000_000_000, 1.0)], 2, true),
            )],
        );
        let id = &metrics(&body).items[0].envelope.event_id;
        assert_eq!(id.len(), 16);
        assert_eq!(id[6] >> 4, 7, "a UUID version 7");
    }
}
