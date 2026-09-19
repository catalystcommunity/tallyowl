//! The compatibility edge, joined to collector intake.
//!
//! `tallyowl_compat` reads the two outside formats and turns them into native
//! items. This module is the join: it puts those items through the **same**
//! intake as a native batch, so a scraped counter and a driver's counter meet
//! the same limits, the same tenancy stamp, the same scrubber, and the same
//! durable-acknowledgement rule.
//!
//! Nothing here acknowledges anything. Intake does, and only after Corndogs
//! holds the batch.
//!
//! # Neither half starts by itself
//!
//! [`start`] returns nothing when configuration asks for nothing, which is the
//! default. See D12.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tallyowl_collector_api::types::{Batch, SubmitBatchRequest, TelemetryItem};
use tallyowl_compat::receiver::{self, Sink};
use tallyowl_compat::scrape::Scraper;
use tallyowl_obs::log::Logger;
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_obs::time::now_ms;

use crate::intake::Intake;

/// The credential a compatibility edge presents to intake.
///
/// A scrape target and an OpenTelemetry exporter do not carry a TallyOwl key,
/// so the collector presents its own. That is what decides the project the
/// data lands in, and it means an operator who enables a receiver has already
/// chosen the project by choosing `collector.apiKey`.
pub struct Edge {
    pub intake: Arc<Intake>,
    pub credential: String,
    pub metrics: Arc<Registry>,
    pub logger: Arc<Logger>,
}

impl Edge {
    pub fn declare_metrics(metrics: &Registry) {
        metrics.declare(
            "tallyowl_compat_items_total",
            tallyowl_obs::MetricKind::Counter,
            "Items a compatibility edge normalized and offered to intake, by source.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_compat_items_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_compat_refused_total",
            tallyowl_obs::MetricKind::Counter,
            "Compatibility batches intake refused, by source.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_compat_refused_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_scrape_failures_total",
            tallyowl_obs::MetricKind::Counter,
            "Scrapes that did not reach their target.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_scrape_failures_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_scrape_line_faults_total",
            tallyowl_obs::MetricKind::Counter,
            "Lines a scrape target published that this build could not read.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_scrape_line_faults_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_scrape_resets_total",
            tallyowl_obs::MetricKind::Counter,
            "Scraped series whose cumulative value fell, which is a target restart.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_scrape_resets_total` is not a name the registry accepts: {}", e.0));
    }

    /// Offer this service's own instruments to intake.
    ///
    /// It travels the same path as everything else, so a self-metric meets the
    /// same limits, the same tenancy stamp, and the same durable rule.
    pub fn offer_self_observation(&self, items: Vec<TelemetryItem>) -> Result<(), String> {
        self.offer("self-observation", items)
    }

    /// Offer normalized items to intake as one batch.
    fn offer(&self, source: &str, items: Vec<TelemetryItem>) -> Result<(), String> {
        if items.is_empty() {
            return Ok(());
        }
        let count = items.len() as u64;
        let request = SubmitBatchRequest {
            batch: Batch {
                // A fresh batch identifier for each offer. A compatibility edge
                // has no stable identifier to reuse across a retry, so this
                // makes no deduplication claim that would not hold.
                batch_id: crate::compat::new_batch_id(),
                items,
                common_properties: None,
                sealed_at: now_ms(),
                compression: None,
            },
            policy_version: None,
            protocol_version: Some(tallyowl_wire::protocol::PROTOCOL_VERSION),
        };
        match self.intake.submit(&self.credential, request) {
            Ok(accepted) => {
                self.metrics.add(
                    "tallyowl_compat_items_total",
                    &labels(&[("source", source)]),
                    count,
                );
                // Every task in the delivery queue says which producer made it.
                //
                // The RPC handler logs an accepted batch and this path did not,
                // so a queue holding tasks from both looked like a queue with
                // more tasks than anything accepted. Correlating the two halves
                // of the path is the first thing anybody does when a count does
                // not add up, and it was impossible.
                self.logger.info(
                    "Accepted a batch into the durable store.",
                    &[
                        ("accepted", &accepted.response.accepted.to_string()),
                        ("task", &accepted.task_uuid),
                        ("batch_id", &hex(&accepted.response.batch_id)),
                        ("producer", source),
                    ],
                );
                Ok(())
            }
            Err(error) => {
                self.metrics.increment(
                    "tallyowl_compat_refused_total",
                    &labels(&[("source", source)]),
                );
                Err(error.message)
            }
        }
    }
}

