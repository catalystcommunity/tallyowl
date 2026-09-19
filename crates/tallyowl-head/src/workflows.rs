//! Scheduled work: alert evaluation, notification delivery, and the projector
//! passes.
//!
//! # Everything here is a Corndogs task, and that is not a stylistic choice
//!
//! `AGENTS.md`: "Corndogs owns durable queue and workflow state." A schedule
//! held in a process is a schedule a restart loses, and the exit criterion for
//! this phase is that a **retry survives a worker restart**. The only way to
//! meet it is for the work itself to be durable before the worker touches it.
//!
//! Two rules follow from the same place and both are easy to get wrong:
//!
//! - **Corndogs evaluates a task timeout only when a caller invokes
//!   `CleanUpTimedOut`.** Retry, backoff, and dead-worker recovery all stop
//!   when that sweep stops, so the sweep runs here and readiness is tied to it;
//! - **a delay is a task timeout and a state swap, never a polling loop and
//!   never a held claim.** A worker that slept holding a claim would be a
//!   worker whose death costs the whole delay again.
//!
//! # What quarantine means
//!
//! Work that will not be retried again goes to a queue it is not retried from,
//! and stays there for a person. It is never dropped. An operator reading the
//! workflow interface sees the count, and a count of zero is the ordinary
//! state rather than the absence of a signal.

use std::sync::{Arc, Mutex};

use tallyowl_obs::error::TallyOwlError;
use tallyowl_obs::log::Logger;
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_queue::{ClaimedTask, DurableQueue};
use tallyowl_store::cbor::{decode, encode, MapBuilder, Value};

/// The queues this installation uses.
pub const ALERT_QUEUE: &str = "tallyowl-alerts";
pub const NOTIFICATION_QUEUE: &str = "tallyowl-notifications";
pub const PROJECTOR_QUEUE: &str = "tallyowl-projector";
pub const QUARANTINE_QUEUE: &str = "tallyowl-workflow-quarantine";

/// How long a worker holds a claim before the sweep takes it back.
///
/// A worker that died mid-evaluation must not hold the work for ever, and an
/// evaluation that is still running must not be taken from it. This is longer
/// than the alert budget allows an evaluation to run.
pub const CLAIM_SECONDS: i64 = 120;

/// What kind of work one task is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    AlertEvaluation,
    Notification,
    ProjectorRebuild,
    Retention,
    Deletion,
    Export,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::AlertEvaluation => "alert-evaluation",
            Kind::Notification => "notification",
            Kind::ProjectorRebuild => "projector-rebuild",
            Kind::Retention => "retention",
            Kind::Deletion => "deletion",
            Kind::Export => "export",
        }
    }

    pub fn parse(name: &str) -> Option<Kind> {
        Some(match name {
            "alert-evaluation" => Kind::AlertEvaluation,
            "notification" => Kind::Notification,
            "projector-rebuild" => Kind::ProjectorRebuild,
            "retention" => Kind::Retention,
            "deletion" => Kind::Deletion,
            "export" => Kind::Export,
            _ => return None,
        })
    }

    /// Which queue this kind of work waits in.
    ///
    /// Alert evaluation and notification are separate queues on purpose: a
    /// receiver that is down must not stop alerts being evaluated, and an
    /// evaluation that is slow must not delay a notification that is ready.
    pub fn queue(&self) -> &'static str {
        match self {
            Kind::AlertEvaluation => ALERT_QUEUE,
            Kind::Notification => NOTIFICATION_QUEUE,
            _ => PROJECTOR_QUEUE,
        }
    }
}

/// One piece of work, as it travels in a task payload.
///
/// **The attempt count is TallyOwl's own.** Corndogs holds task state, atomic
/// claims, and timeout swaps, and it holds no attempt count and no
/// next-attempt time. A payload without one made every retryable failure use
/// the first entry of the backoff table for ever. L012 records that on the
/// delivery path and the same thing is true here.
#[derive(Debug, Clone, PartialEq)]
pub struct Work {
    pub kind: Kind,
    pub project_id: [u8; 16],
    /// The rule an evaluation or a notification belongs to.
    pub rule_id: String,
    /// Which notification target, for a notification. Empty otherwise.
    pub target: String,
    /// The state the notification is about. A notification for a state that
    /// has since changed is dropped rather than sent late.
    pub state: String,
    pub attempt: u64,
    /// The moment this work was first queued, which is what the lag is measured
    /// from.
    pub queued_at: i64,
    pub range_start: i64,
    pub range_end: i64,
    pub destination: String,
}

