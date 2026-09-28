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
//!
//! # One scrape is many batches
//!
//! Intake refuses a batch over `collector.maxBatchBytes` and cannot split it,
//! because a driver owns its batching. This edge is the driver here, so it
//! splits: one scrape of a large target, or one OpenTelemetry push, becomes as
//! many batches as it needs. Without that, a target with a few thousand series
//! is refused whole on every scrape, with advice written for a driver.
//!
//! # One target cannot stop the others
//!
//! Each target has its own next-due time, a small pool of workers reads them,
//! and a worker that panics is caught, counted, and used again. A slow target
//! holds one worker for at most `compatibility.prometheus.timeout`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tallyowl_collector_api::types::{Batch, SubmitBatchRequest, TelemetryItem};
use tallyowl_compat::receiver::{self, Kept, Sink};
use tallyowl_compat::scrape::{self, Scraper};
use tallyowl_obs::log::Logger;
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_obs::time::now_ms;

use tallyowl_obs::error::TallyOwlError;

use crate::intake::{Accepted, Intake};

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

/// What became of one offer that was not refused whole.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Offered {
    pub accepted: u64,
    /// Items intake named as rejected, and items in a batch the durable store
    /// did not take after an earlier batch of the same offer was kept.
    pub rejected: u64,
    /// The first reason, as intake wrote it.
    pub reason: Option<String>,
}

impl Edge {
    pub fn declare_metrics(metrics: &Registry) {
        let declare = |name: &str, help: &str| {
            metrics
                .declare(name, tallyowl_obs::MetricKind::Counter, help, &[])
                .unwrap_or_else(|e| {
                    panic!(
                        "the metric `{name}` is not a name the registry accepts: {}",
                        e.0
                    )
                });
        };
        declare(
            "tallyowl_compat_items_total",
            "Items a compatibility edge normalized and offered to intake, by source.",
        );
        declare(
            "tallyowl_compat_refused_total",
            "Compatibility batches intake refused, by source.",
        );
        declare(
            "tallyowl_compat_items_rejected_total",
            "Items from a compatibility edge that were offered and not kept, by source.",
        );
        declare(
            "tallyowl_compat_unreadable_total",
            "OpenTelemetry pushes answered without being read, by reason.",
        );
        declare(
            "tallyowl_scrape_failures_total",
            "Scrapes that did not reach their target.",
        );
        declare(
            "tallyowl_scrape_panics_total",
            "Scrapes that ended in a defect of this build. The target is skipped for that pass and scraping continues.",
        );
        declare(
            "tallyowl_scrape_overruns_total",
            "Scrapes that were still running when their next one was due, so that one was skipped.",
        );
        declare(
            "tallyowl_scrape_line_faults_total",
            "Lines a scrape target published that this build could not read.",
        );
        declare(
            "tallyowl_scrape_resets_total",
            "Scraped series whose cumulative value fell, which is a target restart.",
        );
    }

    /// Offer this service's own instruments to intake.
    ///
    /// It travels the same path as everything else, so a self-metric meets the
    /// same limits, the same tenancy stamp, and the same durable rule.
    pub fn offer_self_observation(&self, items: Vec<TelemetryItem>) -> Result<(), String> {
        self.offer("self-observation", None, items).map(|_| ())
    }

    /// How many encoded bytes one batch from this edge may hold.
    ///
    /// Half of what intake accepts. The size of a batch is the sum of its items
    /// plus a little framing, and half leaves room for that without measuring
    /// every batch twice.
    fn batch_budget(&self) -> usize {
        (self.intake.limits.max_batch_bytes.max(2) / 2) as usize
    }

    /// Offer normalized items to intake, in as many batches as they need.
    ///
    /// `Err` means nothing was kept, so the caller may offer all of it again.
    /// `Ok` means at least one batch reached the durable store, and the
    /// [`Offered`] counts what did not.
    fn offer(
        &self,
        source: &str,
        target: Option<&str>,
        items: Vec<TelemetryItem>,
    ) -> Result<Offered, String> {
        self.offer_through(source, target, items, &mut |request| {
            self.intake.submit(&self.credential, request)
        })
    }