/// The OpenTelemetry receiver's sink: straight into intake.
struct ReceiverSink {
    edge: Arc<Edge>,
}

impl Sink for ReceiverSink {
    fn accept(&self, items: Vec<TelemetryItem>) -> Result<(), String> {
        self.edge.offer("opentelemetry", items)
    }
}

/// What was started, so a caller can stop it.
pub struct Running {
    pub receiver: Option<receiver::Receiver>,
    stopping: Arc<AtomicBool>,
}

impl Running {
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        if let Some(receiver) = &self.receiver {
            receiver.stop();
        }
    }
}

/// What an operator asked for.
pub struct Settings {
    pub open_telemetry_enabled: bool,
    pub open_telemetry_listen: String,
    pub scrape_targets: Vec<String>,
    pub scrape_interval: Duration,
    pub scrape_timeout: Duration,
}

/// Start whichever halves configuration enabled, and nothing else.
pub fn start(settings: Settings, edge: Arc<Edge>) -> Running {
    let stopping = Arc::new(AtomicBool::new(false));

    let receiver = if settings.open_telemetry_enabled {
        let sink: Arc<dyn Sink> = Arc::new(ReceiverSink {
            edge: Arc::clone(&edge),
        });
        match receiver::start(&settings.open_telemetry_listen, sink) {
            Ok(receiver) => {
                edge.logger.info(
                    "The OpenTelemetry receiver is listening. An operator enabled it.",
                    &[("address", &receiver.local_address().to_string())],
                );
                Some(receiver)
            }
            Err(e) => {
                // Failing to start the receiver does not stop the collector.
                // Native intake is the primary path and a compatibility edge is
                // an addition to it.
                edge.logger.error(
                    "The OpenTelemetry receiver could not listen. Native intake is unaffected.",
                    &[
                        ("address", &settings.open_telemetry_listen),
                        ("reason", &e.to_string()),
                    ],
                );
                None
            }
        }
    } else {
        None
    };

    if !settings.scrape_targets.is_empty() {
        let scraper = Arc::new(Scraper::new(
            settings.scrape_targets.clone(),
            settings.scrape_timeout,
        ));
        edge.logger.info(
            "Scraping the targets an operator configured.",
            &[("targets", &settings.scrape_targets.join(","))],
        );
        let loop_stopping = Arc::clone(&stopping);
        let interval = settings.scrape_interval;
        let _ = std::thread::Builder::new()
            .name("tallyowl-scrape".into())
            .spawn(move || {
                while !loop_stopping.load(Ordering::Relaxed) {
                    scrape_once(&scraper, &edge);
                    // A short sleep step, so a stop does not wait out a whole
                    // interval.
                    let mut slept = Duration::ZERO;
                    while slept < interval && !loop_stopping.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(100));
                        slept += Duration::from_millis(100);
                    }
                }
            });
    }

    Running { receiver, stopping }
}

