//! What keeps the head's background work running, visible, and stoppable.
//!
//! Three defects had one shape. A background thread stopped, or never started,
//! and `/readyz` went on answering ready:
//!
//! - **a panic ended a loop for good.** One alert rule whose filter divided the
//!   smallest integer by minus one stopped the only alert evaluation worker,
//!   and the same stored rule stopped the next one after every restart;
//! - **the durable queue was tried once.** On a first Kubernetes install the
//!   head starts before Corndogs listens. The head logged one warning, and
//!   alert evaluation, notifications, and the projector passes did not run
//!   until somebody restarted it;
//! - **nothing answered a stop signal.** The process parked in a sleep, so a
//!   rolling restart ended every request in progress.
//!
//! [`LoopWatch`] runs each pass of a loop so that a panic is counted and the
//! loop continues, and it reports a loop that has stopped making passes.
//! [`connect_with_backoff`] keeps trying a dependency. [`Stop`] and
//! [`shut_down`] are the stop signal and what it does.
//!
//! Every clock and every wait here is an argument, so a test states the time
//! and never sleeps.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tallyowl_obs::error::TallyOwlError;
use tallyowl_obs::health::{Cause, CheckState, Health};
use tallyowl_obs::log::Logger;
use tallyowl_obs::metrics::{labels, Registry};

const TICK_AGE: &str = "tallyowl_background_loop_tick_age_ms";
const PANICS: &str = "tallyowl_background_loop_panics_total";

/// A loop is reported when it has made no pass for this many of its periods.
const STALE_PERIODS: i64 = 3;
/// And never sooner than this, so a loop with a short period is not reported
/// for one slow pass.
const STALE_FLOOR_MS: i64 = 30_000;

/// The text a panic carried, for a log line and a quarantine reason.
pub fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic that carried no text".to_string())
}

/// Tells every background loop to stop, and wakes the ones that are waiting.
#[derive(Default)]
pub struct Stop {
    stopping: AtomicBool,
    lock: Mutex<()>,
    wake: Condvar,
}

impl Stop {
    pub fn new() -> Arc<Stop> {
        Arc::new(Stop::default())
    }

    pub fn is_set(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    pub fn set(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _held = self.lock.lock().expect("stop lock");
        self.wake.notify_all();
    }

    /// Wait one period, or until the stop. Returns true when the loop must end.
    ///
    /// A loop waits here rather than in `thread::sleep`, so a stop does not
    /// wait out a five-minute maintenance period.
    pub fn wait(&self, period: Duration) -> bool {
        let held = self.lock.lock().expect("stop lock");
        if self.is_set() {
            return true;
        }
        let _ = self
            .wake
            .wait_timeout_while(held, period, |_| !self.is_set())
            .expect("stop lock");
        self.is_set()
    }
}

struct LoopState {
    period_ms: i64,
    last_pass_ms: i64,
}

/// Runs the passes of every background loop and knows when each last ran.
pub struct LoopWatch {
    loops: Mutex<BTreeMap<&'static str, LoopState>>,
    metrics: Arc<Registry>,
    logger: Arc<Logger>,
}

impl LoopWatch {
    pub fn new(metrics: Arc<Registry>, logger: Arc<Logger>) -> Arc<LoopWatch> {
        for (name, kind, help) in [
            (
                TICK_AGE,
                tallyowl_obs::MetricKind::Gauge,
                "How long ago each background loop finished a pass. A value that keeps rising past the loop's period means the loop stopped, or one pass is taking that long.",
            ),
            (
                PANICS,
                tallyowl_obs::MetricKind::Counter,
                "Passes of a background loop that ended in a panic. The loop continued. Each one is a defect in TallyOwl.",
            ),
        ] {
            metrics
                .declare(name, kind, help, &[])
                .unwrap_or_else(|e| {
                    panic!("the metric `{name}` is not a name the registry accepts: {}", e.0)
                });
        }
        Arc::new(LoopWatch {
            loops: Mutex::new(BTreeMap::new()),
            metrics,
            logger,
        })
    }

    /// Name a loop and its period. The loop counts as having run at `now_ms`,
    /// so a loop that waits one period before its first pass is not reported
    /// for that wait.
    pub fn register(&self, name: &'static str, period: Duration, now_ms: i64) {
        self.loops.lock().expect("loop watch").insert(
            name,
            LoopState {
                period_ms: period.as_millis() as i64,
                last_pass_ms: now_ms,
            },
        );
    }

