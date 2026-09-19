//! The canonical generic event projection.
//!
//! CSIL CBOR is a wire and append-log representation. After a successful
//! projection, queryable telemetry belongs in native typed columns rather than a
//! long-lived catch-all payload. See D3 and `AGENTS.md`.
//!
//! This module is that projection: one wire `TelemetryItem` becomes one store
//! `EventRow`. Every telemetry kind projects, so a query over a mixed project
//! counts something a person recognises. The rule it holds is that the store
//! never receives a wire type.
//!
//! # What the projection refuses
//!
//! An item whose envelope and payload disagree, an item with two payloads, and
//! a typed value whose `kind` names a field it did not carry are each refused
//! by name. Each one would otherwise store one thing under the name of another,
//! which `docs/FAILURE_MODES.md` section 2 ranks above a stopped request.

use tallyowl_collector_api::types::{Property, TelemetryItem, TelemetryKind};
use tallyowl_store::row::{EventRow, PropertyValue};
use tallyowl_wire::{collector as wire, collector_items_bridge as items, Value};

/// Why an item could not be projected. A rejected item names its own reason, so
/// the receipt can carry a per-item rejection rather than failing the batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionFailure {
    pub reason: String,
}

impl ProjectionFailure {
    fn new(reason: impl Into<String>) -> ProjectionFailure {
        ProjectionFailure {
            reason: reason.into(),
        }
    }
}

