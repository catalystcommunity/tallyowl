//! What each workflow actually does when a worker claims it.
//!
//! [`crate::workflows`] owns the durability, the retry, and the quarantine.
//! This owns the work. Keeping the two apart is what lets the retry behaviour
//! be tested against a queue with no store behind it, and each pass be tested
//! against a store with no queue.
//!
//! # The four projector passes
//!
//! `docs/PLAN.md` Phase 10 asks for a projector rebuild, retention, deletion,
//! and export. Each one is a pass over retained raw data, and each one is
//! **reproducible from it**, which `AGENTS.md` requires of every derived
//! projection.

use std::sync::Arc;
use std::time::Duration;

use tallyowl_obs::error::TallyOwlError;
use tallyowl_obs::log::Logger;
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_store::SegmentedStore;

use crate::alerts::AlertService;
use crate::notify::{Attempt, Channel, CsilCallback, Notification, Webhook};
use crate::workflows::{Kind, Outcome, Runner, Work, Workflows};

/// Runs one alert rule and queues whatever it decided to say.
///
/// **The evaluation and the notification are two pieces of work on purpose.**
/// A receiver that is not answering must not stop the next evaluation, and an
/// evaluation that is slow must not delay a notification that is ready. Two
/// queues, and the state is safe before the notification is queued at all.
pub struct AlertRunner {
    pub alerts: Arc<AlertService>,
    pub workflows: Arc<Workflows>,
    pub logger: Arc<Logger>,
    pub metrics: Arc<Registry>,
}

impl Runner for AlertRunner {
    fn run(&self, work: &Work) -> Outcome {
        let rules = match self.alerts.rules(work.project_id) {
            Ok(rules) => rules,
            Err(failure) => return Outcome::Retry(failure.message),
        };
        let Some(rule) = rules.into_iter().find(|rule| rule.rule_id == work.rule_id) else {
            // The rule was removed while this evaluation was waiting. There is
            // nothing to do and nothing has gone wrong.
            return Outcome::Done;
        };
        if !rule.enabled {
            return Outcome::Done;
        }

        // The delay against the schedule. `docs/ALERTS.md` section 8: "a rising
        // delay means that alerts no longer detect at their configured
        // interval", and it is the indicator that matters.
        let delay = (tallyowl_obs::time::now_ms() - work.queued_at).max(0);
        self.metrics
            .set_gauge("tallyowl_alert_evaluation_delay_ms", &labels(&[]), delay);

        // **A permit before a query.** `docs/ALERTS.md` section 7: alert
        // evaluation uses a separate budget pool, so alerting cannot occupy
        // more of the storage and query path than an operator allowed.
        //
        // A full pool puts the work back rather than marking the rule
        // `unknown`. A full pool is not a failed evaluation, and a rule that
        // went `unknown` whenever the installation was busy would go quiet
        // exactly when somebody needed it.
        let Some(_permit) = self.alerts.pool.take(Duration::from_millis(250)) else {
            return Outcome::Retry(
                "The alert evaluation pool is full, so this rule waited rather than competing with a person looking at a screen."
                    .to_string(),
            );
        };

        let decision = match self.alerts.evaluate_and_record(&rule) {
            Ok(decision) => decision,
            Err(failure) => return Outcome::Retry(failure.message),
        };

        if decision.notify {
            for target in &rule.notify {
                let mut notification = Work::new(Kind::Notification, work.project_id);
                notification.rule_id = rule.rule_id.clone();
                notification.target = crate::alerts::target_name(target);
                notification.state = decision.instance.state.clone();
                if let Err(failure) = self.workflows.submit(&notification) {
                    // The state is already stored, so a queue that refuses
                    // costs a notification and never the state. Retrying the
                    // evaluation would re-run the query and find the same
                    // state, which sends nothing.
                    self.logger.error(
                        "An alert changed state and its notification could not be queued.",
                        &[
                            ("rule", &rule.rule_id),
                            ("state", &decision.instance.state),
                            ("reason", &failure.message),
                        ],
                    );
                }
            }
        }
        Outcome::Done
    }
}

/// What turns a `secret_ref` into the secret it names.
pub type SecretLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Sends one notification to one target.
pub struct NotificationRunner {
    pub alerts: Arc<AlertService>,
    pub store: Arc<SegmentedStore>,
    pub metrics: Arc<Registry>,
    pub timeout: Duration,
    /// Resolves a `secret_ref` to the secret it names. A secret is a reference
    /// and never a value, which is why this is a function rather than a string.
    pub secrets: SecretLookup,
    /// What delivers a native callback. Absent means this installation has no
    /// callback transport, and a callback target is then refused by name.
    pub callbacks: Option<Arc<dyn crate::notify::CallbackSender>>,
}