    /// Run one pass. A panic inside it is logged and counted, and the loop goes
    /// on to its next pass. Returns false when the pass panicked.
    ///
    /// `AssertUnwindSafe` is sound here because a pass owns nothing the next
    /// pass reads except shared services, and each of those guards its own
    /// state with a lock that reports poisoning.
    pub fn pass(&self, name: &'static str, now_ms: impl Fn() -> i64, body: impl FnOnce()) -> bool {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        if let Some(state) = self.loops.lock().expect("loop watch").get_mut(name) {
            state.last_pass_ms = now_ms();
        }
        match outcome {
            Ok(()) => true,
            Err(panic) => {
                self.metrics.increment(PANICS, &labels(&[("loop", name)]));
                self.logger.error(
                    "A background pass stopped early because of a defect in TallyOwl. The loop continues with its next pass.",
                    &[("loop", name), ("reason", &panic_text(panic.as_ref()))],
                );
                false
            }
        }
    }

    /// Publish each loop's age, and report a loop that stopped making passes.
    ///
    /// A stopped loop is `degraded` and not `failed`. None of these loops is on
    /// the ingest path, and a head that left the rotation because compaction
    /// was slow would stop taking telemetry over work that can wait.
    pub fn evaluate(&self, health: &Health, now_ms: i64) {
        let loops = self.loops.lock().expect("loop watch");
        for (name, state) in loops.iter() {
            let age = (now_ms - state.last_pass_ms).max(0);
            self.metrics
                .set_gauge(TICK_AGE, &labels(&[("loop", name)]), age);
            let allowed = (state.period_ms * STALE_PERIODS).max(STALE_FLOOR_MS);
            let check = format!("loop-{name}");
            if age > allowed {
                health.set(
                    &check,
                    CheckState::Degraded {
                        cause: Cause::Established(format!(
                            "The background loop `{name}` last finished a pass {} seconds ago, and it runs every {} seconds. It stopped, or one pass is taking this long. Read the log for `{name}`, and restart the head if the loop does not continue.",
                            age / 1_000,
                            (state.period_ms / 1_000).max(1)
                        )),
                    },
                );
            } else {
                health.pass(&check);
            }
        }
    }
}

/// Keep trying to reach a dependency until it answers or the stop is set.
///
/// The wait grows to `cap` and carries jitter, so every head that started
/// beside a dependency that is still starting does not ask it again in the same
/// instant. `wait` returns true when the caller must give up, which is how a
/// test runs this with no sleep and how a stop ends it.
pub fn connect_with_backoff<Q>(
    mut connect: impl FnMut() -> Result<Q, TallyOwlError>,
    mut wait: impl FnMut(Duration) -> bool,
    mut failed: impl FnMut(u64, &TallyOwlError, Duration),
    cap: Duration,
    seed: u64,
) -> Option<Q> {
    let mut attempt = 0u64;
    loop {
        match connect() {
            Ok(connected) => return Some(connected),
            Err(failure) => {
                let delay = Duration::from_millis(crate::notify::backoff_ms(
                    attempt,
                    seed.wrapping_add(attempt),
                ) as u64)
                .min(cap);
                failed(attempt, &failure, delay);
                attempt += 1;
                if wait(delay) {
                    return None;
                }
            }
        }
    }
}

/// A durable queue that is not reachable yet, and the one it becomes.
///
/// The alert service, the workflows, and the control operations that read them
/// are built once, at the start. They hold this, so they work from the moment
/// the queue answers and need no restart. Until then every call is refused as
/// `unavailable`, which a caller already treats as "try again".
#[derive(Default)]
pub struct LateQueue {
    held: std::sync::RwLock<Option<Arc<dyn tallyowl_queue::DurableQueue>>>,
}

impl LateQueue {
    pub fn new() -> Arc<LateQueue> {
        Arc::new(LateQueue::default())
    }

    pub fn connect(&self, queue: Arc<dyn tallyowl_queue::DurableQueue>) {
        *self.held.write().expect("late queue") = Some(queue);
    }

    pub fn is_connected(&self) -> bool {
        self.held.read().expect("late queue").is_some()
    }

    fn queue(&self) -> Result<Arc<dyn tallyowl_queue::DurableQueue>, TallyOwlError> {
        self.held.read().expect("late queue").clone().ok_or_else(|| {
            TallyOwlError::unavailable(
                "The durable queue has not answered since this head started, so nothing scheduled can run yet. The head keeps trying. Check that Corndogs is running at `corndogs.endpoint`.",
            )
        })
    }
}

impl tallyowl_queue::DurableQueue for LateQueue {
    fn submit(
        &self,
        queue: &str,
        payload: Vec<u8>,
        priority: i64,
    ) -> Result<String, TallyOwlError> {
        self.queue()?.submit(queue, payload, priority)
    }

    fn claim(
        &self,
        queue: &str,
        timeout_seconds: i64,
    ) -> Result<Option<tallyowl_queue::ClaimedTask>, TallyOwlError> {
        self.queue()?.claim(queue, timeout_seconds)
    }

    fn complete(&self, task: &tallyowl_queue::ClaimedTask) -> Result<(), TallyOwlError> {
        self.queue()?.complete(task)
    }