/// Project one wire item into one stored row.
pub fn project(item: &TelemetryItem) -> Result<EventRow, ProjectionFailure> {
    let envelope = &item.envelope;
    let event_id = to_id(&envelope.event_id)
        .ok_or_else(|| ProjectionFailure::new("This item has no usable identifier.".to_string()))?;

    let payload = items::payload(item).map_err(|e| ProjectionFailure::new(e.message))?;

    let mut row = EventRow::new(
        event_id,
        kind_name(&envelope.kind),
        &payload_name(&payload, &envelope.kind),
        envelope.occurred_at,
    );

    // The collector stamps tenancy. An item that reaches the head without it
    // did not come through intake, and the head must not invent one.
    row.workspace_id = to_id(envelope.workspace_id.as_deref().unwrap_or(&[])).ok_or_else(|| {
        ProjectionFailure::new(
            "This item carries no workspace. It did not come through collector intake.",
        )
    })?;
    row.project_id = to_id(envelope.project_id.as_deref().unwrap_or(&[])).ok_or_else(|| {
        ProjectionFailure::new(
            "This item carries no project. It did not come through collector intake.",
        )
    })?;
    row.source_id = to_id(envelope.source_id.as_deref().unwrap_or(&[])).unwrap_or([0; 16]);

    // Three time facts, kept apart. A missing receive time falls back to the
    // producer time rather than to now: inventing a receive time at commit
    // would make every late batch look punctual.
    row.received_at = envelope.received_at.unwrap_or(envelope.occurred_at);
    row.session_id = envelope.session_id.clone();
    row.request_id = envelope.request_id.clone();
    row.trace_id = envelope.trace_id.as_deref().and_then(to_id);
    // A span identifier is 8 bytes and is not one of the row's own columns, so
    // it travels as a property. Without it a waterfall has no nodes to hang a
    // parent on.
    if let Some(span_id) = &envelope.span_id {
        row.properties.insert(
            "span_id".to_string(),
            (
                PropertyValue::Text(tallyowl_store::row::hex(span_id)),
                "client".to_string(),
            ),
        );
    }
    // The two identity columns. Both are properties rather than row fields, so
    // both are indexed for exact lookup like any other correlation value, and
    // both keep their `client` origin: the producer supplied them, and
    // `AGENTS.md` requires each property to record where it came from.
    //
    // **The anonymous ID is stored, not dropped.** Without it an event before
    // an `identify` belongs to nobody, and every funnel that starts before a
    // sign-in would begin at the sign-in. `docs/DATA_MODEL.md` section 3.5
    // makes it the thing `identify` links from.
    if let Some(end_user_id) = &envelope.end_user_id {
        row.properties.insert(
            crate::identity::END_USER_ID.to_string(),
            (
                PropertyValue::Text(end_user_id.clone()),
                "client".to_string(),
            ),
        );
    }
    if let Some(anonymous_id) = &envelope.anonymous_id {
        row.properties.insert(
            crate::identity::ANONYMOUS_ID.to_string(),
            (
                PropertyValue::Text(anonymous_id.clone()),
                "client".to_string(),
            ),
        );
    }
    // The consent state, kept with the event. D30: "Consent state still travels
    // with an applicable event and TallyOwl stores it, so a later policy can act
    // on it." A project that turns on consent-aware attribution next month can
    // then act on what arrived this month, rather than starting from nothing.
    if let Some(consent) = &envelope.consent {
        row.properties.insert(
            crate::attribution::CONSENT_MARKETING.to_string(),
            (
                PropertyValue::Text(consent_name(&consent.marketing).to_string()),
                "client".to_string(),
            ),
        );
        row.properties.insert(
            crate::attribution::CONSENT_ANALYTICS.to_string(),
            (
                PropertyValue::Text(consent_name(&consent.analytics).to_string()),
                "client".to_string(),
            ),
        );
        if let Some(version) = &consent.policy_version {
            row.properties.insert(
                crate::attribution::CONSENT_POLICY_VERSION.to_string(),
                (PropertyValue::Text(version.clone()), "client".to_string()),
            );
        }
    }
    row.service_name = envelope.service_name.clone();
    row.release = envelope.release.clone();

    for property in &envelope.properties {
        let (value, origin) = project_property(property)?;
        row.properties.insert(property.key.clone(), (value, origin));
    }

    // A measurement is a number with a unit. It becomes a property so one
    // column set answers a query over any telemetry kind, and it keeps its
    // origin as `client` because the event site supplied it.
    for measurement in envelope.measurements.iter().flatten() {
        let value =
            wire::read_measurement(measurement).map_err(|e| ProjectionFailure::new(e.message))?;
        row.properties.insert(
            measurement.key.clone(),
            (to_store_value(value), "client".to_string()),
        );
        if let Some(unit) = &measurement.unit {
            row.properties.insert(
                format!("{}_unit", measurement.key),
                (PropertyValue::Text(unit.clone()), "client".to_string()),
            );
        }
    }

    // The payload's own fields become properties. Phase 4 gives each kind its
    // typed columns; until then one property namespace carries them, which is
    // what D38 asks for anyway.
    for (key, value) in payload_properties(&payload) {
        row.properties.insert(key, (value, "client".to_string()));
    }

    // The channel a touch belongs to, and the classifier that decided. It is
    // derived rather than sent, because a producer that could name its own
    // channel could put paid traffic in the organic column. Its origin is
    // `collector` for the same reason: an operator can trust a value TallyOwl
    // computed and a query can filter on the origin. See D38.
    //
    // Nothing that divides credit reads this. `crate::attribution` classifies
    // again at read time, so a corrected classifier takes effect on the next
    // question rather than needing every stored row rewritten. See
    // `crate::campaign`.
    let touch = crate::campaign::Touch::of(&row);
    if row.kind == "campaign-touch" || touch.has_campaign() {
        for (key, value) in crate::campaign::derived_properties(&touch) {
            row.properties.insert(key, (value, "collector".to_string()));
        }
    }

    Ok(row)
}

fn project_property(property: &Property) -> Result<(PropertyValue, String), ProjectionFailure> {
    let value = wire::read(&property.value).map_err(|e| {
        ProjectionFailure::new(format!(
            "The property `{}` could not be read. {}",
            property.key, e.message
        ))
    })?;
    Ok((
        to_store_value(value),
        wire::origin_name(&property.origin).to_string(),
    ))
}

/// One wire value becomes one stored value. A decimal keeps its exact digits as
/// text, because money never becomes a float.
pub fn to_store_value(value: Value) -> PropertyValue {
    match value {
        Value::Null => PropertyValue::Null,
        Value::Boolean(v) => PropertyValue::Boolean(v),
        Value::Integer(v) => PropertyValue::Integer(v),
        Value::Unsigned(v) => PropertyValue::Unsigned(v),
        Value::Float(v) => PropertyValue::Float(v),
        decimal @ Value::Decimal { .. } => PropertyValue::Decimal(decimal.to_display()),
        Value::Text(v) => PropertyValue::Text(v),
        Value::Bytes(v) => PropertyValue::Bytes(v),
    }
}