    /// [`Edge::offer`], with the submit named by the caller so a test can fail
    /// one batch out of several.
    fn offer_through(
        &self,
        source: &str,
        target: Option<&str>,
        items: Vec<TelemetryItem>,
        submit: &mut dyn FnMut(SubmitBatchRequest) -> Result<Accepted, TallyOwlError>,
    ) -> Result<Offered, String> {
        if items.is_empty() {
            return Ok(Offered::default());
        }
        let mut out = Offered::default();
        let mut kept_any = false;

        for batch in split_by_size(items, self.batch_budget()) {
            let count = batch.len() as u64;
            let request = SubmitBatchRequest {
                batch: Batch {
                    // A fresh batch identifier for each offer. A compatibility
                    // edge has no stable identifier to reuse across a retry, so
                    // this makes no deduplication claim that would not hold.
                    batch_id: new_batch_id(),
                    items: batch,
                    common_properties: None,
                    sealed_at: now_ms(),
                    compression: None,
                },
                policy_version: None,
                protocol_version: Some(tallyowl_wire::protocol::PROTOCOL_VERSION),
            };
            match submit(request) {
                Ok(accepted) => {
                    kept_any = true;
                    self.metrics.add(
                        "tallyowl_compat_items_total",
                        &labels(&[("source", source)]),
                        count,
                    );
                    out.accepted += accepted.response.accepted;
                    if let Some(rejected) = &accepted.response.rejected {
                        out.rejected += rejected.len() as u64;
                        if out.reason.is_none() {
                            out.reason = rejected.first().map(|item| item.message.clone());
                        }
                    }
                    // Every task in the delivery queue says which producer made
                    // it.
                    //
                    // The RPC handler logs an accepted batch and this path did
                    // not, so a queue holding tasks from both looked like a
                    // queue with more tasks than anything accepted. Correlating
                    // the two halves of the path is the first thing anybody
                    // does when a count does not add up, and it was impossible.
                    self.logger.info(
                        "Accepted a batch into the durable store.",
                        &[
                            ("accepted", &accepted.response.accepted.to_string()),
                            ("task", &accepted.task_uuid),
                            ("batch_id", &hex(&accepted.response.batch_id)),
                            ("producer", source),
                        ],
                    );
                }
                Err(error) => {
                    self.metrics.increment(
                        "tallyowl_compat_refused_total",
                        &labels(&[("source", source)]),
                    );
                    if !kept_any {
                        // Nothing is kept yet, so the whole offer can go again.
                        // Trying the rest against a store that just refused
                        // would keep part of it and make a retry a duplicate.
                        return Err(error.message);
                    }
                    out.rejected += count;
                    if out.reason.is_none() {
                        out.reason = Some(error.message);
                    }
                }
            }
        }

        if out.rejected > 0 {
            // A refusal is one a producer can act on, and a scrape target has
            // no reply to read it from. This line is where it is said.
            self.metrics.add(
                "tallyowl_compat_items_rejected_total",
                &labels(&[("source", source)]),
                out.rejected,
            );
            self.logger.warning(
                "A compatibility edge offered items that TallyOwl did not keep.",
                &[
                    ("producer", source),
                    ("target", target.unwrap_or("")),
                    ("rejected", &out.rejected.to_string()),
                    ("accepted", &out.accepted.to_string()),
                    ("first_reason", out.reason.as_deref().unwrap_or("")),
                ],
            );
        }
        Ok(out)
    }
}

