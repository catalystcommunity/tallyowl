//! Built-in alerting: a scheduled query, a state, and a notification.
//!
//! `docs/ALERTS.md` is the owning document and it is deliberately small.
//! Grafana is the primary alerting path for an operator who runs one; this
//! serves an installation that does not.
//!
//! # An alert is a query, and that is the whole design
//!
//! An alert evaluates by running its saved query on a schedule, through the
//! same typed algebra the dashboard uses. It has one property that matters more
//! than any other: **an alert value always matches what a person sees in the
//! dashboard.** A separate evaluation engine gives a second set of semantics,
//! an alert then fires on a number nobody can reproduce, and that destroys
//! trust in every alert.
//!
//! The cost is detection latency, and it equals the evaluation interval. This
//! says so rather than implying that an alert is immediate.
//!
//! # The two rules that stop an alert from lying
//!
//! - **an alert evaluates at `committed` consistency.** A `bounded-stale` read
//!   can answer from a replica that lags, and the alert would then fire on
//!   replication lag and report it as a change in the data. A rule that asks
//!   for `bounded-stale` is refused when it is written, not when it runs;
//! - **an alert never fires on a partial result.** A missing tablet must not
//!   look like a metric drop. A partial result is the `error` outcome and the
//!   rule enters `unknown`, so an operator learns that TallyOwl could not
//!   answer — which is a different fact from a value that crossed a threshold.
//!
//! # The state is durable because a restart must not be a notification storm
//!
//! A state change sends a notification and a repeated evaluation in the same
//! state does not. That rule is only worth anything if the state survives a
//! restart, so the instance lives in the control catalog beside the rule.

use std::sync::Arc;

use tallyowl_control_api::codec::{decode_alert_rule, encode_alert_rule, encode_query_request};
use tallyowl_control_api::types::{
    AlertInstance, AlertOutcome, AlertRule, AlertState, CompareOp, Consistency, NotificationTarget,
    QueryRequest,
};
use tallyowl_obs::error::TallyOwlError;
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_store::control::{AlertInstanceRecord, AlertRuleRecord};
use tallyowl_store::SegmentedStore;

use crate::query::QueryService;

/// Sixteen bytes, or a refusal that says so.
fn to_id(bytes: &[u8]) -> Result<[u8; 16], TallyOwlError> {
    <[u8; 16]>::try_from(bytes).map_err(|_| {
        TallyOwlError::invalid_argument("An identifier must be exactly sixteen bytes.")
    })
}

/// How many evaluations in a row may exceed the budget before TallyOwl disables
/// the rule.
///
/// `docs/ALERTS.md` section 7: "TallyOwl disables and reports an alert that
/// repeatedly exceeds its budget." Three is enough that one slow moment does
/// not disable a rule, and few enough that a rule which is simply too expensive
/// stops competing with a person who is looking at a screen.
pub const BUDGET_FAILURES_BEFORE_DISABLING: u64 = 3;

/// The share of the query runtime budget an alert evaluation may use.
///
/// Section 7 asks for a separate query budget pool, and this is the shape of it
/// that a one-process installation can have: an evaluation runs against a
/// tighter deadline than a person's query, so a heavy alert cannot starve a
/// dashboard that somebody is watching.
pub const ALERT_RUNTIME_SHARE: f64 = 0.5;

/// The shortest interval a rule may ask for.
///
/// One thousand rules on a one-minute interval is about seventeen evaluations
/// each second, and each one is a full query. Section 7 states that; this stops
/// a single rule asking for something an installation cannot serve at all.
pub const MIN_INTERVAL_MS: i64 = 1_000;

/// What one evaluation produced, before the state machine reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    pub outcome: AlertOutcome,
    /// Why an `error` outcome failed, when it failed.
    ///
    /// **A budget failure and an unreachable tablet are different facts.** A
    /// rule that keeps running out of budget is disabled and reported; a rule
    /// that could not reach a tablet is not, because the tablet is what has to
    /// be fixed. Matching on the words of a message to tell them apart is how
    /// that stops working the day somebody rewords the message.
    pub code: Option<tallyowl_obs::ErrorCode>,
    /// Present for `value`, absent for `no-data` and `error`. A zero that means
    /// "nothing" and a zero that means zero are different facts.
    pub value: Option<f64>,
    pub commit_watermark: u64,
    pub reason: String,
}

