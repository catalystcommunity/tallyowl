//! Self-observation: a service's own instruments, as telemetry in TallyOwl.
//!
//! D12: "TallyOwl services expose Prometheus and OpenMetrics metrics. They can
//! also push the same instruments to a protected internal TallyOwl project. A
//! recursion guard and separate retention protect this project."
//!
//! **The same instruments**, from the same registry. `/metrics` and this push
//! read one `Registry`, so the exposition endpoint and a TallyOwl chart cannot
//! disagree about a number.
//!
//! # The recursion guard
//!
//! Publishing self-metrics is work, and instruments measure work. A push that
//! counted its own batch would raise `tallyowl_batches_accepted_total`, which
//! the next push would report, which would raise it again. Two things stop
//! that:
//!
//! - `Registry::begin_publishing` suppresses recording for the length of one
//!   push, so the push does not measure itself. The cost of self-observation is
//!   one batch each period, and the numbers describe the service rather than
//!   the act of describing it.
//! - a push that is already in flight makes the next period skip rather than
//!   overlap, because two overlapping pushes would each measure the other.
//!
//! # Off by default
//!
//! `metrics.selfObservation.enabled` is `false`. Self-metrics cost ingest
//! capacity that an installation may want for its own data, and an operator who
//! wants them asks.
//!
//! # It is cumulative, and it says so
//!
//! Every reading is the total since the process started, and `start_at` is that
//! moment. A restarted service therefore reports a later `start_at` beside a
//! value that begins again at zero, which is exactly what `rate` and `increase`
//! read as a restart rather than as a counter running backwards.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tallyowl_collector_api::types::{
    Envelope, HistogramValue, MetricKind as WireKind, MetricPointPayload,
    MetricPointPayload_temporality as Temporality, PropertyOrigin, TelemetryItem, TelemetryKind,
};
use tallyowl_obs::metrics::{MetricKind, Reading, Registry};
use tallyowl_obs::time::now_ms;
use tallyowl_wire::{collector as wire, collector_items_bridge as items, Value};

use crate::compat::Edge;

/// The service name every self-metric carries, so a chart can tell a collector
/// from a head without reading the labels.
pub fn items_for(registry: &Registry, service_name: &str) -> Vec<TelemetryItem> {
    let started_at = registry.started_at_ms();
    let now = now_ms();
    registry
        .snapshot()
        .into_iter()
        .map(|reading| item_for(reading, service_name, started_at, now))
        .collect()
}

fn item_for(reading: Reading, service_name: &str, started_at: i64, now: i64) -> TelemetryItem {
    let kind = match reading.kind {
        MetricKind::Counter => WireKind::Counter,
        MetricKind::Gauge => WireKind::Gauge,
        MetricKind::Histogram => WireKind::Histogram,
    };
    let gauge = reading.kind == MetricKind::Gauge;
    let payload = MetricPointPayload {
        metric_name: reading.name,
        metric_kind: kind,
        unit: None,
        description: (!reading.help.is_empty()).then_some(reading.help),
        monotonic: reading.kind == MetricKind::Counter,
        // A registry holds a running total, never a period. Reporting a delta
        // would need this module to remember the previous reading, and a lost
        // batch would then be a permanent hole in the total.
        temporality: Temporality::Cumulative,
        // A gauge is an observation at a moment, so both ends of it are now.
        start_at: if gauge { now } else { started_at },
        end_at: now,
        labels: reading
            .labels
            .iter()
            .map(|(key, value)| {
                // The origin is `collector`: these values come from the
                // service's own configuration and its own code, so an operator
                // filtering on origin gets a true answer.
                wire::property(key, Value::Text(value.clone()), PropertyOrigin::Collector)
            })
            .collect(),
        number_value: (!matches!(reading.kind, MetricKind::Histogram)).then_some(reading.value),
        histogram_value: reading.histogram.map(|histogram| HistogramValue {
            count: histogram.count,
            sum: histogram.sum,
            bounds: histogram.bounds,
            counts: histogram.counts,
        }),
        exemplar_trace_id: None,
    };
    items::metric_point(envelope(service_name, now), payload)
}