/// Group items into batches that each fit inside `budget` encoded bytes.
///
/// An item larger than the budget travels alone. Intake then refuses that one
/// item by its own limit and names it, which a batch refusal could not do.
fn split_by_size(items: Vec<TelemetryItem>, budget: usize) -> Vec<Vec<TelemetryItem>> {
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut held = 0usize;
    for item in items {
        let size = tallyowl_collector_api::codec::encode_telemetry_item(&item).len();
        if !current.is_empty() && held + size > budget {
            batches.push(std::mem::take(&mut current));
            held = 0;
        }
        held += size;
        current.push(item);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// The OpenTelemetry receiver's sink: straight into intake.
struct ReceiverSink {
    edge: Arc<Edge>,
}

impl Sink for ReceiverSink {
    fn accept(&self, items: Vec<TelemetryItem>) -> Result<Kept, String> {
        let offered = self.edge.offer("opentelemetry", None, items)?;
        Ok(Kept {
            rejected: offered.rejected,
            reason: offered.reason,
        })
    }

    fn unreadable(&self, reason: &'static str) {
        self.edge.metrics.increment(
            "tallyowl_compat_unreadable_total",
            &labels(&[("reason", reason)]),
        );
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
    /// TLS for the OpenTelemetry receiver, the same certificates intake serves.
    /// `None` on a loopback address. D62.
    pub open_telemetry_tls: Option<Arc<rustls::ServerConfig>>,
}

/// The bounds a scrape runs inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `compatibility.prometheus.maxBodyBytes`.
    pub scrape_max_body_bytes: usize,
    /// `compatibility.prometheus.workers`.
    pub scrape_workers: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            scrape_max_body_bytes: scrape::DEFAULT_MAX_BODY_BYTES,
            scrape_workers: 8,
        }
    }
}

/// Start whichever halves configuration enabled, and nothing else.
pub fn start(settings: Settings, edge: Arc<Edge>) -> Running {
    start_with(settings, Limits::default(), edge)
}