    fn park(
        &self,
        task: &tallyowl_queue::ClaimedTask,
        delay_seconds: i64,
        payload: Option<Vec<u8>>,
    ) -> Result<(), TallyOwlError> {
        self.queue()?.park(task, delay_seconds, payload)
    }

    fn quarantine(
        &self,
        task: &tallyowl_queue::ClaimedTask,
        queue: &str,
    ) -> Result<(), TallyOwlError> {
        self.queue()?.quarantine(task, queue)
    }

    fn sweep(&self, queue: &str, at_nanos: i64) -> Result<i64, TallyOwlError> {
        self.queue()?.sweep(queue, at_nanos)
    }

    fn counts(&self) -> Result<Vec<tallyowl_queue::QueueCounts>, TallyOwlError> {
        self.queue()?.counts()
    }
}

/// What a stop has to reach.
pub struct ShutdownParts<'a> {
    pub health: &'a Health,
    pub stop: &'a Stop,
    pub logger: &'a Logger,
    /// Stop each listener from accepting. One closure for each listener.
    pub stop_listeners: Vec<Box<dyn FnOnce() + 'a>>,
    /// Wait for the requests already accepted, and say whether they finished.
    pub wait_until_quiet: Box<dyn FnOnce(Duration) -> bool + 'a>,
    /// Put what is in memory into its compact durable form. Every accepted row
    /// is already durable in the append log, so a failure here loses nothing.
    pub seal: Box<dyn FnOnce() -> Result<(), String> + 'a>,
    /// How long requests in progress may take to finish.
    pub grace: Duration,
}

/// The name of the check a stop fails.
pub const STOPPING_CHECK: &str = "accepting-work";

