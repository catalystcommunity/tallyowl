//! The forwarder role: drain the durable queue into the head, and keep the
//! timeout sweep running.
//!
//! Two jobs, and the second one is easy to forget:
//!
//! 1. claim a queued batch, send it to head ingest, and complete the task only
//!    after a valid committed receipt;
//! 2. call the Corndogs timeout sweep on an interval.
//!
//! **Nothing retries without the sweep.** Corndogs evaluates a task timeout only
//! when a caller invokes `CleanUpTimedOut`. A forwarder that stops sweeping
//! stops retry, backoff, and dead-worker recovery at the same moment, and every
//! one of those failures is silent. Readiness is therefore tied to the sweep.
//! See D33 and CONVENTIONS.md section 3.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tallyowl_collector_api::codec::encode_commit_batch_request;
use tallyowl_collector_api::types::CommitBatchRequest;
use tallyowl_obs::error::TallyOwlError;
use tallyowl_obs::health::Health;
use tallyowl_obs::log::Logger;
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_obs::time::{now_ms, now_nanos};

use crate::durable::{ClaimedTask, DurableQueue};
use crate::head_client::HeadClient;

/// The health check name the sweep owns. Readiness fails when it stops.
pub const SWEEP_CHECK: &str = "retry-sweep";
/// The health check name the durable store owns.
pub const DURABLE_STORE_CHECK: &str = "durable-store";

/// How long a claim lasts before the sweep returns the task. A worker that dies
/// mid-send releases its task after this, and not before.
const CLAIM_TIMEOUT_SECONDS: i64 = 30;

/// Retry delays, in seconds, indexed by how many attempts have already failed.
///
/// Capped at a minute, so a head that is down for an hour does not produce an
/// hour-long gap after it comes back. The attempt count comes from the task
/// payload, because Corndogs holds no count of its own. See DELIVERY.md
/// section 4 and L012.
const BACKOFF_SECONDS: &[i64] = &[1, 2, 5, 10, 30, 60];

/// The delay for a task that has already failed `attempts` times, with jitter.
///
/// Jitter matters more than the curve. Every batch that a head outage parked
/// comes back at the same moment without it, and the head meets the whole
/// queue at once on the second it recovers.
fn backoff_seconds(attempts: u64, spread: u64) -> i64 {
    let index = (attempts as usize).min(BACKOFF_SECONDS.len() - 1);
    let base = BACKOFF_SECONDS[index];
    // Up to a quarter more, never less. Less would let a retry outrun its own
    // backoff, and the point of the delay is that it is a floor.
    let extra = if base > 3 {
        (spread % ((base / 4) as u64 + 1)) as i64
    } else {
        0
    };
    base + extra
}

/// What the forwarder shares with the health endpoint and the metrics endpoint.
#[derive(Debug, Default)]
pub struct ForwarderState {
    pub last_sweep_ms: AtomicI64,
    pub delivered: AtomicU64,
    pub quarantined: AtomicU64,
    pub attempts: AtomicU64,
    pub stopping: AtomicBool,
    /// What the delivery queue held at the last sweep. The queue is the one
    /// that knows (L146); this is a copy the health report reads without a
    /// round trip.
    pub queue_depth: AtomicI64,
    /// When the oldest batch this process has touched and not delivered was
    /// accepted. Zero means none is known: a restart loses the age and keeps
    /// the depth, and the report saying so beats a plausible wrong number. The
    /// next claim of an old task re-learns it.
    pub oldest_waiting_at: AtomicI64,
}

impl ForwarderState {
    pub fn new() -> Arc<ForwarderState> {
        Arc::new(ForwarderState::default())
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }

    /// Note a batch that is waiting, keeping the oldest acceptance time.
    pub fn note_waiting(&self, accepted_at_ms: i64) {
        if accepted_at_ms <= 0 {
            return;
        }
        let held = self.oldest_waiting_at.load(Ordering::Relaxed);
        if held == 0 || accepted_at_ms < held {
            self.oldest_waiting_at
                .store(accepted_at_ms, Ordering::Relaxed);
        }
    }
}

