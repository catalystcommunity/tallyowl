//! Phase 8: identity, funnels, retention, paths, timelines, and erasure.
//!
//! `docs/PLAN.md` Phase 8 gives four exit criteria. Each one has cases here:
//!
//! | Exit criterion | Where |
//! | --- | --- |
//! | Anonymous-to-known conversion works without cross-project leakage | `anonymous_*`, `two_projects_*` |
//! | Funnel and retention fixtures have explainable exact results | `a_funnel_*`, `a_retention_*` |
//! | Query cost guards reject pathological analysis safely | `a_funnel_with_too_many_steps_*`, `a_path_deeper_than_*` |
//! | The reference application's results match the ledger | `testbed/` and `crates/tallyowl-collector/tests/testbed.rs` |
//!
//! Every fixture here is small enough that a person can work the answer out by
//! hand, which is what "explainable" means: the comment above each case says
//! what the answer should be and why, before the assertion says what it is.
//!
//! `AGENTS.md` forbids mocking the storage interface, so every case writes rows
//! through the real store and reads them back through the real executor.

use std::sync::Arc;

use tallyowl_control_api::types::{
    CompareOp, CorrelationBasis, FunnelQuery, FunnelStep, PathQuery,
    PathQuery_direction as Direction, QueryForm, QueryRequest, RetentionQuery,
    RetentionQuery_period as Period, TelemetryKind, TimeBasis, TimeRange, TimelineQuery,
};
use tallyowl_head::analysis::Guards;
use tallyowl_head::expr::DEFAULT_MAX_DEPTH;
use tallyowl_head::identity::{Identity, Resolution, ANONYMOUS_ID, END_USER_ID};
use tallyowl_head::query::QueryService;
use tallyowl_store::row::{EventRow, PropertyValue};
use tallyowl_store::{SegmentedStore, Store};
use tallyowl_wire::{control as wire, query, Value};

const PROJECT: [u8; 16] = [9; 16];
const OTHER_PROJECT: [u8; 16] = [3; 16];
const DAY: i64 = 86_400_000;
const BASE: i64 = 1_785_628_800_000;