impl Work {
    pub fn new(kind: Kind, project_id: [u8; 16]) -> Work {
        Work {
            kind,
            project_id,
            rule_id: String::new(),
            target: String::new(),
            state: String::new(),
            attempt: 0,
            queued_at: tallyowl_obs::time::now_ms(),
            range_start: 0,
            range_end: 0,
            destination: String::new(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        encode(
            &MapBuilder::new()
                .put("kind", Value::text(self.kind.as_str()))
                .put("project", Value::Bytes(self.project_id.to_vec()))
                .put("rule", Value::text(&self.rule_id))
                .put("target", Value::text(&self.target))
                .put("state", Value::text(&self.state))
                .put("attempt", Value::Unsigned(self.attempt))
                .put("queued", Value::integer(self.queued_at))
                .put("from", Value::integer(self.range_start))
                .put("to", Value::integer(self.range_end))
                .put("destination", Value::text(&self.destination))
                .build(),
        )
    }

    pub fn decode(bytes: &[u8]) -> Result<Work, TallyOwlError> {
        let value = decode(bytes).map_err(|e| {
            TallyOwlError::internal(format!("A queued piece of work could not be read. {e}"))
        })?;
        let kind = value
            .field("kind")
            .and_then(Value::as_text)
            .and_then(Kind::parse)
            .ok_or_else(|| {
                TallyOwlError::invalid_argument(
                    "A queued piece of work names a kind this build does not know. It was queued by a newer version of TallyOwl.",
                )
            })?;
        let project_id = value
            .field("project")
            .and_then(Value::as_bytes)
            .and_then(|bytes| <[u8; 16]>::try_from(bytes).ok())
            .unwrap_or([0; 16]);
        let text = |name: &str| {
            value
                .field(name)
                .and_then(Value::as_text)
                .unwrap_or_default()
                .to_string()
        };
        let number = |name: &str| value.field(name).and_then(Value::as_integer).unwrap_or(0);
        Ok(Work {
            kind,
            project_id,
            rule_id: text("rule"),
            target: text("target"),
            state: text("state"),
            attempt: value
                .field("attempt")
                .and_then(Value::as_unsigned)
                .unwrap_or(0),
            queued_at: number("queued"),
            range_start: number("from"),
            range_end: number("to"),
            destination: text("destination"),
        })
    }
}

/// What one attempt at a piece of work produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Finished. The task is complete.
    Done,
    /// Failed, and trying again could work.
    Retry(String),
    /// Failed, and no retry will fix it. The task goes to quarantine for a
    /// person, and is never dropped.
    Quarantine(String),
}

/// Something that can run one piece of work.
pub trait Runner: Send + Sync {
    fn run(&self, work: &Work) -> Outcome;
}

/// What an operator reads about one workflow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub kind: String,
    pub queue: String,
    pub pending: u64,
    pub in_flight: u64,
    pub quarantined: u64,
    pub oldest_pending_age_ms: i64,
    pub failures: u64,
    pub last_success_at: i64,
    pub last_failure: String,
}

/// What each queue has done, as far as **this process** is concerned.
///
/// **The depths are not here.** Corndogs holds task state, so it is the one
/// that knows how many tasks are waiting, claimed, or in quarantine — and it
/// knows across a restart and across two heads, which a count kept here cannot.
/// [`Workflows::status`] asks it.
///
/// What is here is what Corndogs does not hold: how many attempts have failed
/// and what the last one said. That is the same division the attempt count
/// already follows, and `docs/DELIVERY.md` section 4 states it.
#[derive(Default)]
struct Counters {
    /// When the oldest piece of work this process submitted and has not seen
    /// finish was queued. It is the lag's lower bound; see `status`.
    oldest_pending_at: i64,
    failures: u64,
    last_success_at: i64,
    last_failure: String,
}

/// The durable workflow engine.
pub struct Workflows {
    queue: Arc<dyn DurableQueue>,
    metrics: Arc<Registry>,
    logger: Arc<Logger>,
    counters: Mutex<std::collections::BTreeMap<String, Counters>>,
    /// How long a piece of work may keep being retried before it is
    /// quarantined. It bounds a queue that would otherwise hold a failure for
    /// ever, and it is the same rule `corndogs.maxDeliveryAge` states for a
    /// batch.
    max_age_ms: i64,
}