/// What the state machine decided.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub instance: AlertInstanceRecord,
    /// True when this evaluation must send a notification. A repeated
    /// evaluation in one state sets it false.
    pub notify: bool,
    /// Set when the notification is an escalation rather than a change.
    pub escalated: bool,
}

/// How many alert evaluations may run at once.
///
/// **This is the separate query budget pool `docs/ALERTS.md` section 7 asks
/// for**, and it is worth saying what it does and does not do. It does not
/// reserve anything for a person looking at a screen; there is no way to do
/// that from inside one process without a scheduler this system does not have.
/// It bounds how much of the storage and query path alerting can occupy at
/// once, which is the half that can be bounded.
///
/// **One is the default because one is what the shape already gives.** The head
/// runs one worker for the evaluation queue, so alert concurrency is one by
/// construction today. Making it explicit means a change that adds a second
/// worker cannot silently double the load alerting puts on a dashboard, which
/// is exactly the kind of isolation that is lost by accident rather than by
/// decision.
pub const DEFAULT_EVALUATION_CONCURRENCY: usize = 1;

/// The pool itself: a count of permits and a way to wait a little for one.
///
/// A pool that blocked for ever would hold a worker thread and a queue claim
/// through an outage of its own making. It waits briefly and then says no, and
/// the caller puts the work back with a backoff.
pub struct BudgetPool {
    free: std::sync::Mutex<usize>,
    returned: std::sync::Condvar,
    capacity: usize,
}

/// One permit, returned to the pool when it is dropped.
pub struct Permit<'a> {
    pool: &'a BudgetPool,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut free = self.pool.free.lock().expect("evaluation pool");
        *free += 1;
        self.pool.returned.notify_one();
    }
}

impl Default for BudgetPool {
    fn default() -> BudgetPool {
        BudgetPool::new(DEFAULT_EVALUATION_CONCURRENCY)
    }
}

impl BudgetPool {
    pub fn new(capacity: usize) -> BudgetPool {
        let capacity = capacity.max(1);
        BudgetPool {
            free: std::sync::Mutex::new(capacity),
            returned: std::sync::Condvar::new(),
            capacity,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Take a permit, waiting up to `within` for one.
    ///
    /// `None` means the pool is full. The caller puts the work back rather than
    /// marking the rule `unknown`: **a full pool is not a failed evaluation.**
    /// A rule that went `unknown` whenever the installation was busy would go
    /// quiet exactly when somebody needed it.
    pub fn take(&self, within: std::time::Duration) -> Option<Permit<'_>> {
        let mut free = self.free.lock().expect("evaluation pool");
        let deadline = std::time::Instant::now() + within;
        while *free == 0 {
            let left = deadline.checked_duration_since(std::time::Instant::now())?;
            let (guard, timed_out) = self
                .returned
                .wait_timeout(free, left)
                .expect("evaluation pool");
            free = guard;
            if timed_out.timed_out() && *free == 0 {
                return None;
            }
        }
        *free -= 1;
        Some(Permit { pool: self })
    }
}

/// Alert rules, their state, and the evaluation that moves one to the other.
pub struct AlertService {
    pub store: Arc<SegmentedStore>,
    pub query: Arc<QueryService>,
    pub metrics: Arc<Registry>,
    /// Whether this installation can deliver a native callback.
    ///
    /// **A rule that can never notify is refused when it is written.** The
    /// alternative is a rule an operator saved, watched fire, and never heard
    /// about — and the reason would be in a delivery record they had no cause
    /// to look at. It is the same rule as the one about a stale read: refuse
    /// where the person is looking.
    pub callbacks_available: bool,
    /// How much of the query path alerting may occupy at once. See
    /// [`BudgetPool`].
    pub pool: Arc<BudgetPool>,
}

impl AlertService {
    /// Check a rule and store it.
    ///
    /// Every refusal here is a refusal to store a rule that could lie later.
    pub fn put_rule(&self, rule: &AlertRule, by: &str) -> Result<AlertRule, TallyOwlError> {
        check_rule(rule, self.callbacks_available)?;
        let project_id = to_id(&rule.project_id)?;
        let now = tallyowl_obs::time::now_ms();
        let mut stored = rule.clone();
        stored.updated_at = Some(now);
        stored.updated_by = Some(by.to_string());
        self.store
            .catalog()
            .put_alert_rule(&AlertRuleRecord {
                rule_id: rule.rule_id.clone(),
                project_id,
                encoded: encode_alert_rule(&stored),
                updated_at: now,
                updated_by: by.to_string(),
            })
            .map_err(store_failure)?;
        Ok(stored)
    }