fn envelope(service_name: &str, occurred_at: i64) -> Envelope {
    Envelope {
        event_id: crate::compat::new_event_id(),
        kind: TelemetryKind::MetricPoint,
        schema_version: 1,
        occurred_at,
        observed_at: None,
        // Intake stamps the receive time and the tenancy, exactly as it does
        // for an application's batch. A service observing itself gets no
        // shortcut around the trust boundary.
        received_at: None,
        workspace_id: None,
        project_id: None,
        source_id: None,
        sequence: None,
        release: Some(env!("CARGO_PKG_VERSION").to_string()),
        service_name: Some(service_name.to_string()),
        request_id: None,
        session_id: None,
        end_user_id: None,
        anonymous_id: None,
        trace_id: None,
        span_id: None,
        consent: None,
        sdk_name: "tallyowl-self-observation".to_string(),
        sdk_version: env!("CARGO_PKG_VERSION").to_string(),
        properties: Vec::new(),
        measurements: None,
    }
}

/// The running publisher. Dropping the handle asks it to stop.
pub struct Publisher {
    stopping: Arc<AtomicBool>,
}

impl Publisher {
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }
}

/// Start pushing this process's instruments on a period.
///
/// Returns `None` when an operator did not ask for it, which is the default.
pub fn start(
    enabled: bool,
    period: Duration,
    service_name: &str,
    registry: Arc<Registry>,
    edge: Arc<Edge>,
) -> Option<Publisher> {
    if !enabled {
        edge.logger.info(
            "Self-observation is off, so this service publishes its instruments to its own endpoint and to nothing else. Turn it on with `metrics.selfObservation.enabled`.",
            &[],
        );
        return None;
    }
    let stopping = Arc::new(AtomicBool::new(false));
    let loop_stopping = Arc::clone(&stopping);
    let service_name = service_name.to_string();
    edge.logger.info(
        "Publishing this service's own instruments to the internal project.",
        &[("period_ms", &period.as_millis().to_string())],
    );
    std::thread::Builder::new()
        .name("tallyowl-self-observation".into())
        .spawn(move || {
            while !loop_stopping.load(Ordering::Relaxed) {
                let mut slept = Duration::ZERO;
                while slept < period && !loop_stopping.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                    slept += Duration::from_millis(100);
                }
                if loop_stopping.load(Ordering::Relaxed) {
                    break;
                }
                publish_once(&service_name, &registry, &edge);
            }
        })
        .ok()?;
    Some(Publisher { stopping })
}