impl Workflows {
    pub fn new(
        queue: Arc<dyn DurableQueue>,
        metrics: Arc<Registry>,
        logger: Arc<Logger>,
        max_age_ms: i64,
    ) -> Workflows {
        Workflows {
            queue,
            metrics,
            logger,
            counters: Mutex::new(std::collections::BTreeMap::new()),
            max_age_ms,
        }
    }

    /// Put one piece of work in its queue, durably.
    pub fn submit(&self, work: &Work) -> Result<String, TallyOwlError> {
        let id = self
            .queue
            .submit(work.kind.queue(), work.encode(), priority_of(work.kind))?;
        let mut counters = self.counters.lock().expect("workflow counters");
        let held = counters.entry(work.kind.queue().to_string()).or_default();
        if held.oldest_pending_at == 0 {
            held.oldest_pending_at = work.queued_at;
        }
        Ok(id)
    }

    /// Return every expired task to its ready state.
    ///
    /// **Nothing retries without this.** A head that stops calling it stops
    /// retry and dead-worker recovery at the same moment, and nothing else
    /// would say so.
    pub fn sweep(&self) -> Result<i64, TallyOwlError> {
        let at = tallyowl_obs::time::now_nanos();
        let mut moved = 0;
        for queue in [ALERT_QUEUE, NOTIFICATION_QUEUE, PROJECTOR_QUEUE] {
            moved += self.queue.sweep(queue, at)?;
        }
        Ok(moved)
    }

    /// Claim one piece of work from a queue and run it.
    ///
    /// Returns whether anything was there, so a worker loop can wait when the
    /// queue is empty rather than asking as fast as it can.
    pub fn run_one(&self, queue: &str, runner: &dyn Runner) -> Result<bool, TallyOwlError> {
        let Some(task) = self.queue.claim(queue, CLAIM_SECONDS)? else {
            return Ok(false);
        };
        self.mark_claimed(queue);

        let work = match Work::decode(&task.payload) {
            Ok(work) => work,
            Err(failure) => {
                // A payload this build cannot read is not a payload a retry
                // will fix. It goes to a person rather than around the loop.
                self.quarantine(&task, queue, &failure.message)?;
                return Ok(true);
            }
        };

        match runner.run(&work) {
            Outcome::Done => {
                self.queue.complete(&task)?;
                self.mark_done(queue);
            }
            Outcome::Quarantine(reason) => {
                self.quarantine(&task, queue, &reason)?;
            }
            Outcome::Retry(reason) => {
                let age = tallyowl_obs::time::now_ms() - work.queued_at;
                if age > self.max_age_ms {
                    // **Retry stops before it becomes pointless.** A piece of
                    // work that has been failing for longer than the bound is
                    // not going to start working, and the queue behind it is
                    // real work that is not being done.
                    self.quarantine(
                        &task,
                        queue,
                        &format!(
                            "This has been retried for {} minutes and still fails. Automatic retry stops here. The last failure was: {reason}",
                            age / 60_000
                        ),
                    )?;
                } else {
                    let mut again = work.clone();
                    again.attempt += 1;
                    let delay = crate::notify::backoff_ms(again.attempt, again.queued_at as u64);
                    self.queue
                        .park(&task, (delay / 1000).max(1), Some(again.encode()))?;
                    self.mark_failure(queue, &reason);
                }
            }
        }
        Ok(true)
    }

    fn quarantine(
        &self,
        task: &ClaimedTask,
        queue: &str,
        reason: &str,
    ) -> Result<(), TallyOwlError> {
        self.queue.quarantine(task, QUARANTINE_QUEUE)?;
        let mut counters = self.counters.lock().expect("workflow counters");
        let held = counters.entry(queue.to_string()).or_default();
        held.failures += 1;
        held.last_failure = reason.to_string();
        drop(counters);
        // A quarantined item is a person's problem now, so it is said once and
        // plainly rather than counted and left.
        self.logger.error(
            "A piece of workflow work cannot be finished and no retry will fix it. It is in quarantine for a person to look at.",
            &[("queue", queue), ("reason", reason)],
        );
        Ok(())
    }

    fn mark_claimed(&self, _queue: &str) {}

