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

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
/// The health check name the delivery loop owns. Readiness fails when the loop
/// stops turning, which a sweep that still runs would otherwise hide.
pub const DELIVERY_CHECK: &str = "delivery-loop";

/// How long the delivery loop may go without finishing a turn before readiness
/// fails. One turn is at most a claim, a commit, and a completion, and each has
/// its own deadline, so a loop silent for this long is stuck and not slow.
const DELIVERY_STALENESS_MS: i64 = 120_000;

/// How often a repeated warning may be written. A durable store that is down
/// fails the sweep every interval, and one line for each failure buries the
/// line that says why.
const REPEATED_WARNING_MS: i64 = 30_000;

/// The most waiting batches whose age this process remembers. The oldest are
/// the ones kept, so the age it reports stays right.
const WAITING_REMEMBERED: usize = 10_000;

/// The first wait after the head, or the durable store, stops answering.
const BREAKER_FIRST_MS: i64 = 1_000;
/// The longest wait between two probes.
const BREAKER_LONGEST_MS: i64 = 30_000;

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
    /// When the delivery loop last finished a turn. Zero until the first one.
    pub last_delivery_loop_ms: AtomicI64,
    /// Every batch this process has claimed and not finished, oldest first.
    /// `oldest_waiting_at` is the first of these, so it falls when that batch
    /// is delivered or quarantined rather than rising for ever.
    waiting: Mutex<BTreeSet<(i64, String)>>,
    /// When the sweep last wrote its "did not run" warning.
    last_sweep_warning_ms: AtomicI64,
}

impl ForwarderState {
    pub fn new() -> Arc<ForwarderState> {
        Arc::new(ForwarderState::default())
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }

    /// Note a batch that is waiting, by the task that carries it.
    pub fn note_waiting(&self, task: &str, accepted_at_ms: i64) {
        if accepted_at_ms <= 0 {
            return;
        }
        let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        waiting.insert((accepted_at_ms, task.to_string()));
        // An outage claims every queued batch once. Keep the oldest, which are
        // the ones the age is read from, and let the newest go.
        while waiting.len() > WAITING_REMEMBERED {
            waiting.pop_last();
        }
        self.publish_oldest(&waiting);
    }

    /// A batch is finished: delivered, or set aside for a person. It is no
    /// longer waiting, so it can no longer be the oldest.
    pub fn note_finished(&self, task: &str, accepted_at_ms: i64) {
        let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        waiting.remove(&(accepted_at_ms, task.to_string()));
        self.publish_oldest(&waiting);
    }

    fn forget_waiting(&self) {
        let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        waiting.clear();
        self.publish_oldest(&waiting);
    }

    fn publish_oldest(&self, waiting: &BTreeSet<(i64, String)>) {
        let oldest = waiting.first().map(|(at, _)| *at).unwrap_or(0);
        self.oldest_waiting_at.store(oldest, Ordering::Relaxed);
    }
}

/// Paces the delivery loop when the destination, not the batch, is failing.
///
/// Backoff used to exist for each task only. With a backlog there was always
/// another ready task, so a head outage became a loop of claim, fail, rewrite
/// the payload, and warn, as fast as the durable store allowed, for the whole
/// outage. That loop took the durable store away from intake exactly when
/// intake was the only thing still protecting data.
///
/// After a failure that a retry can fix, the loop waits and then sends one
/// batch as a probe. The wait doubles to thirty seconds and resets on the first
/// success. Every other batch stays queued and untouched in the meantime.
#[derive(Debug, Default)]
pub struct Breaker {
    failures: u32,
    open_until_ms: i64,
}

impl Breaker {
    pub fn new() -> Breaker {
        Breaker::default()
    }

    /// Whether the last attempt failed, so the next one is a probe.
    pub fn is_tripped(&self) -> bool {
        self.failures > 0
    }

    /// How long the loop still has to wait. Zero means it may try.
    pub fn wait_ms(&self, now_ms: i64) -> i64 {
        (self.open_until_ms - now_ms).max(0)
    }

    /// Note a failure, and return how long the loop now waits. `spread` is any
    /// number that differs between two collectors, so that they do not all
    /// probe a recovering head in the same instant.
    pub fn on_failure(&mut self, now_ms: i64, spread: u64) -> i64 {
        let doubled = BREAKER_FIRST_MS
            .saturating_mul(1i64 << self.failures.min(16))
            .min(BREAKER_LONGEST_MS);
        self.failures = self.failures.saturating_add(1);
        // Up to a quarter more, never less, for the same reason as the task
        // backoff: the wait is a floor.
        let wait = doubled + (spread % (doubled as u64 / 4 + 1)) as i64;
        self.open_until_ms = now_ms + wait;
        wait
    }