    pub fn rules(&self, project_id: [u8; 16]) -> Result<Vec<AlertRule>, TallyOwlError> {
        let held = self
            .store
            .catalog()
            .alert_rules(project_id)
            .map_err(store_failure)?;
        held.iter().map(decode_rule).collect()
    }

    /// Every rule in the installation, for the scheduler.
    pub fn every_rule(&self) -> Result<Vec<AlertRule>, TallyOwlError> {
        let held = self
            .store
            .catalog()
            .every_alert_rule()
            .map_err(store_failure)?;
        held.iter().map(decode_rule).collect()
    }

    pub fn remove_rule(&self, project_id: [u8; 16], rule_id: &str) -> Result<(), TallyOwlError> {
        self.store
            .catalog()
            .remove_alert_rule(project_id, rule_id)
            .map_err(store_failure)
    }

    pub fn instances(&self, project_id: [u8; 16]) -> Result<Vec<AlertInstance>, TallyOwlError> {
        Ok(self
            .store
            .catalog()
            .alert_instances(project_id)
            .map_err(store_failure)?
            .iter()
            .map(to_wire_instance)
            .collect())
    }

    pub fn instance(
        &self,
        project_id: [u8; 16],
        rule_id: &str,
    ) -> Result<Option<AlertInstanceRecord>, TallyOwlError> {
        self.store
            .catalog()
            .alert_instance(project_id, rule_id)
            .map_err(store_failure)
    }

    /// Run one rule's query and say what it produced.
    ///
    /// This never decides anything about state. Separating the two is what
    /// makes the state machine testable without a store behind it, and the
    /// state machine is where every rule about not lying lives.
    pub fn evaluate(&self, rule: &AlertRule) -> Evaluation {
        let mut request = rule.query.clone();
        // **The alert's own budget.** Section 7 asks for a separate pool, and a
        // tighter deadline is what one process can give: an evaluation that
        // would starve a person watching a dashboard runs out of time first.
        let allowed = (self.query.max_runtime_ms as f64 * ALERT_RUNTIME_SHARE).max(1.0) as i64;
        let held = request.budget.clone();
        request.budget = Some(tallyowl_control_api::types::QueryBudget {
            deadline_ms: Some(match held.as_ref().and_then(|budget| budget.deadline_ms) {
                Some(asked) => asked.min(allowed),
                None => allowed,
            }),
            max_scanned_bytes: held.as_ref().and_then(|budget| budget.max_scanned_bytes),
            max_scanned_segments: held.as_ref().and_then(|budget| budget.max_scanned_segments),
            max_rows: held.as_ref().and_then(|budget| budget.max_rows),
        });
        // **Never a partial result.** A missing tablet must not look like a
        // metric drop, so the request refuses one whatever the rule asked for.
        request.consistency = Consistency::Committed;
        request.allow_partial = false;

        match self.query.run(request) {
            Err(failure) => Evaluation {
                outcome: AlertOutcome::Error,
                code: Some(failure.code),
                value: None,
                commit_watermark: 0,
                reason: failure.message,
            },
            Ok(response) => {
                if !response.metadata.complete {
                    return Evaluation {
                        outcome: AlertOutcome::Error,
                        code: Some(tallyowl_obs::ErrorCode::IncompleteResult),
                        value: None,
                        commit_watermark: response.metadata.commit_watermark,
                        reason: "Part of the data this alert reads could not be answered, so the value would be smaller than the truth."
                            .to_string(),
                    };
                }
                let alias = rule
                    .threshold
                    .as_ref()
                    .map(|threshold| threshold.alias.clone())
                    .unwrap_or_default();
                match read_measure(&response, &alias) {
                    None => Evaluation {
                        outcome: AlertOutcome::NoData,
                        code: None,
                        value: None,
                        commit_watermark: response.metadata.commit_watermark,
                        reason: "The query answered with no rows.".to_string(),
                    },
                    Some(value) => Evaluation {
                        outcome: AlertOutcome::Value,
                        code: None,
                        value: Some(value),
                        commit_watermark: response.metadata.commit_watermark,
                        reason: String::new(),
                    },
                }
            }
        }
    }