impl Runner for NotificationRunner {
    fn run(&self, work: &Work) -> Outcome {
        let rules = match self.alerts.rules(work.project_id) {
            Ok(rules) => rules,
            Err(failure) => return Outcome::Retry(failure.message),
        };
        let Some(rule) = rules.into_iter().find(|rule| rule.rule_id == work.rule_id) else {
            return Outcome::Done;
        };
        let Some(instance) = self
            .alerts
            .instance(work.project_id, &work.rule_id)
            .ok()
            .flatten()
        else {
            return Outcome::Done;
        };
        // **A notification for a state that has since changed is dropped.** It
        // waited in a queue while the alert recovered, and sending it now would
        // tell somebody a firing alert is firing when it stopped ten minutes
        // ago. The change that recovered it queued its own notification.
        if instance.state != work.state {
            return Outcome::Done;
        }

        let Some(target) = rule
            .notify
            .iter()
            .find(|target| crate::alerts::target_name(target) == work.target)
        else {
            // The target was removed from the rule while this waited.
            return Outcome::Done;
        };

        let notification = Notification {
            rule_id: rule.rule_id.clone(),
            rule_name: rule.name.clone(),
            project_id: tallyowl_store::row::hex(&work.project_id),
            state: instance.state.clone(),
            outcome: instance.outcome.clone(),
            observed_value: instance.has_value.then_some(instance.observed_value),
            evaluated_at: instance.last_evaluated_at,
            query_link: format!(
                "/projects/{}/alerts/{}",
                tallyowl_store::row::hex(&work.project_id),
                rule.rule_id
            ),
            reason: instance.reason.clone(),
            escalation: work.attempt == 0 && instance.notifications_sent > 1,
        };

        let channel = match self.channel_for(target) {
            Ok(channel) => channel,
            Err(failure) => {
                self.record(work, &Attempt::failed(failure.message, false));
                return Outcome::Quarantine(
                    "This notification target cannot be reached by this installation.".to_string(),
                );
            }
        };

        let attempt = channel.send(&notification);
        self.record(work, &attempt);
        self.metrics.increment(
            "tallyowl_notifications_total",
            &labels(&[
                ("channel", channel_name(target)),
                (
                    "outcome",
                    match attempt.delivered {
                        true => "delivered",
                        false => "failed",
                    },
                ),
            ]),
        );

        // **A failed delivery never changes the alert state.** The state is
        // what the data said; whether a receiver answered is a fact about the
        // network. `docs/ALERTS.md` section 6.
        match (attempt.delivered, attempt.retryable) {
            (true, _) => Outcome::Done,
            (false, true) => Outcome::Retry(attempt.detail),
            (false, false) => Outcome::Quarantine(attempt.detail),
        }
    }
}

impl NotificationRunner {
    fn channel_for(
        &self,
        target: &tallyowl_control_api::types::NotificationTarget,
    ) -> Result<Box<dyn Channel>, TallyOwlError> {
        match target.kind {
            tallyowl_control_api::types::NotificationTarget_kind::Webhook => {
                Ok(Box::new(Webhook {
                    url: target.url.clone().unwrap_or_default(),
                    secret: target
                        .secret_ref
                        .as_deref()
                        .and_then(|reference| (self.secrets)(reference))
                        .unwrap_or_default(),
                    timeout: self.timeout,
                }))
            }
            tallyowl_control_api::types::NotificationTarget_kind::CsilCallback => {
                let sender = self.callbacks.clone().ok_or_else(|| {
                    TallyOwlError::new(
                        tallyowl_obs::ErrorCode::FailedPrecondition,
                        "This installation has no native callback transport, so a `csil-callback` target cannot be delivered. Use a webhook.".to_string(),
                    )
                })?;
                Ok(Box::new(CsilCallback {
                    address: target.url.clone().unwrap_or_default(),
                    timeout: self.timeout,
                    sender,
                }))
            }
        }
    }