    pub fn on_success(&mut self) {
        self.failures = 0;
        self.open_until_ms = 0;
    }
}

/// What one turn of the delivery loop found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Nothing was waiting.
    Idle,
    /// A batch was delivered, or set aside for a person. The destination works.
    Finished,
    /// The head or the durable store did not take the work, and a retry can fix
    /// it. The failure belongs to the destination.
    DestinationFailed,
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
            "How long the oldest batch this process has claimed and not finished \
             has been waiting. It falls when that batch is delivered. A restart \
             reads zero until the next claim, which is a lower bound and says so.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_delivery_oldest_waiting_ms` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_delivery_loop_age_seconds",
            tallyowl_obs::MetricKind::Gauge,
            "How long ago the delivery loop last finished a turn. Delivery has stopped when this keeps rising.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_delivery_loop_age_seconds` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_delivery_paused_ms",
            tallyowl_obs::MetricKind::Gauge,
            "How long the delivery loop is waiting before it probes a destination that stopped answering. Zero means it is delivering.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_delivery_paused_ms` is not a name the registry accepts: {}", e.0));
    }

    /// One sweep and one depth count. Public so a test can run exactly one
    /// rather than racing a thread.
    ///
    /// The running forwarder does not call this. It sweeps on one thread and
    /// counts on another, because the count reads every waiting task and a slow
    /// count must never make the sweep look stopped.
    pub fn sweep_once(&self) {
        if self.sweep_only() {
            self.count_once();
        }
    }

    /// One depth count, published.
    pub fn count_once(&self) {
        self.publish_queue_depth();
    }

    /// One sweep. Returns whether it ran.
    pub fn sweep_only(&self) -> bool {
        let swept = self.queue.sweep(&self.queue_name, now_nanos());
        let ran = swept.is_ok();
        match swept {
            Ok(moved) => {
                // Stamped before anything else, so nothing that follows can make
                // a sweep that ran look like one that did not.
                self.state.last_sweep_ms.store(now_ms(), Ordering::Relaxed);
                self.state.last_sweep_warning_ms.store(0, Ordering::Relaxed);
                self.metrics
                    .increment("tallyowl_retry_sweeps_total", &labels(&[]));
                self.health.pass(SWEEP_CHECK);
                self.health.pass(DURABLE_STORE_CHECK);
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
                // Once, and then at a slow repeat. The sweep fails every
                // interval for as long as the durable store is away.
                let now = now_ms();
                let last = self.state.last_sweep_warning_ms.load(Ordering::Relaxed);
                if last == 0 || now - last >= REPEATED_WARNING_MS {
                    self.state
                        .last_sweep_warning_ms
                        .store(now, Ordering::Relaxed);
                    self.logger.warning(
                        "The retry sweep did not run. This line repeats every 30 seconds until it does.",
                        &[("reason", &e.message)],
                    );
                }
            }
        }
        self.publish_sweep_age();
        ran
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

    /// Readiness for the delivery loop, judged by age at `now_ms`.
    ///
    /// The watchdog calls this, not the delivery loop: a loop that is stuck
    /// cannot report that it is stuck.
    pub fn check_delivery_age(&self, now_ms: i64) {
        let last = self.state.last_delivery_loop_ms.load(Ordering::Relaxed);
        if last == 0 {
            return;
        }
        let age = now_ms - last;
        self.metrics.set_gauge(
            "tallyowl_delivery_loop_age_seconds",
            &labels(&[]),
            age / 1000,
        );
        if age > DELIVERY_STALENESS_MS {
            self.health.fail(
                DELIVERY_CHECK,
                "The delivery loop has stopped turning, so accepted batches are not reaching the head.",
            );
        } else {
            self.health.pass(DELIVERY_CHECK);
        }
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
            // Nothing is waiting, so nothing is oldest. A batch that another
            // forwarder finished would otherwise stay remembered here.
            self.state.forget_waiting();
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
        let before = self.state.attempts.load(Ordering::Relaxed);
        self.deliver_step(false);
        self.state.attempts.load(Ordering::Relaxed) != before
    }

    /// One turn of the delivery loop at `now_ms`: wait out an open breaker, or
    /// claim and deliver one batch. Returns how long the loop sleeps next.
    ///
    /// The clock is a parameter so a test can walk through an outage without
    /// waiting for one.
    pub fn delivery_tick(&self, breaker: &mut Breaker, now_ms: i64) -> Duration {
        let waiting = breaker.wait_ms(now_ms);
        self.metrics
            .set_gauge("tallyowl_delivery_paused_ms", &labels(&[]), waiting);
        if waiting > 0 {
            // Short sleeps, so a stop is noticed and the heartbeat keeps going.
            return Duration::from_millis(waiting.min(250) as u64);
        }
        match self.deliver_step(breaker.is_tripped()) {
            // Nothing waiting. Sleep briefly rather than spinning; a hot loop
            // against the queue is the failure D33 warns about in the other
            // direction.
            Step::Idle => Duration::from_millis(25),
            Step::Finished => {
                if breaker.is_tripped() {
                    self.logger
                        .info("The destination is answering again. Delivery resumes.", &[]);
                }
                breaker.on_success();
                Duration::ZERO
            }
            Step::DestinationFailed => {
                let wait = breaker.on_failure(now_ms, self.spread());
                self.metrics
                    .set_gauge("tallyowl_delivery_paused_ms", &labels(&[]), wait);
                Duration::from_millis(wait.min(250) as u64)
            }
        }
    }

    /// Claim and deliver one batch.
    ///
    /// `probing` says the last attempt failed at the destination. A probe that
    /// fails is parked without its payload being written again, because the
    /// failure says nothing new about the batch.
    fn deliver_step(&self, probing: bool) -> Step {
        let claimed = match self.queue.claim(&self.queue_name, CLAIM_TIMEOUT_SECONDS) {
            Ok(Some(task)) => task,
            Ok(None) => return Step::Idle,
            Err(e) => {
                self.health
                    .fail(DURABLE_STORE_CHECK, "Cannot reach the durable store.");
                self.logger
                    .warning("Could not claim a batch.", &[("reason", &e.message)]);
                return Step::DestinationFailed;
            }
        };
        self.state.attempts.fetch_add(1, Ordering::Relaxed);
        self.deliver(&claimed, probing)
    }

    fn deliver(&self, task: &ClaimedTask, probing: bool) -> Step {
        // A payload that does not decode cannot become valid later, and neither
        // can one this build is too old to read. A retry loop over either burns
        // the queue for nothing.
        let mut delivery = match crate::task::decode(&task.payload) {
            Ok(delivery) => delivery,
            Err(e) => {
                self.quarantine(task, &e.message);
                return Step::Finished;
            }
        };
        // The oldest-waiting age re-learns itself from the work: a claim of an
        // old batch after a restart brings the age back.
        let accepted_at = delivery.accepted_at;
        self.state.note_waiting(&task.uuid, accepted_at);
        let batch = match crate::task::open(&delivery, self.max_payload_bytes) {
            Ok(batch) => batch,
            Err(e) => {
                if self.quarantine(task, &e.message) {
                    self.state.note_finished(&task.uuid, accepted_at);
                }
                return Step::Finished;
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
                    return Step::DestinationFailed;
                }
                self.state.note_finished(&task.uuid, accepted_at);
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
                Step::Finished
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
                    let set_aside = self.quarantine(
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
                    if set_aside {
                        self.state.note_finished(&task.uuid, accepted_at);
                    }
                    // The head still did not answer, so the loop keeps its pace.
                    return Step::DestinationFailed;
                }

                self.metrics.increment(
                    "tallyowl_delivery_attempts_total",
                    &labels(&[("outcome", "retry")]),
                );
                let delay = backoff_seconds(delivery.attempts, self.spread());
                // The attempt count lives in the payload, so counting an attempt
                // writes the whole batch again. The first failure is counted. A
                // probe that fails while the destination is already known to be
                // down is not: it says nothing new about this batch, and the
                // durable store is the one thing still protecting the data.
                let payload = if probing {
                    None
                } else {
                    crate::task::note_attempt(&mut delivery, now, delay * 1_000, &e.message);
                    Some(crate::task::encode(&delivery))
                };
                if let Err(park_error) = self.queue.park(task, delay, payload) {
                    self.logger.warning(
                        "A batch could not be delayed for another attempt.",
                        &[("batch_id", &batch_id), ("reason", &park_error.message)],
                    );
                    return Step::DestinationFailed;
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
                Step::DestinationFailed
            }
            Err(e) => {
                // Permanent. A retry storm against a rejection helps nobody.
                self.metrics.increment(
                    "tallyowl_delivery_attempts_total",
                    &labels(&[("outcome", "quarantined")]),
                );
                if self.quarantine(task, &e.message) {
                    self.state.note_finished(&task.uuid, accepted_at);
                }
                // The head answered. It is the batch that is wrong.
                Step::Finished
            }
        }
    }

    /// Set a batch aside for a person. Returns whether the durable store took
    /// the change.
    fn quarantine(&self, task: &ClaimedTask, reason: &str) -> bool {
        if let Err(e) = self.queue.quarantine(task, &self.quarantine_queue) {
            self.logger.error(
                "A batch could not be quarantined and will be tried again.",
                &[("reason", &e.message)],
            );
            return false;
        }
        self.state.quarantined.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .increment("tallyowl_batches_quarantined_total", &labels(&[]));
        self.logger.error(
            "A batch cannot be delivered and no retry will fix it. It is in quarantine for a person to look at.",
            &[("task", &task.uuid), ("reason", reason)],
        );
        true
    }
}