/// One push, with the recursion guard held for the whole of it.
pub fn publish_once(service_name: &str, registry: &Registry, edge: &Edge) {
    let Some(_guard) = registry.begin_publishing() else {
        // A push is already in flight. Skipping is right: two overlapping
        // pushes would each measure the other.
        return;
    };
    let items = items_for(registry, service_name);
    if items.is_empty() {
        return;
    }
    if let Err(reason) = edge.offer_self_observation(items) {
        edge.logger.warning(
            "This service could not publish its own instruments. Its endpoint still serves them.",
            &[("reason", &reason)],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tallyowl_obs::metrics::labels;

    fn registry() -> Arc<Registry> {
        let registry = Registry::new();
        registry
            .declare(
                "tallyowl_batches_accepted_total",
                MetricKind::Counter,
                "Batches accepted.",
                &[],
            )
            .unwrap();
        registry
            .declare(
                "tallyowl_queue_depth_count",
                MetricKind::Gauge,
                "Tasks waiting.",
                &[],
            )
            .unwrap();
        registry
            .declare(
                "tallyowl_commit_seconds",
                MetricKind::Histogram,
                "How long a commit took.",
                &[0.01, 0.1, 1.0],
            )
            .unwrap();
        registry
    }

    fn point(item: &TelemetryItem) -> &MetricPointPayload {
        item.metric_point.as_ref().expect("a metric point")
    }

    fn find<'a>(items: &'a [TelemetryItem], name: &str) -> &'a MetricPointPayload {
        items
            .iter()
            .map(point)
            .find(|p| p.metric_name == name)
            .expect("the snapshot holds this metric")
    }

    #[test]
    fn every_instrument_reaches_the_native_path_in_the_same_shape_the_endpoint_shows() {
        let registry = registry();
        let none = labels(&[]);
        registry.add("tallyowl_batches_accepted_total", &none, 7);
        registry.set_gauge("tallyowl_queue_depth_count", &none, -3);
        registry.observe("tallyowl_commit_seconds", &none, 0.05);

        let items = items_for(&registry, "tallyowl-collector");
        assert_eq!(items.len(), 3);

        let counter = find(&items, "tallyowl_batches_accepted_total");
        assert_eq!(counter.number_value, Some(7.0));
        assert!(counter.monotonic);
        assert_eq!(counter.temporality, Temporality::Cumulative);
        assert_eq!(counter.description.as_deref(), Some("Batches accepted."));

        // A gauge holds a negative level, and the text exposition says the same.
        let gauge = find(&items, "tallyowl_queue_depth_count");
        assert_eq!(gauge.number_value, Some(-3.0));
        assert!(registry
            .render_text()
            .contains("tallyowl_queue_depth_count -3"));

        let histogram = find(&items, "tallyowl_commit_seconds")
            .histogram_value
            .as_ref()
            .expect("a histogram point carries buckets");
        assert_eq!(histogram.bounds, vec![0.01, 0.1, 1.0]);
        assert_eq!(histogram.counts, vec![0, 1, 1]);
        assert_eq!(histogram.count, 1);
    }

    #[test]
    fn a_counter_reports_the_process_start_so_a_restart_is_visible() {
        let registry = registry();
        registry.add("tallyowl_batches_accepted_total", &labels(&[]), 1);
        let items = items_for(&registry, "tallyowl-collector");
        let counter = find(&items, "tallyowl_batches_accepted_total");
        assert_eq!(counter.start_at, registry.started_at_ms());
        assert!(counter.end_at >= counter.start_at);
    }

    #[test]
    fn a_gauge_reports_a_moment_rather_than_a_period() {
        let registry = registry();
        registry.set_gauge("tallyowl_queue_depth_count", &labels(&[]), 4);
        let items = items_for(&registry, "tallyowl-collector");
        let gauge = find(&items, "tallyowl_queue_depth_count");
        assert_eq!(gauge.start_at, gauge.end_at);
    }

    #[test]
    fn the_recursion_guard_stops_a_push_from_measuring_itself() {
        // This is the guard D12 asks for. While a push is in flight, recording
        // is suppressed, so the batch the push creates does not raise the
        // counter the push is reporting.
        let registry = registry();
        let none = labels(&[]);
        registry.add("tallyowl_batches_accepted_total", &none, 5);

        {
            let _guard = registry
                .begin_publishing()
                .expect("nothing else is publishing");
            // The work the push does. None of it lands.
            registry.add("tallyowl_batches_accepted_total", &none, 1);
            registry.add("tallyowl_batches_accepted_total", &none, 1);
        }

        assert_eq!(
            registry.counter_value("tallyowl_batches_accepted_total", &none),
            5,
            "a push does not measure itself"
        );

        // The guard is gone, so ordinary work counts again.
        registry.add("tallyowl_batches_accepted_total", &none, 2);
        assert_eq!(
            registry.counter_value("tallyowl_batches_accepted_total", &none),
            7
        );
    }

    #[test]
    fn a_second_push_skips_rather_than_overlapping_the_first() {
        let registry = registry();
        let first = registry.begin_publishing().expect("the first one takes it");
        assert!(
            registry.begin_publishing().is_none(),
            "a push already in flight makes the next period skip"
        );
        drop(first);
        assert!(registry.begin_publishing().is_some());
    }

    #[test]
    fn a_self_metric_never_carries_tenancy_of_its_own() {
        // Never accept tenancy from a payload. Intake stamps it, and a service
        // observing itself gets no shortcut around the trust boundary.
        let registry = registry();
        registry.add("tallyowl_batches_accepted_total", &labels(&[]), 1);
        let items = items_for(&registry, "tallyowl-collector");
        let envelope = &items[0].envelope;
        assert!(envelope.workspace_id.is_none());
        assert!(envelope.project_id.is_none());
        assert!(envelope.received_at.is_none());
        assert_eq!(envelope.service_name.as_deref(), Some("tallyowl-collector"));
    }

    #[test]
    fn a_label_on_a_self_metric_carries_a_collector_origin() {
        let registry = registry();
        registry.add(
            "tallyowl_batches_accepted_total",
            &labels(&[("reason", "ok")]),
            1,
        );
        let items = items_for(&registry, "tallyowl-collector");
        let label = &point(&items[0]).labels[0];
        assert_eq!(label.key, "reason");
        assert_eq!(label.origin, PropertyOrigin::Collector);
    }

    #[test]
    fn an_empty_registry_publishes_nothing() {
        let registry = Registry::new();
        assert!(items_for(&registry, "tallyowl-collector").is_empty());
    }
}