pub struct Forwarder {
    pub queue: Arc<dyn DurableQueue>,
    pub queue_name: String,
    pub quarantine_queue: String,
    pub head: Arc<dyn HeadClient>,
    pub health: Arc<Health>,
    pub metrics: Arc<Registry>,
    pub logger: Arc<Logger>,
    pub state: Arc<ForwarderState>,
    /// How stale the last sweep may be before readiness fails. Twice the
    /// interval, so one slow tick is not an outage.
    pub sweep_staleness_ms: i64,
    /// The largest batch this forwarder will produce from a queued payload.
    pub max_payload_bytes: u64,
    /// How long a batch may keep being retried before it goes to quarantine.
    ///
    /// D36 pairs the retry window with the deduplication window: an automated
    /// retry must stop before the head forgets the batch ID, or a later retry
    /// commits a second logical batch. A batch that reaches this age needs a
    /// person and an explicit replay, not another attempt.
    pub max_delivery_age_ms: i64,
}

impl Forwarder {
    pub fn declare_metrics(metrics: &Registry) {
        metrics.declare(
            "tallyowl_batches_delivered_total",
            tallyowl_obs::MetricKind::Counter,
            "Batches the forwarder delivered to the head and completed.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_batches_delivered_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_delivery_attempts_total",
            tallyowl_obs::MetricKind::Counter,
            "Delivery attempts, by outcome.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_delivery_attempts_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_batches_quarantined_total",
            tallyowl_obs::MetricKind::Counter,
            "Batches moved to quarantine because no retry can fix them.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_batches_quarantined_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_retry_sweeps_total",
            tallyowl_obs::MetricKind::Counter,
            "Calls to the durable store's timeout sweep. Retry stops when this stops.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_retry_sweeps_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_retry_sweep_age_seconds",
            tallyowl_obs::MetricKind::Gauge,
            "How long ago the timeout sweep last ran.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_retry_sweep_age_seconds` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_delivery_queue_depth_count",
            tallyowl_obs::MetricKind::Gauge,
            "Batches the delivery queue holds that have not committed. Work in \
             a backoff counts as waiting: it is going to run and it has not.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_delivery_queue_depth_count` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_delivery_oldest_waiting_ms",
            tallyowl_obs::MetricKind::Gauge,
            "How long the oldest waiting batch this process has touched has \
             been waiting. A restart reads zero until the next claim, which is \
             a lower bound and says so.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_delivery_oldest_waiting_ms` is not a name the registry accepts: {}", e.0));
    }

    /// One sweep. Public so a test can run exactly one rather than racing a
    /// thread.
    pub fn sweep_once(&self) {
        match self.queue.sweep(&self.queue_name, now_nanos()) {
            Ok(moved) => {
                self.state.last_sweep_ms.store(now_ms(), Ordering::Relaxed);
                self.metrics
                    .increment("tallyowl_retry_sweeps_total", &labels(&[]));
                self.health.pass(SWEEP_CHECK);
                self.health.pass(DURABLE_STORE_CHECK);
                self.publish_queue_depth();
                if moved > 0 {
                    self.logger.info(
                        "Returned batches for another delivery attempt.",
                        &[("returned", &moved.to_string())],
                    );
                }
            }
            Err(e) => {
                // A stopped sweep stops retry and dead-worker recovery together,
                // so this is a readiness failure rather than a warning.
                self.health.fail(
                    SWEEP_CHECK,
                    "Cannot reach the durable store, so a failed delivery will not be tried again.",
                );
                self.health
                    .fail(DURABLE_STORE_CHECK, "Cannot reach the durable store.");
                self.logger
                    .warning("The retry sweep did not run.", &[("reason", &e.message)]);
            }
        }
        self.publish_sweep_age();
    }

    fn publish_sweep_age(&self) {
        let last = self.state.last_sweep_ms.load(Ordering::Relaxed);
        if last > 0 {
            self.metrics.set_gauge(
                "tallyowl_retry_sweep_age_seconds",
                &labels(&[]),
                (now_ms() - last) / 1000,
            );
        }
    }

    /// Readiness for the sweep, judged by age rather than by the last result. A
    /// sweep that succeeded once and then stopped being called is exactly the
    /// silent failure this check exists to catch.
    pub fn check_sweep_age(&self) {
        let last = self.state.last_sweep_ms.load(Ordering::Relaxed);
        if last == 0 {
            return;
        }
        if now_ms() - last > self.sweep_staleness_ms {
            self.health.fail(
                SWEEP_CHECK,
                "The retry sweep has stopped running, so a failed delivery will not be tried again.",
            );
        }
        self.publish_sweep_age();
    }

    /// Read what the delivery queue holds and publish it.
    ///
    /// The depth comes from the queue, which is the one that knows (L146). The
    /// age is this process's own lower bound, and a depth with no age means a
    /// restart lost the age and kept the depth.
    fn publish_queue_depth(&self) {
        let Ok(counts) = self.queue.counts() else {
            return;
        };
        let depth = counts
            .iter()
            .filter(|c| c.queue == self.queue_name)
            .map(|c| {
                c.in_state(crate::durable::STATE_QUEUED)
                    + c.in_state(crate::durable::STATE_SENDING)
                    + c.in_state(crate::durable::STATE_BACKOFF)
            })
            .sum::<i64>();
        self.state.queue_depth.store(depth, Ordering::Relaxed);
        if depth == 0 {
            // Nothing is waiting, so nothing is oldest. Without this, the age
            // of a batch long delivered would keep rising for ever.
            self.state.oldest_waiting_at.store(0, Ordering::Relaxed);
        }
        self.metrics
            .set_gauge("tallyowl_delivery_queue_depth_count", &labels(&[]), depth);
        let oldest = self.state.oldest_waiting_at.load(Ordering::Relaxed);
        let age = if depth > 0 && oldest > 0 {
            now_ms() - oldest
        } else {
            0
        };
        self.metrics
            .set_gauge("tallyowl_delivery_oldest_waiting_ms", &labels(&[]), age);
    }

    /// Claim and deliver one batch. Returns whether it found work.
    pub fn deliver_one(&self) -> bool {
        let claimed = match self.queue.claim(&self.queue_name, CLAIM_TIMEOUT_SECONDS) {
            Ok(Some(task)) => task,
            Ok(None) => return false,
            Err(e) => {
                self.health
                    .fail(DURABLE_STORE_CHECK, "Cannot reach the durable store.");
                self.logger
                    .warning("Could not claim a batch.", &[("reason", &e.message)]);
                return false;
            }
        };
        self.state.attempts.fetch_add(1, Ordering::Relaxed);
        self.deliver(&claimed);
        true
    }

    fn deliver(&self, task: &ClaimedTask) {
        // A payload that does not decode cannot become valid later, and neither
        // can one this build is too old to read. A retry loop over either burns
        // the queue for nothing.
        let mut delivery = match crate::task::decode(&task.payload) {
            Ok(delivery) => delivery,
            Err(e) => {
                self.quarantine(task, &e.message);
                return;
            }
        };
        // The oldest-waiting age re-learns itself from the work: a claim of an
        // old batch after a restart brings the age back.
        self.state.note_waiting(delivery.accepted_at);
        let batch = match crate::task::open(&delivery, self.max_payload_bytes) {
            Ok(batch) => batch,
            Err(e) => {
                self.quarantine(task, &e.message);
                return;
            }
        };
        let batch_id = hex(&batch.batch_id);
        let source_id = delivery.source_id.clone();

        let request = CommitBatchRequest {
            batch,
            source_id,
            // The attempt travels, so the head can see a retry for what it is.
            // It is Corndogs workflow metadata and never part of the logical
            // identity of the batch. See DELIVERY.md section 2.
            attempt: Some(delivery.attempts),
            // This collector's own protocol version. The head accepts it and
            // the version before it, which is what makes the upgrade order in
            // DEPLOYMENT.md section 7 safe: the head goes first, so a
            // collector is briefly one version behind the head it forwards to.
            protocol_version: Some(tallyowl_wire::protocol::PROTOCOL_VERSION),
        };

        match self
            .head
            .commit_batch(encode_commit_batch_request(&request))
        {
            Ok(receipt) => {
                // The task completes only after a valid committed receipt. If
                // the connection had dropped after the commit, the retry would
                // have carried the same batch ID and the head would have
                // returned the prior receipt.
                if let Err(e) = self.queue.complete(task) {
                    // The head committed and the queue did not hear it. The
                    // sweep returns the task, the head deduplicates the batch
                    // ID, and the result is one logical commit.
                    self.logger.warning(
                        "A batch committed and the durable store did not record it. It will be delivered again and counted once.",
                        &[("batch_id", &batch_id), ("reason", &e.message)],
                    );
                    return;
                }
                self.state.delivered.fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .increment("tallyowl_batches_delivered_total", &labels(&[]));
                self.metrics.increment(
                    "tallyowl_delivery_attempts_total",
                    &labels(&[("outcome", "committed")]),
                );
                self.logger.info(
                    "Delivered a batch.",
                    &[
                        ("batch_id", &batch_id),
                        ("accepted", &receipt.accepted.to_string()),
                        ("deduplicated", &receipt.deduplicated.to_string()),
                    ],
                );
            }
            Err(e) if e.retryable => {
                let now = now_ms();
                let age = now - delivery.accepted_at;
                if age > self.max_delivery_age_ms {
                    // Retrying past the deduplication window would commit a
                    // second logical batch. Stop, and say what has to happen
                    // instead.
                    self.metrics.increment(
                        "tallyowl_delivery_attempts_total",
                        &labels(&[("outcome", "gave-up")]),
                    );
                    self.quarantine(
                        task,
                        &format!(
                            "This batch has been retried for {} minutes and still cannot reach \
                             the head. Automatic retry stops here, because a retry after the \
                             head has forgotten the batch would be counted twice. The last \
                             failure was: {}",
                            age / 60_000,
                            e.message
                        ),
                    );
                    return;
                }

                self.metrics.increment(
                    "tallyowl_delivery_attempts_total",
                    &labels(&[("outcome", "retry")]),
                );
                let delay = backoff_seconds(delivery.attempts, self.spread());
                crate::task::note_attempt(&mut delivery, now, delay * 1_000, &e.message);
                if let Err(park_error) =
                    self.queue
                        .park(task, delay, Some(crate::task::encode(&delivery)))
                {
                    self.logger.warning(
                        "A batch could not be delayed for another attempt.",
                        &[("batch_id", &batch_id), ("reason", &park_error.message)],
                    );
                    return;
                }
                self.logger.warning(
                    "A batch did not reach the head and will be tried again.",
                    &[
                        ("batch_id", &batch_id),
                        ("reason", &e.message),
                        ("attempts", &delivery.attempts.to_string()),
                        ("retry_in_seconds", &delay.to_string()),
                    ],
                );
            }
            Err(e) => {
                // Permanent. A retry storm against a rejection helps nobody.
                self.metrics.increment(
                    "tallyowl_delivery_attempts_total",
                    &labels(&[("outcome", "quarantined")]),
                );
                self.quarantine(task, &e.message);
            }
        }
    }

    fn quarantine(&self, task: &ClaimedTask, reason: &str) {
        if let Err(e) = self.queue.quarantine(task, &self.quarantine_queue) {
            self.logger.error(
                "A batch could not be quarantined and will be tried again.",
                &[("reason", &e.message)],
            );
            return;
        }
        self.state.quarantined.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .increment("tallyowl_batches_quarantined_total", &labels(&[]));
        self.logger.error(
            "A batch cannot be delivered and no retry will fix it. It is in quarantine for a person to look at.",
            &[("task", &task.uuid), ("reason", reason)],
        );
    }
}