fn to_id(bytes: &[u8]) -> Option<[u8; 16]> {
    bytes.try_into().ok()
}

pub fn kind_name(kind: &TelemetryKind) -> &'static str {
    match kind {
        TelemetryKind::Event => "event",
        TelemetryKind::PageView => "page-view",
        TelemetryKind::SessionStart => "session-start",
        TelemetryKind::SessionEnd => "session-end",
        TelemetryKind::SessionHeartbeat => "session-heartbeat",
        TelemetryKind::Interaction => "interaction",
        TelemetryKind::FeatureExposure => "feature-exposure",
        TelemetryKind::Identify => "identify",
        TelemetryKind::Alias => "alias",
        TelemetryKind::Group => "group",
        TelemetryKind::Conversion => "conversion",
        TelemetryKind::Error => "error",
        TelemetryKind::Span => "span",
        TelemetryKind::MetricPoint => "metric-point",
        TelemetryKind::CampaignTouch => "campaign-touch",
        TelemetryKind::CampaignCost => "campaign-cost",
    }
}

/// The name a query groups by. An event has one; every other kind gets the most
/// specific name its payload carries, so a trend over a mixed project counts
/// something a person recognises.
fn payload_name(payload: &items::Payload<'_>, kind: &TelemetryKind) -> String {
    use items::Payload as P;
    match payload {
        P::Event(event) => event.name.clone(),
        P::PageView(page) => page.route.clone(),
        P::Interaction(interaction) => format!("{}:{}", interaction.target, interaction.action),
        P::FeatureExposure(exposure) => exposure.feature.clone(),
        P::Conversion(conversion) => conversion.goal.clone(),
        P::Error(error) => error.error_type.clone(),
        P::Span(span) => span.operation.clone(),
        P::MetricPoint(metric) => metric.metric_name.clone(),
        P::CampaignCost(cost) => cost.campaign.clone(),
        P::CampaignTouch(touch) => touch
            .campaign
            .campaign
            .clone()
            .unwrap_or_else(|| "campaign-touch".to_string()),
        P::Group(group) => group.group_id.clone(),
        _ => kind_name(kind).to_string(),
    }
}