    /// Write the attempt where an operator can read it.
    ///
    /// A metric says how many failed and cannot say which rule or which
    /// address. `docs/ALERTS.md` section 6 asks for both.
    fn record(&self, work: &Work, attempt: &Attempt) {
        let record = tallyowl_store::control::NotificationRecord {
            rule_id: work.rule_id.clone(),
            target: work.target.clone(),
            state: work.state.clone(),
            attempts: work.attempt + 1,
            delivered: attempt.delivered,
            last_error: match attempt.delivered {
                true => String::new(),
                false => attempt.detail.clone(),
            },
            next_attempt_at: match attempt.delivered || !attempt.retryable {
                true => 0,
                false => {
                    tallyowl_obs::time::now_ms()
                        + crate::notify::backoff_ms(work.attempt + 1, work.queued_at as u64)
                }
            },
            at: tallyowl_obs::time::now_ms(),
        };
        let _ = self.store.catalog().put_notification(&record);
        let _ = self.store.catalog().trim_notifications(500);
    }
}

fn channel_name(target: &tallyowl_control_api::types::NotificationTarget) -> &'static str {
    match target.kind {
        tallyowl_control_api::types::NotificationTarget_kind::Webhook => "webhook",
        tallyowl_control_api::types::NotificationTarget_kind::CsilCallback => "csil-callback",
    }
}

/// The four passes over retained raw data.
pub struct ProjectorRunner {
    pub store: Arc<SegmentedStore>,
    pub identity: Arc<crate::identity::IdentityCache>,
    pub logger: Arc<Logger>,
    /// `retention.raw`, and what a retention pass removes past it.
    pub raw_retention_ms: i64,
    /// `storage.deduplicationWindow`. A receipt older than this can go.
    pub receipt_window_ms: i64,
    pub reserve_bytes: u64,
    /// Where an export writes when the request names no destination.
    pub export_root: std::path::PathBuf,
}

impl Runner for ProjectorRunner {
    fn run(&self, work: &Work) -> Outcome {
        let result = match work.kind {
            Kind::ProjectorRebuild => self.rebuild(work),
            Kind::Retention => self.retention(work),
            Kind::Deletion => self.deletion(work),
            Kind::Export => self.export(work),
            // An alert or a notification never reaches this runner. Saying so
            // is better than treating it as a rebuild.
            other => Err(TallyOwlError::internal(format!(
                "`{}` is not a projector pass.",
                other.as_str()
            ))),
        };
        match result {
            Ok(said) => {
                self.logger.info(
                    "A workflow pass finished.",
                    &[("workflow", work.kind.as_str()), ("outcome", &said)],
                );
                Outcome::Done
            }
            Err(failure) if failure.retryable => Outcome::Retry(failure.message),
            Err(failure) => Outcome::Quarantine(failure.message),
        }
    }
}

impl ProjectorRunner {
    /// Rebuild what is derived, from what is retained.
    ///
    /// **Everything derived in this build is a cache or a projection over the
    /// stored rows**, so a rebuild is: forget the derived thing, and let it be
    /// derived again. The identity graph is the one with a cache in front of
    /// it; the catalog's own rebuild from checksummed manifests is the other
    /// half and it is `rebuild_from_manifests`.
    fn rebuild(&self, _work: &Work) -> Result<String, TallyOwlError> {
        self.identity.clear();
        let manifests = self
            .store
            .catalog()
            .manifests()
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        let generation = self
            .store
            .catalog()
            .rebuild_from_manifests(&manifests)
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        Ok(format!(
            "The identity graph was forgotten and the catalog was rebuilt from {} manifests at generation {generation}.",
            manifests.len()
        ))
    }

    /// Apply retention.
    ///
    /// Two things age out, and they age out for different reasons:
    ///
    /// - a **receipt** past the deduplication window. It exists so that a
    ///   repeated batch stays one logical commit, and past the window a repeat
    ///   cannot arrive;
    /// - **raw rows** past `retention.raw`. They go through a tombstone rather
    ///   than a delete, because a tombstone is a standing predicate and hides
    ///   the late arrivals that a one-time delete could not name.
    fn retention(&self, work: &Work) -> Result<String, TallyOwlError> {
        let now = tallyowl_obs::time::now_ms();
        let expired = self
            .store
            .catalog()
            .expire_receipts(now - self.receipt_window_ms)
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;

        let mut removed = 0;
        if self.raw_retention_ms > 0 {
            let horizon = now - self.raw_retention_ms;
            let tombstone = tallyowl_store::catalog::Tombstone {
                tombstone_id: retention_id(work.project_id, horizon),
                generation: 0,
                project_id: work.project_id,
                event_ids: Vec::new(),
                property: None,
                except_kinds: Vec::new(),
                range: Some((i64::MIN / 2, horizon)),
                requested_at: now,
                // A retention predicate has no late-arrival horizon to reach:
                // anything that arrives with an occurrence time in that range
                // is already past retention when it lands.
                horizon: i64::MAX,
                reason: format!(
                    "Retention: raw telemetry is kept for {} days.",
                    self.raw_retention_ms / 86_400_000
                ),
            };
            removed = self
                .store
                .erase(&tombstone)
                .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        }
        Ok(format!(
            "{expired} receipts expired, and the retention predicate is at generation {removed}."
        ))
    }