    fn mark_done(&self, queue: &str) {
        let mut counters = self.counters.lock().expect("workflow counters");
        let held = counters.entry(queue.to_string()).or_default();
        held.last_success_at = tallyowl_obs::time::now_ms();
        // The oldest thing this process was waiting on has gone. The next
        // submission starts the clock again; the depth itself comes from the
        // queue.
        held.oldest_pending_at = 0;
    }

    fn mark_failure(&self, queue: &str, reason: &str) {
        let mut counters = self.counters.lock().expect("workflow counters");
        let held = counters.entry(queue.to_string()).or_default();
        held.failures += 1;
        held.last_failure = reason.to_string();
    }

    /// What every workflow is doing, for the operator interface.
    ///
    /// **The depths come from Corndogs.** It holds the task state, so it is the
    /// only thing that knows what is waiting after this head restarted, and the
    /// only thing that sees both heads when there are two. A count kept in this
    /// process reads as zero after a restart, which looks like "there is no
    /// work" rather than like "this number is not the truth".
    ///
    /// A queue that cannot be reached is reported as unknown rather than as
    /// empty, for the same reason.
    pub fn status(&self) -> Vec<Status> {
        let depths = self.queue.counts().unwrap_or_default();
        let counters = self.counters.lock().expect("workflow counters");
        let now = tallyowl_obs::time::now_ms();
        let mut out = Vec::new();
        for (kind, queue) in [
            (Kind::AlertEvaluation, ALERT_QUEUE),
            (Kind::Notification, NOTIFICATION_QUEUE),
            (Kind::ProjectorRebuild, PROJECTOR_QUEUE),
        ] {
            let held = counters.get(queue);
            let depth = depths.iter().find(|counts| counts.queue == queue);
            let oldest = held.map(|h| h.oldest_pending_at).unwrap_or(0);
            let pending = depth
                .map(|counts| {
                    // Waiting means queued, and work parked in a backoff is
                    // waiting as well: it is going to run and it has not.
                    counts.in_state(tallyowl_queue::STATE_QUEUED)
                        + counts.in_state(tallyowl_queue::STATE_BACKOFF)
                })
                .unwrap_or(0);
            out.push(Status {
                kind: kind.as_str().to_string(),
                queue: queue.to_string(),
                pending: pending.max(0) as u64,
                in_flight: depth
                    .map(|counts| counts.in_state(tallyowl_queue::STATE_SENDING))
                    .unwrap_or(0)
                    .max(0) as u64,
                quarantined: depths
                    .iter()
                    .find(|counts| counts.queue == QUARANTINE_QUEUE)
                    .map(|counts| counts.total)
                    .unwrap_or(0)
                    .max(0) as u64,
                // **The lag is a lower bound and says so by being zero when
                // nothing is waiting.** Corndogs reports how many tasks are in
                // a state and not when each one arrived, so the age comes from
                // the oldest thing this process is still waiting on. A restart
                // loses it; the depth beside it does not.
                oldest_pending_age_ms: match (pending > 0, oldest) {
                    (true, at) if at > 0 => (now - at).max(0),
                    _ => 0,
                },
                failures: held.map(|h| h.failures).unwrap_or(0),
                last_success_at: held.map(|h| h.last_success_at).unwrap_or(0),
                last_failure: held.map(|h| h.last_failure.clone()).unwrap_or_default(),
            });
        }
        out
    }

    /// Publish the workflow gauges an operator alerts on.
    pub fn sample(&self) {
        for status in self.status() {
            let with = labels(&[("workflow", status.kind.as_str())]);
            self.metrics.set_gauge(
                "tallyowl_workflow_pending_count",
                &with,
                status.pending as i64,
            );
            self.metrics.set_gauge(
                "tallyowl_workflow_quarantined_count",
                &with,
                status.quarantined as i64,
            );
            self.metrics.set_gauge(
                "tallyowl_workflow_lag_ms",
                &with,
                status.oldest_pending_age_ms,
            );
        }
    }
}