/// One pass over every target.
pub fn scrape_once(scraper: &Scraper, edge: &Edge) {
    for target in scraper.targets() {
        match scraper.scrape_one(target, now_ms()) {
            Ok(result) => {
                if result.resets > 0 {
                    edge.metrics
                        .add("tallyowl_scrape_resets_total", &labels(&[]), result.resets);
                }
                if !result.faults.is_empty() {
                    edge.metrics.add(
                        "tallyowl_scrape_line_faults_total",
                        &labels(&[]),
                        result.faults.len() as u64,
                    );
                    edge.logger.warning(
                        "A scrape target published lines this build could not read. The rest of its metrics arrived.",
                        &[
                            ("target", target),
                            ("first_reason", &result.faults[0].reason),
                        ],
                    );
                }
                let items: Vec<TelemetryItem> = result
                    .points
                    .into_iter()
                    .map(|point| {
                        tallyowl_wire::collector_items_bridge::metric_point(
                            scraped_envelope(target, point.end_at),
                            point,
                        )
                    })
                    .collect();
                if let Err(reason) = edge.offer("prometheus", items) {
                    edge.logger.warning(
                        "A scrape reached its target and TallyOwl could not keep the result.",
                        &[("target", target), ("reason", &reason)],
                    );
                }
            }
            Err(e) => {
                edge.metrics
                    .increment("tallyowl_scrape_failures_total", &labels(&[]));
                edge.logger.warning(
                    "A scrape target could not be read.",
                    &[("target", target), ("reason", &e.message)],
                );
            }
        }
    }
}

/// The envelope a scraped point travels in.
///
/// The target address becomes the service name, because a scraped metric with
/// no service is impossible to tell apart from another target's on a chart.
fn scraped_envelope(target: &str, occurred_at: i64) -> tallyowl_collector_api::types::Envelope {
    use tallyowl_collector_api::types::{Envelope, TelemetryKind};
    Envelope {
        event_id: new_batch_id(),
        kind: TelemetryKind::MetricPoint,
        schema_version: 1,
        occurred_at,
        observed_at: Some(now_ms()),
        // Intake stamps the receive time and the tenancy, exactly as it does
        // for a native batch.
        received_at: None,
        workspace_id: None,
        project_id: None,
        source_id: None,
        sequence: None,
        release: None,
        service_name: Some(target.to_string()),
        request_id: None,
        session_id: None,
        end_user_id: None,
        anonymous_id: None,
        trace_id: None,
        span_id: None,
        consent: None,
        sdk_name: "tallyowl-compat-prometheus".to_string(),
        sdk_version: env!("CARGO_PKG_VERSION").to_string(),
        properties: Vec::new(),
        measurements: None,
    }
}

/// Hexadecimal, for a log line that has to be correlated with another one.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A UUIDv7 for a batch and for an item a compatibility edge produced.
pub(crate) fn new_event_id() -> Vec<u8> {
    new_batch_id()
}