    /// Re-apply every tombstone in the erasure ledger.
    ///
    /// **This is what stops a replay resurrecting deleted data.** The ledger is
    /// a second file for exactly this reason: a catalog that was lost or
    /// rebuilt must not take the erasure record with it, because the rows it
    /// hid would come back and nothing would say so.
    fn deletion(&self, _work: &Work) -> Result<String, TallyOwlError> {
        let restored = self
            .store
            .catalog()
            .restore_tombstones_from_ledger()
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        let generation = self
            .store
            .tombstone_generation()
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        Ok(format!(
            "{restored} erasure predicates were re-applied from the ledger, at generation {generation}."
        ))
    }

    /// Export one range to Parquet.
    fn export(&self, work: &Work) -> Result<String, TallyOwlError> {
        let into = match work.destination.is_empty() {
            true => self.export_root.join(format!(
                "{}-{}-{}.parquet",
                tallyowl_store::row::hex(&work.project_id),
                work.range_start,
                work.range_end
            )),
            false => std::path::PathBuf::from(&work.destination),
        };
        if let Some(parent) = into.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                TallyOwlError::invalid_argument(format!(
                    "The export cannot be written to {}. {e}",
                    parent.display()
                ))
            })?;
        }
        let generation = self
            .store
            .tombstone_generation()
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        let manifest = tallyowl_export::export_events(
            self.store.as_ref(),
            &tallyowl_export::ExportRequest {
                project_id: work.project_id,
                range_start: work.range_start,
                range_end: work.range_end,
                basis: tallyowl_store::TimeBasis::OccurredAt,
                into: into.clone(),
                tombstone_generation: generation,
                reserve_bytes: self.reserve_bytes,
            },
        )
        .map_err(|e| TallyOwlError::internal(e.message))?;
        Ok(format!(
            "{} rows were written to {}.",
            manifest.rows,
            into.display()
        ))
    }
}

/// A stable identifier for one retention predicate.
///
/// **Stable, so running the pass twice is running it once.** A fresh identifier
/// each time would make one predicate for every pass, and the erasure ledger
/// would grow with the schedule rather than with the obligations.
fn retention_id(project_id: [u8; 16], horizon: i64) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tallyowl-retention");
    hasher.update(&project_id);
    // The horizon is rounded to the day, so two passes on one day agree.
    hasher.update(&(horizon / 86_400_000).to_be_bytes());
    let mut id = [0u8; 16];
    id.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    id
}

/// Puts alert evaluations in the queue when they are due, and runs the sweep.
///
/// **The sweep is the part nothing else can do.** Corndogs evaluates a task
/// timeout only when a caller invokes `CleanUpTimedOut`, so retry, backoff, and
/// dead-worker recovery all stop when this thread stops.
pub struct Scheduler {
    pub alerts: Arc<AlertService>,
    pub workflows: Arc<Workflows>,
    pub logger: Arc<Logger>,
    /// The last time each rule was queued, so a rule is queued on its interval
    /// rather than on every tick.
    last: std::sync::Mutex<std::collections::BTreeMap<String, i64>>,
    /// A stagger for each rule, so a thousand rules on one interval do not all
    /// evaluate on the same second. `docs/ALERTS.md` section 7.
    stagger: std::sync::Mutex<std::collections::BTreeMap<String, i64>>,
    /// When this scheduler first saw each rule. The stagger is measured from
    /// here, and measuring it from `now` instead is what stopped a rule ever
    /// becoming due.
    seen: std::sync::Mutex<std::collections::BTreeMap<String, i64>>,
}