    /// Run one rule and store what it decided.
    ///
    /// Returns the decision so the caller can send what it says to send. The
    /// notification is a separate durable task; this function is finished when
    /// the state is safe.
    pub fn evaluate_and_record(&self, rule: &AlertRule) -> Result<Decision, TallyOwlError> {
        let project_id = to_id(&rule.project_id)?;
        let held = self.instance(project_id, &rule.rule_id)?;
        let evaluation = self.evaluate(rule);
        let now = tallyowl_obs::time::now_ms();

        self.metrics.increment(
            "tallyowl_alert_evaluations_total",
            &labels(&[("outcome", outcome_name(&evaluation.outcome))]),
        );

        let decision = decide(rule, held.as_ref(), &evaluation, now, project_id);
        self.store
            .catalog()
            .put_alert_instance(&decision.instance)
            .map_err(store_failure)?;
        if decision.notify {
            self.metrics.increment(
                "tallyowl_alert_state_changes_total",
                &labels(&[("state", decision.instance.state.as_str())]),
            );
        }
        // A rule that keeps running out of budget is disabled and reported,
        // rather than left competing with a person looking at a screen.
        if decision.instance.budget_failures >= BUDGET_FAILURES_BEFORE_DISABLING
            && rule.enabled
            && rule.disabled_reason.is_none()
        {
            let mut disabled = rule.clone();
            disabled.enabled = false;
            disabled.disabled_reason = Some(format!(
                "This rule ran out of its evaluation budget {} times in a row and TallyOwl stopped running it. Narrow its range or its query, then enable it again.",
                decision.instance.budget_failures
            ));
            self.put_rule(&disabled, "tallyowl")?;
            self.metrics
                .increment("tallyowl_alerts_disabled_by_budget_total", &labels(&[]));
        }
        Ok(decision)
    }

    /// Suppress notification for a period. The evaluation still runs.
    pub fn silence(
        &self,
        project_id: [u8; 16],
        rule_id: &str,
        until: i64,
        reason: &str,
    ) -> Result<AlertInstance, TallyOwlError> {
        let mut rule = self.rule(project_id, rule_id)?;
        rule.silenced_until = Some(until);
        rule.silence_reason = (!reason.is_empty()).then(|| reason.to_string());
        self.put_rule(&rule, "operator")?;

        let mut instance = self
            .instance(project_id, rule_id)?
            .unwrap_or_else(|| new_instance(rule_id, project_id));
        instance.state = state_name(&AlertState::Silenced).to_string();
        instance.reason = reason.to_string();
        instance.since = tallyowl_obs::time::now_ms();
        self.store
            .catalog()
            .put_alert_instance(&instance)
            .map_err(store_failure)?;
        Ok(to_wire_instance(&instance))
    }

