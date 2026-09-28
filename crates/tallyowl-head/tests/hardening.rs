//! Failure paths a hostile or broken producer reaches through the head's data
//! plane. Each one was a way to stop the head or to read across a project
//! boundary, and each test fails without its fix.

use std::path::PathBuf;
use std::sync::Arc;

use tallyowl_collector_api::types::{
    Batch, CommitBatchRequest, ConversionPayload, CsilDecimal, Envelope, EventPayload,
    ReceiptPolicy, TelemetryItem, TelemetryKind,
};
use tallyowl_head::ingest::Ingest;
use tallyowl_obs::metrics::Registry;
use tallyowl_store::{SegmentedStore, Store, TimeBasis};
use tallyowl_wire::{collector as wire, collector_items_bridge as items, Value};

const PROJECT: [u8; 16] = [9; 16];
const WORKSPACE: [u8; 16] = [8; 16];
const SOURCE: [u8; 16] = [7; 16];
const BASE_TIME: i64 = 1_785_628_800_000;

fn temporary_directory(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("head-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn envelope(id: u8, kind: TelemetryKind) -> Envelope {
    Envelope {
        event_id: vec![id; 16],
        kind,
        schema_version: 1,
        occurred_at: BASE_TIME,
        observed_at: None,
        received_at: Some(BASE_TIME + 5),
        workspace_id: Some(WORKSPACE.to_vec()),
        project_id: Some(PROJECT.to_vec()),
        source_id: Some(SOURCE.to_vec()),
        sequence: None,
        release: None,
        service_name: None,
        request_id: None,
        session_id: None,
        end_user_id: None,
        anonymous_id: None,
        trace_id: None,
        span_id: None,
        consent: None,
        sdk_name: "tallyowl-driver-rust".into(),
        sdk_version: "0.0.0".into(),
        properties: Vec::new(),
        measurements: None,
    }
}

fn event(id: u8, name: &str) -> TelemetryItem {
    items::event(
        envelope(id, TelemetryKind::Event),
        EventPayload {
            name: name.into(),
            route: None,
            page_title: None,
        },
    )
}

fn ingest(name: &str) -> (Ingest, Arc<dyn Store>) {
    let store: Arc<dyn Store> =
        Arc::new(SegmentedStore::open(temporary_directory(name)).expect("open"));
    let metrics = Registry::new();
    Ingest::declare_metrics(&metrics);
    (
        Ingest {
            golden_signal_bucket_ms: 60_000,
            store: Arc::clone(&store),
            metrics,
            receipt_policy: ReceiptPolicy::LocalOne,
            open_traces: None,
            policy: None,
            sources: None,
        },
        store,
    )
}

fn request(batch_id: u8, items: Vec<TelemetryItem>) -> CommitBatchRequest {
    CommitBatchRequest {
        batch: Batch {
            batch_id: vec![batch_id; 16],
            items,
            common_properties: None,
            sealed_at: BASE_TIME,
            compression: None,
        },
        source_id: SOURCE.to_vec(),
        attempt: None,
        protocol_version: None,
    }
}

#[test]
fn one_item_with_an_exponent_of_a_trillion_is_one_rejected_item() {
    // A few bytes on the wire. Before the bound, the projection wrote a
    // trillion zeros, the head stopped, and the forwarder delivered the same
    // durable batch again after every restart.
    let (ingest, store) = ingest("decimal-exponent");

    let mut as_property = event(1, "checkout");
    as_property.envelope.properties.push(wire::property(
        "total",
        Value::Decimal {
            exponent: 1_000_000_000_000,
            mantissa: 1,
        },
        tallyowl_collector_api::types::PropertyOrigin::Client,
    ));

    let mut as_measurement = event(2, "checkout");
    as_measurement.envelope.measurements = Some(vec![wire::measurement(
        "weight",
        Value::Decimal {
            exponent: i64::MIN,
            mantissa: 1,
        },
        None,
    )
    .unwrap()]);

    let as_conversion = items::conversion(
        envelope(3, TelemetryKind::Conversion),
        ConversionPayload {
            goal: "purchase".into(),
            value: Some(CsilDecimal {
                exponent: i64::MAX,
                mantissa: 1,
            }),
            currency: Some("USD".into()),
            order_id: None,
            campaign: None,
            touch_event_id: None,
        },
    );

    let receipt = ingest
        .commit(request(
            1,
            vec![as_property, as_measurement, as_conversion, event(4, "kept")],
        ))
        .expect("the batch commits");

    assert_eq!(receipt.accepted, 1, "the good item commits");
    let rejected = receipt.rejected.expect("three items are named");
    assert_eq!(rejected.len(), 3);
    for item in &rejected {
        assert!(item.message.contains("exponent"), "{}", item.message);
    }
    let scanned = store
        .scan(PROJECT, i64::MIN, i64::MAX, TimeBasis::OccurredAt)
        .expect("the store reads");
    assert_eq!(scanned.rows.len(), 1);
    assert_eq!(scanned.rows[0].name, "kept");
}

// ---------------------------------------------------------------------------
// Alerts: the project boundary, the tree depth, and the sustain window
// ---------------------------------------------------------------------------

use tallyowl_control_api::types::{
    AbsenceCondition, AlertOutcome, AlertRule, CompareOp, NotificationTarget,
    NotificationTarget_kind as TargetKind, ThresholdCondition, TimeBasis as WireBasis, TimeRange,
};
use tallyowl_head::alerts::{decide, AlertService, Evaluation};
use tallyowl_head::query::QueryService;
use tallyowl_wire::query;

const OTHER_PROJECT: [u8; 16] = [0xB; 16];

fn whole_range() -> TimeRange {
    TimeRange {
        range_start: BASE_TIME - 1,
        range_end: BASE_TIME + 1_000_000,
        basis: WireBasis::OccurredAt,
        timezone: None,
    }
}

fn counting_rule(id: &str, reads: &[u8; 16]) -> AlertRule {
    AlertRule {
        rule_id: id.to_string(),
        name: "More than one event".into(),
        project_id: PROJECT.to_vec(),
        query: query::trend(1, query::events(reads, whole_range()), 1_000_000, "events"),
        interval_ms: 60_000,
        threshold: Some(ThresholdCondition {
            alias: "events".into(),
            compare: CompareOp::Gt,
            value: 1.0,
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

fn services(name: &str) -> (Arc<SegmentedStore>, Arc<QueryService>, AlertService) {
    let store = Arc::new(SegmentedStore::open(temporary_directory(name)).expect("open"));
    let query = Arc::new(QueryService {
        store: Arc::clone(&store) as Arc<dyn Store>,
        max_runtime_ms: 30_000,
        max_expression_depth: tallyowl_head::expr::DEFAULT_MAX_DEPTH,
        guards: tallyowl_head::analysis::Guards::default(),
        attribution: Default::default(),
        policy: Default::default(),
        identity: Default::default(),
    });
    let metrics = Registry::new();
    tallyowl_head::alerts::declare(&metrics);
    let alerts = AlertService {
        store: Arc::clone(&store),
        query: Arc::clone(&query),
        metrics,
        callbacks_available: false,
        pool: Default::default(),
    };
    (store, query, alerts)
}

#[test]
fn an_alert_rule_whose_query_reads_another_project_is_refused_when_it_is_written() {
    let (_store, _query, alerts) = services("alert-foreign-put");
    let refused = alerts
        .put_rule(&counting_rule("peek", &OTHER_PROJECT), "test")
        .expect_err("a rule in one project read another");
    assert_eq!(refused.code, tallyowl_obs::ErrorCode::PermissionDenied);
    assert!(alerts.rules(PROJECT).unwrap().is_empty(), "it was stored");

    // The same rule over its own project is an ordinary rule.
    alerts
        .put_rule(&counting_rule("own", &PROJECT), "test")
        .expect("a rule over its own project");
}

#[test]
fn a_stored_rule_that_reads_another_project_evaluates_to_an_error_and_reads_nothing() {
    // A rule written before `put_rule` made the check is still in the catalog.
    let (store, _query, alerts) = services("alert-foreign-evaluate");
    let mut row = tallyowl_store::row::EventRow::new([1; 16], "event", "secret", BASE_TIME + 1);
    row.project_id = OTHER_PROJECT;
    let mut second = row.clone();
    second.event_id = [2; 16];
    store
        .commit([7; 16], [1; 16], vec![row, second])
        .expect("the other tenant's rows");

    let evaluation = alerts.evaluate(&counting_rule("peek", &OTHER_PROJECT));
    assert_eq!(evaluation.outcome, AlertOutcome::Error);
    assert_eq!(
        evaluation.value, None,
        "the other project's count came back"
    );
    assert_eq!(
        evaluation.code,
        Some(tallyowl_obs::ErrorCode::PermissionDenied)
    );
}

#[test]
fn a_query_tree_five_thousand_operators_deep_is_refused_and_not_executed() {
    // The executor recurses once for each level. An alert rule's stored query
    // reaches it without the authorization walk that bounds `run-query`, and
    // this tree overflowed the worker's stack, which stops the process.
    let (_store, query_service, _alerts) = services("deep-tree");
    let mut node = query::node::scan(query::events(&PROJECT, whole_range()));
    for _ in 0..5_000 {
        node = query::limited(&node, 10, None);
    }
    let refused = query_service
        .run(query::request(1, &node))
        .expect_err("a tree this deep ran");
    assert_eq!(refused.code, tallyowl_obs::ErrorCode::BudgetExceeded);
    assert!(refused.message.contains("deep"), "{}", refused.message);
}

fn measured(value: f64) -> Evaluation {
    Evaluation {
        outcome: AlertOutcome::Value,
        code: None,
        value: Some(value),
        commit_watermark: 0,
        reason: String::new(),
    }
}

fn no_data() -> Evaluation {
    Evaluation {
        outcome: AlertOutcome::NoData,
        code: None,
        value: None,
        commit_watermark: 0,
        reason: String::new(),
    }
}

#[test]
fn a_condition_that_holds_for_its_whole_sustain_window_fires() {
    // The clock is the `now` argument. Before the fix the count started again
    // on every evaluation, so a rule with a sustain window never fired.
    let mut rule = counting_rule("busy", &PROJECT);
    rule.threshold.as_mut().unwrap().sustained_ms = Some(300_000);

    let first = decide(&rule, None, &measured(9.0), BASE_TIME, PROJECT);
    assert_eq!(first.instance.state, "ok", "one spike fired");
    assert!(!first.notify);

    let mut held = first.instance;
    for minute in 1..5 {
        let next = decide(
            &rule,
            Some(&held),
            &measured(9.0),
            BASE_TIME + minute * 60_000,
            PROJECT,
        );
        assert_eq!(next.instance.state, "ok", "minute {minute}");
        held = next.instance;
    }
    let fired = decide(
        &rule,
        Some(&held),
        &measured(9.0),
        BASE_TIME + 300_000,
        PROJECT,
    );
    assert_eq!(fired.instance.state, "firing");
    assert!(fired.notify, "it fired and told nobody");
}

#[test]
fn a_condition_that_clears_inside_its_sustain_window_starts_the_count_again() {
    let mut rule = counting_rule("busy", &PROJECT);
    rule.threshold.as_mut().unwrap().sustained_ms = Some(300_000);

    let a = decide(&rule, None, &measured(9.0), BASE_TIME, PROJECT);
    let b = decide(
        &rule,
        Some(&a.instance),
        &measured(9.0),
        BASE_TIME + 240_000,
        PROJECT,
    );
    // Back under the line, one minute before the window would have closed.
    let c = decide(
        &rule,
        Some(&b.instance),
        &measured(0.0),
        BASE_TIME + 250_000,
        PROJECT,
    );
    let d = decide(
        &rule,
        Some(&c.instance),
        &measured(9.0),
        BASE_TIME + 260_000,
        PROJECT,
    );
    let e = decide(
        &rule,
        Some(&d.instance),
        &measured(9.0),
        BASE_TIME + 320_000,
        PROJECT,
    );
    assert_eq!(e.instance.state, "ok", "the earlier spike counted");
    let f = decide(
        &rule,
        Some(&e.instance),
        &measured(9.0),
        BASE_TIME + 560_000,
        PROJECT,
    );
    assert_eq!(f.instance.state, "firing");
}

#[test]
fn an_absence_rule_fires_after_the_data_has_been_gone_for_its_whole_window() {
    let mut rule = counting_rule("quiet", &PROJECT);
    rule.threshold = None;
    rule.absence = Some(AbsenceCondition { for_ms: 600_000 });

    let first = decide(&rule, None, &no_data(), BASE_TIME, PROJECT);
    assert_ne!(first.instance.state, "firing");
    let later = decide(
        &rule,
        Some(&first.instance),
        &no_data(),
        BASE_TIME + 300_000,
        PROJECT,
    );
    assert_ne!(later.instance.state, "firing");
    let fired = decide(
        &rule,
        Some(&later.instance),
        &no_data(),
        BASE_TIME + 600_000,
        PROJECT,
    );
    assert_eq!(fired.instance.state, "firing");
}

#[test]
fn an_instance_stored_before_the_pending_state_existed_still_reaches_firing() {
    let mut rule = counting_rule("busy", &PROJECT);
    rule.threshold.as_mut().unwrap().sustained_ms = Some(120_000);
    let mut legacy = tallyowl_head::alerts::new_instance("busy", PROJECT);
    legacy.last_evaluated_at = BASE_TIME;
    assert_eq!(legacy.pending_state, "");

    let a = decide(
        &rule,
        Some(&legacy),
        &measured(9.0),
        BASE_TIME + 60_000,
        PROJECT,
    );
    let b = decide(
        &rule,
        Some(&a.instance),
        &measured(9.0),
        BASE_TIME + 120_000,
        PROJECT,
    );
    let c = decide(
        &rule,
        Some(&b.instance),
        &measured(9.0),
        BASE_TIME + 180_000,
        PROJECT,
    );
    assert_eq!(c.instance.state, "firing");
}

// ---------------------------------------------------------------------------
// Ingest: tenancy comes from the source record, never from the payload
// ---------------------------------------------------------------------------

use tallyowl_head::ingest::SourceCheck;

fn checked_ingest(name: &str, require_known: bool) -> (Ingest, Arc<SegmentedStore>) {
    let store = Arc::new(SegmentedStore::open(temporary_directory(name)).expect("open"));
    store
        .catalog()
        .put_source(&tallyowl_store::control::Source {
            source_id: SOURCE,
            project_id: PROJECT,
            workspace_id: WORKSPACE,
            name: "web".into(),
            created_at: BASE_TIME,
        })
        .expect("the source is stored");
    let metrics = Registry::new();
    Ingest::declare_metrics(&metrics);
    (
        Ingest {
            golden_signal_bucket_ms: 0,
            store: Arc::clone(&store) as Arc<dyn Store>,
            metrics,
            receipt_policy: ReceiptPolicy::LocalOne,
            open_traces: None,
            policy: None,
            sources: Some(SourceCheck {
                store: Arc::clone(&store),
                require_known,
            }),
        },
        store,
    )
}

#[test]
fn an_item_that_names_another_tenants_project_is_rejected_and_the_rest_commit() {
    let (ingest, store) = checked_ingest("tenancy-mismatch", true);
    let mut foreign = event(1, "planted");
    foreign.envelope.project_id = Some(OTHER_PROJECT.to_vec());
    let mut foreign_workspace = event(2, "planted");
    foreign_workspace.envelope.workspace_id = Some(vec![0xC; 16]);

    let receipt = ingest
        .commit(request(
            1,
            vec![foreign, foreign_workspace, event(3, "own")],
        ))
        .expect("the batch commits");
    assert_eq!(receipt.accepted, 1);
    assert_eq!(receipt.rejected.expect("two are named").len(), 2);

    let planted = store
        .scan(OTHER_PROJECT, i64::MIN, i64::MAX, TimeBasis::OccurredAt)
        .expect("the store reads");
    assert!(planted.rows.is_empty(), "a row reached another project");
}

#[test]
fn a_batch_from_a_source_nobody_issued_is_refused_whole_and_not_retried() {
    let (ingest, store) = checked_ingest("unknown-source", true);
    let mut batch = request(1, vec![event(1, "planted")]);
    batch.source_id = vec![0xEE; 16];
    let refused = ingest.commit(batch).expect_err("an unknown source wrote");
    assert_eq!(refused.code, tallyowl_obs::ErrorCode::PermissionDenied);
    assert!(!refused.retryable, "a forwarder would retry this for a day");
    assert_eq!(store.row_count(), 0);

    // An installation that turned the requirement off still checks the sources
    // it does know.
    let (relaxed, _store) = checked_ingest("unknown-source-relaxed", false);
    let mut batch = request(2, vec![event(2, "kept")]);
    batch.source_id = vec![0xEE; 16];
    assert_eq!(relaxed.commit(batch).expect("it commits").accepted, 1);
}

// ---------------------------------------------------------------------------
// The rest: money with too many digits, a rule address, and an analysis deadline
// ---------------------------------------------------------------------------

#[test]
fn a_conversion_value_with_more_digits_than_money_has_is_rejected_at_ingest() {
    // A stored one fails every attribution question of its project until
    // somebody erases it, so it is refused where it costs one item.
    let (ingest, _store) = ingest("money-digits");
    let huge = items::conversion(
        envelope(1, TelemetryKind::Conversion),
        ConversionPayload {
            goal: "purchase".into(),
            value: Some(CsilDecimal {
                exponent: 0,
                mantissa: 10i128.pow(27),
            }),
            currency: Some("USD".into()),
            order_id: None,
            campaign: None,
            touch_event_id: None,
        },
    );
    let ordinary = items::conversion(
        envelope(2, TelemetryKind::Conversion),
        ConversionPayload {
            goal: "purchase".into(),
            value: Some(CsilDecimal {
                exponent: -2,
                mantissa: 1_999,
            }),
            currency: Some("USD".into()),
            order_id: None,
            campaign: None,
            touch_event_id: None,
        },
    );
    let receipt = ingest
        .commit(request(1, vec![huge, ordinary]))
        .expect("the batch commits");
    assert_eq!(receipt.accepted, 1);
    let rejected = receipt.rejected.expect("one is named");
    assert!(
        rejected[0].message.contains("digits"),
        "{}",
        rejected[0].message
    );
}

#[test]
fn a_rule_whose_webhook_address_holds_a_line_break_is_refused_when_it_is_written() {
    let (_store, _query, alerts) = services("alert-crlf");
    let mut rule = counting_rule("inject", &PROJECT);
    rule.notify[0].url =
        Some("http://127.0.0.1:5111/x HTTP/1.1\r\nHost: internal\r\n\r\n".to_string());
    let refused = alerts.put_rule(&rule, "test").expect_err("it was stored");
    assert_eq!(refused.code, tallyowl_obs::ErrorCode::InvalidArgument);
}

#[test]
fn an_analysis_stops_inside_its_loop_when_its_deadline_has_passed() {
    // The domain forms never read the deadline at all.
    let rows: Vec<tallyowl_store::row::EventRow> = (1..=3u8)
        .map(|n| {
            let mut row =
                tallyowl_store::row::EventRow::new([n; 16], "event", "view", BASE_TIME + n as i64);
            row.project_id = PROJECT;
            row
        })
        .collect();
    let identity = tallyowl_head::identity::Identity::build(PROJECT, &rows);
    let passed = tallyowl_head::query::Deadline::started_at(
        std::time::Instant::now() - std::time::Duration::from_secs(3_600),
        1,
    );
    let guards = tallyowl_head::analysis::Guards::default().within(passed);

    let steps = vec![tallyowl_head::analysis::Step {
        name: "view".into(),
        matches: tallyowl_head::expr::prepare_unchecked(
            &query::expression_ref(&query::compare(
                CompareOp::Eq,
                &query::expression::field(query::field("name")),
                &query::expression::literal(tallyowl_wire::control::write(&Value::Text(
                    "view".to_string(),
                ))),
            )),
            tallyowl_head::expr::DEFAULT_MAX_DEPTH,
        )
        .expect("the step reads"),
        exclusion: false,
    }];
    let funnel = tallyowl_head::analysis::FunnelQuestion {
        basis: tallyowl_head::identity::Basis::Session,
        resolution: tallyowl_head::identity::Resolution::LatestKnown,
        window_ms: 60_000,
        ordered: true,
        breakdown: None,
    };
    let refused =
        tallyowl_head::analysis::funnel(&rows, false, &identity, &steps, &funnel, &guards)
            .expect_err("the funnel ran past its deadline");
    assert_eq!(refused.code, tallyowl_obs::ErrorCode::BudgetExceeded);
}

// ---------------------------------------------------------------------------
// Golden signals that were owed, and the alert schedule across projects
// ---------------------------------------------------------------------------

use tallyowl_collector_api::types::{SpanKind, SpanPayload, SpanPayload_status as SpanStatus};

fn span(id: u8) -> TelemetryItem {
    let mut envelope = envelope(id, TelemetryKind::Span);
    envelope.trace_id = Some(vec![id; 16]);
    envelope.span_id = Some(vec![id; 8]);
    envelope.service_name = Some("checkout".into());
    items::span(
        envelope,
        SpanPayload {
            operation: "GET /cart".into(),
            kind: SpanKind::Server,
            start_at: BASE_TIME,
            duration_ms: 20,
            status: SpanStatus::Ok,
            resource: None,
            parent_span_id: None,
            links: None,
            error_event_id: None,
            sampling_reason: None,
        },
    )
}

fn signal_rows(store: &Arc<dyn Store>) -> usize {
    store
        .scan(PROJECT, i64::MIN, i64::MAX, TimeBasis::OccurredAt)
        .expect("the store reads")
        .rows
        .iter()
        .filter(|row| row.kind == "metric-point")
        .count()
}

#[test]
fn a_redelivered_batch_whose_golden_signals_never_committed_commits_them_and_nothing_else() {
    let (ingest, store) = ingest("owed-signals");

    // The first delivery, as it looks when the batch committed and the signals
    // did not: the batch has its receipt and the derived batch has none.
    let first = Ingest {
        golden_signal_bucket_ms: 0,
        store: Arc::clone(&store),
        metrics: Registry::new(),
        receipt_policy: ReceiptPolicy::LocalOne,
        open_traces: None,
        policy: None,
        sources: None,
    };
    first
        .commit(request(1, vec![span(1), span(2)]))
        .expect("the batch commits");
    assert_eq!(signal_rows(&store), 0);

    // The collector delivers it again. The receipt used to answer at once, so
    // nothing ever produced these signals.
    let again = ingest
        .commit(request(1, vec![span(1), span(2)]))
        .expect("the redelivery is answered");
    assert_eq!(again.deduplicated, Some(true));
    assert_eq!(again.accepted, 2);
    let signals = signal_rows(&store);
    assert!(signals > 0, "the signals are still owed");

    // A third delivery owes nothing and adds nothing.
    ingest
        .commit(request(1, vec![span(1), span(2)]))
        .expect("answered");
    assert_eq!(signal_rows(&store), signals);
    let spans = store
        .scan(PROJECT, i64::MIN, i64::MAX, TimeBasis::OccurredAt)
        .unwrap()
        .rows
        .iter()
        .filter(|row| row.kind == "span")
        .count();
    assert_eq!(spans, 2, "the batch committed twice");
}

#[test]
fn two_projects_with_a_rule_of_one_name_are_both_evaluated() {
    // A rule identifier is unique inside one project. Keyed by the identifier
    // alone, queueing one project's rule moved the other's due time forward,
    // and with equal intervals the second never ran.
    use tallyowl_head::passes::Scheduler;
    use tallyowl_head::workflows::Workflows;
    use tallyowl_queue::testing::FakeQueue;

    let (_store, _query, alerts) = services("schedule-two-projects");
    let alerts = Arc::new(alerts);
    let mut other = counting_rule("error-rate", &OTHER_PROJECT);
    other.project_id = OTHER_PROJECT.to_vec();
    alerts
        .put_rule(&counting_rule("error-rate", &PROJECT), "test")
        .expect("stored");
    alerts.put_rule(&other, "test").expect("stored");

    let queue = Arc::new(FakeQueue::default());
    let logger = Arc::new(tallyowl_obs::log::Logger::new(
        "test",
        "0.0.0",
        tallyowl_obs::log::Severity::Error,
    ));
    let workflows = Arc::new(Workflows::new(
        Arc::clone(&queue) as Arc<dyn tallyowl_queue::DurableQueue>,
        Registry::new(),
        Arc::clone(&logger),
        3_600_000,
    ));
    let scheduler = Scheduler::new(Arc::clone(&alerts), workflows, logger);

    // Ten intervals, one tick a second. Each rule is due ten times.
    let queued: usize = (0..=600)
        .map(|second| {
            scheduler
                .tick(BASE_TIME + second * 1_000)
                .expect("it ticks")
        })
        .sum();
    assert_eq!(queued, 20, "one project's rule kept the other from running");

    // A rule that is removed leaves nothing behind, and one written again
    // under the same name starts its own schedule.
    alerts
        .remove_rule(OTHER_PROJECT, "error-rate")
        .expect("removed");
    scheduler.tick(BASE_TIME + 601_000).expect("it ticks");
    alerts.put_rule(&other, "test").expect("stored again");
    let after: usize = (602..=720)
        .map(|second| {
            scheduler
                .tick(BASE_TIME + second * 1_000)
                .expect("it ticks")
        })
        .sum();
    assert!(
        after >= 3,
        "the rule written again never became due: {after}"
    );
}