fn new_batch_id() -> Vec<u8> {
    let mut id = vec![0u8; 16];
    let ms = now_ms().max(0) as u64;
    id[0] = (ms >> 40) as u8;
    id[1] = (ms >> 32) as u8;
    id[2] = (ms >> 24) as u8;
    id[3] = (ms >> 16) as u8;
    id[4] = (ms >> 8) as u8;
    id[5] = ms as u8;
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    id[6..14].copy_from_slice(&counter.to_le_bytes());
    id[6] = (id[6] & 0x0f) | 0x70;
    id[8] = (id[8] & 0x3f) | 0x80;
    id
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable::testing::FakeQueue;

    fn edge(queue: Arc<FakeQueue>, metrics: Arc<Registry>) -> Arc<Edge> {
        let intake = Arc::new(crate::tests::intake_for_compat(queue, Arc::clone(&metrics)));
        Edge::declare_metrics(&metrics);
        Arc::new(Edge {
            intake,
            credential: "key-a".to_string(),
            metrics,
            logger: Arc::new(tallyowl_obs::log::Logger::new(
                "test",
                "0.0.0",
                tallyowl_obs::log::Severity::Error,
            )),
        })
    }

    #[test]
    fn a_scraped_point_goes_through_the_same_intake_as_a_native_batch() {
        let queue = FakeQueue::new();
        let metrics = Registry::new();
        let edge = edge(Arc::clone(&queue), Arc::clone(&metrics));
        let scraper = Scraper::new(vec![], Duration::from_secs(1));
        let result = scraper.normalize(
            "http://target/metrics",
            "# TYPE requests_total counter\nrequests_total{route=\"/a\"} 5\n",
            1_000,
        );
        let items: Vec<TelemetryItem> = result
            .points
            .into_iter()
            .map(|point| {
                tallyowl_wire::collector_items_bridge::metric_point(
                    scraped_envelope("http://target/metrics", point.end_at),
                    point,
                )
            })
            .collect();
        edge.offer("prometheus", items).expect("intake accepts it");

        assert_eq!(queue.depth(), 1, "it reached the durable store");
        assert_eq!(
            metrics.counter_value(
                "tallyowl_compat_items_total",
                &labels(&[("source", "prometheus")])
            ),
            1
        );

        // The tenancy stamp is intake's, exactly as for a native batch. A
        // compatibility edge never sets one.
        let stored = crate::tests::stored_batch_for_compat(&queue);
        let envelope = &stored.items[0].envelope;
        assert!(envelope.project_id.is_some());
        assert!(envelope.received_at.is_some());
        assert_eq!(
            envelope.service_name.as_deref(),
            Some("http://target/metrics")
        );
    }

    #[test]
    fn an_empty_offer_reaches_nothing_rather_than_sending_an_empty_batch() {
        let queue = FakeQueue::new();
        let edge = edge(Arc::clone(&queue), Registry::new());
        edge.offer("prometheus", Vec::new()).expect("nothing to do");
        assert_eq!(queue.depth(), 0);
    }

    #[test]
    fn intake_refusing_the_batch_reaches_the_caller_rather_than_being_swallowed() {
        // An OpenTelemetry exporter must read a failure, or it will not retry.
        let queue = FakeQueue::new();
        let metrics = Registry::new();
        let edge = edge(Arc::clone(&queue), Arc::clone(&metrics));
        queue.refuse(true);
        let scraper = Scraper::new(vec![], Duration::from_secs(1));
        let result = scraper.normalize("t", "# TYPE x_total counter\nx_total 1\n", 1_000);
        let items: Vec<TelemetryItem> = result
            .points
            .into_iter()
            .map(|point| {
                tallyowl_wire::collector_items_bridge::metric_point(
                    scraped_envelope("t", point.end_at),
                    point,
                )
            })
            .collect();
        assert!(edge.offer("prometheus", items).is_err());
        assert_eq!(
            metrics.counter_value(
                "tallyowl_compat_refused_total",
                &labels(&[("source", "prometheus")])
            ),
            1
        );
    }

    #[test]
    fn nothing_starts_when_configuration_asks_for_nothing() {
        // The default installation. No port, no request.
        let queue = FakeQueue::new();
        let edge = edge(Arc::clone(&queue), Registry::new());
        let running = start(
            Settings {
                open_telemetry_enabled: false,
                open_telemetry_listen: "127.0.0.1:0".to_string(),
                scrape_targets: Vec::new(),
                scrape_interval: Duration::from_secs(60),
                scrape_timeout: Duration::from_secs(5),
            },
            edge,
        );
        assert!(running.receiver.is_none());
        running.stop();
    }

    #[test]
    fn the_receiver_listens_only_when_an_operator_enables_it() {
        let queue = FakeQueue::new();
        let edge = edge(Arc::clone(&queue), Registry::new());
        let running = start(
            Settings {
                open_telemetry_enabled: true,
                open_telemetry_listen: "127.0.0.1:0".to_string(),
                scrape_targets: Vec::new(),
                scrape_interval: Duration::from_secs(60),
                scrape_timeout: Duration::from_secs(5),
            },
            edge,
        );
        assert!(running.receiver.is_some());
        running.stop();
    }

    #[test]
    fn an_identifier_from_this_edge_is_a_uuid_version_7_and_does_not_repeat() {
        let first = new_batch_id();
        let second = new_batch_id();
        assert_eq!(first.len(), 16);
        assert_eq!(first[6] >> 4, 7);
        assert_ne!(first, second);
    }
}