    /// Put a rule back to `ok` by hand.
    pub fn resolve(
        &self,
        project_id: [u8; 16],
        rule_id: &str,
        reason: &str,
    ) -> Result<AlertInstance, TallyOwlError> {
        // A resolve clears a silence as well. An operator who resolves a rule
        // means "this is over", and leaving it quiet afterwards would hide the
        // next occurrence.
        let mut rule = self.rule(project_id, rule_id)?;
        rule.silenced_until = None;
        rule.silence_reason = None;
        self.put_rule(&rule, "operator")?;

        let mut instance = self
            .instance(project_id, rule_id)?
            .unwrap_or_else(|| new_instance(rule_id, project_id));
        instance.state = state_name(&AlertState::Ok).to_string();
        instance.reason = reason.to_string();
        instance.since = tallyowl_obs::time::now_ms();
        instance.notifications_sent = 0;
        self.store
            .catalog()
            .put_alert_instance(&instance)
            .map_err(store_failure)?;
        Ok(to_wire_instance(&instance))
    }

    fn rule(&self, project_id: [u8; 16], rule_id: &str) -> Result<AlertRule, TallyOwlError> {
        self.rules(project_id)?
            .into_iter()
            .find(|rule| rule.rule_id == rule_id)
            .ok_or_else(|| {
                TallyOwlError::new(
                    tallyowl_obs::ErrorCode::NotFound,
                    format!("There is no alert rule called `{rule_id}` in this project."),
                )
            })
    }
}

/// The state machine, with no store and no clock of its own.
///
/// Every rule `docs/ALERTS.md` states about what an alert may and may not do
/// lives here, so a test can drive all of it from a table.
pub fn decide(
    rule: &AlertRule,
    held: Option<&AlertInstanceRecord>,
    evaluation: &Evaluation,
    now: i64,
    project_id: [u8; 16],
) -> Decision {
    let previous = held.cloned().unwrap_or_else(|| {
        let mut fresh = new_instance(&rule.rule_id, project_id);
        fresh.since = now;
        fresh
    });

    let wanted = match evaluation.outcome {
        // A partial or failed evaluation is `unknown`. It is not `ok`, because
        // "we could not answer" is a different fact from "the condition is not
        // satisfied", and it is not `firing`, because nothing was measured.
        AlertOutcome::Error => AlertState::Unknown,
        AlertOutcome::NoData => match rule.absence.is_some() {
            true => AlertState::Firing,
            false => AlertState::NoData,
        },
        AlertOutcome::Value => match (&rule.threshold, evaluation.value) {
            (Some(threshold), Some(value)) => {
                match crosses(threshold.compare.clone(), value, threshold.value) {
                    true => AlertState::Firing,
                    false => AlertState::Ok,
                }
            }
            // A rule with no threshold and a value is a rule about absence that
            // got data, which is the healthy case.
            _ => AlertState::Ok,
        },
    };

    // **How long the condition has held.** A threshold with `sustained_ms`
    // fires only after it has been true for that long, so a single spike does
    // not wake somebody up.
    let same_as_before = state_name(&wanted) == previous.state;
    let holding = match same_as_before {
        true => previous.holding_ms + (now - previous.last_evaluated_at).max(0),
        false => 0,
    };
    let sustained = rule
        .threshold
        .as_ref()
        .and_then(|threshold| threshold.sustained_ms)
        .unwrap_or(0);
    let absence_for = rule
        .absence
        .as_ref()
        .map(|absence| absence.for_ms)
        .unwrap_or(0);
    let needed = match evaluation.outcome {
        AlertOutcome::NoData => absence_for,
        _ => sustained,
    };
    let holding_long_enough = wanted != AlertState::Firing || holding >= needed;

    let silenced = rule.silenced_until.is_some_and(|until| until > now);

    let state = match (holding_long_enough, silenced) {
        // Still counting up to the sustain window. The rule stays where it was
        // rather than flickering into `firing` and out again.
        (false, _) => match previous.state.as_str() {
            "" => AlertState::Ok,
            _ => state_from(&previous.state),
        },
        (true, true) => AlertState::Silenced,
        (true, false) => wanted,
    };

    let changed = state_name(&state) != previous.state;
    let escalate_after = rule.escalate_after_ms.unwrap_or(0);
    // An escalation repeats a notification while a rule stays firing. A rule
    // that asks for none sends one notification for each change, which is what
    // section 5 states.
    let escalated = !changed
        && state == AlertState::Firing
        && escalate_after > 0
        && now - previous.last_notified_at >= escalate_after;

    // **A silenced rule records state and sends nothing.** An operator who
    // silenced a rule still needs to see what it did while it was quiet.
    let notify = (changed || escalated) && state != AlertState::Silenced;

    let mut instance = AlertInstanceRecord {
        rule_id: rule.rule_id.clone(),
        project_id,
        state: state_name(&state).to_string(),
        outcome: outcome_name(&evaluation.outcome).to_string(),
        since: match changed {
            true => now,
            false => previous.since,
        },
        last_evaluated_at: now,
        observed_value: evaluation.value.unwrap_or(0.0),
        has_value: evaluation.value.is_some(),
        reason: evaluation.reason.clone(),
        notifications_sent: previous.notifications_sent + u64::from(notify),
        last_notified_at: match notify {
            true => now,
            false => previous.last_notified_at,
        },
        holding_ms: holding,
        commit_watermark: evaluation.commit_watermark,
        budget_failures: previous.budget_failures,
    };
    // A state change starts the count again, because "how many times has this
    // one alert notified" is what an operator reads it as.
    if changed {
        instance.notifications_sent = u64::from(notify);
    }
    instance.budget_failures = match over_budget(evaluation) {
        true => previous.budget_failures + 1,
        false => 0,
    };

    Decision {
        instance,
        notify,
        escalated,
    }
}

/// Whether this evaluation ran out of its budget rather than failing some other
/// way. A budget failure disables a rule after enough of them; an unreachable
/// tablet must not.
fn over_budget(evaluation: &Evaluation) -> bool {
    matches!(
        evaluation.code,
        Some(tallyowl_obs::ErrorCode::BudgetExceeded)
            | Some(tallyowl_obs::ErrorCode::ResourceExhausted)
    )
}

fn crosses(compare: CompareOp, value: f64, against: f64) -> bool {
    match compare {
        CompareOp::Gt => value > against,
        CompareOp::Ge => value >= against,
        CompareOp::Lt => value < against,
        CompareOp::Le => value <= against,
        CompareOp::Eq => value == against,
        CompareOp::Ne => value != against,
    }
}

/// The measure a threshold names, out of the first row of the answer.
///
/// A rule names an alias rather than a column position, because a query that
/// gained a dimension would otherwise start alerting on a different number
/// without anybody changing the rule.
fn read_measure(response: &tallyowl_control_api::types::QueryResponse, alias: &str) -> Option<f64> {
    let row = response.rows.first()?;
    let at = match alias.is_empty() {
        // A rule with no threshold is an absence rule, and it only needs to
        // know whether anything came back.
        true => return row.values.first().and_then(as_number).or(Some(0.0)),
        false => response.columns.iter().position(|column| column == alias)?,
    };
    row.values.get(at).and_then(as_number)
}

fn as_number(value: &tallyowl_control_api::types::TypedValue) -> Option<f64> {
    value
        .float_value
        .or_else(|| value.int_value.map(|v| v as f64))
        .or_else(|| value.uint_value.map(|v| v as f64))
        .or_else(|| value.decimal_value.as_ref().and_then(decimal_as_float))
}

fn decimal_as_float(decimal: &tallyowl_control_api::types::CsilDecimal) -> Option<f64> {
    Some(decimal.mantissa as f64 * 10f64.powi(decimal.exponent as i32))
}

/// Every reason a rule is refused when it is written rather than when it runs.
pub fn check_rule(rule: &AlertRule, callbacks_available: bool) -> Result<(), TallyOwlError> {
    if rule.rule_id.trim().is_empty() {
        return Err(TallyOwlError::invalid_argument(
            "An alert rule needs an identifier.",
        ));
    }
    if rule.threshold.is_none() && rule.absence.is_none() {
        return Err(TallyOwlError::invalid_argument(
            "This alert rule has no condition. Give it a threshold on a measure, or an absence rule that fires when the data stops.",
        ));
    }
    if rule.threshold.is_some() && rule.absence.is_some() {
        return Err(TallyOwlError::invalid_argument(
            "This alert rule has both a threshold and an absence rule. Built-in alerting answers one condition for each rule; write two rules.",
        ));
    }
    if rule.interval_ms < MIN_INTERVAL_MS {
        return Err(TallyOwlError::invalid_argument(format!(
            "This alert asks to evaluate every {} ms, and the shortest interval is {MIN_INTERVAL_MS} ms. An alert is a whole query each time it runs.",
            rule.interval_ms
        )));
    }
    // **A `bounded-stale` alert is refused, and it is refused here.** A stale
    // read can answer from a replica that lags, and the alert would then fire
    // on replication lag and report it as a change in the data. Refusing when
    // the rule is written means an operator finds out while they are looking at
    // it rather than at three in the morning.
    if rule.query.consistency == Consistency::BoundedStale {
        return Err(TallyOwlError::invalid_argument(
            "An alert reads at `committed` consistency and this rule asks for a stale read. A stale read can answer from a replica that is behind, so the alert would fire on replication lag and report it as a change in the data.",
        ));
    }
    for target in &rule.notify {
        match target.kind {
            tallyowl_control_api::types::NotificationTarget_kind::Webhook => {
                let url = target.url.as_deref().unwrap_or_default();
                if url.is_empty() {
                    return Err(TallyOwlError::invalid_argument(
                        "A webhook notification needs an address to send to.",
                    ));
                }
                if !url.starts_with("https://") && !url.starts_with("http://") {
                    return Err(TallyOwlError::invalid_argument(format!(
                        "`{url}` is not an address TallyOwl can send a webhook to. Write a full address, such as `https://example.test/alerts`."
                    )));
                }
            }
            tallyowl_control_api::types::NotificationTarget_kind::CsilCallback => {
                if target.url.as_deref().unwrap_or_default().is_empty() {
                    return Err(TallyOwlError::invalid_argument(
                        "A native callback needs the address of the service to call.",
                    ));
                }
                if !callbacks_available {
                    return Err(TallyOwlError::new(
                        tallyowl_obs::ErrorCode::FailedPrecondition,
                        "This installation cannot deliver a native callback, so this rule would fire and tell nobody. Use a webhook.".to_string(),
                    ));
                }
            }
        }
    }
    // A rule with no target evaluates and records state and tells nobody, which
    // is a legitimate thing to want while a rule is being tuned. It is worth
    // saying that it is what will happen.
    Ok(())
}

pub fn new_instance(rule_id: &str, project_id: [u8; 16]) -> AlertInstanceRecord {
    AlertInstanceRecord {
        rule_id: rule_id.to_string(),
        project_id,
        state: state_name(&AlertState::Ok).to_string(),
        outcome: outcome_name(&AlertOutcome::Value).to_string(),
        ..AlertInstanceRecord::default()
    }
}

pub fn state_name(state: &AlertState) -> &'static str {
    match state {
        AlertState::Ok => "ok",
        AlertState::Firing => "firing",
        AlertState::NoData => "no-data",
        AlertState::Unknown => "unknown",
        AlertState::Silenced => "silenced",
    }
}

