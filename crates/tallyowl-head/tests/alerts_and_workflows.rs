//! Built-in alerting and the workflow passes, against a real store.
//!
//! `docs/ALERTS.md` section 9 lists nine required tests and each one is here,
//! named for the property rather than for the function it exercises. The three
//! Phase 10 exit criteria are here as well:
//!
//! - repeated evaluations do not duplicate notifications;
//! - retries survive worker restart;
//! - deletion tombstones prevent replay resurrection.
//!
//! `AGENTS.md` forbids mocking the storage interface, so every case writes rows
//! through the real commit path and reads them back through the real executor.
//! The durable queue is another product's boundary and its test double stands
//! in for that, which is a different thing.

use std::sync::Arc;

use tallyowl_control_api::types::{
    AbsenceCondition, AlertRule, AlertState, CompareOp, Consistency, NotificationTarget,
    NotificationTarget_kind as TargetKind, ThresholdCondition, TimeBasis, TimeRange,
};
use tallyowl_head::alerts::AlertService;
use tallyowl_head::expr::DEFAULT_MAX_DEPTH;
use tallyowl_head::notify::{Attempt, Notification};
use tallyowl_head::passes::{AlertRunner, NotificationRunner, ProjectorRunner};
use tallyowl_head::query::QueryService;
use tallyowl_head::workflows::{Kind, Runner, Work, Workflows};
use tallyowl_queue::testing::FakeQueue;
use tallyowl_store::row::EventRow;
use tallyowl_store::{SegmentedStore, Store};
use tallyowl_wire::query;

const PROJECT: [u8; 16] = [9; 16];
const BASE: i64 = 1_785_628_800_000;

fn directory(name: &str) -> std::path::PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("target"));
    let path = base
        .join("alert-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn row(id: u8, name: &str, at: i64) -> EventRow {
    let mut row = EventRow::new([id; 16], "event", name, at);
    row.project_id = PROJECT;
    row.workspace_id = [8; 16];
    row.received_at = at;
    row
}

/// A head with a store, an executor, and an alert service over both.
struct Bed {
    store: Arc<SegmentedStore>,
    query: Arc<QueryService>,
    alerts: Arc<AlertService>,
    queue: Arc<FakeQueue>,
    workflows: Arc<Workflows>,
    metrics: Arc<tallyowl_obs::metrics::Registry>,
}

fn bed(name: &str, rows: Vec<EventRow>) -> Bed {
    let store = Arc::new(SegmentedStore::open(directory(name)).expect("the store opens"));
    if !rows.is_empty() {
        store
            .commit([7; 16], [1; 16], rows)
            .expect("the rows commit");
    }
    let query = Arc::new(QueryService {
        store: Arc::clone(&store) as Arc<dyn Store>,
        max_runtime_ms: 30_000,
        max_expression_depth: DEFAULT_MAX_DEPTH,
        guards: tallyowl_head::analysis::Guards::default(),
        attribution: Default::default(),
        policy: Default::default(),
        identity: Default::default(),
    });
    let metrics = tallyowl_obs::metrics::Registry::new();
    tallyowl_head::alerts::declare(&metrics);
    let alerts = Arc::new(AlertService {
        store: Arc::clone(&store),
        query: Arc::clone(&query),
        metrics: Arc::clone(&metrics),
        callbacks_available: false,
        pool: Default::default(),
    });
    let queue = Arc::new(FakeQueue::default());
    let workflows = Arc::new(Workflows::new(
        Arc::clone(&queue) as Arc<dyn tallyowl_queue::DurableQueue>,
        Arc::clone(&metrics),
        logger(),
        3_600_000,
    ));
    Bed {
        store,
        query,
        alerts,
        queue,
        workflows,
        metrics,
    }
}

/// Enough rows that a one-millisecond deadline cannot be met.
fn many_rows(count: u16) -> Vec<EventRow> {
    (0..count)
        .map(|n| {
            let mut held = row(1, "checkout", BASE + i64::from(n));
            held.event_id[14..16].copy_from_slice(&n.to_be_bytes());
            held
        })
        .collect()
}

fn logger() -> Arc<tallyowl_obs::log::Logger> {
    Arc::new(tallyowl_obs::log::Logger::new(
        "test",
        "0.0.0",
        tallyowl_obs::log::Severity::Error,
    ))
}

/// A rule that counts events in a range and fires above a threshold.
fn counting_rule(id: &str, above: f64) -> AlertRule {
    AlertRule {
        rule_id: id.to_string(),
        name: format!("More than {above} events"),
        project_id: PROJECT.to_vec(),
        query: query::trend(
            1,
            query::events(
                &PROJECT,
                TimeRange {
                    range_start: BASE - 1,
                    range_end: BASE + 1_000_000,
                    basis: TimeBasis::OccurredAt,
                    timezone: None,
                },
            ),
            1_000_000,
            "events",
        ),
        interval_ms: 60_000,
        threshold: Some(ThresholdCondition {
            alias: "events".into(),
            compare: CompareOp::Gt,
            value: above,
            sustained_ms: None,
        }),
        absence: None,
        notify: vec![NotificationTarget {
            kind: TargetKind::Webhook,
            url: Some("http://example.test/alerts".into()),
            secret_ref: None,
        }],
        enabled: true,
        escalate_after_ms: None,
        silenced_until: None,
        silence_reason: None,
        disabled_reason: None,
        updated_at: None,
        updated_by: None,
    }
}