/// A notification is more urgent than a rebuild, and an evaluation sits between
/// them. Corndogs orders by priority within a queue, and these are separate
/// queues, so this only matters when one queue holds more than one kind.
fn priority_of(kind: Kind) -> i64 {
    match kind {
        Kind::Notification => 100,
        Kind::AlertEvaluation => 50,
        Kind::Deletion => 40,
        Kind::Retention => 20,
        Kind::ProjectorRebuild | Kind::Export => 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tallyowl_queue::testing::FakeQueue;

    fn workflows(queue: Arc<FakeQueue>, max_age_ms: i64) -> Workflows {
        Workflows::new(
            queue,
            Registry::new(),
            Arc::new(Logger::new(
                "test",
                "0.0.0",
                tallyowl_obs::log::Severity::Error,
            )),
            max_age_ms,
        )
    }

    struct Always(Outcome);
    impl Runner for Always {
        fn run(&self, _work: &Work) -> Outcome {
            self.0.clone()
        }
    }

    /// A runner that fails a fixed number of times and then works, and counts
    /// how many times it was asked.
    struct FailsThenWorks {
        failures: std::sync::atomic::AtomicU64,
        until: u64,
        attempts: std::sync::Mutex<Vec<u64>>,
    }

    impl Runner for FailsThenWorks {
        fn run(&self, work: &Work) -> Outcome {
            self.attempts.lock().unwrap().push(work.attempt);
            let so_far = self
                .failures
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match so_far < self.until {
                true => Outcome::Retry("the receiver did not answer".into()),
                false => Outcome::Done,
            }
        }
    }

    #[test]
    fn a_piece_of_work_survives_the_trip_through_a_payload() {
        let mut work = Work::new(Kind::Notification, [3; 16]);
        work.rule_id = "checkout-errors".into();
        work.target = "webhook http://example.test/alerts".into();
        work.state = "firing".into();
        work.attempt = 4;
        let read = Work::decode(&work.encode()).expect("it reads back");
        assert_eq!(read, work);
    }

    #[test]
    fn work_from_a_newer_version_is_quarantined_rather_than_guessed_at() {
        // A payload naming a kind this build does not know was written by a
        // newer TallyOwl. Guessing what it meant is how a deletion becomes an
        // export.
        let bytes = encode(
            &MapBuilder::new()
                .put("kind", Value::text("time-travel"))
                .build(),
        );
        let failure = Work::decode(&bytes).expect_err("refused");
        assert!(failure.message.contains("newer version"));
    }

    #[test]
    fn a_retry_survives_a_worker_that_died_holding_the_claim() {
        // **A Phase 10 exit criterion.** The work is durable before a worker
        // touches it, so a worker that dies mid-attempt loses nothing: the
        // sweep returns the task and another worker takes it.
        let queue = Arc::new(FakeQueue::default());
        let engine = workflows(Arc::clone(&queue), 3_600_000);
        engine
            .submit(&Work::new(Kind::Notification, [1; 16]))
            .expect("it queues");

        // A worker claims it and dies. Nothing completes and nothing parks.
        let claimed = queue
            .claim(NOTIFICATION_QUEUE, CLAIM_SECONDS)
            .expect("a claim")
            .expect("something was waiting");
        assert_eq!(
            queue
                .claim(NOTIFICATION_QUEUE, CLAIM_SECONDS)
                .expect("a claim"),
            None,
            "a claimed task was handed out twice"
        );

        // The sweep is what recovers it. Without the sweep, nothing does.
        queue.expire_claims();
        let again = queue
            .claim(NOTIFICATION_QUEUE, CLAIM_SECONDS)
            .expect("a claim")
            .expect("the sweep returned it");
        assert_eq!(again.payload, claimed.payload, "a different piece of work");
    }

    #[test]
    fn a_retryable_failure_is_parked_with_a_rising_attempt_count() {
        // L012: without the count in the payload, every retryable failure used
        // the first entry of the backoff table for ever.
        let queue = Arc::new(FakeQueue::default());
        let engine = workflows(Arc::clone(&queue), 3_600_000);
        engine
            .submit(&Work::new(Kind::Notification, [1; 16]))
            .expect("it queues");

        let runner = FailsThenWorks {
            failures: std::sync::atomic::AtomicU64::new(0),
            until: 3,
            attempts: std::sync::Mutex::new(Vec::new()),
        };
        for _ in 0..4 {
            engine
                .run_one(NOTIFICATION_QUEUE, &runner)
                .expect("it runs");
            queue.release_parked();
        }
        assert_eq!(
            *runner.attempts.lock().unwrap(),
            vec![0, 1, 2, 3],
            "the attempt count did not rise across retries"
        );
    }

    #[test]
    fn work_that_has_failed_for_too_long_goes_to_quarantine_and_is_never_dropped() {
        let queue = Arc::new(FakeQueue::default());
        let engine = workflows(Arc::clone(&queue), 0);
        let mut work = Work::new(Kind::Notification, [1; 16]);
        // Queued an hour ago, and the bound is zero.
        work.queued_at = tallyowl_obs::time::now_ms() - 3_600_000;
        engine.submit(&work).expect("it queues");

        engine
            .run_one(
                NOTIFICATION_QUEUE,
                &Always(Outcome::Retry("nothing answers".into())),
            )
            .expect("it runs");

        assert_eq!(
            queue.quarantined_count(),
            1,
            "it was dropped rather than kept"
        );
        let status = engine
            .status()
            .into_iter()
            .find(|status| status.queue == NOTIFICATION_QUEUE)
            .expect("the queue is reported");
        assert_eq!(status.quarantined, 1);
        assert!(
            status.last_failure.contains("nothing answers"),
            "the operator interface does not say why: {}",
            status.last_failure
        );
    }

    #[test]
    fn an_empty_queue_says_so_rather_than_looking_like_work() {
        let queue = Arc::new(FakeQueue::default());
        let engine = workflows(queue, 3_600_000);
        assert!(!engine
            .run_one(ALERT_QUEUE, &Always(Outcome::Done))
            .expect("it asks"));
        let status = engine.status();
        assert!(status.iter().all(|s| s.pending == 0));
        assert!(
            status.iter().all(|s| s.oldest_pending_age_ms == 0),
            "an empty queue reported the age of the epoch as its lag"
        );
    }

    #[test]
    fn what_is_waiting_survives_a_head_that_restarted() {
        // **The queue is the one that knows.** A count kept in this process
        // reads as zero after a restart, and zero looks like "there is no work"
        // rather than like "this number is not the truth". An operator reading
        // it during a rolling upgrade would see every queue go empty and every
        // one come back.
        let queue = Arc::new(FakeQueue::default());
        let before = workflows(Arc::clone(&queue), 3_600_000);
        for _ in 0..3 {
            before
                .submit(&Work::new(Kind::AlertEvaluation, [1; 16]))
                .expect("it queues");
        }
        assert_eq!(waiting(&before, ALERT_QUEUE), 3);

        // A new engine over the same durable queue, which is what a restart is.
        let after = workflows(Arc::clone(&queue), 3_600_000);
        assert_eq!(
            waiting(&after, ALERT_QUEUE),
            3,
            "a restart reported an empty queue that was not empty"
        );
    }

    #[test]
    fn work_parked_in_a_backoff_is_still_waiting() {
        // It is going to run and it has not. Counting only the queued state
        // would show an empty queue while every piece of work in it was in a
        // backoff, which is exactly when an operator is looking.
        let queue = Arc::new(FakeQueue::default());
        let engine = workflows(Arc::clone(&queue), 3_600_000);
        engine
            .submit(&Work::new(Kind::Notification, [1; 16]))
            .expect("it queues");
        engine
            .run_one(
                NOTIFICATION_QUEUE,
                &Always(Outcome::Retry("nothing answers".into())),
            )
            .expect("it runs");
        assert_eq!(
            waiting(&engine, NOTIFICATION_QUEUE),
            1,
            "work in a backoff was reported as nothing waiting"
        );
    }

    fn waiting(engine: &Workflows, queue: &str) -> u64 {
        engine
            .status()
            .into_iter()
            .find(|status| status.queue == queue)
            .map(|status| status.pending)
            .unwrap_or(0)
    }

    #[test]
    fn the_lag_is_how_long_the_oldest_waiting_item_has_waited() {
        let queue = Arc::new(FakeQueue::default());
        let engine = workflows(queue, 3_600_000);
        let mut work = Work::new(Kind::AlertEvaluation, [1; 16]);
        work.queued_at = tallyowl_obs::time::now_ms() - 90_000;
        engine.submit(&work).expect("it queues");

        let status = engine
            .status()
            .into_iter()
            .find(|status| status.queue == ALERT_QUEUE)
            .expect("the queue is reported");
        assert!(
            status.oldest_pending_age_ms >= 90_000,
            "the lag was {} and the work has waited ninety seconds",
            status.oldest_pending_age_ms
        );
    }
}