/// The payload fields a query can filter on.
fn payload_properties(payload: &items::Payload<'_>) -> Vec<(String, PropertyValue)> {
    use items::Payload as P;
    let text = |key: &str, value: &str| (key.to_string(), PropertyValue::Text(value.to_string()));
    let mut out = Vec::new();
    match payload {
        P::SessionHeartbeat => {}
        P::Event(event) => {
            if let Some(route) = &event.route {
                out.push(text("route", route));
            }
            if let Some(title) = &event.page_title {
                out.push(text("page_title", title));
            }
        }
        P::PageView(page) => {
            out.push(text("route", &page.route));
            if let Some(title) = &page.page_title {
                out.push(text("page_title", title));
            }
            if let Some(referrer) = &page.referrer {
                out.push(text("referrer", referrer));
            }
            if let Some(campaign) = &page.campaign {
                out.extend(campaign_properties(campaign));
            }
        }
        P::SessionStart(start) => {
            if let Some(route) = &start.entry_route {
                out.push(text("entry_route", route));
            }
        }
        P::SessionEnd(end) => {
            use tallyowl_collector_api::types::SessionEndPayload_reason as Reason;
            out.push(text(
                "reason",
                match end.reason {
                    Reason::Explicit => "explicit",
                    Reason::Timeout => "timeout",
                    Reason::MaximumLifetime => "maximum-lifetime",
                },
            ));
        }
        P::Interaction(interaction) => {
            out.push(text("target", &interaction.target));
            out.push(text("action", &interaction.action));
        }
        P::FeatureExposure(exposure) => {
            out.push(text("feature", &exposure.feature));
            out.push(text("variant", &exposure.variant));
        }
        P::Identify(identify) => out.push(text("end_user_id", &identify.end_user_id)),
        P::Alias(alias) => {
            out.push(text("from_id", &alias.from_id));
            out.push(text("to_id", &alias.to_id));
        }
        P::Group(group) => {
            out.push(text("group_id", &group.group_id));
            if let Some(kind) = &group.group_kind {
                out.push(text("group_kind", kind));
            }
        }
        P::Conversion(conversion) => {
            out.push(text("goal", &conversion.goal));
            if let Some(value) = &conversion.value {
                // Money keeps its exact digits. A float here would make a
                // revenue total disagree with the customer's own records.
                out.push((
                    "value".to_string(),
                    PropertyValue::Decimal(
                        Value::Decimal {
                            exponent: value.exponent,
                            mantissa: value.mantissa,
                        }
                        .to_display(),
                    ),
                ));
            }
            if let Some(currency) = &conversion.currency {
                out.push(text("currency", currency));
            }
            if let Some(order_id) = &conversion.order_id {
                out.push(text("order_id", order_id));
            }
            // The optional links DATA_MODEL.md section 3.6 describes. An
            // application that already knows which touch earned a conversion
            // says so, and attribution does not have to find it again.
            if let Some(campaign) = &conversion.campaign {
                out.extend(campaign_properties(campaign));
            }
            if let Some(touch_event_id) = &conversion.touch_event_id {
                out.push(text(
                    "touch_event_id",
                    &tallyowl_store::row::hex(touch_event_id),
                ));
            }
        }
        P::Error(error) => {
            use tallyowl_collector_api::types::ErrorPayload_severity as Severity;
            out.push(text("error_type", &error.error_type));
            out.push(text("message", &error.message));
            out.push(("handled".to_string(), PropertyValue::Boolean(error.handled)));
            out.push(text(
                "severity",
                match error.severity {
                    Severity::Fatal => "fatal",
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                    Severity::Info => "info",
                },
            ));
            if let Some(frames) = &error.frames {
                out.push((
                    "frame_count".to_string(),
                    PropertyValue::Unsigned(frames.len() as u64),
                ));
                // The top in-app frame is what a person looks at first, and it
                // is what a group is named after in a list. The whole stack is
                // in the raw payload; this is the one line that goes in a
                // column so a query can filter and group on it.
                if let Some(top) = frames.iter().find(|frame| frame.in_app).or(frames.first()) {
                    if let Some(module) = &top.module {
                        out.push(text("top_module", module));
                    }
                    if let Some(function) = &top.function {
                        out.push(text("top_function", function));
                    }
                    if let Some(file) = &top.file {
                        out.push(text("top_file", &tallyowl_wire::scrub::path(file)));
                    }
                }
            }
            if let Some(breadcrumbs) = &error.breadcrumbs {
                out.push((
                    "breadcrumb_count".to_string(),
                    PropertyValue::Unsigned(breadcrumbs.len() as u64),
                ));
            }

            // The projector computes the group. A producer never controls it.
            // The inputs and the rule travel with the digest, so a later
            // fingerprint version can rebuild every group from retained raw
            // data rather than from a digest it cannot reverse. See D39.
            let group = crate::errors::fingerprint(error);
            out.push(text("error_group", &group.digest));
            out.push(text("error_group_rule", group.rule.as_str()));
            out.push((
                "error_group_version".to_string(),
                PropertyValue::Unsigned(group.version),
            ));
            out.push(text("error_group_inputs", &group.inputs.join(" | ")));
        }
        P::Span(span) => {
            use tallyowl_collector_api::types::{SpanKind, SpanPayload_status as Status};
            out.push(text("operation", &span.operation));
            out.push(text(
                "span_kind",
                match span.kind {
                    SpanKind::Internal => "internal",
                    SpanKind::Server => "server",
                    SpanKind::Client => "client",
                    SpanKind::Producer => "producer",
                    SpanKind::Consumer => "consumer",
                },
            ));
            out.push(text(
                "status",
                match span.status {
                    Status::Ok => "ok",
                    Status::Error => "error",
                    Status::Unset => "unset",
                },
            ));
            out.push((
                "duration_ms".to_string(),
                PropertyValue::Integer(span.duration_ms),
            ));
            if let Some(resource) = &span.resource {
                out.push(text("resource", resource));
            }
            out.push((
                "start_at".to_string(),
                PropertyValue::Integer(span.start_at),
            ));
            // The parent is what makes a waterfall a tree rather than a list.
            // It travels as text because a query filters and groups on text and
            // never does arithmetic on a span identifier.
            if let Some(parent) = &span.parent_span_id {
                out.push(text("parent_span_id", &tallyowl_store::row::hex(parent)));
            }
            if let Some(error_event_id) = &span.error_event_id {
                // The link that answers "what went wrong in this trace?" from
                // the span side. The error carries the trace ID from the other
                // side, so the two meet without a join.
                out.push(text(
                    "error_event_id",
                    &tallyowl_store::row::hex(error_event_id),
                ));
            }
            if let Some(reason) = &span.sampling_reason {
                out.push(text("sampling_reason", reason));
            }
            if let Some(links) = &span.links {
                out.push((
                    "link_count".to_string(),
                    PropertyValue::Unsigned(links.len() as u64),
                ));
            }
        }
        P::MetricPoint(metric) => {
            use tallyowl_collector_api::types::{
                MetricKind, MetricPointPayload_temporality as Temporality,
            };
            out.push(text("metric_name", &metric.metric_name));
            out.push(text(
                "metric_kind",
                match metric.metric_kind {
                    MetricKind::Counter => "counter",
                    MetricKind::Gauge => "gauge",
                    MetricKind::Histogram => "histogram",
                },
            ));
            out.push(text(
                "temporality",
                match metric.temporality {
                    Temporality::Delta => "delta",
                    Temporality::Cumulative => "cumulative",
                },
            ));
            out.push((
                "monotonic".to_string(),
                PropertyValue::Boolean(metric.monotonic),
            ));
            if let Some(value) = metric.number_value {
                out.push(("value".to_string(), PropertyValue::Float(value)));
            }
            if let Some(unit) = &metric.unit {
                out.push(text("unit", unit));
            }
            // Both ends of the period. `rate` and `increase` need them, and
            // `start_at` is what makes a counter reset visible: a smaller value
            // with a later start is a restart rather than a defect. See
            // QUERY.md section 12.8.
            out.push((
                "start_at".to_string(),
                PropertyValue::Integer(metric.start_at),
            ));
            out.push(("end_at".to_string(), PropertyValue::Integer(metric.end_at)));

            // The identity of the series, computed once here rather than
            // reconstructed by every query. `rate` over a group that holds more
            // than one series must not read two series as one that jumped.
            out.push(text("series_key", &tallyowl_compat::series_key(metric)));

            if let Some(histogram) = &metric.histogram_value {
                out.push((
                    "histogram_count".to_string(),
                    PropertyValue::Unsigned(histogram.count),
                ));
                out.push((
                    "histogram_sum".to_string(),
                    PropertyValue::Float(histogram.sum),
                ));
                // A row has no vector value, and a bucket list is a small fixed
                // vector rather than a payload. Two canonical number lists keep
                // it readable, comparable, and mergeable without a catch-all
                // blob. See docs/IMPLEMENTATION_LOG.md.
                out.push(text("histogram_bounds", &join_floats(&histogram.bounds)));
                out.push(text("histogram_counts", &join_unsigned(&histogram.counts)));
            }
            if let Some(trace_id) = &metric.exemplar_trace_id {
                out.push(text(
                    "exemplar_trace_id",
                    &tallyowl_store::row::hex(trace_id),
                ));
            }

            // A label is a dimension a query groups by, so it arrives under its
            // own name. A label that would take the name of one of the fields
            // above is prefixed instead of overwriting it, because a producer
            // must not be able to change what `value` means.
            for label in &metric.labels {
                let Ok(value) = wire::read(&label.value) else {
                    continue;
                };
                let key = if METRIC_FIELDS.contains(&label.key.as_str()) {
                    format!("label.{}", label.key)
                } else {
                    label.key.clone()
                };
                out.push((key, to_store_value(value)));
            }
        }
        P::CampaignTouch(touch) => {
            out.extend(campaign_properties(&touch.campaign));
            if let Some(referrer) = &touch.referrer {
                out.push(text("referrer", referrer));
            }
            // The host from the whole address, when the client did not send one
            // on its own. A classifier reads the host, and a client that sent
            // only the address should not be unclassifiable because of it.
            if let Some(domain) = touch
                .referrer_domain
                .clone()
                .or_else(|| touch.referrer.as_deref().and_then(crate::campaign::host_of))
            {
                out.push(text("referrer_domain", &domain));
            }
            if let Some(route) = &touch.landing_route {
                out.push(text("landing_route", route));
            }
        }
        P::CampaignCost(cost) => {
            out.push(text("campaign", &cost.campaign));
            if let Some(platform) = &cost.platform {
                out.push(text("platform", platform));
            }
            out.push((
                "cost".to_string(),
                PropertyValue::Decimal(
                    Value::Decimal {
                        exponent: cost.cost.exponent,
                        mantissa: cost.cost.mantissa,
                    }
                    .to_display(),
                ),
            ));
            out.push(text("currency", &cost.currency));
        }
    }
    out
}