/// Stop the head in the order that loses nothing.
///
/// 1. Readiness fails first, so a load balancer stops sending work while the
///    listeners still answer what it already sent.
/// 2. The listeners stop accepting, and the requests in progress finish.
/// 3. The background loops stop at the end of their current pass.
/// 4. The open buffer seals. Its rows are already durable, so this is for the
///    next start, which then replays less.
///
/// The signal handler calls this, and a test calls it with no signal.
pub fn shut_down(parts: ShutdownParts<'_>) {
    parts.health.fail(
        STOPPING_CHECK,
        "This head was told to stop. It is finishing the requests it already accepted.",
    );
    parts.logger.info(
        "Stopping. Readiness now fails, and the requests already accepted will finish.",
        &[],
    );
    for stop_listener in parts.stop_listeners {
        stop_listener();
    }
    if !(parts.wait_until_quiet)(parts.grace) {
        parts.logger.warning(
            "Some requests were still in progress when the stop ran out of time. A collector delivers an unanswered batch again, and the head deduplicates it.",
            &[("grace_ms", &parts.grace.as_millis().to_string())],
        );
    }
    parts.stop.set();
    if let Err(reason) = (parts.seal)() {
        parts.logger.warning(
            "The open buffer did not seal before the stop. Its rows are durable in the append log, and the next start reads them from there.",
            &[("reason", &reason)],
        );
    }
    parts.logger.info("Stopped.", &[]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn watch() -> (Arc<LoopWatch>, Arc<Registry>) {
        let metrics = Registry::new();
        let logger = Arc::new(Logger::new(
            "test",
            "0.0.0",
            tallyowl_obs::log::Severity::Error,
        ));
        (LoopWatch::new(Arc::clone(&metrics), logger), metrics)
    }

    fn check(health: &Health, name: &str) -> CheckState {
        health
            .report()
            .checks
            .into_iter()
            .find(|check| check.name == name)
            .map(|check| check.state)
            .expect("the check exists")
    }

    #[test]
    fn a_pass_that_panics_is_counted_and_the_next_pass_runs() {
        let (watch, metrics) = watch();
        watch.register("alert-schedule", Duration::from_secs(1), 0);
        let ran = AtomicU64::new(0);

        assert!(!watch.pass("alert-schedule", || 1_000, || panic!("a defect")));
        assert!(watch.pass(
            "alert-schedule",
            || 2_000,
            || {
                ran.fetch_add(1, Ordering::SeqCst);
            }
        ));

        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "the loop ended with the panic"
        );
        let text = metrics.render_text();
        assert!(
            text.contains("tallyowl_background_loop_panics_total{loop=\"alert-schedule\"} 1"),
            "{text}"
        );
    }

    #[test]
    fn a_loop_that_stops_making_passes_is_reported_and_one_that_continues_recovers() {
        let (watch, _metrics) = watch();
        let health = Health::new();
        watch.register("maintenance", Duration::from_secs(300), 0);

        // Inside three periods. A loop that waits before its first pass is here.
        watch.evaluate(&health, 600_000);
        assert_eq!(check(&health, "loop-maintenance"), CheckState::Ok);

        watch.evaluate(&health, 901_000);
        match check(&health, "loop-maintenance") {
            CheckState::Degraded { cause } => {
                assert!(
                    cause.summary().contains("maintenance"),
                    "{}",
                    cause.summary()
                );
            }
            other => panic!("expected degraded, got {other:?}"),
        }
        assert!(
            health.is_ready(),
            "a slow pass took the head out of rotation"
        );

        watch.pass("maintenance", || 905_000, || {});
        watch.evaluate(&health, 906_000);
        assert_eq!(check(&health, "loop-maintenance"), CheckState::Ok);
    }

    #[test]
    fn a_short_period_is_not_reported_for_one_slow_pass() {
        let (watch, _metrics) = watch();
        let health = Health::new();
        watch.register("work-alerts", Duration::from_millis(250), 0);
        watch.evaluate(&health, 29_000);
        assert_eq!(check(&health, "loop-work-alerts"), CheckState::Ok);
        watch.evaluate(&health, 31_000);
        assert!(matches!(
            check(&health, "loop-work-alerts"),
            CheckState::Degraded { .. }
        ));
    }

    #[test]
    fn a_dependency_that_is_not_listening_yet_is_tried_again_until_it_answers() {
        // Corndogs starts after the head on a first install. The waits are
        // recorded and not slept.
        let mut attempts = 0;
        let mut waits: Vec<Duration> = Vec::new();
        let connected = connect_with_backoff(
            || {
                attempts += 1;
                match attempts {
                    1..=5 => Err(TallyOwlError::unavailable("not listening yet")),
                    _ => Ok("queue"),
                }
            },
            |delay| {
                waits.push(delay);
                false
            },
            |_, _, _| {},
            Duration::from_secs(30),
            7,
        );
        assert_eq!(connected, Some("queue"));
        assert_eq!(waits.len(), 5);
        assert!(waits.iter().all(|wait| *wait <= Duration::from_secs(30)));
        assert!(waits[4] > waits[0], "the wait did not grow: {waits:?}");
    }

    #[test]
    fn a_stop_ends_the_attempts_and_the_wait_never_passes_its_cap() {
        let mut waits: Vec<Duration> = Vec::new();
        let connected: Option<()> = connect_with_backoff(
            || Err(TallyOwlError::unavailable("gone")),
            |delay| {
                waits.push(delay);
                waits.len() == 40
            },
            |_, _, _| {},
            Duration::from_secs(30),
            11,
        );
        assert_eq!(connected, None);
        assert_eq!(waits.len(), 40);
        assert!(waits.iter().all(|wait| *wait <= Duration::from_secs(30)));
    }

    #[test]
    fn a_stop_wakes_a_waiting_loop_at_once() {
        let stop = Stop::new();
        stop.set();
        // An hour, which the test would not survive if the stop did not wake it.
        assert!(stop.wait(Duration::from_secs(3_600)));
    }

    #[test]
    fn a_stop_fails_readiness_first_and_seals_last() {
        let health = Health::new();
        health.pass("storage");
        let stop = Stop::new();
        let logger = Logger::new("test", "0.0.0", tallyowl_obs::log::Severity::Error);
        let order: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let note = |what: &str| order.lock().unwrap().push(what.to_string());

        shut_down(ShutdownParts {
            health: &health,
            stop: &stop,
            logger: &logger,
            stop_listeners: vec![
                Box::new(|| note(&format!("ingest ready={}", health.is_ready()))),
                Box::new(|| note("dashboard")),
            ],
            wait_until_quiet: Box::new(|grace| {
                note(&format!("quiet {} stop={}", grace.as_secs(), stop.is_set()));
                true
            }),
            seal: Box::new(|| {
                note(&format!("seal stop={}", stop.is_set()));
                Err("the device is full".to_string())
            }),
            grace: Duration::from_secs(20),
        });

        assert_eq!(
            *order.lock().unwrap(),
            vec![
                "ingest ready=false",
                "dashboard",
                "quiet 20 stop=false",
                "seal stop=true",
            ]
        );
        assert!(!health.is_ready());
        assert!(stop.is_set());
    }

    #[test]
    fn a_queue_that_has_not_answered_refuses_as_unavailable_and_then_works() {
        use tallyowl_queue::DurableQueue;
        let late = LateQueue::new();
        let refused = late
            .submit("q", vec![1], 0)
            .expect_err("nothing is connected");
        assert_eq!(refused.code, tallyowl_obs::ErrorCode::Unavailable);
        assert!(refused.retryable, "a caller must try this again");
        assert!(!late.is_connected());

        late.connect(Arc::new(tallyowl_queue::testing::FakeQueue::default()));
        assert!(late.is_connected());
        late.submit("q", vec![1], 0).expect("the queue answers now");
        assert!(late.claim("q", 30).unwrap().is_some());
    }
}