/// Run the forwarder until its state says stop.
///
/// Four threads, each with one job, so that one slow job cannot hide another:
///
/// - the sweep, on its interval;
/// - delivery, paced by a [`Breaker`];
/// - the depth count, on a slower interval, because it reads every waiting
///   task;
/// - the watchdog, which judges the other loops by their age. A loop that is
///   stuck cannot report that it is stuck, so the delivery loop used to be the
///   only judge of the sweep and nothing judged the delivery loop.
pub fn run(
    forwarder: Arc<Forwarder>,
    sweep_interval: Duration,
) -> Vec<std::thread::JoinHandle<()>> {
    run_with(forwarder, sweep_interval, Duration::from_secs(15))
}

/// [`run`], with the depth count interval stated.
pub fn run_with(
    forwarder: Arc<Forwarder>,
    sweep_interval: Duration,
    depth_interval: Duration,
) -> Vec<std::thread::JoinHandle<()>> {
    let sweeper = Arc::clone(&forwarder);
    let sweep_thread = std::thread::Builder::new()
        .name("tallyowl-sweep".into())
        .spawn(move || {
            while !sweeper.state.stopping.load(Ordering::Relaxed) {
                sweeper.sweep_only();
                sleep_unless_stopping(&sweeper.state, sweep_interval);
            }
        })
        .expect("the sweep thread starts");

    let deliverer = Arc::clone(&forwarder);
    let delivery_thread = std::thread::Builder::new()
        .name("tallyowl-delivery".into())
        .spawn(move || {
            let mut breaker = Breaker::new();
            while !deliverer.state.stopping.load(Ordering::Relaxed) {
                let pause = deliverer.delivery_tick(&mut breaker, now_ms());
                deliverer
                    .state
                    .last_delivery_loop_ms
                    .store(now_ms(), Ordering::Relaxed);
                if !pause.is_zero() {
                    std::thread::sleep(pause);
                }
            }
        })
        .expect("the delivery thread starts");

    let counter = Arc::clone(&forwarder);
    let depth_thread = std::thread::Builder::new()
        .name("tallyowl-depth".into())
        .spawn(move || {
            while !counter.state.stopping.load(Ordering::Relaxed) {
                counter.count_once();
                sleep_unless_stopping(&counter.state, depth_interval);
            }
        })
        .expect("the depth thread starts");

    let watcher = Arc::clone(&forwarder);
    let watchdog_thread = std::thread::Builder::new()
        .name("tallyowl-watchdog".into())
        .spawn(move || {
            while !watcher.state.stopping.load(Ordering::Relaxed) {
                watcher.check_sweep_age();
                watcher.check_delivery_age(now_ms());
                sleep_unless_stopping(&watcher.state, Duration::from_secs(1));
            }
        })
        .expect("the watchdog thread starts");

    vec![sweep_thread, delivery_thread, depth_thread, watchdog_thread]
}

/// Sleep for `length`, in short steps, so a stop is noticed within a moment
/// rather than after a whole interval.
fn sleep_unless_stopping(state: &ForwarderState, length: Duration) {
    let step = Duration::from_millis(100);
    let mut left = length;
    while !left.is_zero() && !state.stopping.load(Ordering::Relaxed) {
        let nap = left.min(step);
        std::thread::sleep(nap);
        left -= nap;
    }
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