/// The names a metric point owns. A label of one of these names is prefixed, so
/// a producer cannot change what `value` or `series_key` means.
pub(crate) const METRIC_FIELDS: &[&str] = &[
    "metric_name",
    "metric_kind",
    "temporality",
    "monotonic",
    "value",
    "unit",
    "start_at",
    "end_at",
    "series_key",
    "histogram_count",
    "histogram_sum",
    "histogram_bounds",
    "histogram_counts",
    "exemplar_trace_id",
];

/// A canonical list of numbers, so two producers of the same bucket layout
/// write the same text and a merge can compare them by equality.
fn join_floats(values: &[f64]) -> String {
    values
        .iter()
        .map(|v| format_float(*v))
        .collect::<Vec<String>>()
        .join(",")
}

fn join_unsigned(values: &[u64]) -> String {
    values
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<String>>()
        .join(",")
}

/// A whole number renders without a decimal point, so `1` and `1.0` are one
/// bucket layout rather than two that never merge.
pub fn format_float(value: f64) -> String {
    if value == f64::INFINITY {
        "+Inf".to_string()
    } else if value == f64::NEG_INFINITY {
        "-Inf".to_string()
    } else if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// The name a consent state is stored under.
fn consent_name(state: &tallyowl_collector_api::types::ConsentState) -> &'static str {
    use tallyowl_collector_api::types::ConsentState as S;
    match state {
        S::Granted => "granted",
        S::Denied => "denied",
        S::Absent => "absent",
    }
}