/// Run the forwarder until its state says stop. Two threads: one sweeps on the
/// interval, one delivers.
pub fn run(
    forwarder: Arc<Forwarder>,
    sweep_interval: Duration,
) -> Vec<std::thread::JoinHandle<()>> {
    let sweeper = Arc::clone(&forwarder);
    let sweep_thread = std::thread::Builder::new()
        .name("tallyowl-sweep".into())
        .spawn(move || {
            while !sweeper.state.stopping.load(Ordering::Relaxed) {
                sweeper.sweep_once();
                std::thread::sleep(sweep_interval);
            }
        })
        .expect("the sweep thread starts");

    let deliverer = Arc::clone(&forwarder);
    let delivery_thread = std::thread::Builder::new()
        .name("tallyowl-delivery".into())
        .spawn(move || {
            while !deliverer.state.stopping.load(Ordering::Relaxed) {
                deliverer.check_sweep_age();
                if !deliverer.deliver_one() {
                    // Nothing waiting. Sleep briefly rather than spinning; a
                    // hot loop against the queue is the failure D33 warns about
                    // in the other direction.
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        })
        .expect("the delivery thread starts");

    vec![sweep_thread, delivery_thread]
}

impl Forwarder {
    /// A number that differs between two workers and between two moments. The
    /// jitter only has to spread a thundering herd, so the clock is enough and
    /// a random source would be one more thing to seed and reproduce.
    fn spread(&self) -> u64 {
        self.state
            .attempts
            .load(Ordering::Relaxed)
            .wrapping_mul(2_654_435_761)
            ^ (now_nanos() as u64)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A delivery outcome from the head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReceipt {
    pub accepted: u64,
    pub committed_at: i64,
    pub commit_watermark: u64,
    pub deduplicated: bool,
}

pub type DeliveryResult = Result<CommitReceipt, TallyOwlError>;