pub fn state_from(name: &str) -> AlertState {
    match name {
        "firing" => AlertState::Firing,
        "no-data" => AlertState::NoData,
        "unknown" => AlertState::Unknown,
        "silenced" => AlertState::Silenced,
        _ => AlertState::Ok,
    }
}

pub fn outcome_name(outcome: &AlertOutcome) -> &'static str {
    match outcome {
        AlertOutcome::Value => "value",
        AlertOutcome::NoData => "no-data",
        AlertOutcome::Error => "error",
    }
}

fn outcome_from(name: &str) -> AlertOutcome {
    match name {
        "no-data" => AlertOutcome::NoData,
        "error" => AlertOutcome::Error,
        _ => AlertOutcome::Value,
    }
}

pub fn to_wire_instance(record: &AlertInstanceRecord) -> AlertInstance {
    AlertInstance {
        rule_id: record.rule_id.clone(),
        state: state_from(&record.state),
        since: record.since,
        observed_value: record.has_value.then_some(record.observed_value),
        reason: (!record.reason.is_empty()).then(|| record.reason.clone()),
        outcome: Some(outcome_from(&record.outcome)),
        last_evaluated_at: Some(record.last_evaluated_at),
        notifications_sent: Some(record.notifications_sent),
        last_notified_at: (record.last_notified_at > 0).then_some(record.last_notified_at),
        holding_ms: Some(record.holding_ms),
        commit_watermark: Some(record.commit_watermark),
    }
}