/// A rule whose query cannot fit inside its own budget.
///
/// **The budget is a row count rather than a deadline.** A deadline makes the
/// test a race against the machine it runs on, and one that passes on a slow
/// afternoon and fails on a fast one proves nothing either way. Four thousand
/// rows a millisecond apart, bucketed by the millisecond, is four thousand
/// answer rows against a budget of ten, every time.
fn over_budget_rule(id: &str) -> AlertRule {
    let mut rule = counting_rule(id, 1.0);
    rule.query = query::trend(
        1,
        query::events(
            &PROJECT,
            TimeRange {
                range_start: BASE - 1,
                range_end: BASE + 1_000_000,
                basis: TimeBasis::OccurredAt,
                timezone: None,
            },
        ),
        1,
        "events",
    );
    rule.query.budget = Some(tallyowl_control_api::types::QueryBudget {
        deadline_ms: None,
        max_scanned_bytes: None,
        max_scanned_segments: None,
        max_rows: Some(10),
    });
    rule
}

/// A rule that fires when the data stops.
fn absence_rule(id: &str) -> AlertRule {
    let mut rule = counting_rule(id, 0.0);
    rule.threshold = None;
    rule.absence = Some(AbsenceCondition { for_ms: 0 });
    rule
}

/// Run the evaluation runner once, and say how many notifications it queued.
fn evaluate_once(bed: &Bed, rule: &AlertRule) -> usize {
    let before = bed.queue.depth();
    let runner = AlertRunner {
        alerts: Arc::clone(&bed.alerts),
        workflows: Arc::clone(&bed.workflows),
        logger: logger(),
        metrics: Arc::clone(&bed.metrics),
    };
    let mut work = Work::new(Kind::AlertEvaluation, PROJECT);
    work.rule_id = rule.rule_id.clone();
    runner.run(&work);
    bed.queue.depth().saturating_sub(before)
}