fn campaign_properties(
    campaign: &tallyowl_collector_api::types::CampaignParameters,
) -> Vec<(String, PropertyValue)> {
    let mut out = Vec::new();
    let mut push = |key: &str, value: &Option<String>| {
        if let Some(value) = value {
            out.push((key.to_string(), PropertyValue::Text(value.clone())));
        }
    };
    push("campaign_source", &campaign.source);
    push("campaign_medium", &campaign.medium);
    push("campaign", &campaign.campaign);
    push("campaign_term", &campaign.term);
    push("campaign_content", &campaign.content);
    push("campaign_click_id", &campaign.click_id);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tallyowl_collector_api::types::{
        CampaignParameters, ConversionPayload, CsilDecimal, Envelope, ErrorPayload,
        ErrorPayload_severity, EventPayload, PageViewPayload, PropertyOrigin,
    };

    fn envelope(id: u8) -> Envelope {
        Envelope {
            event_id: vec![id; 16],
            kind: TelemetryKind::Event,
            schema_version: 1,
            occurred_at: 1_000,
            observed_at: None,
            received_at: Some(1_050),
            workspace_id: Some(vec![8; 16]),
            project_id: Some(vec![9; 16]),
            source_id: Some(vec![7; 16]),
            sequence: None,
            release: Some("2026.8.1".into()),
            service_name: Some("checkout".into()),
            request_id: Some("r-1".into()),
            session_id: Some("s-1".into()),
            end_user_id: None,
            anonymous_id: None,
            trace_id: Some(vec![3; 16]),
            span_id: None,
            consent: None,
            sdk_name: "tallyowl-driver-rust".into(),
            sdk_version: "0.0.0".into(),
            properties: Vec::new(),
            measurements: None,
        }
    }

    fn item(id: u8) -> TelemetryItem {
        items::event(
            envelope(id),
            EventPayload {
                name: "checkout-started".into(),
                route: Some("/checkout".into()),
                page_title: None,
            },
        )
    }

    #[test]
    fn an_event_projects_with_every_correlation_field() {
        let row = project(&item(1)).unwrap();
        assert_eq!(row.event_id, [1; 16]);
        assert_eq!(row.name, "checkout-started");
        assert_eq!(row.kind, "event");
        assert_eq!(row.session_id.as_deref(), Some("s-1"));
        assert_eq!(row.request_id.as_deref(), Some("r-1"));
        assert_eq!(row.trace_id, Some([3; 16]));
        assert_eq!(row.release.as_deref(), Some("2026.8.1"));
        assert_eq!(
            row.properties["route"].0,
            PropertyValue::Text("/checkout".into())
        );
    }

    #[test]
    fn the_three_time_facts_stay_separate_through_the_projection() {
        let row = project(&item(1)).unwrap();
        assert_eq!(row.occurred_at, 1_000);
        assert_eq!(row.received_at, 1_050);
        assert_eq!(row.committed_at, 0, "the store sets the commit time");
    }

    #[test]
    fn an_item_with_no_receive_time_keeps_the_producer_time_rather_than_looking_punctual() {
        let mut item = item(1);
        item.envelope.received_at = None;
        let row = project(&item).unwrap();
        assert_eq!(row.received_at, 1_000);
    }

    #[test]
    fn an_item_with_no_tenancy_is_refused_rather_than_given_one() {
        // A head that invented a workspace here would put one tenant's data
        // somewhere a query could find it under another.
        let mut item = item(1);
        item.envelope.workspace_id = None;
        let failure = project(&item).unwrap_err();
        assert!(failure.reason.contains("workspace"));
        assert!(failure.reason.contains("collector intake"));

        let mut item = self::item(1);
        item.envelope.project_id = None;
        assert!(project(&item).unwrap_err().reason.contains("project"));
    }

    #[test]
    fn every_typed_property_keeps_its_type() {
        let mut item = item(1);
        item.envelope.properties = vec![
            wire::property("count", Value::Integer(-3), PropertyOrigin::Client),
            wire::property("enabled", Value::Boolean(true), PropertyOrigin::Driver),
            wire::property(
                "region",
                Value::Text("us-west2".into()),
                PropertyOrigin::Collector,
            ),
            wire::property("ratio", Value::Float(0.5), PropertyOrigin::Client),
            wire::property("attempts", Value::Unsigned(4), PropertyOrigin::Client),
        ];
        let row = project(&item).unwrap();
        assert_eq!(row.properties["count"].0, PropertyValue::Integer(-3));
        assert_eq!(row.properties["enabled"].0, PropertyValue::Boolean(true));
        assert_eq!(row.properties["ratio"].0, PropertyValue::Float(0.5));
        // An unsigned value stays unsigned. The old bare choice could not carry
        // this distinction out of a dynamically typed client at all.
        assert_eq!(row.properties["attempts"].0, PropertyValue::Unsigned(4));
        assert_eq!(row.properties["region"].1, "collector");
        assert_eq!(row.properties["count"].1, "client");
        assert_eq!(row.properties["enabled"].1, "driver");
    }

    #[test]
    fn a_property_whose_kind_names_an_absent_value_is_refused_by_name() {
        let mut item = item(1);
        let mut broken = wire::write(&Value::Text("x".into()));
        broken.text_value = None;
        item.envelope.properties = vec![Property {
            key: "region".into(),
            value: broken,
            origin: PropertyOrigin::Client,
        }];
        let failure = project(&item).unwrap_err();
        assert!(failure.reason.contains("region"), "{}", failure.reason);
    }

    #[test]
    fn a_page_view_takes_its_route_as_its_name() {
        let item = items::page_view(
            envelope(1),
            PageViewPayload {
                route: "/pricing".into(),
                page_title: None,
                referrer: Some("https://example.test/".into()),
                campaign: Some(CampaignParameters {
                    source: Some("newsletter".into()),
                    medium: Some("email".into()),
                    campaign: Some("spring".into()),
                    term: None,
                    content: None,
                    click_id: None,
                }),
            },
        );
        let row = project(&item).unwrap();
        assert_eq!(row.kind, "page-view");
        assert_eq!(row.name, "/pricing");
        assert_eq!(
            row.properties["referrer"].0.to_display(),
            "https://example.test/"
        );
        assert_eq!(
            row.properties["campaign_source"].0.to_display(),
            "newsletter"
        );
        assert_eq!(row.properties["campaign"].0.to_display(), "spring");
    }

    #[test]
    fn a_conversion_keeps_its_exact_money() {
        // 19.99 as a float is 19.989999999999998. A revenue total built from
        // that disagrees with the customer's own records.
        let item = items::conversion(
            envelope(1),
            ConversionPayload {
                campaign: None,
                touch_event_id: None,
                goal: "purchase".into(),
                value: Some(CsilDecimal {
                    exponent: -2,
                    mantissa: 1999,
                }),
                currency: Some("USD".into()),
                order_id: Some("o-1".into()),
            },
        );
        let row = project(&item).unwrap();
        assert_eq!(row.kind, "conversion");
        assert_eq!(row.name, "purchase");
        assert_eq!(
            row.properties["value"].0,
            PropertyValue::Decimal("19.99".into())
        );
        assert_eq!(row.properties["currency"].0.to_display(), "USD");
    }

    #[test]
    fn an_error_projects_its_severity_and_whether_it_was_handled() {
        let item = items::error(
            envelope(1),
            ErrorPayload {
                error_type: "TypeError".into(),
                message: "x is not a function".into(),
                handled: false,
                severity: ErrorPayload_severity::Fatal,
                mechanism: None,
                runtime: None,
                frames: None,
                breadcrumbs: None,
            },
        );
        let row = project(&item).unwrap();
        assert_eq!(row.kind, "error");
        assert_eq!(row.name, "TypeError");
        assert_eq!(row.properties["handled"].0, PropertyValue::Boolean(false));
        assert_eq!(row.properties["severity"].0.to_display(), "fatal");
    }

    #[test]
    fn an_item_whose_envelope_and_payload_disagree_is_refused() {
        // The check that the old shape could not make, because the payload
        // carried no name of its own.
        let mut item = items::page_view(
            envelope(1),
            PageViewPayload {
                route: "/pricing".into(),
                page_title: None,
                referrer: None,
                campaign: None,
            },
        );
        item.envelope.kind = TelemetryKind::Conversion;
        let failure = project(&item).unwrap_err();
        assert!(failure.reason.contains("conversion"), "{}", failure.reason);
        assert!(failure.reason.contains("page-view"), "{}", failure.reason);
    }

    #[test]
    fn an_item_with_no_payload_is_refused() {
        let item = items::empty_item(envelope(1));
        assert!(project(&item).is_err());
    }

    #[test]
    fn a_session_heartbeat_projects_with_no_payload() {
        let mut item = items::empty_item(envelope(1));
        item.envelope.kind = TelemetryKind::SessionHeartbeat;
        let row = project(&item).unwrap();
        assert_eq!(row.kind, "session-heartbeat");
        assert_eq!(row.name, "session-heartbeat");
    }

    #[test]
    fn a_measurement_becomes_a_property_with_its_unit() {
        let mut item = item(1);
        item.envelope.measurements = Some(vec![
            wire::measurement("render", Value::Float(12.5), Some("ms")).unwrap(),
            wire::measurement("items", Value::Integer(3), None).unwrap(),
        ]);
        let row = project(&item).unwrap();
        assert_eq!(row.properties["render"].0, PropertyValue::Float(12.5));
        assert_eq!(row.properties["render_unit"].0.to_display(), "ms");
        assert_eq!(row.properties["items"].0, PropertyValue::Integer(3));
    }

    #[test]
    fn an_identifier_of_the_wrong_length_is_refused() {
        let mut item = item(1);
        item.envelope.event_id = vec![1, 2, 3];
        assert!(project(&item).is_err());
    }
}