impl Scheduler {
    pub fn new(
        alerts: Arc<AlertService>,
        workflows: Arc<Workflows>,
        logger: Arc<Logger>,
    ) -> Scheduler {
        Scheduler {
            alerts,
            workflows,
            logger,
            last: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            stagger: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            seen: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// One tick: queue what is due, and return the queues to their ready state.
    pub fn tick(&self, now: i64) -> Result<usize, TallyOwlError> {
        // The sweep first. Work that a dead worker was holding should be
        // available to this round rather than to the next one.
        if let Err(failure) = self.workflows.sweep() {
            self.logger.error(
                "The workflow sweep did not run, so nothing will be retried until it does.",
                &[("reason", &failure.message)],
            );
        }

        let rules = self.alerts.every_rule()?;
        let mut queued = 0;
        for rule in rules {
            if !rule.enabled {
                continue;
            }
            let project_id = match <[u8; 16]>::try_from(rule.project_id.as_slice()) {
                Ok(id) => id,
                Err(_) => continue,
            };
            let interval = rule.interval_ms.max(crate::alerts::MIN_INTERVAL_MS);
            let offset = *self
                .stagger
                .lock()
                .expect("scheduler")
                .entry(rule.rule_id.clone())
                .or_insert_with(|| stagger_for(&rule.rule_id, interval));
            let mut last = self.last.lock().expect("scheduler");
            // **The stagger is measured from when this scheduler first saw the
            // rule, not from now.** Measuring it from now moves the deadline
            // forward on every tick, so a rule is always about to be due and
            // never is. The running loop found that and no test did: every test
            // called `tick` with a time it chose.
            let first_seen = *self
                .seen
                .lock()
                .expect("scheduler")
                .entry(rule.rule_id.clone())
                .or_insert(now);
            let due_at = match last.get(&rule.rule_id) {
                Some(previous) => previous + interval,
                // The first evaluation of a rule waits its stagger, so a
                // restart does not evaluate every rule in the installation on
                // the same second.
                None => first_seen + offset,
            };
            if now < due_at {
                continue;
            }
            last.insert(rule.rule_id.clone(), now);
            drop(last);

            let mut work = Work::new(Kind::AlertEvaluation, project_id);
            work.rule_id = rule.rule_id.clone();
            work.queued_at = now;
            match self.workflows.submit(&work) {
                Ok(_) => queued += 1,
                Err(failure) => self.logger.error(
                    "An alert evaluation could not be queued, so this rule did not run.",
                    &[("rule", &rule.rule_id), ("reason", &failure.message)],
                ),
            }
        }
        Ok(queued)
    }
}

/// A rule's own offset inside its interval, from its identifier.
///
/// It is derived rather than random so that a restart puts a rule back on the
/// same offset, which keeps the spread rather than reshuffling it.
fn stagger_for(rule_id: &str, interval_ms: i64) -> i64 {
    let digest = blake3::hash(rule_id.as_bytes());
    let bytes: [u8; 8] = digest.as_bytes()[..8].try_into().unwrap_or([0; 8]);
    (u64::from_be_bytes(bytes) % interval_ms.max(1) as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_rules_on_one_interval_do_not_evaluate_on_the_same_second() {
        // ALERTS.md section 7: "the scheduler staggers evaluations to avoid a
        // burst on an interval boundary". One thousand rules on a one-minute
        // interval is about seventeen evaluations each second when they are
        // spread, and one thousand at once when they are not.
        let offsets: Vec<i64> = (0..200)
            .map(|n| stagger_for(&format!("rule-{n}"), 60_000))
            .collect();
        let mut sorted = offsets.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert!(
            sorted.len() > 190,
            "only {} of 200 rules got a distinct offset",
            sorted.len()
        );
        assert!(offsets.iter().all(|offset| (0..60_000).contains(offset)));
    }

    #[test]
    fn a_stagger_is_the_same_after_a_restart() {
        // A random offset would reshuffle every rule on every restart, which
        // turns a spread into a new burst each time.
        assert_eq!(
            stagger_for("checkout", 60_000),
            stagger_for("checkout", 60_000)
        );
    }

    #[test]
    fn a_retention_predicate_has_the_same_identifier_all_day() {
        // A fresh identifier for each pass would put one predicate in the
        // erasure ledger for every run of the schedule.
        let morning = retention_id([1; 16], 1_785_628_800_000);
        let afternoon = retention_id([1; 16], 1_785_628_800_000 + 3_600_000);
        assert_eq!(morning, afternoon);
        assert_ne!(morning, retention_id([2; 16], 1_785_628_800_000));
    }
}