fn state_of(bed: &Bed, rule_id: &str) -> String {
    bed.alerts
        .instance(PROJECT, rule_id)
        .expect("the instance reads")
        .map(|instance| instance.state)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The exit criteria
// ---------------------------------------------------------------------------

#[test]
fn a_repeated_evaluation_in_one_state_sends_one_notification() {
    // **A Phase 10 exit criterion, and `docs/ALERTS.md` section 9 rule 1.**
    // Three events and a threshold of one, evaluated four times. The first
    // evaluation is a change from `ok` to `firing` and sends. The other three
    // find the same state and say nothing.
    let bed = bed(
        "repeated-evaluation",
        (1..=3)
            .map(|n| row(n, "checkout", BASE + n as i64))
            .collect(),
    );
    let rule = bed
        .alerts
        .put_rule(&counting_rule("busy", 1.0), "test")
        .expect("the rule is stored");

    assert_eq!(evaluate_once(&bed, &rule), 1, "the change sent nothing");
    assert_eq!(state_of(&bed, "busy"), "firing");
    for round in 2..=4 {
        assert_eq!(
            evaluate_once(&bed, &rule),
            0,
            "evaluation {round} sent a notification for a state that had not changed"
        );
    }
    let instance = bed
        .alerts
        .instance(PROJECT, "busy")
        .unwrap()
        .expect("an instance");
    assert_eq!(instance.notifications_sent, 1);
    assert!(instance.last_evaluated_at > 0);
}

#[test]
fn a_worker_restart_does_not_resend_a_notification() {
    // **`docs/ALERTS.md` section 9 rule 2.** The state lives in the control
    // catalog rather than in a process, so a head that restarts finds the rule
    // already firing and says nothing. An instance held in memory would make
    // every restart a notification storm.
    let place = directory("restart");
    let rows: Vec<EventRow> = (1..=3)
        .map(|n| row(n, "checkout", BASE + n as i64))
        .collect();

    let first = {
        let store = Arc::new(SegmentedStore::open(&place).expect("opens"));
        store.commit([7; 16], [1; 16], rows).expect("commits");
        drop(store);
        // A fresh process against the same directory.
        let store = Arc::new(SegmentedStore::open(&place).expect("reopens"));
        bed_over(store)
    };
    let rule = first
        .alerts
        .put_rule(&counting_rule("busy", 1.0), "test")
        .expect("stored");
    assert_eq!(evaluate_once(&first, &rule), 1);
    drop(first);

    // The restart. Nothing in memory survives it; the catalog does.
    let second = bed_over(Arc::new(SegmentedStore::open(&place).expect("reopens")));
    let rule = second
        .alerts
        .rules(PROJECT)
        .expect("the rule survived")
        .into_iter()
        .find(|rule| rule.rule_id == "busy")
        .expect("the rule survived the restart");
    assert_eq!(state_of(&second, "busy"), "firing", "the state was lost");
    assert_eq!(
        evaluate_once(&second, &rule),
        0,
        "the restart resent a notification for a state that already fired"
    );
}

fn bed_over(store: Arc<SegmentedStore>) -> Bed {
    let query = Arc::new(QueryService {
        store: Arc::clone(&store) as Arc<dyn Store>,
        max_runtime_ms: 30_000,
        max_expression_depth: DEFAULT_MAX_DEPTH,
        guards: tallyowl_head::analysis::Guards::default(),
        attribution: Default::default(),
        policy: Default::default(),
        identity: Default::default(),
    });
    let metrics = tallyowl_obs::metrics::Registry::new();
    tallyowl_head::alerts::declare(&metrics);
    let alerts = Arc::new(AlertService {
        store: Arc::clone(&store),
        query: Arc::clone(&query),
        metrics: Arc::clone(&metrics),
        callbacks_available: false,
        pool: Default::default(),
    });
    let queue = Arc::new(FakeQueue::default());
    let workflows = Arc::new(Workflows::new(
        Arc::clone(&queue) as Arc<dyn tallyowl_queue::DurableQueue>,
        Arc::clone(&metrics),
        logger(),
        3_600_000,
    ));
    Bed {
        store,
        query,
        alerts,
        queue,
        workflows,
        metrics,
    }
}

#[test]
fn a_deletion_tombstone_stops_a_replay_bringing_the_data_back() {
    // **A Phase 10 exit criterion.** A tombstone is a standing predicate rather
    // than a one-time delete, so a row that arrives *after* the erasure and
    // matches it never becomes visible. That is what makes a replay safe: the
    // batch is committed again, deduplication makes it one logical commit, and
    // the predicate hides it either way.
    let bed = bed(
        "replay-resurrection",
        vec![row(1, "checkout", BASE), row(2, "checkout", BASE + 1)],
    );
    let before = bed
        .store
        .scan(
            PROJECT,
            BASE - 1,
            BASE + 1_000,
            tallyowl_store::TimeBasis::OccurredAt,
        )
        .expect("a scan")
        .rows
        .len();
    assert_eq!(before, 2);

    bed.store
        .erase(&tallyowl_store::catalog::Tombstone {
            tombstone_id: [5; 16],
            generation: 0,
            project_id: PROJECT,
            event_ids: vec![[1; 16]],
            property: None,
            except_kinds: Vec::new(),
            range: None,
            requested_at: BASE,
            horizon: BASE + 365 * 86_400_000,
            reason: "a person asked".into(),
        })
        .expect("the erasure applies");

    // The replay. A different batch identifier, so deduplication does not stop
    // it: this is the case where the same row genuinely arrives again.
    bed.store
        .commit([7; 16], [2; 16], vec![row(1, "checkout", BASE)])
        .expect("the replay commits");

    let after = bed
        .store
        .scan(
            PROJECT,
            BASE - 1,
            BASE + 1_000,
            tallyowl_store::TimeBasis::OccurredAt,
        )
        .expect("a scan")
        .rows;
    assert_eq!(
        after.len(),
        1,
        "an erased row came back through a replay: {:?}",
        after.iter().map(|row| row.event_id[0]).collect::<Vec<_>>()
    );
    assert_eq!(after[0].event_id[0], 2, "the wrong row survived");

    // And the deletion pass re-applies the ledger, which is what a rebuilt
    // catalog needs. Running it changes nothing here and must not lose the
    // predicate.
    let pass = ProjectorRunner {
        store: Arc::clone(&bed.store),
        identity: Arc::clone(&bed.query.identity),
        logger: logger(),
        raw_retention_ms: 0,
        receipt_window_ms: 3_600_000,
        reserve_bytes: 0,
        export_root: directory("exports"),
    };
    let mut work = Work::new(Kind::Deletion, PROJECT);
    work.range_end = BASE + 1_000;
    assert_eq!(
        pass.run(&work),
        tallyowl_head::workflows::Outcome::Done,
        "the deletion pass failed"
    );
    assert_eq!(
        bed.store
            .scan(
                PROJECT,
                BASE - 1,
                BASE + 1_000,
                tallyowl_store::TimeBasis::OccurredAt
            )
            .expect("a scan")
            .rows
            .len(),
        1,
        "re-applying the ledger lost the predicate"
    );
}

// ---------------------------------------------------------------------------
// The rest of `docs/ALERTS.md` section 9
// ---------------------------------------------------------------------------

#[test]
fn ingest_stops_and_the_absence_rule_fires() {
    // Rule 3, and the reason `no-data` is a separate outcome. A threshold on a
    // count never fires when ingest stops, because a dead pipeline returns no
    // rows rather than a low number.
    let quiet = bed("absence", Vec::new());
    let rule = quiet
        .alerts
        .put_rule(&absence_rule("nothing-arriving"), "test")
        .expect("stored");
    assert_eq!(evaluate_once(&quiet, &rule), 1);
    assert_eq!(state_of(&quiet, "nothing-arriving"), "firing");

    // A threshold rule over the same silence does not fire. It has nothing to
    // compare, and saying "the count is below the threshold" would be a value
    // nothing measured.
    let threshold = quiet
        .alerts
        .put_rule(&counting_rule("busy", 100.0), "test")
        .expect("stored");
    evaluate_once(&quiet, &threshold);
    assert_eq!(
        state_of(&quiet, "busy"),
        "no-data",
        "a dead pipeline was reported as a value below the threshold"
    );
}

#[test]
fn a_part_of_the_data_that_could_not_be_read_is_unknown_and_not_a_metric_drop() {
    // Rule 4. A missing tablet must not look like a metric drop. The store
    // reports an incomplete answer, the evaluation is `error`, and the rule
    // enters `unknown` — which tells an operator that TallyOwl could not
    // answer, a different fact from a value that crossed a threshold.
    let bed = bed(
        "unknown",
        (1..=3)
            .map(|n| row(n, "checkout", BASE + n as i64))
            .collect(),
    );
    let rule = bed
        .alerts
        .put_rule(&counting_rule("busy", 1.0), "test")
        .expect("stored");
    evaluate_once(&bed, &rule);
    assert_eq!(state_of(&bed, "busy"), "firing");

    // Damage one segment, so the next scan cannot read all of it. **One
    // process owns one data directory**, so the first store has to be closed
    // before the second opens it.
    bed.store.seal_now().expect("a seal");
    let place = bed.store.directory().to_path_buf();
    damage_a_segment(&place);
    drop(bed);
    let reopened = bed_over(Arc::new(
        SegmentedStore::open_waiting(&place, std::time::Duration::from_secs(5)).expect("reopens"),
    ));
    let rule = reopened
        .alerts
        .rules(PROJECT)
        .unwrap()
        .into_iter()
        .find(|r| r.rule_id == "busy")
        .expect("the rule");
    let sent = evaluate_once(&reopened, &rule);
    assert_eq!(
        state_of(&reopened, "busy"),
        "unknown",
        "damaged data was answered as a value"
    );
    assert_eq!(
        sent, 1,
        "a move into `unknown` is a state change and is told"
    );
    let instance = reopened.alerts.instance(PROJECT, "busy").unwrap().unwrap();
    assert!(!instance.has_value, "an `unknown` state carried a value");
    assert_eq!(instance.outcome, "error");
}

fn damage_a_segment(place: &std::path::Path) {
    let segments = place.join("segments");
    let path = std::fs::read_dir(&segments)
        .expect("segments")
        .next()
        .expect("a segment")
        .expect("a segment")
        .path();
    let mut bytes = std::fs::read(&path).expect("read");
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    std::fs::write(&path, &bytes).expect("write");
}

#[test]
fn an_alert_that_asks_for_a_stale_read_is_refused_when_it_is_written() {
    // Rule 5. A stale read can answer from a replica that lags, and the alert
    // would fire on replication lag and report it as a change in the data.
    // Refusing when the rule is written means an operator finds out while they
    // are looking at it rather than at three in the morning.
    let bed = bed("stale", Vec::new());
    let mut rule = counting_rule("stale", 1.0);
    rule.query.consistency = Consistency::BoundedStale;
    let failure = bed
        .alerts
        .put_rule(&rule, "test")
        .expect_err("a stale alert is refused");
    assert_eq!(failure.code, tallyowl_obs::ErrorCode::InvalidArgument);
    assert!(
        failure.message.contains("replication lag"),
        "{}",
        failure.message
    );
}

#[test]
fn an_evaluation_that_runs_out_of_budget_is_an_error_and_never_a_false_ok() {
    // Rule 6. A budget failure that answered `ok` would be an alert that goes
    // quiet exactly when the system is slowest.
    let bed = bed("budget", many_rows(4_000));
    let rule = bed
        .alerts
        .put_rule(&over_budget_rule("busy"), "test")
        .expect("stored");
    evaluate_once(&bed, &rule);
    let state = state_of(&bed, "busy");
    assert_ne!(state, "ok", "a budget failure answered `ok`");
    assert_eq!(state, "unknown");
    let instance = bed.alerts.instance(PROJECT, "busy").unwrap().unwrap();
    assert_eq!(instance.outcome, "error");
    assert!(!instance.reason.is_empty(), "it did not say why");
}

#[test]
fn a_silenced_rule_records_its_state_and_sends_nothing() {
    // Rule 7. An operator who silenced a rule still needs to see what it did
    // while it was quiet, so the evaluation runs and the state is written.
    let bed = bed(
        "silence",
        (1..=3)
            .map(|n| row(n, "checkout", BASE + n as i64))
            .collect(),
    );
    let rule = bed
        .alerts
        .put_rule(&counting_rule("busy", 1.0), "test")
        .expect("stored");
    bed.alerts
        .silence(
            PROJECT,
            &rule.rule_id,
            tallyowl_obs::time::now_ms() + 3_600_000,
            "we know",
        )
        .expect("it silences");

    let rule = bed
        .alerts
        .rules(PROJECT)
        .unwrap()
        .into_iter()
        .find(|r| r.rule_id == "busy")
        .unwrap();
    assert_eq!(evaluate_once(&bed, &rule), 0, "a silenced rule notified");
    let instance = bed.alerts.instance(PROJECT, "busy").unwrap().unwrap();
    assert_eq!(instance.state, "silenced");
    assert!(
        instance.last_evaluated_at > 0,
        "a silenced rule did not evaluate, so nothing was recorded"
    );

    // And a resolve clears the silence, so the next occurrence is not hidden.
    bed.alerts
        .resolve(PROJECT, "busy", "it is over")
        .expect("it resolves");
    let rule = bed
        .alerts
        .rules(PROJECT)
        .unwrap()
        .into_iter()
        .find(|r| r.rule_id == "busy")
        .unwrap();
    assert_eq!(rule.silenced_until, None);
    assert_eq!(state_of(&bed, "busy"), "ok");
    assert_eq!(
        evaluate_once(&bed, &rule),
        1,
        "the rule stayed quiet after it was resolved"
    );
}

#[test]
fn a_webhook_that_fails_is_retried_and_the_alert_state_does_not_move() {
    // Rule 8, and the reason: the state is what the data said, and whether a
    // receiver answered is a fact about the network. A delivery failure that
    // cleared a firing alert would turn an outage in the receiver into an
    // all-clear.
    let bed = bed(
        "webhook-failure",
        (1..=3)
            .map(|n| row(n, "checkout", BASE + n as i64))
            .collect(),
    );
    let rule = bed
        .alerts
        .put_rule(&counting_rule("busy", 1.0), "test")
        .expect("stored");
    assert_eq!(evaluate_once(&bed, &rule), 1);
    assert_eq!(state_of(&bed, "busy"), "firing");

    // The notification is queued; the receiver refuses. The address in the
    // fixture points at nothing, so the attempt fails for a reason a retry
    // could fix.
    let runner = NotificationRunner {
        alerts: Arc::clone(&bed.alerts),
        store: Arc::clone(&bed.store),
        metrics: Arc::clone(&bed.metrics),
        timeout: std::time::Duration::from_millis(200),
        secrets: Arc::new(|_| None),
        callbacks: None,
    };
    for _ in 0..3 {
        bed.workflows
            .run_one(tallyowl_head::workflows::NOTIFICATION_QUEUE, &runner)
            .expect("it runs");
        bed.queue.release_parked();
    }

    assert_eq!(
        state_of(&bed, "busy"),
        "firing",
        "a delivery failure moved the alert state"
    );
    let attempts = bed.store.catalog().notifications().expect("attempts");
    assert!(
        !attempts.is_empty(),
        "no attempt was recorded for an operator"
    );
    assert!(
        attempts.iter().all(|attempt| !attempt.delivered),
        "an attempt against nothing reported delivery"
    );
    assert!(
        attempts.iter().any(|attempt| attempt.next_attempt_at > 0),
        "a retryable failure named no next attempt"
    );
}

#[test]
fn an_alert_value_is_the_value_the_same_query_gives_a_person() {
    // Rule 9, and the property `docs/ALERTS.md` section 2 says matters more
    // than any other. The alert runs the same request through the same
    // executor, so the two cannot disagree — and a test that only compared two
    // calls to one function would prove nothing about that, so this compares
    // the alert's own evaluation against the answer a dashboard reads.
    let bed = bed(
        "same-value",
        (1..=7)
            .map(|n| row(n, "checkout", BASE + n as i64))
            .collect(),
    );
    let rule = counting_rule("busy", 1.0);
    let evaluation = bed.alerts.evaluate(&rule);

    let response = bed
        .query
        .run(rule.query.clone())
        .expect("the dashboard asks");
    let at = response
        .columns
        .iter()
        .position(|column| column == "events")
        .expect("the measure");
    let from_dashboard = response.rows[0].values[at]
        .uint_value
        .map(|value| value as f64)
        .or(response.rows[0].values[at].float_value)
        .expect("a number");

    assert_eq!(evaluation.value, Some(from_dashboard));
    assert_eq!(from_dashboard, 7.0, "seven events went in");
}

// ---------------------------------------------------------------------------
// The state machine's own edges
// ---------------------------------------------------------------------------

#[test]
fn a_threshold_that_must_be_sustained_does_not_fire_on_one_spike() {
    // A rule with `sustained_ms` fires only after the condition has held that
    // long, so a single evaluation over the line does not wake somebody up.
    let bed = bed(
        "sustained",
        (1..=3)
            .map(|n| row(n, "checkout", BASE + n as i64))
            .collect(),
    );
    let mut rule = counting_rule("busy", 1.0);
    rule.threshold.as_mut().unwrap().sustained_ms = Some(3_600_000);
    let rule = bed.alerts.put_rule(&rule, "test").expect("stored");

    assert_eq!(evaluate_once(&bed, &rule), 0, "one spike fired");
    assert_eq!(state_of(&bed, "busy"), "ok");
}

#[test]
fn a_rule_that_keeps_running_out_of_budget_is_disabled_and_says_so() {
    // `docs/ALERTS.md` section 7: TallyOwl disables and reports an alert that
    // repeatedly exceeds its budget, so a rule that is simply too expensive
    // stops competing with a person who is looking at a screen.
    let bed = bed("disable", many_rows(4_000));
    let mut rule = bed
        .alerts
        .put_rule(&over_budget_rule("expensive"), "test")
        .expect("stored");
    for _ in 0..tallyowl_head::alerts::BUDGET_FAILURES_BEFORE_DISABLING {
        bed.alerts.evaluate_and_record(&rule).expect("it evaluates");
        rule = bed
            .alerts
            .rules(PROJECT)
            .unwrap()
            .into_iter()
            .find(|r| r.rule_id == "expensive")
            .unwrap();
    }
    assert!(!rule.enabled, "a rule that never finishes kept running");
    let reason = rule.disabled_reason.expect("it says why");
    assert!(reason.contains("budget"), "{reason}");
}

#[test]
fn a_notification_for_a_state_that_has_since_changed_is_dropped() {
    // It waited in a queue while the alert recovered. Sending it now would tell
    // somebody a firing alert is firing when it stopped ten minutes ago, and
    // the recovery queued its own notification.
    let bed = bed(
        "stale-notification",
        (1..=3)
            .map(|n| row(n, "checkout", BASE + n as i64))
            .collect(),
    );
    let rule = bed
        .alerts
        .put_rule(&counting_rule("busy", 1.0), "test")
        .expect("stored");
    evaluate_once(&bed, &rule);
    // The operator resolved it while the notification was still queued.
    bed.alerts
        .resolve(PROJECT, "busy", "handled")
        .expect("resolved");

    let runner = NotificationRunner {
        alerts: Arc::clone(&bed.alerts),
        store: Arc::clone(&bed.store),
        metrics: Arc::clone(&bed.metrics),
        timeout: std::time::Duration::from_millis(200),
        secrets: Arc::new(|_| None),
        callbacks: None,
    };
    bed.workflows
        .run_one(tallyowl_head::workflows::NOTIFICATION_QUEUE, &runner)
        .expect("it runs");
    assert!(
        bed.store
            .catalog()
            .notifications()
            .expect("attempts")
            .is_empty(),
        "a notification was sent for a state that had already changed"
    );
}

// ---------------------------------------------------------------------------
// The projector passes
// ---------------------------------------------------------------------------

#[test]
fn a_rebuild_pass_forgets_what_is_derived_and_keeps_what_is_stored() {
    let bed = bed(
        "rebuild",
        (1..=5)
            .map(|n| row(n, "checkout", BASE + n as i64))
            .collect(),
    );
    bed.store.seal_now().expect("a seal");
    let pass = ProjectorRunner {
        store: Arc::clone(&bed.store),
        identity: Arc::clone(&bed.query.identity),
        logger: logger(),
        raw_retention_ms: 0,
        receipt_window_ms: 3_600_000,
        reserve_bytes: 0,
        export_root: directory("exports"),
    };
    assert_eq!(
        pass.run(&Work::new(Kind::ProjectorRebuild, PROJECT)),
        tallyowl_head::workflows::Outcome::Done
    );
    assert_eq!(
        bed.store
            .scan(
                PROJECT,
                BASE - 1,
                BASE + 1_000,
                tallyowl_store::TimeBasis::OccurredAt
            )
            .expect("a scan")
            .rows
            .len(),
        5,
        "a rebuild lost the rows it is supposed to rebuild from"
    );
}

#[test]
fn an_export_pass_writes_a_file_a_person_can_take_away() {
    let bed = bed(
        "export",
        (1..=4)
            .map(|n| row(n, "checkout", BASE + n as i64))
            .collect(),
    );
    let into = directory("export-out");
    let pass = ProjectorRunner {
        store: Arc::clone(&bed.store),
        identity: Arc::clone(&bed.query.identity),
        logger: logger(),
        raw_retention_ms: 0,
        receipt_window_ms: 3_600_000,
        reserve_bytes: 0,
        export_root: into.clone(),
    };
    let mut work = Work::new(Kind::Export, PROJECT);
    work.range_start = BASE - 1;
    work.range_end = BASE + 1_000;
    assert_eq!(
        pass.run(&work),
        tallyowl_head::workflows::Outcome::Done,
        "the export failed"
    );
    let written: Vec<_> = std::fs::read_dir(&into)
        .expect("the export directory")
        .flatten()
        .map(|entry| entry.path())
        .collect();
    assert!(
        written
            .iter()
            .any(|path| path.extension().is_some_and(|e| e == "parquet")),
        "no Parquet file was written: {written:?}"
    );
}

#[test]
fn a_retention_pass_removes_what_is_past_the_window_and_keeps_the_rest() {
    let now = tallyowl_obs::time::now_ms();
    let bed = bed(
        "retention",
        vec![
            row(1, "checkout", now - 90 * 86_400_000),
            row(2, "checkout", now - 1_000),
        ],
    );
    let pass = ProjectorRunner {
        store: Arc::clone(&bed.store),
        identity: Arc::clone(&bed.query.identity),
        logger: logger(),
        // Thirty days.
        raw_retention_ms: 30 * 86_400_000,
        receipt_window_ms: 3_600_000,
        reserve_bytes: 0,
        export_root: directory("exports"),
    };
    assert_eq!(
        pass.run(&Work::new(Kind::Retention, PROJECT)),
        tallyowl_head::workflows::Outcome::Done
    );
    let kept = bed
        .store
        .scan(
            PROJECT,
            now - 365 * 86_400_000,
            now + 1_000,
            tallyowl_store::TimeBasis::OccurredAt,
        )
        .expect("a scan")
        .rows;
    assert_eq!(kept.len(), 1, "retention removed the wrong rows");
    assert_eq!(kept[0].event_id[0], 2, "it kept the old one");
}

#[test]
fn a_notification_body_never_carries_a_row() {
    // The rule that matters most about a webhook: it goes to a system TallyOwl
    // does not own, over a network it does not own, and a row can hold anything
    // an application put in it.
    let notification = Notification {
        rule_id: "busy".into(),
        rule_name: "Busy".into(),
        project_id: "0909".into(),
        state: "firing".into(),
        outcome: "value".into(),
        observed_value: Some(7.0),
        evaluated_at: BASE,
        query_link: "/projects/0909/alerts/busy".into(),
        reason: String::new(),
        escalation: false,
    };
    let body = notification.body();
    assert!(body.contains("\"value\":7"));
    assert!(
        !body.contains("checkout"),
        "an event name reached a webhook"
    );
}

#[test]
fn a_failed_attempt_says_whether_asking_again_could_help() {
    // A refused address never becomes a good one, and retrying it for a day
    // fills a queue with work that cannot succeed and hides the real failures
    // behind it.
    let permanent = Attempt::failed("the receiver refused this", false);
    assert!(!permanent.retryable);
    let temporary = Attempt::failed("the receiver did not answer", true);
    assert!(temporary.retryable);
}

#[test]
fn a_rule_with_no_condition_is_refused_rather_than_never_firing() {
    let bed = bed("no-condition", Vec::new());
    let mut rule = counting_rule("nothing", 1.0);
    rule.threshold = None;
    let failure = bed.alerts.put_rule(&rule, "test").expect_err("refused");
    assert!(
        failure.message.contains("no condition"),
        "{}",
        failure.message
    );
}

#[test]
fn a_rule_that_asks_to_evaluate_faster_than_the_floor_is_refused() {
    // One thousand rules on a one-minute interval is about seventeen
    // evaluations each second, and each one is a full query.
    let bed = bed("too-fast", Vec::new());
    let mut rule = counting_rule("busy", 1.0);
    rule.interval_ms = 10;
    let failure = bed.alerts.put_rule(&rule, "test").expect_err("refused");
    assert!(
        failure.message.contains("shortest interval"),
        "{}",
        failure.message
    );
}

#[test]
fn a_removed_rule_takes_its_state_with_it() {
    let bed = bed(
        "removal",
        (1..=3)
            .map(|n| row(n, "checkout", BASE + n as i64))
            .collect(),
    );
    let rule = bed
        .alerts
        .put_rule(&counting_rule("busy", 1.0), "test")
        .expect("stored");
    evaluate_once(&bed, &rule);
    assert_eq!(state_of(&bed, "busy"), "firing");

    bed.alerts.remove_rule(PROJECT, "busy").expect("removed");
    assert!(bed.alerts.rules(PROJECT).unwrap().is_empty());
    assert_eq!(
        state_of(&bed, "busy"),
        "",
        "a removed rule left its state behind, so recreating it would look already-firing"
    );
}

#[test]
fn the_alert_state_names_every_state_the_document_does() {
    // A state a person reads in the interface and a state the code can produce
    // have to be the same list. A state that existed in one and not the other
    // would be a screen nobody could explain.
    for state in [
        AlertState::Ok,
        AlertState::Firing,
        AlertState::NoData,
        AlertState::Unknown,
        AlertState::Silenced,
    ] {
        let name = tallyowl_head::alerts::state_name(&state);
        assert_eq!(
            tallyowl_head::alerts::state_from(name),
            state,
            "`{name}` does not read back as the state it names"
        );
    }
}

// ---------------------------------------------------------------------------
// The scheduler
// ---------------------------------------------------------------------------

#[test]
fn a_rule_becomes_due_rather_than_always_being_about_to() {
    // **The running loop found this and no test did.** The stagger used to be
    // measured from `now`, so every tick moved the deadline forward by the same
    // amount and a rule was permanently a few seconds away from its first
    // evaluation. A rule was written, the scheduler ran for twenty seconds, and
    // nothing ever evaluated.
    //
    // Every earlier test called the runner directly with a piece of work it
    // built itself, which is why the whole suite passed against it. This drives
    // the scheduler over a clock instead.
    let bed = bed(
        "scheduling",
        (1..=3)
            .map(|n| row(n, "checkout", BASE + n as i64))
            .collect(),
    );
    bed.alerts
        .put_rule(&counting_rule("busy", 1.0), "test")
        .expect("stored");
    let scheduler = tallyowl_head::passes::Scheduler::new(
        Arc::clone(&bed.alerts),
        Arc::clone(&bed.workflows),
        logger(),
    );

    let start = tallyowl_obs::time::now_ms();
    // Ticks over the first interval. The stagger is somewhere inside it, so at
    // least one of these must queue the evaluation.
    let mut queued = 0;
    for step in 0..=12 {
        queued += scheduler
            .tick(start + step * 5_000)
            .expect("the scheduler ticks");
    }
    assert!(
        queued > 0,
        "the rule never became due, so nothing ever evaluated"
    );

    // And it is queued on its interval rather than on every tick. Twelve ticks
    // across one minute, with a sixty-second interval, is one or two
    // evaluations and never twelve.
    assert!(
        queued <= 2,
        "the rule was queued {queued} times in one interval"
    );
}

#[test]
fn a_scheduler_that_sees_no_rule_queues_nothing() {
    let bed = bed("scheduling-empty", Vec::new());
    let scheduler = tallyowl_head::passes::Scheduler::new(
        Arc::clone(&bed.alerts),
        Arc::clone(&bed.workflows),
        logger(),
    );
    assert_eq!(scheduler.tick(tallyowl_obs::time::now_ms()).unwrap(), 0);
}

#[test]
fn a_disabled_rule_is_not_scheduled() {
    let bed = bed("scheduling-disabled", Vec::new());
    let mut rule = counting_rule("off", 1.0);
    rule.enabled = false;
    bed.alerts.put_rule(&rule, "test").expect("stored");
    let scheduler = tallyowl_head::passes::Scheduler::new(
        Arc::clone(&bed.alerts),
        Arc::clone(&bed.workflows),
        logger(),
    );
    let start = tallyowl_obs::time::now_ms();
    let queued: usize = (0..=12)
        .map(|step| scheduler.tick(start + step * 5_000).unwrap())
        .sum();
    assert_eq!(queued, 0, "a disabled rule was evaluated");
}

/// A bed whose head can deliver a native callback.
fn bed_with_callbacks(name: &str, rows: Vec<EventRow>) -> Bed {
    let mut held = bed(name, rows);
    held.alerts = Arc::new(AlertService {
        store: Arc::clone(&held.store),
        query: Arc::clone(&held.query),
        metrics: Arc::clone(&held.metrics),
        callbacks_available: true,
        pool: Default::default(),
    });
    held
}

#[test]
fn a_native_callback_reaches_a_service_that_answers_it() {
    // **The native channel, end to end over a real socket.** A service that
    // already speaks CSIL-RPC declares one operation and takes the same
    // notification a webhook receives, without an HTTP endpoint, a signature
    // check, and a JSON parser.
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Receiver {
        taken: Arc<AtomicUsize>,
        body: std::sync::Mutex<Vec<u8>>,
    }
    impl tallyowl_rpc::Dispatcher for Receiver {
        fn dispatch(&self, request: &tallyowl_rpc::Request) -> tallyowl_rpc::Outcome {
            if request.op != tallyowl_head::notify::CALLBACK_OPERATION {
                return tallyowl_rpc::unknown_operation(&request.service, &request.op);
            }
            self.taken.fetch_add(1, Ordering::Relaxed);
            *self.body.lock().unwrap() = request.payload.clone();
            tallyowl_rpc::reply("Empty", Vec::new())
        }
    }

    let taken = Arc::new(AtomicUsize::new(0));
    let receiver = Arc::new(Receiver {
        taken: Arc::clone(&taken),
        body: std::sync::Mutex::new(Vec::new()),
    });
    let server = tallyowl_rpc::serve(
        "127.0.0.1:0",
        Arc::clone(&receiver) as Arc<dyn tallyowl_rpc::Dispatcher>,
        1024 * 1024,
    )
    .expect("the receiver listens");

    let bed = bed_with_callbacks(
        "callback",
        (1..=3)
            .map(|n| row(n, "checkout", BASE + n as i64))
            .collect(),
    );
    let mut rule = counting_rule("busy", 1.0);
    rule.notify = vec![NotificationTarget {
        kind: TargetKind::CsilCallback,
        url: Some(server.local_address().to_string()),
        secret_ref: None,
    }];
    let rule = bed.alerts.put_rule(&rule, "test").expect("stored");
    assert_eq!(evaluate_once(&bed, &rule), 1);

    let runner = NotificationRunner {
        alerts: Arc::clone(&bed.alerts),
        store: Arc::clone(&bed.store),
        metrics: Arc::clone(&bed.metrics),
        timeout: std::time::Duration::from_secs(5),
        secrets: Arc::new(|_| None),
        callbacks: Some(Arc::new(tallyowl_head::notify::RpcCallbacks {
            max_frame_bytes: 1024 * 1024,
        })),
    };
    bed.workflows
        .run_one(tallyowl_head::workflows::NOTIFICATION_QUEUE, &runner)
        .expect("it runs");

    assert_eq!(
        taken.load(Ordering::Relaxed),
        1,
        "the receiver was not called"
    );
    let body = String::from_utf8(receiver.body.lock().unwrap().clone()).expect("text");
    assert!(body.contains("\"state\":\"firing\""), "{body}");
    // The same rule as the webhook: a notification never carries a row.
    assert!(
        !body.contains("checkout"),
        "an event name reached a callback"
    );

    let attempts = bed.store.catalog().notifications().expect("attempts");
    assert!(
        attempts.iter().all(|attempt| attempt.delivered),
        "a delivery that worked was recorded as a failure"
    );
}

#[test]
fn a_callback_to_a_service_that_does_not_answer_is_recorded_and_retried() {
    let bed = bed_with_callbacks(
        "callback-down",
        (1..=3)
            .map(|n| row(n, "checkout", BASE + n as i64))
            .collect(),
    );
    let mut rule = counting_rule("busy", 1.0);
    rule.notify = vec![NotificationTarget {
        kind: TargetKind::CsilCallback,
        // Nothing is listening here.
        url: Some("127.0.0.1:1".to_string()),
        secret_ref: None,
    }];
    let rule = bed.alerts.put_rule(&rule, "test").expect("stored");
    evaluate_once(&bed, &rule);

    let runner = NotificationRunner {
        alerts: Arc::clone(&bed.alerts),
        store: Arc::clone(&bed.store),
        metrics: Arc::clone(&bed.metrics),
        timeout: std::time::Duration::from_millis(200),
        secrets: Arc::new(|_| None),
        callbacks: Some(Arc::new(tallyowl_head::notify::RpcCallbacks {
            max_frame_bytes: 1024 * 1024,
        })),
    };
    bed.workflows
        .run_one(tallyowl_head::workflows::NOTIFICATION_QUEUE, &runner)
        .expect("it runs");

    assert_eq!(
        state_of(&bed, "busy"),
        "firing",
        "a delivery failure moved the alert state"
    );
    let attempts = bed.store.catalog().notifications().expect("attempts");
    assert!(!attempts.is_empty(), "nothing was recorded for an operator");
    assert!(attempts.iter().all(|attempt| !attempt.delivered));
}

#[test]
fn a_rule_that_this_installation_could_never_deliver_is_refused_when_it_is_written() {
    // A `csil-callback` target on an installation with no callback transport
    // is a rule an operator saves, watches fire, and never hears about — and
    // the reason would be in a delivery record they had no cause to look at.
    // The refusal reaches them where they are looking, which is the same rule
    // as the one about a stale read.
    let bed = bed("no-callback", Vec::new());
    let mut rule = counting_rule("busy", 1.0);
    rule.notify = vec![NotificationTarget {
        kind: TargetKind::CsilCallback,
        url: Some("127.0.0.1:5300".into()),
        secret_ref: None,
    }];
    let failure = bed.alerts.put_rule(&rule, "test").expect_err("refused");
    assert_eq!(failure.code, tallyowl_obs::ErrorCode::FailedPrecondition);
    assert!(
        failure.message.contains("tell nobody"),
        "{}",
        failure.message
    );
}

// ---------------------------------------------------------------------------
// The evaluation budget pool
// ---------------------------------------------------------------------------

#[test]
fn a_full_pool_puts_the_evaluation_back_rather_than_marking_the_rule_unknown() {
    // **A full pool is not a failed evaluation.** A rule that went `unknown`
    // whenever the installation was busy would go quiet exactly when somebody
    // needed it, and an operator would learn to ignore the state.
    let bed = bed(
        "pool-full",
        (1..=3)
            .map(|n| row(n, "checkout", BASE + n as i64))
            .collect(),
    );
    let rule = bed
        .alerts
        .put_rule(&counting_rule("busy", 1.0), "test")
        .expect("stored");
    evaluate_once(&bed, &rule);
    assert_eq!(state_of(&bed, "busy"), "firing");

    // Somebody else holds the only permit.
    let held = bed
        .alerts
        .pool
        .take(std::time::Duration::from_millis(10))
        .expect("the pool has a permit");

    let runner = AlertRunner {
        alerts: Arc::clone(&bed.alerts),
        workflows: Arc::clone(&bed.workflows),
        logger: logger(),
        metrics: Arc::clone(&bed.metrics),
    };
    let mut work = Work::new(Kind::AlertEvaluation, PROJECT);
    work.rule_id = "busy".into();
    let outcome = runner.run(&work);

    assert!(
        matches!(outcome, tallyowl_head::workflows::Outcome::Retry(_)),
        "a full pool did not put the work back: {outcome:?}"
    );
    assert_eq!(
        state_of(&bed, "busy"),
        "firing",
        "a full pool changed the rule's state"
    );
    drop(held);
}

#[test]
fn a_permit_comes_back_when_it_is_dropped() {
    // A pool that leaked a permit would shrink to nothing over an installation's
    // life and stop evaluating anything, slowly.
    let pool = tallyowl_head::alerts::BudgetPool::new(1);
    for _ in 0..5 {
        let permit = pool
            .take(std::time::Duration::from_millis(10))
            .expect("a permit");
        drop(permit);
    }
    assert!(pool.take(std::time::Duration::from_millis(10)).is_some());
}

#[test]
fn a_pool_of_two_lets_two_run_and_holds_the_third() {
    let pool = tallyowl_head::alerts::BudgetPool::new(2);
    assert_eq!(pool.capacity(), 2);
    let one = pool
        .take(std::time::Duration::from_millis(10))
        .expect("one");
    let two = pool
        .take(std::time::Duration::from_millis(10))
        .expect("two");
    assert!(
        pool.take(std::time::Duration::from_millis(10)).is_none(),
        "a pool of two handed out three permits"
    );
    drop(one);
    assert!(pool.take(std::time::Duration::from_millis(50)).is_some());
    drop(two);
}

#[test]
fn a_pool_of_nothing_is_a_pool_of_one_rather_than_a_head_that_never_alerts() {
    // A configuration mistake must not be the reason nothing is ever evaluated.
    let pool = tallyowl_head::alerts::BudgetPool::new(0);
    assert_eq!(pool.capacity(), 1);
    assert!(pool.take(std::time::Duration::from_millis(10)).is_some());
}