fn directory(name: &str) -> std::path::PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("target"));
    let path = base
        .join("phase8-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn head(name: &str, rows: Vec<EventRow>) -> QueryService {
    let store: Arc<dyn Store> = Arc::new(SegmentedStore::open(directory(name)).expect("open"));
    store
        .commit([7; 16], [1; 16], rows)
        .expect("the rows commit");
    QueryService {
        store,
        max_runtime_ms: 30_000,
        max_expression_depth: DEFAULT_MAX_DEPTH,
        guards: Guards::default(),
        attribution: Default::default(),
        policy: Default::default(),
        identity: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// Fixture builders. Every one is named for what it is, so a case reads as the
// story it tells rather than as a struct literal.
// ---------------------------------------------------------------------------

fn base_row(id: u8, kind: &str, name: &str, at: i64) -> EventRow {
    let mut row = EventRow::new([id; 16], kind, name, at);
    row.project_id = PROJECT;
    row.workspace_id = [8; 16];
    row.received_at = at;
    row
}

/// One event by an anonymous visitor.
fn anonymous(id: u8, name: &str, at: i64, anonymous_id: &str) -> EventRow {
    let mut row = base_row(id, "event", name, at);
    row.session_id = Some(format!("session-{anonymous_id}"));
    row.properties.insert(
        ANONYMOUS_ID.to_string(),
        (
            PropertyValue::Text(anonymous_id.to_string()),
            "client".to_string(),
        ),
    );
    row
}

/// One event by a known end user.
fn known(id: u8, name: &str, at: i64, end_user_id: &str) -> EventRow {
    let mut row = base_row(id, "event", name, at);
    row.session_id = Some(format!("session-{end_user_id}"));
    row.properties.insert(
        END_USER_ID.to_string(),
        (
            PropertyValue::Text(end_user_id.to_string()),
            "client".to_string(),
        ),
    );
    row
}

/// The moment an anonymous timeline joined a known end user.
fn identify(id: u8, at: i64, anonymous_id: &str, end_user_id: &str) -> EventRow {
    let mut row = anonymous(id, "identify", at, anonymous_id);
    row.kind = "identify".to_string();
    row.properties.insert(
        END_USER_ID.to_string(),
        (
            PropertyValue::Text(end_user_id.to_string()),
            "client".to_string(),
        ),
    );
    row
}

/// An explicit merge of two known identifiers.
fn alias(id: u8, at: i64, from_id: &str, to_id: &str) -> EventRow {
    let mut row = base_row(id, "alias", "alias", at);
    for (key, value) in [("from_id", from_id), ("to_id", to_id)] {
        row.properties.insert(
            key.to_string(),
            (PropertyValue::Text(value.to_string()), "client".to_string()),
        );
    }
    row
}

fn matches_name(name: &str) -> Vec<u8> {
    query::expression_ref(&query::compare(
        CompareOp::Eq,
        &query::expression::field(query::field("name")),
        &query::expression::literal(wire::write(&Value::Text(name.to_string()))),
    ))
}

fn a_range() -> TimeRange {
    TimeRange {
        range_start: BASE - DAY,
        range_end: BASE + 100 * DAY,
        basis: TimeBasis::OccurredAt,
        timezone: None,
    }
}

fn run(head: &QueryService, request: QueryRequest) -> Vec<Vec<Value>> {
    let response = head.run(request).expect("the query answers");
    response
        .rows
        .iter()
        .map(|row| row.values.iter().map(|v| wire::read(v).unwrap()).collect())
        .collect()
}

fn signed(value: &Value) -> i64 {
    match value {
        Value::Integer(n) => *n,
        Value::Unsigned(n) => *n as i64,
        other => panic!("expected a number and got {other:?}"),
    }
}

fn unsigned(value: &Value) -> u64 {
    match value {
        Value::Unsigned(n) => *n,
        Value::Integer(n) => *n as u64,
        other => panic!("expected a number and got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

#[test]
fn an_anonymous_visitor_becomes_a_known_end_user_from_the_moment_they_identify() {
    // Three events by `a1`: one before the `identify` and two after. Event-time
    // identity says the first belongs to `a1` and the rest to `person-1`;
    // latest-known identity says all three belong to `person-1`. Both are true
    // answers to different questions, and `docs/DATA_MODEL.md` section 3.5 asks
    // for both.
    let before = anonymous(1, "view-pricing", BASE, "a1");
    let joining = identify(2, BASE + 100, "a1", "person-1");
    let after = anonymous(3, "start-trial", BASE + 200, "a1");

    let identity = Identity::build(PROJECT, &[before.clone(), joining, after.clone()]);

    assert_eq!(
        identity.who(&before, Resolution::EventTime).as_deref(),
        Some("a1"),
        "before the identify, this row belonged to an anonymous visitor"
    );
    assert_eq!(
        identity.who(&after, Resolution::EventTime).as_deref(),
        Some("person-1"),
        "after it, the same anonymous identifier resolves to the person"
    );
    assert_eq!(
        identity.who(&before, Resolution::LatestKnown).as_deref(),
        Some("person-1"),
        "asked who this row belongs to now, the answer is the person"
    );
    assert!(!identity.is_known(&before, Resolution::EventTime));
    assert!(identity.is_known(&before, Resolution::LatestKnown));
}

#[test]
fn two_projects_that_use_the_same_anonymous_identifier_are_two_different_people() {
    // The exit criterion: "anonymous-to-known conversion works without
    // cross-project leakage." `a1` in one project and `a1` in another are two
    // people, and an `identify` in one must not name the other.
    let mut theirs = identify(1, BASE, "a1", "their-person");
    theirs.project_id = OTHER_PROJECT;
    let ours = anonymous(2, "view-pricing", BASE + 100, "a1");

    let identity = Identity::build(PROJECT, &[theirs.clone(), ours.clone()]);
    assert_eq!(
        identity.foreign_rows_ignored(),
        1,
        "the other project's row was read and set aside, not read into the graph"
    );
    assert_eq!(
        identity.who(&ours, Resolution::LatestKnown).as_deref(),
        Some("a1"),
        "our anonymous visitor did not become their known person"
    );
    assert!(
        identity.who(&theirs, Resolution::LatestKnown).is_none(),
        "a row from another project has no identity in this graph at all"
    );
}

#[test]
fn an_alias_merges_two_known_identifiers_without_rewriting_a_row() {
    // `docs/DATA_MODEL.md` section 3.5: "alias is an explicit, auditable merge
    // edge; it does not rewrite raw events." The rows keep the identifiers they
    // were sent with, and resolution follows the edge.
    let old = known(1, "sign-in", BASE, "person-old");
    let merge = alias(2, BASE + 50, "person-old", "person-new");
    let new = known(3, "buy", BASE + 100, "person-new");

    let identity = Identity::build(PROJECT, &[old.clone(), merge, new.clone()]);
    assert_eq!(identity.canonical("person-old"), "person-new");
    assert_eq!(
        identity.who(&old, Resolution::LatestKnown).as_deref(),
        Some("person-new")
    );
    assert_eq!(
        tallyowl_head::identity::text(&old, END_USER_ID).as_deref(),
        Some("person-old"),
        "the stored row still says what it was sent with"
    );
    assert_eq!(identity.merges().len(), 1, "the merge is on the record");
    assert_eq!(identity.merges()[0].at, BASE + 50);
}

#[test]
fn an_alias_loop_terminates_rather_than_running_for_ever() {
    // Somebody sent `a -> b` and `b -> a`. It is a defect in what was sent, and
    // it must not be a defect in what reads it.
    let there = alias(1, BASE, "a", "b");
    let back = alias(2, BASE + 1, "b", "a");
    let identity = Identity::build(PROJECT, &[there, back]);
    let canonical = identity.canonical("a");
    assert!(canonical == "a" || canonical == "b");
}

#[test]
fn one_person_on_three_devices_is_one_person_for_an_erasure() {
    // A person signs in on three devices, so there are three anonymous
    // identifiers. An erasure that named only the one the request carried would
    // leave two timelines behind.
    let rows = vec![
        identify(1, BASE, "phone", "person-1"),
        identify(2, BASE + 1, "laptop", "person-1"),
        identify(3, BASE + 2, "tablet", "person-1"),
    ];
    let identity = Identity::build(PROJECT, &rows);
    let every = identity.every_identifier_of("person-1");
    for expected in ["person-1", "phone", "laptop", "tablet"] {
        assert!(
            every.iter().any(|id| id == expected),
            "{expected} is one of this person's identifiers: {every:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Funnel
// ---------------------------------------------------------------------------

fn funnel_request(steps: Vec<FunnelStep>, ordered: bool, window_ms: i64) -> QueryRequest {
    let mut request = query::empty_request(1, QueryForm::Funnel);
    request.funnel = Some(FunnelQuery {
        project_id: PROJECT.to_vec(),
        range: a_range(),
        steps,
        window_ms,
        basis: CorrelationBasis::EndUser,
        ordered,
        breakdown: None,
        resolution: None,
    });
    request
}

fn step(name: &str, exclusion: bool) -> FunnelStep {
    FunnelStep {
        name: name.to_string(),
        r#match: matches_name(name),
        exclusion: exclusion.then_some(true),
    }
}

#[test]
fn a_funnel_counts_each_person_once_and_the_answer_can_be_worked_out_by_hand() {
    // Three people. `p1` did all three steps. `p2` did the first two. `p3` did
    // the first only, and then did it again — which is one person, not two.
    // So the answer is 3, 2, 1.
    let head = head(
        "funnel-basic",
        vec![
            known(1, "view", BASE, "p1"),
            known(2, "cart", BASE + 100, "p1"),
            known(3, "buy", BASE + 200, "p1"),
            known(4, "view", BASE, "p2"),
            known(5, "cart", BASE + 100, "p2"),
            known(6, "view", BASE, "p3"),
            known(7, "view", BASE + 10, "p3"),
        ],
    );

    let rows = run(
        &head,
        funnel_request(
            vec![step("view", false), step("cart", false), step("buy", false)],
            true,
            DAY,
        ),
    );
    assert_eq!(rows.len(), 3, "one row for each step");
    assert_eq!(unsigned(&rows[0][2]), 3, "everybody viewed");
    assert_eq!(unsigned(&rows[1][2]), 2, "two reached the cart");
    assert_eq!(unsigned(&rows[2][2]), 1, "one bought");
}

#[test]
fn an_ordered_funnel_refuses_a_sequence_that_happened_backwards() {
    // `p1` bought and then viewed. In ordered mode that is not a sequence, so
    // the second step counts nobody. In unordered mode it is, because unordered
    // asks only whether each step happened inside the window.
    let rows = vec![
        known(1, "buy", BASE, "p1"),
        known(2, "view", BASE + 100, "p1"),
    ];
    let ordered = head("funnel-ordered", rows.clone());
    let answer = run(
        &ordered,
        funnel_request(vec![step("view", false), step("buy", false)], true, DAY),
    );
    assert_eq!(unsigned(&answer[0][2]), 1, "one person viewed");
    assert_eq!(
        unsigned(&answer[1][2]),
        0,
        "nobody bought after viewing, because the buy came first"
    );

    let unordered = head("funnel-unordered", rows);
    let answer = run(
        &unordered,
        funnel_request(vec![step("view", false), step("buy", false)], false, DAY),
    );
    assert_eq!(
        unsigned(&answer[1][2]),
        1,
        "unordered mode counts both steps inside the window"
    );
}

#[test]
fn a_funnel_window_that_closes_stops_the_sequence() {
    // `p1` viewed on day one and bought a week later. With a one-day window
    // that is not one sequence.
    let head = head(
        "funnel-window",
        vec![
            known(1, "view", BASE, "p1"),
            known(2, "buy", BASE + 7 * DAY, "p1"),
        ],
    );
    let answer = run(
        &head,
        funnel_request(vec![step("view", false), step("buy", false)], true, DAY),
    );
    assert_eq!(unsigned(&answer[0][2]), 1);
    assert_eq!(unsigned(&answer[1][2]), 0, "the window had closed");
}

#[test]
fn an_exclusion_step_voids_a_sequence() {
    // `p1` viewed, refunded, and bought. `p2` viewed and bought. The exclusion
    // voids `p1`, so one person converts.
    let head = head(
        "funnel-exclusion",
        vec![
            known(1, "view", BASE, "p1"),
            known(2, "refund", BASE + 50, "p1"),
            known(3, "buy", BASE + 100, "p1"),
            known(4, "view", BASE, "p2"),
            known(5, "buy", BASE + 100, "p2"),
        ],
    );
    let answer = run(
        &head,
        funnel_request(
            vec![
                step("view", false),
                step("buy", false),
                step("refund", true),
            ],
            true,
            DAY,
        ),
    );
    assert_eq!(unsigned(&answer[0][2]), 1, "the refunder is not counted");
    assert_eq!(unsigned(&answer[1][2]), 1);
}

#[test]
fn a_funnel_starts_where_the_person_started_rather_than_where_they_signed_in() {
    // The reason event-time identity exists. `a1` viewed anonymously, signed in
    // as `p1`, and bought. Both events belong to one sequence, so the funnel
    // counts one person through both steps rather than one anonymous visitor
    // who vanished and one customer who appeared.
    let head = head(
        "funnel-conversion",
        vec![
            anonymous(1, "view", BASE, "a1"),
            identify(2, BASE + 50, "a1", "p1"),
            known(3, "buy", BASE + 100, "p1"),
        ],
    );
    let answer = run(
        &head,
        funnel_request(vec![step("view", false), step("buy", false)], true, DAY),
    );
    assert_eq!(unsigned(&answer[0][2]), 1);
    assert_eq!(
        unsigned(&answer[1][2]),
        1,
        "the anonymous view and the known purchase are one person"
    );
}

#[test]
fn a_funnel_with_more_steps_than_the_guard_allows_is_refused_and_the_guard_is_named() {
    // The exit criterion: "query cost guards reject pathological analysis
    // safely." Safely means a typed refusal that says the limit and the
    // observed value, not a slow answer and not a crash.
    let head = head("funnel-guard", vec![known(1, "view", BASE, "p1")]);
    let steps: Vec<FunnelStep> = (0..Guards::default().max_steps + 1)
        .map(|n| step(&format!("step-{n}"), false))
        .collect();
    let refused = head
        .run(funnel_request(steps, true, DAY))
        .expect_err("a funnel this wide is refused");
    assert_eq!(refused.code, tallyowl_obs::ErrorCode::BudgetExceeded);
    assert!(
        refused.message.contains("steps") && refused.message.contains("20"),
        "the refusal names the budget and the limit: {}",
        refused.message
    );
}

#[test]
fn a_funnel_of_only_exclusions_is_refused_rather_than_answered_with_nothing() {
    let head = head("funnel-all-exclusions", vec![known(1, "view", BASE, "p1")]);
    let refused = head
        .run(funnel_request(vec![step("refund", true)], true, DAY))
        .expect_err("nothing can enter this funnel");
    assert!(refused.message.contains("exclusion"), "{}", refused.message);
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

fn retention_request(periods: u64, first_time_only: bool) -> QueryRequest {
    let mut request = query::empty_request(1, QueryForm::Retention);
    request.retention = Some(RetentionQuery {
        project_id: PROJECT.to_vec(),
        range: TimeRange {
            range_start: BASE,
            range_end: BASE + 100 * DAY,
            basis: TimeBasis::OccurredAt,
            timezone: None,
        },
        initial: matches_name("sign-up"),
        returning: matches_name("visit"),
        period: Period::Day,
        periods,
        first_time_only,
        resolution: None,
    });
    request
}

#[test]
fn a_retention_matrix_can_be_worked_out_by_hand() {
    // Two people sign up on day zero. `p1` visits on day zero, day one, and day
    // three. `p2` visits on day zero only. So the cohort is 2, and it returns
    // 2, 1, 0, 1 over four days.
    let head = head(
        "retention-basic",
        vec![
            known(1, "sign-up", BASE, "p1"),
            known(2, "visit", BASE + 10, "p1"),
            known(3, "visit", BASE + DAY, "p1"),
            known(4, "visit", BASE + 3 * DAY, "p1"),
            known(5, "sign-up", BASE, "p2"),
            known(6, "visit", BASE + 20, "p2"),
        ],
    );
    let rows = run(&head, retention_request(4, false));
    assert_eq!(rows.len(), 1, "one cohort, because both signed up on day 0");
    assert_eq!(unsigned(&rows[0][1]), 2, "the cohort holds two people");
    // The columns after the size are pairs of count and rate.
    assert_eq!(unsigned(&rows[0][2]), 2, "both visited on day 0");
    assert_eq!(unsigned(&rows[0][4]), 1, "one visited on day 1");
    assert_eq!(unsigned(&rows[0][6]), 0, "nobody visited on day 2");
    assert_eq!(unsigned(&rows[0][8]), 1, "one visited on day 3");
}

#[test]
fn a_person_who_returns_twice_in_one_period_is_counted_once() {
    // The question is how many people came back, so two visits in one day are
    // one person on that day.
    let head = head(
        "retention-twice",
        vec![
            known(1, "sign-up", BASE, "p1"),
            known(2, "visit", BASE + DAY, "p1"),
            known(3, "visit", BASE + DAY + 100, "p1"),
        ],
    );
    let rows = run(&head, retention_request(3, false));
    assert_eq!(unsigned(&rows[0][1]), 1);
    assert_eq!(unsigned(&rows[0][4]), 1, "one person, not two visits");
}

#[test]
fn first_time_semantics_leave_out_somebody_who_was_already_here() {
    // `p1` visited before they signed up, so their sign-up is not their first
    // event. With first-time semantics they do not enter the cohort, because
    // counting them would make an established person look like a new one.
    let head = head(
        "retention-first-time",
        vec![
            known(1, "visit", BASE, "p1"),
            known(2, "sign-up", BASE + 100, "p1"),
            known(3, "visit", BASE + DAY, "p1"),
            known(4, "sign-up", BASE, "p2"),
            known(5, "visit", BASE + DAY, "p2"),
        ],
    );
    let rows = run(&head, retention_request(3, true));
    let total: u64 = rows.iter().map(|row| unsigned(&row[1])).sum();
    assert_eq!(
        total, 1,
        "only the person whose first event was the sign-up"
    );
}

#[test]
fn a_retention_matrix_longer_than_the_guard_allows_is_refused() {
    let head = head("retention-guard", vec![known(1, "sign-up", BASE, "p1")]);
    let refused = head
        .run(retention_request(
            Guards::default().max_periods as u64 + 1,
            false,
        ))
        .expect_err("a matrix this long is refused");
    assert_eq!(refused.code, tallyowl_obs::ErrorCode::BudgetExceeded);
    assert!(refused.message.contains("periods"), "{}", refused.message);
}

// ---------------------------------------------------------------------------
// Path
// ---------------------------------------------------------------------------

fn path_request(depth: u64, min_frequency: u64, collapse_loops: bool) -> QueryRequest {
    let mut request = query::empty_request(1, QueryForm::Path);
    request.path = Some(PathQuery {
        project_id: PROJECT.to_vec(),
        range: a_range(),
        anchor: matches_name("home"),
        direction: Direction::Next,
        depth,
        min_frequency,
        collapse_loops,
        resolution: None,
    });
    request
}

#[test]
fn a_path_tree_counts_what_people_did_next() {
    // Two sessions go home → pricing. One goes home → docs. So the tree under
    // `home` is pricing 2, docs 1.
    let head = head(
        "path-basic",
        vec![
            known(1, "home", BASE, "p1"),
            known(2, "pricing", BASE + 10, "p1"),
            known(3, "home", BASE, "p2"),
            known(4, "pricing", BASE + 10, "p2"),
            known(5, "home", BASE, "p3"),
            known(6, "docs", BASE + 10, "p3"),
        ],
    );
    let rows = run(&head, path_request(2, 1, false));
    assert_eq!(rows[0][1], Value::Text("home".into()));
    assert_eq!(unsigned(&rows[0][2]), 3, "three sessions started at home");
    assert_eq!(rows[1][1], Value::Text("pricing".into()));
    assert_eq!(unsigned(&rows[1][2]), 2);
    assert_eq!(rows[2][1], Value::Text("docs".into()));
    assert_eq!(unsigned(&rows[2][2]), 1);
}

#[test]
fn a_branch_below_the_minimum_is_dropped_and_the_result_says_a_branch_was_dropped() {
    // A tree that pruned in silence would read as the whole picture.
    let head = head(
        "path-pruned",
        vec![
            known(1, "home", BASE, "p1"),
            known(2, "pricing", BASE + 10, "p1"),
            known(3, "home", BASE, "p2"),
            known(4, "pricing", BASE + 10, "p2"),
            known(5, "home", BASE, "p3"),
            known(6, "docs", BASE + 10, "p3"),
        ],
    );
    let response = head
        .run(path_request(2, 2, false))
        .expect("the path answers");
    let warnings = response.metadata.warnings.unwrap_or_default();
    assert!(
        warnings.iter().any(|w| w.contains("fewer than 2")),
        "the answer says a branch was dropped: {warnings:?}"
    );
}

#[test]
fn a_path_deeper_than_the_guard_allows_is_refused() {
    let head = head("path-guard", vec![known(1, "home", BASE, "p1")]);
    let refused = head
        .run(path_request(
            Guards::default().max_path_depth as u64 + 1,
            1,
            false,
        ))
        .expect_err("a path this deep is refused");
    assert_eq!(refused.code, tallyowl_obs::ErrorCode::BudgetExceeded);
    assert!(refused.message.contains("depth"), "{}", refused.message);
}

// ---------------------------------------------------------------------------
// Timeline
// ---------------------------------------------------------------------------

#[test]
fn a_timeline_holds_everything_that_turned_out_to_be_one_persons() {
    // Latest-known identity, so what somebody did before they signed in is on
    // their timeline. Three events, in the order they happened.
    let head = head(
        "timeline-basic",
        vec![
            anonymous(1, "view", BASE, "a1"),
            identify(2, BASE + 50, "a1", "p1"),
            known(3, "buy", BASE + 100, "p1"),
        ],
    );
    let mut request = query::empty_request(1, QueryForm::Timeline);
    request.timeline = Some(TimelineQuery {
        project_id: PROJECT.to_vec(),
        range: a_range(),
        end_user_id: Some("p1".to_string()),
        session_id: None,
        kinds: Vec::new(),
        limit: 10,
        cursor: None,
        resolution: None,
    });
    let rows = run(&head, request);
    assert_eq!(rows.len(), 3, "the anonymous view is on the timeline too");
    assert_eq!(rows[0][2], Value::Text("view".into()));
    assert_eq!(rows[2][2], Value::Text("buy".into()));
}

#[test]
fn a_timeline_resumes_where_its_cursor_left_off_without_repeating_a_row() {
    let head = head(
        "timeline-cursor",
        vec![
            known(1, "one", BASE, "p1"),
            known(2, "two", BASE + 10, "p1"),
            known(3, "three", BASE + 20, "p1"),
        ],
    );
    let timeline = |limit: u64, cursor: Option<Vec<u8>>| {
        let mut request = query::empty_request(1, QueryForm::Timeline);
        request.timeline = Some(TimelineQuery {
            project_id: PROJECT.to_vec(),
            range: a_range(),
            end_user_id: Some("p1".to_string()),
            session_id: None,
            kinds: vec![TelemetryKind::Event],
            limit,
            cursor,
            resolution: None,
        });
        request
    };

    let first = head.run(timeline(2, None)).expect("the first page");
    assert_eq!(first.rows.len(), 2);
    let cursor = first.metadata.next_cursor.expect("there is more");

    let second = head
        .run(timeline(2, Some(cursor)))
        .expect("the second page");
    assert_eq!(second.rows.len(), 1, "the last row, and not the first two");
    let name = wire::read(&second.rows[0].values[2]).unwrap();
    assert_eq!(name, Value::Text("three".into()));
}

// ---------------------------------------------------------------------------
// Erasure
// ---------------------------------------------------------------------------

#[test]
fn erasing_one_person_removes_every_timeline_that_was_theirs() {
    // The person signed in on two devices. An erasure that named only the
    // identifier the request carried would leave the other timeline behind, and
    // they would still be here under a name nobody looked for.
    let rows = vec![
        anonymous(1, "view", BASE, "phone"),
        identify(2, BASE + 10, "phone", "p1"),
        anonymous(3, "view", BASE + 20, "laptop"),
        identify(4, BASE + 30, "laptop", "p1"),
        known(5, "buy", BASE + 40, "p1"),
        known(6, "buy", BASE + 50, "somebody-else"),
    ];
    let head = head("erasure-person", rows.clone());
    let identity = head.identity_of(PROJECT).expect("the graph builds");

    let report = tallyowl_head::erasure::run(
        head.store.as_ref(),
        &tallyowl_head::erasure::Request {
            project_id: PROJECT,
            target: tallyowl_head::erasure::Target::EndUser("p1".to_string()),
            reason: "the person asked to be removed".to_string(),
            horizon_ms: tallyowl_head::erasure::DEFAULT_HORIZON_MS,
            requested_at: BASE + 1_000,
        },
        &identity,
    )
    .expect("the erasure runs");
    assert!(report.predicates >= 2, "one for each identifier");

    let left = head
        .store
        .scan(
            PROJECT,
            BASE - DAY,
            BASE + DAY,
            tallyowl_store::TimeBasis::OccurredAt,
        )
        .expect("a scan");
    assert_eq!(
        left.rows.len(),
        1,
        "only the other person's row is still visible: {:?}",
        left.rows.iter().map(|r| r.name.clone()).collect::<Vec<_>>()
    );
    assert_eq!(left.rows[0].name, "buy");
    assert_eq!(
        tallyowl_head::identity::text(&left.rows[0], END_USER_ID).as_deref(),
        Some("somebody-else")
    );
}

#[test]
fn an_erasure_keeps_hiding_what_arrives_afterwards() {
    // `AGENTS.md`: a tombstone "also hides matching data that arrives after the
    // erasure request". Telemetry for an erased person can still be in a
    // collector queue when the erasure lands.
    let head = head("erasure-late", vec![known(1, "buy", BASE, "p1")]);
    let identity = head.identity_of(PROJECT).expect("the graph builds");
    tallyowl_head::erasure::run(
        head.store.as_ref(),
        &tallyowl_head::erasure::Request {
            project_id: PROJECT,
            target: tallyowl_head::erasure::Target::EndUser("p1".to_string()),
            reason: "removal".to_string(),
            horizon_ms: tallyowl_head::erasure::DEFAULT_HORIZON_MS,
            requested_at: BASE,
        },
        &identity,
    )
    .expect("the erasure runs");

    // A late arrival for the same person.
    head.store
        .commit([7; 16], [2; 16], vec![known(9, "buy", BASE + 100, "p1")])
        .expect("the late batch commits");

    let left = head
        .store
        .scan(
            PROJECT,
            BASE - DAY,
            BASE + DAY,
            tallyowl_store::TimeBasis::OccurredAt,
        )
        .expect("a scan");
    assert!(
        left.rows.is_empty(),
        "the predicate still hides them: {:?}",
        left.rows.iter().map(|r| r.name.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn an_erasure_with_no_reason_is_refused() {
    let head = head("erasure-no-reason", vec![known(1, "buy", BASE, "p1")]);
    let identity = head.identity_of(PROJECT).expect("the graph builds");
    let refused = tallyowl_head::erasure::plan(
        &tallyowl_head::erasure::Request {
            project_id: PROJECT,
            target: tallyowl_head::erasure::Target::EndUser("p1".to_string()),
            reason: "  ".to_string(),
            horizon_ms: 0,
            requested_at: BASE,
        },
        &identity,
    )
    .expect_err("a reason is needed");
    assert!(refused.message.contains("reason"), "{}", refused.message);
}

#[test]
fn a_repeated_erasure_is_one_erasure() {
    // The predicates have stable identifiers, so a retry replaces the same
    // ledger records rather than writing a second set. An erasure a retry
    // duplicates is an erasure nobody can count.
    let head = head("erasure-repeat", vec![known(1, "buy", BASE, "p1")]);
    let identity = head.identity_of(PROJECT).expect("the graph builds");
    let request = tallyowl_head::erasure::Request {
        project_id: PROJECT,
        target: tallyowl_head::erasure::Target::EndUser("p1".to_string()),
        reason: "removal".to_string(),
        horizon_ms: 0,
        requested_at: BASE,
    };
    let first = tallyowl_head::erasure::plan(&request, &identity).expect("a plan");
    let second = tallyowl_head::erasure::plan(&request, &identity).expect("the same plan");
    let ids: Vec<[u8; 16]> = first.iter().map(|t| t.tombstone_id).collect();
    let again: Vec<[u8; 16]> = second.iter().map(|t| t.tombstone_id).collect();
    assert_eq!(ids, again, "the same request produces the same predicates");
}

// ---------------------------------------------------------------------------
// Collection policy
// ---------------------------------------------------------------------------

#[test]
fn a_narrower_level_overrides_a_wider_one_and_a_refusal_accumulates() {
    use tallyowl_head::policy::{Document, Policies, Scope};

    let mut policies = Policies::new();
    let mut installation = Document::at(Scope::Installation, "");
    installation.head_sample_rate = Some(1.0);
    installation.blocked_event_names = vec!["debug-ping".to_string()];
    policies.put(installation).expect("valid");

    let mut project = Document::at(Scope::Project, "p");
    project.head_sample_rate = Some(0.5);
    project.blocked_property_keys = vec!["password".to_string()];
    policies.put(project).expect("valid");

    let compiled = policies.compile("", "p", "", "");
    assert_eq!(
        compiled.head_sample_rate, 0.5,
        "the narrower level won the value it set"
    );
    assert!(
        compiled.blocked_event_names.contains("debug-ping"),
        "the wider level's refusal still holds"
    );
    assert!(compiled.blocked_property_keys.contains("password"));
    assert_eq!(compiled.from_levels, vec!["installation", "project:p"]);
}

#[test]
fn invalid_policy_never_replaces_valid_policy() {
    use tallyowl_head::policy::{Document, Policies, Scope};

    let mut policies = Policies::new();
    let mut good = Document::at(Scope::Project, "p");
    good.head_sample_rate = Some(0.25);
    policies.put(good).expect("valid");

    let mut bad = Document::at(Scope::Project, "p");
    bad.head_sample_rate = Some(7.0);
    policies.put(bad).expect_err("a rate above one is refused");

    assert_eq!(
        policies.compile("", "p", "", "").head_sample_rate,
        0.25,
        "the valid policy is still the one that applies"
    );
}

#[test]
fn a_blocked_property_is_removed_and_a_redacted_one_keeps_its_name() {
    use tallyowl_head::policy::{Document, Policies, Scope, REDACTED};

    let mut policies = Policies::new();
    let mut document = Document::at(Scope::Project, tallyowl_store::row::hex(&PROJECT));
    document.blocked_property_keys = vec!["password".to_string()];
    document.redact_property_keys = vec!["email".to_string()];
    policies.put(document).expect("valid");
    let compiled = policies.compile("", &tallyowl_store::row::hex(&PROJECT), "", "");

    let mut row = known(1, "sign-in", BASE, "p1");
    for key in ["password", "email"] {
        row.properties.insert(
            key.to_string(),
            (PropertyValue::Text("secret".into()), "client".to_string()),
        );
    }
    let (blocked, redacted) = compiled.apply_properties(&mut row);
    assert_eq!((blocked, redacted), (1, 1));
    assert!(
        !row.properties.contains_key("password"),
        "a blocked key is gone"
    );
    assert_eq!(
        row.properties.get("email").map(|(value, _)| value.clone()),
        Some(PropertyValue::Text(REDACTED.to_string())),
        "a redacted key keeps its name so a query can see the field was there"
    );
}

// ---------------------------------------------------------------------------
// Saved analyses and dashboards
// ---------------------------------------------------------------------------

#[test]
fn a_dashboard_cannot_show_an_analysis_that_is_not_there() {
    use tallyowl_head::saved::{Analysis, Dashboard, Panel, Saved};

    let mut saved = Saved::new();
    let dashboard = Dashboard {
        dashboard_id: "d1".into(),
        project_id: PROJECT,
        name: "Overview".into(),
        panels: vec![Panel {
            analysis_id: "missing".into(),
            title: None,
            column: 0,
            row: 0,
            width: 6,
            height: 4,
        }],
        updated_at: BASE,
        updated_by: "tod".into(),
    };
    let refused = saved
        .put_dashboard(dashboard.clone())
        .expect_err("a panel with nothing behind it is refused");
    assert!(refused.message.contains("missing"), "{}", refused.message);

    saved
        .put_analysis(
            Analysis {
                analysis_id: "missing".into(),
                project_id: PROJECT,
                name: "Sign-ups".into(),
                form: "funnel".into(),
                request: vec![1, 2, 3],
                algebra_version: 1,
                created_at: BASE,
                updated_at: BASE,
                updated_by: "tod".into(),
            },
            1,
        )
        .expect("the analysis saves");
    saved
        .put_dashboard(dashboard)
        .expect("now the panel has something behind it");
}

#[test]
fn removing_an_analysis_a_dashboard_shows_is_refused_and_the_dashboard_is_named() {
    use tallyowl_head::saved::{Analysis, Dashboard, Panel, Saved};

    let mut saved = Saved::new();
    saved
        .put_analysis(
            Analysis {
                analysis_id: "a1".into(),
                project_id: PROJECT,
                name: "Sign-ups".into(),
                form: "funnel".into(),
                request: vec![1],
                algebra_version: 1,
                created_at: BASE,
                updated_at: BASE,
                updated_by: "tod".into(),
            },
            1,
        )
        .expect("saves");
    saved
        .put_dashboard(Dashboard {
            dashboard_id: "d1".into(),
            project_id: PROJECT,
            name: "Overview".into(),
            panels: vec![Panel {
                analysis_id: "a1".into(),
                title: None,
                column: 0,
                row: 0,
                width: 6,
                height: 4,
            }],
            updated_at: BASE,
            updated_by: "tod".into(),
        })
        .expect("saves");

    let refused = saved
        .remove_analysis(PROJECT, "a1")
        .expect_err("a dashboard shows it");
    assert!(refused.message.contains("Overview"), "{}", refused.message);
}

#[test]
fn an_analysis_written_for_a_newer_query_version_is_refused_with_both_numbers() {
    use tallyowl_head::saved::{Analysis, Saved};

    let mut saved = Saved::new();
    let refused = saved
        .put_analysis(
            Analysis {
                analysis_id: "a1".into(),
                project_id: PROJECT,
                name: "From the future".into(),
                form: "node".into(),
                request: vec![1],
                algebra_version: 99,
                created_at: BASE,
                updated_at: BASE,
                updated_by: "tod".into(),
            },
            1,
        )
        .expect_err("this build cannot answer it");
    assert!(
        refused.message.contains("99") && refused.message.contains("version 1"),
        "{}",
        refused.message
    );
}

#[test]
fn a_panel_that_runs_past_the_grid_is_refused_rather_than_wrapped() {
    use tallyowl_head::saved::{Analysis, Dashboard, Panel, Saved, COLUMNS};

    let mut saved = Saved::new();
    saved
        .put_analysis(
            Analysis {
                analysis_id: "a1".into(),
                project_id: PROJECT,
                name: "Wide".into(),
                form: "node".into(),
                request: vec![1],
                algebra_version: 1,
                created_at: BASE,
                updated_at: BASE,
                updated_by: "tod".into(),
            },
            1,
        )
        .expect("saves");
    let refused = saved
        .put_dashboard(Dashboard {
            dashboard_id: "d1".into(),
            project_id: PROJECT,
            name: "Overview".into(),
            panels: vec![Panel {
                analysis_id: "a1".into(),
                title: None,
                column: COLUMNS - 2,
                row: 0,
                width: 6,
                height: 4,
            }],
            updated_at: BASE,
            updated_by: "tod".into(),
        })
        .expect_err("a panel that wrapped would move every panel after it");
    assert!(refused.message.contains("12"), "{}", refused.message);
}

// ---------------------------------------------------------------------------
// Durability. L112.
//
// A policy and a saved analysis are control state, and `docs/FAILURE_MODES.md`
// section 7 lists saved control state among the nine things a catalog rebuild
// cannot restore. A restart certainly cannot either, so it must not have to.
// ---------------------------------------------------------------------------

/// Open a store twice over one directory, the way a restart does.
fn a_data_directory(name: &str) -> std::path::PathBuf {
    let path = directory(name);
    std::fs::create_dir_all(&path).expect("a directory");
    path
}

#[test]
fn a_collection_policy_survives_a_restart() {
    use tallyowl_control_api::types::{PolicyDocument, PolicyScope};
    use tallyowl_head::policy::PolicyService;

    let place = a_data_directory("policy-restart");
    let project = tallyowl_store::row::hex(&PROJECT);

    let document = PolicyDocument {
        scope: PolicyScope::Project,
        scope_id: Some(project.clone()),
        enabled_kinds: None,
        head_sample_rate: Some(0.25),
        session_max_lifetime_ms: None,
        max_event_bytes: None,
        max_properties: None,
        blocked_event_names: Some(vec!["debug-ping".into()]),
        blocked_property_keys: Some(vec!["password".into()]),
        redact_property_keys: None,
        campaign_linking: None,
        attribution_needs_consent: None,
        kill_switch: None,
    };

    let version = {
        let store = Arc::new(SegmentedStore::open(&place).expect("a store opens"));
        let (policy, refused) = PolicyService::open(Arc::clone(&store));
        assert!(refused.is_empty(), "{refused:?}");
        let compiled = policy.put(&document).expect("the policy is stored");
        assert_eq!(compiled.head_sample_rate, 0.25);
        assert!(policy.version() > 0, "a write raises the version");
        policy.version()
    };

    // A new process over the same directory. Nothing was handed across.
    let store = Arc::new(SegmentedStore::open(&place).expect("the store reopens"));
    let (policy, refused) = PolicyService::open(store);
    assert!(refused.is_empty(), "{refused:?}");
    assert_eq!(
        policy.version(),
        version,
        "the version a collector compares against is the same number after a restart"
    );

    let compiled = policy.for_project(PROJECT);
    assert_eq!(compiled.head_sample_rate, 0.25);
    assert!(
        compiled.blocked_event_names.contains("debug-ping"),
        "the refusal survived"
    );
    assert!(compiled.blocked_property_keys.contains("password"));
}

#[test]
fn a_saved_analysis_and_its_dashboard_survive_a_restart() {
    use tallyowl_control_api::types::{DashboardPanel, QueryForm, SavedAnalysis, SavedDashboard};
    use tallyowl_head::saved::SavedService;

    let place = a_data_directory("saved-restart");
    let analysis = SavedAnalysis {
        analysis_id: "sign-ups".into(),
        project_id: PROJECT.to_vec(),
        name: "Sign-ups".into(),
        form: QueryForm::Funnel,
        request: vec![1, 2, 3, 4],
        algebra_version: 1,
        created_at: None,
        updated_at: None,
        updated_by: None,
    };
    let dashboard = SavedDashboard {
        dashboard_id: "overview".into(),
        project_id: PROJECT.to_vec(),
        name: "Overview".into(),
        panels: vec![DashboardPanel {
            analysis_id: "sign-ups".into(),
            title: Some("How many people sign up".into()),
            column: 0,
            row: 0,
            width: 6,
            height: 4,
        }],
        updated_at: None,
        updated_by: None,
    };

    {
        let store = Arc::new(SegmentedStore::open(&place).expect("a store opens"));
        // The project has to exist, because a saved analysis is loaded for each
        // project the catalog names.
        store
            .catalog()
            .put_project(&tallyowl_store::control::Project {
                project_id: PROJECT,
                workspace_id: [8; 16],
                name: "seedstore".into(),
                description: None,
                created_at: BASE,
            })
            .expect("the project is stored");
        let (saved, refused) = SavedService::open(Arc::clone(&store), 1);
        assert!(refused.is_empty(), "{refused:?}");
        saved.put_analysis(&analysis, "tod").expect("it saves");
        saved.put_dashboard(&dashboard, "tod").expect("it saves");
    }

    let store = Arc::new(SegmentedStore::open(&place).expect("the store reopens"));
    let (saved, refused) = SavedService::open(store, 1);
    assert!(refused.is_empty(), "{refused:?}");

    let analyses = saved.list_analyses(PROJECT);
    assert_eq!(analyses.analyses.len(), 1);
    assert_eq!(analyses.analyses[0].name, "Sign-ups");
    assert_eq!(analyses.analyses[0].request, vec![1, 2, 3, 4]);
    assert_eq!(analyses.analyses[0].form, QueryForm::Funnel);
    assert_eq!(analyses.analyses[0].updated_by.as_deref(), Some("tod"));

    let dashboards = saved.list_dashboards(PROJECT);
    assert_eq!(dashboards.dashboards.len(), 1);
    assert_eq!(dashboards.dashboards[0].panels.len(), 1);
    assert_eq!(
        dashboards.dashboards[0].panels[0].title.as_deref(),
        Some("How many people sign up"),
        "a panel's own title survived, and did not fall back to the analysis's name"
    );

    // And removing it takes it out of the catalog too, so it does not come back.
    saved
        .remove_analysis(PROJECT, "sign-ups")
        .expect_err("a dashboard still shows it");
}

#[test]
fn removing_a_saved_analysis_removes_it_from_the_catalog_as_well() {
    use tallyowl_control_api::types::{QueryForm, SavedAnalysis};
    use tallyowl_head::saved::SavedService;

    let place = a_data_directory("saved-remove");
    let analysis = SavedAnalysis {
        analysis_id: "temporary".into(),
        project_id: PROJECT.to_vec(),
        name: "Temporary".into(),
        form: QueryForm::Node,
        request: vec![9],
        algebra_version: 1,
        created_at: None,
        updated_at: None,
        updated_by: None,
    };

    {
        let store = Arc::new(SegmentedStore::open(&place).expect("a store opens"));
        store
            .catalog()
            .put_project(&tallyowl_store::control::Project {
                project_id: PROJECT,
                workspace_id: [8; 16],
                name: "seedstore".into(),
                description: None,
                created_at: BASE,
            })
            .expect("the project is stored");
        let (saved, _) = SavedService::open(Arc::clone(&store), 1);
        saved.put_analysis(&analysis, "tod").expect("it saves");
        saved
            .remove_analysis(PROJECT, "temporary")
            .expect("nothing shows it");
    }

    let store = Arc::new(SegmentedStore::open(&place).expect("the store reopens"));
    let (saved, _) = SavedService::open(store, 1);
    assert!(
        saved.list_analyses(PROJECT).analyses.is_empty(),
        "a removal that only reached memory would bring it back here"
    );
}

#[test]
fn a_stored_policy_this_build_cannot_read_is_named_rather_than_stopping_the_head() {
    use tallyowl_head::policy::PolicyService;

    let place = a_data_directory("policy-unreadable");
    let store = Arc::new(SegmentedStore::open(&place).expect("a store opens"));
    // A level a newer release wrote, and one this build understands.
    store
        .catalog()
        .put_policy(&tallyowl_store::control::PolicyRecord {
            scope: "constellation".into(),
            scope_id: "somewhere".into(),
            ..Default::default()
        })
        .expect("it is stored");
    store
        .catalog()
        .put_policy(&tallyowl_store::control::PolicyRecord {
            scope: "installation".into(),
            head_sample_rate: Some(0.5),
            ..Default::default()
        })
        .expect("it is stored");

    let (policy, refused) = PolicyService::open(Arc::clone(&store));
    assert_eq!(refused.len(), 1, "one record did not load: {refused:?}");
    assert!(
        refused[0].contains("constellation"),
        "the refusal names which one: {}",
        refused[0]
    );
    assert_eq!(
        policy.for_project(PROJECT).head_sample_rate,
        0.5,
        "the level this build understands still applies"
    );
}

/// An installation nobody has configured has no policy to send. Phase 9.
///
/// The running loop found this. A clean installation answered every collector
/// fetch with a snapshot at version 0, the collector refused it — correctly,
/// because the head raises the version on every write and never hands out 0 for
/// a policy somebody set — and the collector logged a refusal every fetch
/// interval for ever.
///
/// The honest answer is that there is nothing here to apply, which is what a
/// collector that holds no policy already means: it collects everything.
#[test]
fn a_head_with_no_policy_at_all_sends_none_rather_than_one_at_version_zero() {
    use std::sync::Arc;
    use tallyowl_collector_api::codec::{
        decode_fetch_policy_response, encode_fetch_policy_request,
    };
    use tallyowl_collector_api::types::FetchPolicyRequest;
    use tallyowl_control_api::types::{PolicyDocument, PolicyScope};
    use tallyowl_head::service::HeadService;
    use tallyowl_obs::log::{Logger, Severity};
    use tallyowl_obs::metrics::Registry;
    use tallyowl_rpc::{Dispatcher, Outcome, Request};

    let place = a_data_directory("policy-none");
    let segmented =
        Arc::new(tallyowl_store::SegmentedStore::open(&place).expect("the store opens"));
    let store: Arc<dyn tallyowl_store::Store> =
        Arc::clone(&segmented) as Arc<dyn tallyowl_store::Store>;

    let workspace = tallyowl_store::control::Workspace {
        workspace_id: [1; 16],
        name: "home".into(),
        created_at: 0,
    };
    let project = tallyowl_store::control::Project {
        project_id: PROJECT,
        workspace_id: workspace.workspace_id,
        name: "local".into(),
        description: None,
        created_at: 0,
    };
    let source = tallyowl_store::control::Source {
        source_id: [3; 16],
        project_id: project.project_id,
        workspace_id: workspace.workspace_id,
        name: "reference".into(),
        created_at: 0,
    };
    segmented.catalog().put_workspace(&workspace).unwrap();
    segmented.catalog().put_project(&project).unwrap();
    segmented.catalog().put_source(&source).unwrap();

    let policy = Arc::new(tallyowl_head::policy::PolicyService::open(Arc::clone(&segmented)).0);
    let head = HeadService {
        ingest: Arc::new(tallyowl_head::ingest::Ingest {
            golden_signal_bucket_ms: 60_000,
            store: Arc::clone(&store),
            metrics: Registry::new(),
            receipt_policy: tallyowl_collector_api::types::ReceiptPolicy::LocalOne,
            open_traces: None,
            policy: Some(Arc::clone(&policy)),
        }),
        enrollment: Arc::new(tallyowl_head::enrollment::EnrollmentService {
            store: Arc::clone(&segmented),
            metrics: Registry::new(),
        }),
        query: Arc::new(QueryService {
            store: Arc::clone(&store),
            max_runtime_ms: 30_000,
            max_expression_depth: tallyowl_head::expr::DEFAULT_MAX_DEPTH,
            guards: Guards::default(),
            attribution: Default::default(),
            policy: Arc::clone(&policy),
            identity: Default::default(),
        }),
        control: Arc::new(tallyowl_head::control::ControlService {
            store: Arc::clone(&segmented),
            metrics: Registry::new(),
            key_cache_ttl_ms: 30_000,
        }),
        sign_in: Arc::new(tallyowl_head::linkkeys::SignIn {
            store: Arc::clone(&segmented),
            metrics: Registry::new(),
            settings: tallyowl_head::linkkeys::LinkKeysSettings {
                enabled: false,
                trusted_domains: Vec::new(),
                callback_url: String::new(),
                app_name: "TallyOwl".into(),
                session_lifetime_ms: 3_600_000,
            },
        }),
        logger: Arc::new(Logger::new("tallyowl-head", "0.0.0", Severity::Error)),
        policy: Arc::clone(&policy),
        saved: Arc::new(tallyowl_head::saved::SavedService::default()),
        attribution: Arc::new(tallyowl_head::attribution::AttributionService::default()),
        alerts: None,
        workflows: None,
    };

    let ask = |known: Option<u64>| {
        let request = Request {
            service: "TallyOwlCollector".to_string(),
            op: "fetch-policy".to_string(),
            id: Some(1),
            auth: None,
            payload: encode_fetch_policy_request(&FetchPolicyRequest {
                source_id: source.source_id.to_vec(),
                known_version: known,
            }),
        };
        match head.dispatch(&request) {
            Outcome::Reply { payload, .. } => {
                decode_fetch_policy_response(&payload).expect("a policy answer")
            }
            _ => panic!("the head refused the collection policy fetch"),
        }
    };

    // Nothing configured. There is nothing to apply, and the answer says so
    // rather than sending the defaults at version 0.
    let answer = ask(None);
    assert!(answer.unchanged);
    assert!(
        answer.policy.is_none(),
        "an installation with no policy sent one anyway"
    );

    // Now set one. The next fetch carries it, at a version a collector can
    // apply.
    head.policy
        .put(&PolicyDocument {
            scope: PolicyScope::Project,
            scope_id: Some(tallyowl_store::row::hex(&project.project_id)),
            enabled_kinds: None,
            head_sample_rate: None,
            session_max_lifetime_ms: None,
            max_event_bytes: None,
            max_properties: None,
            blocked_event_names: Some(vec!["debug-ping".into()]),
            blocked_property_keys: None,
            redact_property_keys: None,
            campaign_linking: None,
            attribution_needs_consent: None,
            kill_switch: None,
        })
        .expect("the policy is stored");

    let answer = ask(None);
    assert!(!answer.unchanged);
    let sent = answer.policy.expect("a policy this time");
    assert!(sent.policy_version > 0, "a stored policy has a version");
    assert_eq!(
        sent.blocked_event_names.as_deref(),
        Some(["debug-ping".to_string()].as_slice())
    );

    // And a collector that already holds that version gets nothing back, which
    // is what makes a short fetch interval affordable.
    let answer = ask(Some(sent.policy_version));
    assert!(answer.unchanged);
    assert!(answer.policy.is_none());
}

// ---------------------------------------------------------------------------
// Calendar periods. L110 asked for these and L134 built them.
// ---------------------------------------------------------------------------

/// A retention matrix over calendar months, in a named zone.
fn monthly_retention(from: &str, to: &str, zone: Option<&str>, periods: u64) -> QueryRequest {
    let mut request = query::empty_request(1, QueryForm::Retention);
    request.retention = Some(RetentionQuery {
        project_id: PROJECT.to_vec(),
        range: TimeRange {
            range_start: at(from),
            range_end: at(to),
            basis: TimeBasis::OccurredAt,
            timezone: zone.map(str::to_string),
        },
        initial: matches_name("sign-up"),
        returning: matches_name("visit"),
        period: Period::Month,
        periods,
        first_time_only: false,
        resolution: None,
    });
    request
}

fn at(text: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(text)
        .expect("a timestamp")
        .timestamp_millis()
}

#[test]
fn a_monthly_retention_matrix_counts_calendar_months() {
    // **This is the fixture L110 could not write.** One person signs up on
    // 15 January and visits on 15 February, 15 March, and 15 April. Those are
    // months one, two, and three, and a person can check the row by reading it.
    //
    // It is worth saying that this fixture agrees with the 28-day rule, because
    // the two rules only part company once the drift reaches a whole period.
    // The next test is the one that separates them.
    let head = head(
        "retention-months",
        vec![
            known(1, "sign-up", at("2026-01-15T12:00:00Z"), "p1"),
            known(2, "visit", at("2026-02-15T12:00:00Z"), "p1"),
            known(3, "visit", at("2026-03-15T12:00:00Z"), "p1"),
            known(4, "visit", at("2026-04-15T12:00:00Z"), "p1"),
        ],
    );
    let rows = run(
        &head,
        monthly_retention("2026-01-01T00:00:00Z", "2026-06-01T00:00:00Z", None, 5),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(unsigned(&rows[0][1]), 1, "one person");
    assert_eq!(
        unsigned(&rows[0][2]),
        0,
        "nothing in January but the sign-up"
    );
    assert_eq!(unsigned(&rows[0][4]), 1, "February");
    assert_eq!(unsigned(&rows[0][6]), 1, "March");
    assert_eq!(unsigned(&rows[0][8]), 1, "April");
}

#[test]
fn a_month_and_a_twenty_eight_day_period_part_company_in_august() {
    // **The row this moves.** From 1 January, 15 August is day 226. Twenty-eight
    // days divides into that eight times, so the old rule filed an August visit
    // under the ninth column of the matrix. It is the seventh month after
    // January, and the seventh column is where a person looks for it.
    //
    // 1 August is day 212 and still lands in period seven both ways, which is
    // why the drift has to be checked on a date that is far enough into the
    // month. The two rules agree on every earlier month of this range.
    let head = head(
        "retention-drift",
        vec![
            known(1, "sign-up", at("2026-01-15T12:00:00Z"), "p1"),
            known(2, "visit", at("2026-08-15T12:00:00Z"), "p1"),
        ],
    );
    let rows = run(
        &head,
        monthly_retention("2026-01-01T00:00:00Z", "2026-12-01T00:00:00Z", None, 10),
    );
    assert_eq!(unsigned(&rows[0][1]), 1);
    assert_eq!(
        unsigned(&rows[0][2 + 2 * 7]),
        1,
        "August is the seventh month after January"
    );
    assert_eq!(
        unsigned(&rows[0][2 + 2 * 8]),
        0,
        "the eighth column is where twenty-eight days put it"
    );
}

#[test]
fn a_cohort_starts_at_local_midnight_in_the_zone_the_range_named() {
    // 23:30 UTC on 31 January is already 1 February in Berlin, so this person
    // belongs to the February cohort there and to the January cohort in UTC.
    // The same rows, two right answers, and the query says which it asked for.
    let rows = vec![
        known(1, "sign-up", at("2026-01-31T23:30:00Z"), "p1"),
        known(2, "visit", at("2026-02-15T12:00:00Z"), "p1"),
    ];
    let utc = head("retention-zone-utc", rows.clone());
    let berlin = head("retention-zone-berlin", rows);

    let in_utc = run(
        &utc,
        monthly_retention("2026-01-01T00:00:00Z", "2026-04-01T00:00:00Z", None, 3),
    );
    assert_eq!(
        signed(&in_utc[0][0]),
        at("2026-01-01T00:00:00Z"),
        "the cohort starts in January in UTC"
    );

    let in_berlin = run(
        &berlin,
        monthly_retention(
            "2026-01-01T00:00:00Z",
            "2026-04-01T00:00:00Z",
            Some("Europe/Berlin"),
            3,
        ),
    );
    assert_eq!(
        signed(&in_berlin[0][0]),
        at("2026-01-31T23:00:00Z"),
        "the Berlin February starts at 23:00 UTC on 31 January"
    );
}

// ---------------------------------------------------------------------------
// The materialised identity graph. L103 asked for it and L135 built it.
// ---------------------------------------------------------------------------

/// A head whose identity graph is never reused, which is what every question
/// did before the graph was materialised.
fn head_without_materialisation(name: &str, rows: Vec<EventRow>) -> QueryService {
    let mut service = head(name, rows);
    service.identity = Arc::new(tallyowl_head::identity::IdentityCache::new(
        std::time::Duration::ZERO,
    ));
    service
}

/// A funnel over a sign-up flow, which is the shape that needs identity.
fn identity_funnel() -> QueryRequest {
    let mut request = query::empty_request(1, QueryForm::Funnel);
    request.funnel = Some(FunnelQuery {
        project_id: PROJECT.to_vec(),
        range: TimeRange {
            range_start: BASE,
            range_end: BASE + 30 * DAY,
            basis: TimeBasis::OccurredAt,
            timezone: None,
        },
        steps: vec![step("visit", false), step("purchase", false)],
        window_ms: 30 * DAY,
        basis: CorrelationBasis::EndUser,
        ordered: false,
        breakdown: None,
        resolution: None,
    });
    request
}

#[test]
fn a_materialised_graph_answers_what_a_rebuilt_one_answers() {
    // The property that makes a cache safe: it is a cache. The same rows, the
    // same question, one head that reuses its graph and one that never does.
    // A person visits anonymously, identifies, and buys as themselves, so the
    // answer depends on the graph rather than on the rows alone.
    let rows = vec![
        anonymous(1, "visit", BASE + 60_000, "a1"),
        identify(2, BASE + 120_000, "a1", "person-1"),
        known(3, "purchase", BASE + 180_000, "person-1"),
    ];
    let cached = head("identity-materialised", rows.clone());
    let rebuilt = head_without_materialisation("identity-rebuilt", rows);

    let first = run(&cached, identity_funnel());
    // The second question is the one that reads the held graph rather than
    // building one.
    let second = run(&cached, identity_funnel());
    let never_held = run(&rebuilt, identity_funnel());

    assert_eq!(first, second, "the held graph answered differently");
    assert_eq!(
        first, never_held,
        "materialising the graph changed the answer"
    );
    assert_eq!(
        unsigned(&first[0][2]),
        1,
        "one person reached the first step"
    );
    assert_eq!(
        unsigned(&first[1][2]),
        1,
        "and the same person reached the second, which needs the identify"
    );
}

#[test]
fn a_held_graph_does_not_let_a_later_identify_change_a_past_answer() {
    // **A cache must not make an answer depend on how fresh it is.** A held
    // graph knows every identity row the installation has, including rows after
    // the range a question asks about. A question about last week has to answer
    // with what was known last week — the same answer before the graph was
    // held, and the same answer tomorrow.
    let head = head(
        "identity-horizon",
        vec![
            anonymous(1, "visit", BASE + 60_000, "a1"),
            known(2, "purchase", BASE + 180_000, "person-1"),
        ],
    );
    let before = run(&head, identity_funnel());
    assert_eq!(
        unsigned(&before[1][2]),
        0,
        "nothing links the anonymous visit to the purchase yet"
    );

    // An `identify` well after the range the question asks about.
    head.store
        .commit(
            [7; 16],
            [2; 16],
            vec![identify(3, BASE + 90 * DAY, "a1", "person-1")],
        )
        .expect("the identify commits");

    let after = run(&head, identity_funnel());
    assert_eq!(
        before, after,
        "an identify from after the range changed an answer about the range"
    );
}

#[test]
fn an_erasure_is_not_answered_from_a_graph_built_before_it() {
    // The graph *is* the rows, so removing a row removes what it said. A cache
    // in front of it would keep it, and the erasure generation is what stops a
    // held graph from answering after one.
    //
    // **The erasure names the `identify` row alone, and that is the point.**
    // Erasing the person's events would prove nothing: the rows would be hidden
    // at the scan and the funnel would count nobody whether the graph was held
    // or rebuilt. Removing only the link leaves both events visible and makes
    // the two graphs disagree — a held one still ties the anonymous visit to the
    // purchase, and a rebuilt one has nothing to tie them with.
    let head = head(
        "identity-erasure",
        vec![
            anonymous(1, "visit", BASE + 60_000, "a1"),
            identify(2, BASE + 120_000, "a1", "person-1"),
            known(3, "purchase", BASE + 180_000, "person-1"),
        ],
    );
    let before = run(&head, identity_funnel());
    assert_eq!(unsigned(&before[1][2]), 1, "the person converted");

    head.store
        .erase(&tallyowl_store::catalog::Tombstone {
            tombstone_id: [9; 16],
            generation: 0,
            project_id: PROJECT,
            event_ids: vec![[2; 16]],
            property: None,
            except_kinds: Vec::new(),
            range: None,
            requested_at: BASE,
            horizon: BASE + 365 * DAY,
            reason: "a person asked".to_string(),
        })
        .expect("the erasure is recorded");

    let after = run(&head, identity_funnel());
    assert_eq!(
        unsigned(&after[0][2]),
        1,
        "the visit is still there; only the link was removed"
    );
    assert_eq!(
        unsigned(&after[1][2]),
        0,
        "a graph built before the erasure still tied the two events together"
    );
}