fn decode_rule(record: &AlertRuleRecord) -> Result<AlertRule, TallyOwlError> {
    decode_alert_rule(&record.encoded).map_err(|e| {
        TallyOwlError::internal(format!(
            "A stored alert rule could not be read, so it did not run. {e}"
        ))
    })
}

/// The encoded form of one rule's query, for a task payload.
pub fn encoded_query(rule: &AlertRule) -> Vec<u8> {
    encode_query_request(&rule.query)
}

/// A target as a person would read it in the operator interface.
pub fn target_name(target: &NotificationTarget) -> String {
    match target.kind {
        tallyowl_control_api::types::NotificationTarget_kind::Webhook => {
            format!("webhook {}", target.url.clone().unwrap_or_default())
        }
        tallyowl_control_api::types::NotificationTarget_kind::CsilCallback => {
            format!("callback {}", target.url.clone().unwrap_or_default())
        }
    }
}

/// Declare every instrument `docs/ALERTS.md` section 8 asks for.
pub fn declare(metrics: &Registry) {
    // **The result is expected at every call site.** L124 records what happened
    // twice when it was not: a name the registry refused vanished, and the
    // missing panel was how somebody found out.
    let declare = |name: &str, kind: tallyowl_obs::MetricKind, help: &str| {
        metrics
            .declare(name, kind, help, &[])
            .unwrap_or_else(|rule| panic!("{}", rule.0));
    };
    declare(
        "tallyowl_alert_evaluations_total",
        tallyowl_obs::MetricKind::Counter,
        "Alert evaluations that finished, by outcome.",
    );
    declare(
        "tallyowl_alert_state_changes_total",
        tallyowl_obs::MetricKind::Counter,
        "Alert state changes, by the state they moved to.",
    );
    declare(
        "tallyowl_alerts_disabled_by_budget_total",
        tallyowl_obs::MetricKind::Counter,
        "Alert rules TallyOwl stopped running because they kept exceeding their evaluation budget.",
    );
    declare(
        "tallyowl_notifications_total",
        tallyowl_obs::MetricKind::Counter,
        "Notification attempts, by channel and outcome.",
    );
    declare(
        "tallyowl_alert_evaluation_delay_ms",
        tallyowl_obs::MetricKind::Gauge,
        "How far behind its schedule the latest alert evaluation ran. A rising value means alerts no longer detect at their configured interval.",
    );
    declare(
        "tallyowl_workflow_pending_count",
        tallyowl_obs::MetricKind::Gauge,
        "Work waiting in a TallyOwl workflow queue.",
    );
    declare(
        "tallyowl_workflow_quarantined_count",
        tallyowl_obs::MetricKind::Gauge,
        "Work a TallyOwl workflow will not retry again and that needs a person.",
    );
    declare(
        "tallyowl_workflow_lag_ms",
        tallyowl_obs::MetricKind::Gauge,
        "How long the oldest waiting item in a TallyOwl workflow queue has waited.",
    );
}

fn store_failure(e: tallyowl_store::catalog::CatalogError) -> TallyOwlError {
    TallyOwlError::internal(e.to_string())
}

/// The request an evaluation runs, for a caller that wants to see it without
/// running it.
pub fn evaluation_request(rule: &AlertRule) -> QueryRequest {
    rule.query.clone()
}