/// As [`start`], with the scrape bounds an operator configured.
pub fn start_with(settings: Settings, limits: Limits, edge: Arc<Edge>) -> Running {
    let stopping = Arc::new(AtomicBool::new(false));

    let receiver = if settings.open_telemetry_enabled {
        let sink: Arc<dyn Sink> = Arc::new(ReceiverSink {
            edge: Arc::clone(&edge),
        });
        match receiver::start_secured(
            &settings.open_telemetry_listen,
            sink,
            settings.open_telemetry_tls.clone(),
        ) {
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
        let scraper = Arc::new(
            Scraper::new(settings.scrape_targets.clone(), settings.scrape_timeout)
                .with_max_body_bytes(limits.scrape_max_body_bytes),
        );
        let workers = limits
            .scrape_workers
            .clamp(1, settings.scrape_targets.len());
        edge.logger.info(
            "Scraping the targets an operator configured.",
            &[
                ("targets", &settings.scrape_targets.join(",")),
                ("workers", &workers.to_string()),
            ],
        );
        start_scraping(
            scraper,
            edge,
            settings.scrape_interval,
            workers,
            Arc::clone(&stopping),
        );
    }

    Running { receiver, stopping }
}

/// When each target is next due, and which ones a worker is reading now.
///
/// The clock is the `now_ms` a caller passes, so the whole rule is tested
/// without waiting for anything.
#[derive(Debug)]
pub struct Schedule {
    interval_ms: i64,
    due_at: Vec<i64>,
    running: Vec<bool>,
}

impl Schedule {
    /// Spread the first scrape of each target across one interval.
    ///
    /// Targets that all fire at one instant make one burst of connections, one
    /// burst of batches, and one burst of load on every target at once. With
    /// an offset they arrive evenly, and they stay that way, because each
    /// target keeps its own slot.
    pub fn new(targets: usize, interval_ms: i64, start_ms: i64) -> Schedule {
        let interval_ms = interval_ms.max(1);
        let count = targets.max(1) as i64;
        Schedule {
            interval_ms,
            due_at: (0..targets as i64)
                .map(|index| start_ms + interval_ms * index / count)
                .collect(),
            running: vec![false; targets],
        }
    }

    /// The targets to read now. A target a worker is still reading is never
    /// handed out twice, so a scrape slower than its interval cannot pile up.
    pub fn take_due(&mut self, now_ms: i64) -> Vec<usize> {
        let mut due = Vec::new();
        for index in 0..self.due_at.len() {
            if !self.running[index] && self.due_at[index] <= now_ms {
                self.running[index] = true;
                due.push(index);
            }
        }
        due
    }

    /// A worker finished this target. Its next scrape keeps its slot, and
    /// the return value is how many slots passed while it ran.
    pub fn finished(&mut self, index: usize, now_ms: i64) -> u64 {
        self.running[index] = false;
        let mut next = self.due_at[index] + self.interval_ms;
        let mut skipped = 0;
        if next <= now_ms {
            skipped = (now_ms - next) / self.interval_ms + 1;
            next += skipped * self.interval_ms;
        }
        self.due_at[index] = next;
        skipped as u64
    }
}

/// How often the scheduler looks for a due target, and a worker for a stop.
const TICK: Duration = Duration::from_millis(100);

fn start_scraping(
    scraper: Arc<Scraper>,
    edge: Arc<Edge>,
    interval: Duration,
    workers: usize,
    stopping: Arc<AtomicBool>,
) {
    let schedule = Arc::new(Mutex::new(Schedule::new(
        scraper.targets().len(),
        interval.as_millis() as i64,
        now_ms(),
    )));
    // At most one entry for each target is ever in the channel, because a
    // target is not handed out again until a worker finishes it.
    let (sender, receiver) = mpsc::channel::<usize>();
    let receiver = Arc::new(Mutex::new(receiver));

    for worker in 0..workers {
        let scraper = Arc::clone(&scraper);
        let edge = Arc::clone(&edge);
        let schedule = Arc::clone(&schedule);
        let receiver = Arc::clone(&receiver);
        let stopping = Arc::clone(&stopping);
        let _ = std::thread::Builder::new()
            .name(format!("tallyowl-scrape-{worker}"))
            .spawn(move || {
                while !stopping.load(Ordering::Relaxed) {
                    let next = receiver
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .recv_timeout(TICK);
                    let index = match next {
                        Ok(index) => index,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    let target = &scraper.targets()[index];
                    run_guarded(&edge, target, || scrape_target(&scraper, &edge, target));
                    let skipped = schedule
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .finished(index, now_ms());
                    if skipped > 0 {
                        edge.metrics
                            .add("tallyowl_scrape_overruns_total", &labels(&[]), skipped);
                    }
                }
            });
    }

    let _ = std::thread::Builder::new()
        .name("tallyowl-scrape".into())
        .spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                let due = schedule
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take_due(now_ms());
                for index in due {
                    if sender.send(index).is_err() {
                        return;
                    }
                }
                // A short step, so a stop does not wait out a whole interval.
                std::thread::sleep(TICK);
            }
        });
}

/// Run one target's scrape so that a defect in it stops nothing else.
///
/// A panic on a scrape thread used to end that thread with no log line and no
/// metric, and no target was read again until the collector restarted.
fn run_guarded(edge: &Edge, target: &str, work: impl FnOnce()) {
    if let Err(panic) = catch_unwind(AssertUnwindSafe(work)) {
        let reason = panic
            .downcast_ref::<&str>()
            .map(|text| text.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "no message".to_string());
        edge.metrics
            .increment("tallyowl_scrape_panics_total", &labels(&[]));
        edge.logger.error(
            "A scrape ended in a defect of this build. This target is skipped for this pass, and scraping continues. Report it with the reason below.",
            &[("target", target), ("reason", &reason)],
        );
    }
}

/// One pass over every target, one after another.
pub fn scrape_once(scraper: &Scraper, edge: &Edge) {
    for target in scraper.targets() {
        run_guarded(edge, target, || scrape_target(scraper, edge, target));
    }
}

/// Read one target and offer what it published, with how the scrape went.
fn scrape_target(scraper: &Scraper, edge: &Edge, target: &str) {
    let started = Instant::now();
    let now = now_ms();
    let mut points = Vec::new();
    let up = match scraper.scrape_one(target, now) {
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
            points = result.points;
            true
        }
        Err(e) => {
            edge.metrics
                .increment("tallyowl_scrape_failures_total", &labels(&[]));
            edge.logger.warning(
                "A scrape target could not be read.",
                &[("target", target), ("reason", &e.message)],
            );
            false
        }
    };

    // `up` travels with every scrape, including a failed one. A target that is
    // down otherwise leaves nothing to query: its series simply stop, which is
    // the same shape as a target nobody configured.
    points.extend(scrape::target_health(
        target,
        up,
        started.elapsed().as_secs_f64(),
        now,
    ));
    let items: Vec<TelemetryItem> = points
        .into_iter()
        .map(|point| {
            tallyowl_wire::collector_items_bridge::metric_point(
                scraped_envelope(target, point.end_at),
                point,
            )
        })
        .collect();
    if let Err(reason) = edge.offer("prometheus", Some(target), items) {
        edge.logger.warning(
            "A scrape reached its target and TallyOwl could not keep the result.",
            &[("target", target), ("reason", &reason)],
        );
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
///
/// `tallyowl_compat::ids` holds the layout, and the reason it is written out:
/// the head removes duplicates by this value, so two items that share one are
/// one row.
pub(crate) fn new_event_id() -> Vec<u8> {
    new_batch_id()
}

fn new_batch_id() -> Vec<u8> {
    tallyowl_compat::ids::new_id(now_ms())
}

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
        edge.offer("prometheus", None, items)
            .expect("intake accepts it");

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
        edge.offer("prometheus", None, Vec::new())
            .expect("nothing to do");
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
        assert!(edge.offer("prometheus", None, items).is_err());
        assert_eq!(
            metrics.counter_value(
                "tallyowl_compat_refused_total",
                &labels(&[("source", "prometheus")])
            ),
            1
        );
    }

    fn scraped(text: &str, target: &str, now: i64) -> Vec<TelemetryItem> {
        Scraper::new(vec![], Duration::from_secs(1))
            .normalize(target, text, now)
            .points
            .into_iter()
            .map(|point| {
                tallyowl_wire::collector_items_bridge::metric_point(
                    scraped_envelope(target, point.end_at),
                    point,
                )
            })
            .collect()
    }

    fn many_series(count: usize) -> String {
        let mut text = String::from("# TYPE requests_total counter\n");
        for index in 0..count {
            text.push_str(&format!("requests_total{{route=\"/r{index}\"}} {index}\n"));
        }
        text
    }

    fn edge_with_batch_limit(
        queue: Arc<FakeQueue>,
        metrics: Arc<Registry>,
        max_batch_bytes: i64,
    ) -> Arc<Edge> {
        let mut intake = crate::tests::intake_for_compat(queue, Arc::clone(&metrics));
        intake.limits.max_batch_bytes = max_batch_bytes;
        Edge::declare_metrics(&metrics);
        Arc::new(Edge {
            intake: Arc::new(intake),
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
    fn a_scrape_larger_than_one_batch_is_split_rather_than_refused_whole() {
        // A target with a few thousand series is an ordinary target. As one
        // batch it is over the limit on every scrape and none of it arrives.
        let queue = FakeQueue::new();
        let metrics = Registry::new();
        let edge = edge_with_batch_limit(Arc::clone(&queue), Arc::clone(&metrics), 16 * 1024);
        let items = scraped(&many_series(400), "http://target:9100/metrics", 1_000);
        assert!(
            tallyowl_collector_api::codec::encode_batch(&Batch {
                batch_id: new_batch_id(),
                items: items.clone(),
                common_properties: None,
                sealed_at: 0,
                compression: None,
            })
            .len()
                > 16 * 1024,
            "the scrape must be larger than one batch for this to prove anything"
        );

        let offered = edge
            .offer("prometheus", Some("http://target:9100/metrics"), items)
            .expect("every batch fits");
        assert_eq!(offered.accepted, 400);
        assert_eq!(offered.rejected, 0);
        assert!(queue.depth() > 1, "it took more than one batch");
        assert_eq!(
            metrics.counter_value(
                "tallyowl_compat_refused_total",
                &labels(&[("source", "prometheus")])
            ),
            0
        );
    }

    #[test]
    fn every_batch_stays_inside_the_budget_and_an_oversized_item_travels_alone() {
        let items = scraped(&many_series(50), "t", 1_000);
        let one = tallyowl_collector_api::codec::encode_telemetry_item(&items[0]).len();
        let batches = split_by_size(items.clone(), one * 4);
        assert_eq!(
            batches.iter().map(Vec::len).sum::<usize>(),
            50,
            "nothing is lost"
        );
        for batch in &batches {
            let size: usize = batch
                .iter()
                .map(|item| tallyowl_collector_api::codec::encode_telemetry_item(item).len())
                .sum();
            assert!(size <= one * 4 || batch.len() == 1);
        }
        // A budget smaller than any item still makes progress: one for each.
        assert_eq!(split_by_size(items, 1).len(), 50);
    }

    #[test]
    fn an_item_intake_rejects_is_counted_and_its_reason_reaches_the_caller() {
        // L068: a refusal must be one a producer can act on. On this path the
        // producer hears nothing unless the edge passes it on.
        let queue = FakeQueue::new();
        let metrics = Registry::new();
        let edge = edge(Arc::clone(&queue), Arc::clone(&metrics));
        let mut items = scraped("# TYPE x_total counter\nx_total 1\n", "t", 1_000);
        // A point with no time. Intake rejects it by ID and keeps the rest.
        let mut undated = items[0].clone();
        undated.envelope.event_id = new_event_id();
        undated.envelope.occurred_at = 0;
        items.push(undated);

        let offered = edge
            .offer("prometheus", Some("t"), items)
            .expect("the batch is kept");
        assert_eq!(offered.accepted, 1);
        assert_eq!(offered.rejected, 1);
        assert!(offered.reason.is_some());
        assert_eq!(
            metrics.counter_value(
                "tallyowl_compat_items_rejected_total",
                &labels(&[("source", "prometheus")])
            ),
            1
        );
    }

    #[test]
    fn a_store_that_refuses_the_first_batch_is_not_tried_again_for_the_rest() {
        let queue = FakeQueue::new();
        let metrics = Registry::new();
        let edge = edge_with_batch_limit(Arc::clone(&queue), Arc::clone(&metrics), 16 * 1024);
        queue.refuse(true);
        let items = scraped(&many_series(400), "t", 1_000);
        assert!(edge.offer("opentelemetry", None, items).is_err());
        assert_eq!(
            metrics.counter_value(
                "tallyowl_compat_refused_total",
                &labels(&[("source", "opentelemetry")])
            ),
            1,
            "nothing was kept, so the exporter sends all of it again"
        );
        assert_eq!(queue.depth(), 0);
    }

    #[test]
    fn a_batch_that_fails_after_one_was_kept_is_a_partial_success_and_not_a_retry() {
        // A retry of the whole push would store the kept batch a second time
        // under new identifiers. So the exporter is told how many did not
        // arrive, and why.
        let queue = FakeQueue::new();
        let metrics = Registry::new();
        let edge = edge_with_batch_limit(Arc::clone(&queue), Arc::clone(&metrics), 16 * 1024);
        let items = scraped(&many_series(400), "t", 1_000);
        let mut calls = 0;
        let mut kept = 0u64;
        let offered = edge
            .offer_through("opentelemetry", None, items, &mut |request| {
                calls += 1;
                if calls == 2 {
                    return Err(TallyOwlError::unavailable(
                        "The durable store is unreachable.",
                    ));
                }
                kept += request.batch.items.len() as u64;
                edge.intake.submit(&edge.credential, request)
            })
            .expect("part of it was kept");
        assert!(
            calls > 2,
            "the batches after the failed one are still offered"
        );
        assert_eq!(offered.accepted, kept);
        assert_eq!(offered.accepted + offered.rejected, 400);
        assert!(offered
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("unreachable")));
    }

    #[test]
    fn a_receiver_sink_passes_on_what_was_not_kept_and_counts_an_unreadable_push() {
        let queue = FakeQueue::new();
        let metrics = Registry::new();
        let edge = edge(Arc::clone(&queue), Arc::clone(&metrics));
        let sink = ReceiverSink {
            edge: Arc::clone(&edge),
        };
        let mut items = scraped("# TYPE x_total counter\nx_total 1\n", "t", 1_000);
        items[0].envelope.occurred_at = 0;
        let kept = sink.accept(items).expect("the batch itself is accepted");
        assert_eq!(kept.rejected, 1);
        assert!(kept.reason.is_some());

        sink.unreadable("compressed");
        assert_eq!(
            metrics.counter_value(
                "tallyowl_compat_unreadable_total",
                &labels(&[("reason", "compressed")])
            ),
            1
        );
    }

    // ---- the schedule ------------------------------------------------------

    #[test]
    fn targets_do_not_all_fire_at_one_instant() {
        let mut schedule = Schedule::new(4, 60_000, 1_000);
        assert_eq!(
            schedule.take_due(1_000),
            vec![0],
            "only the first is due at the start"
        );
        assert_eq!(schedule.take_due(16_000), vec![1]);
        assert_eq!(schedule.take_due(46_000), vec![2, 3]);
    }

    #[test]
    fn a_target_still_being_read_is_not_handed_out_a_second_time() {
        let mut schedule = Schedule::new(1, 1_000, 0);
        assert_eq!(schedule.take_due(0), vec![0]);
        // Three intervals pass and the worker has not come back.
        assert!(schedule.take_due(3_500).is_empty());
        assert_eq!(
            schedule.finished(0, 3_500),
            3,
            "three slots passed while it ran"
        );
        // It keeps its slot and does not fire at once to catch up.
        assert!(schedule.take_due(3_900).is_empty());
        assert_eq!(schedule.take_due(4_000), vec![0]);
    }

    #[test]
    fn a_target_that_finishes_in_time_keeps_its_slot_exactly() {
        let mut schedule = Schedule::new(2, 1_000, 0);
        assert_eq!(schedule.take_due(500), vec![0, 1]);
        assert_eq!(schedule.finished(1, 700), 0);
        assert!(
            schedule.take_due(1_400).is_empty(),
            "target 1 is due at 1500"
        );
        assert_eq!(schedule.take_due(1_500), vec![1]);
    }

    #[test]
    fn a_defect_in_one_scrape_is_counted_and_the_next_scrape_still_runs() {
        let metrics = Registry::new();
        let edge = edge(FakeQueue::new(), Arc::clone(&metrics));
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        run_guarded(&edge, "http://bad/metrics", || panic!("a defect"));
        std::panic::set_hook(hook);
        assert_eq!(
            metrics.counter_value("tallyowl_scrape_panics_total", &labels(&[])),
            1
        );
        let mut ran = false;
        run_guarded(&edge, "http://good/metrics", || ran = true);
        assert!(ran);
    }

    #[test]
    fn a_target_that_is_down_still_leaves_something_to_query() {
        // Port 1 refuses at once, so nothing here waits.
        let queue = FakeQueue::new();
        let metrics = Registry::new();
        let edge = edge(Arc::clone(&queue), Arc::clone(&metrics));
        let scraper = Scraper::new(vec![], Duration::from_millis(200));
        scrape_target(&scraper, &edge, "http://127.0.0.1:1/metrics");

        assert_eq!(
            metrics.counter_value("tallyowl_scrape_failures_total", &labels(&[])),
            1
        );
        let stored = crate::tests::stored_batch_for_compat(&queue);
        let up = stored
            .items
            .iter()
            .filter_map(|item| item.metric_point.as_ref())
            .find(|point| point.metric_name == "up")
            .expect("an `up` point");
        assert_eq!(up.number_value, Some(0.0));
        assert!(up
            .labels
            .iter()
            .any(|label| label.key == "instance"
                && tallyowl_compat::label_text(label) == "127.0.0.1:1"));
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
                open_telemetry_tls: None,
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
                open_telemetry_tls: None,
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
