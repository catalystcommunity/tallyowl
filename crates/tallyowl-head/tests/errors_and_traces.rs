//! Phase 5: errors, traces, and tail sampling, against a real store.
//!
//! `docs/PLAN.md` Phase 5 gives the exit criteria. Each one has a case here:
//!
//! | Exit criterion | Where |
//! | --- | --- |
//! | One frontend error and one backend error correlate to a trace, session, and release | `errors_correlate_*` |
//! | The projector rebuilds grouping at a new version from raw data | `a_group_is_rebuilt_*` |
//! | Sensitive-data fixtures prove default scrubbing | `crates/tallyowl-collector/src/tests.rs` and `tallyowl-wire` |
//! | A tail-sampled trace keeps every one of its spans | `a_kept_trace_keeps_every_span` |
//! | A dropped trace leaves no queryable span | `a_dropped_trace_leaves_no_queryable_span` |
//! | An always-keep error survives when the tail rules drop its trace | `an_error_survives_the_drop_of_its_trace` |

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tallyowl_collector_api::types::{
    Batch, CommitBatchRequest, Envelope, ErrorPayload, ErrorPayload_severity as Severity,
    ReceiptPolicy, SpanKind, SpanPayload, SpanPayload_status as SpanStatus, StackFrame,
    TelemetryItem, TelemetryKind,
};
use tallyowl_control_api::types::{QueryForm, TraceQuery};
use tallyowl_head::errors::{fingerprint, Rule};
use tallyowl_head::ingest::Ingest;
use tallyowl_head::query::QueryService;
use tallyowl_head::sampling::{OpenTraces, TailSampler, TailSettings};
use tallyowl_obs::log::{Logger, Severity as LogSeverity};
use tallyowl_obs::metrics::Registry;
use tallyowl_store::{SegmentedStore, Store};
use tallyowl_wire::{collector_items_bridge as items, control as wire, query, Value};

const PROJECT: [u8; 16] = [9; 16];
const WORKSPACE: [u8; 16] = [8; 16];
const SOURCE: [u8; 16] = [7; 16];
const TRACE: [u8; 16] = [0x11; 16];
const OTHER_TRACE: [u8; 16] = [0x22; 16];
const BASE_TIME: i64 = 1_785_628_800_000;

fn directory(name: &str) -> std::path::PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("target"));
    let path = base
        .join("phase5-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// A whole head: the commit path, the query path, and the tail projector, over
/// one real store.
struct Head {
    ingest: Ingest,
    query: QueryService,
    sampler: Arc<TailSampler>,
    store: Arc<SegmentedStore>,
}

fn head(name: &str, settings: TailSettings) -> Head {
    let segmented = Arc::new(SegmentedStore::open(directory(name)).expect("open"));
    let store: Arc<dyn Store> = Arc::clone(&segmented) as Arc<dyn Store>;
    let metrics = Registry::new();
    Ingest::declare_metrics(&metrics);
    TailSampler::declare_metrics(&metrics);
    let open = OpenTraces::new();
    Head {
        ingest: Ingest {
            golden_signal_bucket_ms: 60_000,
            store: Arc::clone(&store),
            metrics: Arc::clone(&metrics),
            receipt_policy: ReceiptPolicy::LocalOne,
            open_traces: Some(Arc::clone(&open)),
            policy: None,
        },
        query: QueryService {
            store: Arc::clone(&store),
            max_runtime_ms: 30_000,
            max_expression_depth: tallyowl_head::expr::DEFAULT_MAX_DEPTH,
            guards: tallyowl_head::analysis::Guards::default(),
            attribution: Default::default(),
            policy: Default::default(),
            identity: Default::default(),
        },
        sampler: Arc::new(TailSampler {
            store: Arc::clone(&segmented),
            erasing: Arc::clone(&store),
            open,
            settings,
            metrics,
            logger: Arc::new(Logger::new("test", "0.0.0", LogSeverity::Error)),
            stopping: AtomicBool::new(false),
            late_after_grace: AtomicU64::new(0),
        }),
        store: segmented,
    }
}

fn envelope(id: u8, kind: TelemetryKind, at: i64) -> Envelope {
    Envelope {
        event_id: vec![id; 16],
        kind,
        schema_version: 1,
        occurred_at: at,
        observed_at: None,
        received_at: Some(at),
        workspace_id: Some(WORKSPACE.to_vec()),
        project_id: Some(PROJECT.to_vec()),
        source_id: Some(SOURCE.to_vec()),
        sequence: None,
        release: Some("2026.8.1".into()),
        service_name: Some("checkout".into()),
        request_id: Some("req-1".into()),
        session_id: Some("s-1".into()),
        end_user_id: None,
        anonymous_id: None,
        trace_id: Some(TRACE.to_vec()),
        span_id: None,
        consent: None,
        sdk_name: "test".into(),
        sdk_version: "0".into(),
        properties: Vec::new(),
        measurements: None,
    }
}

fn span(id: u8, operation: &str, at: i64, duration_ms: i64, parent: Option<u8>) -> TelemetryItem {
    let mut item = items::empty_item(envelope(id, TelemetryKind::Span, at));
    item.envelope.span_id = Some(vec![id; 8]);
    item.span = Some(SpanPayload {
        operation: operation.into(),
        kind: SpanKind::Server,
        start_at: at,
        duration_ms,
        status: SpanStatus::Ok,
        resource: None,
        parent_span_id: parent.map(|p| vec![p; 8]),
        links: None,
        error_event_id: None,
        sampling_reason: None,
    });
    item
}

fn failing_span(id: u8, at: i64, error_event_id: Option<u8>) -> TelemetryItem {
    let mut item = span(id, "POST /charge", at, 5, None);
    if let Some(payload) = item.span.as_mut() {
        payload.status = SpanStatus::Error;
        payload.error_event_id = error_event_id.map(|e| vec![e; 16]);
    }
    item
}

fn error_item(id: u8, at: i64, error_type: &str, message: &str, in_app: bool) -> TelemetryItem {
    let mut item = items::empty_item(envelope(id, TelemetryKind::Error, at));
    item.error = Some(ErrorPayload {
        error_type: error_type.into(),
        message: message.into(),
        handled: false,
        severity: Severity::Error,
        mechanism: None,
        runtime: Some("rust".into()),
        frames: Some(vec![StackFrame {
            module: Some("checkout".into()),
            function: Some("charge".into()),
            file: Some("/app/checkout.rs".into()),
            line: Some(40),
            in_app,
        }]),
        breadcrumbs: None,
    });
    item
}

fn commit(head: &Head, batch: u8, items: Vec<TelemetryItem>) {
    head.ingest
        .commit(CommitBatchRequest {
            batch: Batch {
                batch_id: vec![batch; 16],
                items,
                common_properties: None,
                sealed_at: BASE_TIME,
                compression: None,
            },
            source_id: SOURCE.to_vec(),
            attempt: None,
            protocol_version: None,
        })
        .expect("the batch commits");
}

/// Every span this installation can still see for one trace.
fn trace_rows(head: &Head, trace_id: [u8; 16]) -> Vec<Vec<Value>> {
    let mut request = query::empty_request(1, QueryForm::Trace);
    request.trace = Some(TraceQuery {
        project_id: PROJECT.to_vec(),
        trace_id: trace_id.to_vec(),
    });
    let response = head.query.run(request).expect("the trace query answers");
    response
        .rows
        .iter()
        .map(|row| row.values.iter().map(|v| wire::read(v).unwrap()).collect())
        .collect()
}

// ---------------------------------------------------------------------------
// Correlation
// ---------------------------------------------------------------------------

#[test]
fn a_backend_error_correlates_to_its_trace_session_and_release() {
    let head = head("correlate", TailSettings::default());
    commit(
        &head,
        1,
        vec![error_item(1, BASE_TIME, "Timeout", "took too long", true)],
    );

    let rows = head
        .store
        .scan(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 1_000,
            tallyowl_store::TimeBasis::OccurredAt,
        )
        .expect("the scan reads")
        .rows;
    let row = rows.first().expect("one error");
    assert_eq!(row.trace_id, Some(TRACE));
    assert_eq!(row.session_id.as_deref(), Some("s-1"));
    assert_eq!(row.release.as_deref(), Some("2026.8.1"));
    assert_eq!(row.request_id.as_deref(), Some("req-1"));
}

#[test]
fn an_error_and_a_span_meet_through_the_trace_without_a_join() {
    // The link that answers "what went wrong in this trace?" from both sides:
    // the error carries the trace ID, and the span carries the error's event
    // ID. Neither side needs the other to have arrived first.
    let head = head("link", TailSettings::default());
    commit(
        &head,
        1,
        vec![
            error_item(1, BASE_TIME, "Timeout", "took too long", true),
            failing_span(2, BASE_TIME + 1, Some(1)),
        ],
    );

    let rows = trace_rows(&head, TRACE);
    assert_eq!(
        rows.len(),
        2,
        "the error and the span are both in the trace"
    );
    let linked = rows
        .iter()
        .find(|row| row[9] != Value::Null)
        .expect("the span names its error");
    assert_eq!(linked[9].to_display(), "01".repeat(16));
}

// ---------------------------------------------------------------------------
// Grouping
// ---------------------------------------------------------------------------

#[test]
fn two_occurrences_of_one_defect_reach_one_group_and_a_third_does_not() {
    let head = head("grouping", TailSettings::default());
    commit(
        &head,
        1,
        vec![
            error_item(1, BASE_TIME, "Timeout", "took 30 seconds", true),
            error_item(2, BASE_TIME + 1, "Timeout", "took 41 seconds", true),
            error_item(3, BASE_TIME + 2, "Refused", "connection refused", true),
        ],
    );

    let request = query::breakdown(
        1,
        query::events(&PROJECT, whole_range()),
        "error_group",
        "occurrences",
    );
    let response = head.query.run(request).expect("the breakdown answers");
    let counts: Vec<u64> = response
        .rows
        .iter()
        .filter_map(|row| match wire::read(&row.values[1]) {
            Ok(Value::Unsigned(count)) => Some(count),
            _ => None,
        })
        .collect();
    assert_eq!(response.rows.len(), 2, "two groups, not three");
    assert!(counts.contains(&2));
    assert!(counts.contains(&1));
}

#[test]
fn a_group_is_rebuilt_from_the_stored_inputs_rather_than_from_the_digest() {
    // D39: a later fingerprint version rebuilds every group from retained raw
    // data. That only works if the inputs are stored, so this asserts they are.
    let head = head("rebuild", TailSettings::default());
    commit(
        &head,
        1,
        vec![error_item(1, BASE_TIME, "Timeout", "slow", true)],
    );

    let row = head
        .store
        .scan(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 1_000,
            tallyowl_store::TimeBasis::OccurredAt,
        )
        .unwrap()
        .rows
        .pop()
        .expect("one error");

    let inputs = row.properties["error_group_inputs"].0.to_display();
    assert!(inputs.contains("Timeout"));
    assert!(inputs.contains("checkout"));
    assert_eq!(row.properties["error_group_version"].0.to_display(), "1");
    assert_eq!(
        row.properties["error_group_rule"].0.to_display(),
        Rule::InAppFrames.as_str()
    );
}

#[test]
fn a_producer_cannot_name_its_own_group() {
    // The projector computes the fingerprint. A client that could name its own
    // group could split one defect into a thousand groups.
    let mut item = error_item(1, BASE_TIME, "Timeout", "slow", true);
    item.envelope
        .properties
        .push(tallyowl_wire::collector::property(
            "error_group",
            Value::Text("a group I chose".into()),
            tallyowl_collector_api::types::PropertyOrigin::Client,
        ));
    let head = head("no-client-group", TailSettings::default());
    commit(&head, 1, vec![item]);

    let row = head
        .store
        .scan(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 1_000,
            tallyowl_store::TimeBasis::OccurredAt,
        )
        .unwrap()
        .rows
        .pop()
        .unwrap();
    let stored = row.properties["error_group"].0.to_display();
    assert_ne!(stored, "a group I chose");
    assert_eq!(
        stored,
        fingerprint(
            error_item(1, 0, "Timeout", "slow", true)
                .error
                .as_ref()
                .unwrap()
        )
        .digest
    );
}

#[test]
fn a_release_is_a_column_a_regression_check_can_group_by() {
    // Release tracking, and the shape a regression is found in: one group seen
    // in a release, absent in the next, and back in the one after.
    let head = head("release", TailSettings::default());
    let mut first = error_item(1, BASE_TIME, "Timeout", "slow", true);
    first.envelope.release = Some("2026.8.1".into());
    let mut later = error_item(2, BASE_TIME + 1, "Timeout", "slow", true);
    later.envelope.release = Some("2026.8.3".into());
    commit(&head, 1, vec![first, later]);

    let request = query::breakdown(1, query::events(&PROJECT, whole_range()), "release", "seen");
    let response = head.query.run(request).unwrap();
    let releases: Vec<String> = response
        .rows
        .iter()
        .map(|row| wire::read(&row.values[0]).unwrap().to_display())
        .collect();
    assert_eq!(releases, vec!["2026.8.1", "2026.8.3"]);
}

fn whole_range() -> tallyowl_control_api::types::TimeRange {
    tallyowl_control_api::types::TimeRange {
        range_start: BASE_TIME - 1,
        range_end: BASE_TIME + 86_400_000,
        basis: tallyowl_control_api::types::TimeBasis::OccurredAt,
        timezone: None,
    }
}

// ---------------------------------------------------------------------------
// Trace assembly
// ---------------------------------------------------------------------------

#[test]
fn a_trace_comes_back_as_a_waterfall_with_parents_before_children() {
    let head = head("waterfall", TailSettings::default());
    commit(
        &head,
        1,
        vec![
            // Deliberately out of order, because a producer sends them that way.
            span(3, "SELECT orders", BASE_TIME + 20, 5, Some(2)),
            span(1, "GET /checkout", BASE_TIME, 100, None),
            span(2, "charge", BASE_TIME + 10, 40, Some(1)),
        ],
    );

    let rows = trace_rows(&head, TRACE);
    let depths: Vec<String> = rows.iter().map(|row| row[0].to_display()).collect();
    let operations: Vec<String> = rows.iter().map(|row| row[4].to_display()).collect();
    assert_eq!(depths, vec!["0", "1", "2"]);
    assert_eq!(operations, vec!["GET /checkout", "charge", "SELECT orders"]);
}

#[test]
fn a_span_whose_parent_is_missing_is_shown_rather_than_hidden() {
    // A hidden span is the one somebody is looking for. A parent can be absent
    // because it was dropped, because it is in another project, or because it
    // has not arrived yet.
    let head = head("orphan", TailSettings::default());
    commit(
        &head,
        1,
        vec![span(3, "SELECT orders", BASE_TIME, 5, Some(9))],
    );
    let rows = trace_rows(&head, TRACE);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].to_display(), "0", "it is shown as a root");
}

#[test]
fn one_project_cannot_assemble_another_projects_trace() {
    let head = head("trace-isolation", TailSettings::default());
    commit(
        &head,
        1,
        vec![span(1, "GET /checkout", BASE_TIME, 10, None)],
    );

    let mut request = query::empty_request(1, QueryForm::Trace);
    request.trace = Some(TraceQuery {
        project_id: [0xaa; 16].to_vec(),
        trace_id: TRACE.to_vec(),
    });
    let response = head.query.run(request).expect("the query answers");
    assert!(
        response.rows.is_empty(),
        "another project sees nothing, however good its guess at a trace ID"
    );
}

// ---------------------------------------------------------------------------
// Tail sampling
// ---------------------------------------------------------------------------

/// Sampling that drops every ordinary trace, so a test asserts the decision
/// rather than a share.
fn drop_everything_ordinary() -> TailSettings {
    TailSettings {
        decision_window_ms: 1_000,
        late_span_grace_ms: 500,
        keep_percent: 0.0,
        keep_slower_than_ms: i64::MAX,
    }
}

#[test]
fn a_kept_trace_keeps_every_one_of_its_spans() {
    let head = head(
        "tail-keep",
        TailSettings {
            keep_percent: 100.0,
            ..drop_everything_ordinary()
        },
    );
    commit(
        &head,
        1,
        vec![
            span(1, "GET /checkout", BASE_TIME, 100, None),
            span(2, "charge", BASE_TIME + 10, 40, Some(1)),
            span(3, "SELECT orders", BASE_TIME + 20, 5, Some(2)),
        ],
    );

    // The window closes.
    head.sampler.sweep(BASE_TIME + 10_000);
    assert_eq!(trace_rows(&head, TRACE).len(), 3, "every span, not most");
}

#[test]
fn a_dropped_trace_leaves_no_queryable_span() {
    let head = head("tail-drop", drop_everything_ordinary());
    commit(
        &head,
        1,
        vec![
            span(1, "GET /health", BASE_TIME, 3, None),
            span(2, "SELECT 1", BASE_TIME + 1, 1, Some(1)),
        ],
    );
    assert_eq!(trace_rows(&head, TRACE).len(), 2, "before the decision");

    let decided = head.sampler.sweep(BASE_TIME + 10_000);
    assert_eq!(decided, 1);
    assert!(
        trace_rows(&head, TRACE).is_empty(),
        "a dropped trace leaves nothing a query can reach"
    );
}

#[test]
fn an_error_survives_the_drop_of_its_trace() {
    // D35: "An unhandled error or a critical business event must survive even
    // when the tail rules later drop its trace."
    //
    // The rules keep a trace holding an error, so this test forces the drop by
    // judging the spans alone and then dropping the whole trace anyway, which
    // is what a stricter operator rule set would do.
    let head = head("tail-always-keep", drop_everything_ordinary());
    commit(
        &head,
        1,
        vec![
            span(1, "GET /checkout", BASE_TIME, 3, None),
            error_item(2, BASE_TIME + 1, "Timeout", "slow", true),
        ],
    );

    // The shipped rules keep this trace, because it holds an error. That is
    // the first half of the promise.
    head.sampler.sweep(BASE_TIME + 10_000);
    assert_eq!(trace_rows(&head, TRACE).len(), 2);

    // The second half: a tombstone that names the trace still leaves the error.
    let tombstone = tallyowl_store::catalog::Tombstone {
        tombstone_id: [0x33; 16],
        generation: 0,
        project_id: PROJECT,
        event_ids: Vec::new(),
        property: Some(("trace_id".into(), tallyowl_store::row::hex(&TRACE))),
        except_kinds: tallyowl_head::sampling::ALWAYS_KEEP_KINDS
            .iter()
            .map(|k| k.to_string())
            .collect(),
        range: None,
        requested_at: BASE_TIME,
        horizon: BASE_TIME + 86_400_000,
        reason: "an operator rule set that drops everything".into(),
    };
    head.store.erase(&tombstone).expect("the erasure lands");

    let left = trace_rows(&head, TRACE);
    assert_eq!(left.len(), 1, "the span went and the error stayed");
    assert_eq!(left[0][3].to_display(), "error");
}

#[test]
fn a_span_that_arrives_after_the_grace_period_cannot_change_an_applied_decision() {
    let head = head("tail-late", drop_everything_ordinary());
    commit(&head, 1, vec![span(1, "GET /health", BASE_TIME, 3, None)]);
    head.sampler.sweep(BASE_TIME + 10_000);
    assert!(trace_rows(&head, TRACE).is_empty(), "the trace was dropped");

    // A late span of a dropped trace. The tombstone is a standing predicate, so
    // it hides the arrival that no list of event IDs could have named.
    commit(
        &head,
        2,
        vec![span(4, "SELECT 1", BASE_TIME + 40_000, 1, None)],
    );
    assert!(
        trace_rows(&head, TRACE).is_empty(),
        "a late span never resurrects a dropped trace"
    );

    // And the sampler counts it rather than deciding the trace again.
    head.sampler.sweep(BASE_TIME + 60_000);
    assert!(head.sampler.late_after_grace.load(Ordering::Relaxed) >= 1);
}

#[test]
fn a_trace_that_is_still_open_is_not_decided_yet() {
    let head = head("tail-open", drop_everything_ordinary());
    commit(&head, 1, vec![span(1, "GET /health", BASE_TIME, 3, None)]);
    // Inside the decision window.
    assert_eq!(head.sampler.sweep(BASE_TIME + 100), 0);
    assert_eq!(trace_rows(&head, TRACE).len(), 1);
}

#[test]
fn a_failing_span_keeps_its_trace_even_when_the_share_is_zero() {
    let head = head("tail-failing", drop_everything_ordinary());
    commit(&head, 1, vec![failing_span(1, BASE_TIME, None)]);
    head.sampler.sweep(BASE_TIME + 10_000);
    assert_eq!(
        trace_rows(&head, TRACE).len(),
        1,
        "a failed span is the one somebody is looking for"
    );
}

#[test]
fn a_slow_trace_keeps_itself() {
    let head = head(
        "tail-slow",
        TailSettings {
            keep_slower_than_ms: 50,
            ..drop_everything_ordinary()
        },
    );
    commit(&head, 1, vec![span(1, "GET /report", BASE_TIME, 900, None)]);
    head.sampler.sweep(BASE_TIME + 10_000);
    assert_eq!(trace_rows(&head, TRACE).len(), 1);
}

#[test]
fn one_traces_decision_never_reaches_another_trace() {
    let head = head("tail-isolation", drop_everything_ordinary());
    let mut other = span(5, "GET /other", BASE_TIME, 3, None);
    other.envelope.trace_id = Some(OTHER_TRACE.to_vec());
    // The second trace holds an error, so the rules keep it.
    let mut kept = error_item(6, BASE_TIME + 1, "Timeout", "slow", true);
    kept.envelope.trace_id = Some(OTHER_TRACE.to_vec());

    commit(
        &head,
        1,
        vec![span(1, "GET /health", BASE_TIME, 3, None), other, kept],
    );
    head.sampler.sweep(BASE_TIME + 10_000);

    assert!(trace_rows(&head, TRACE).is_empty(), "the ordinary one went");
    assert_eq!(trace_rows(&head, OTHER_TRACE).len(), 2, "the other stayed");
}

#[test]
fn a_decision_is_reproducible_rather_than_random() {
    // The same trace decides the same way on every node and after every
    // restart. A random source would make a sampled result unexplainable.
    let head = head(
        "tail-reproducible",
        TailSettings {
            keep_percent: 50.0,
            ..drop_everything_ordinary()
        },
    );
    let rows = vec![tallyowl_store::row::EventRow::new(
        [1; 16], "span", "op", BASE_TIME,
    )];
    let first = head.sampler.judge(&rows, TRACE);
    let second = head.sampler.judge(&rows, TRACE);
    assert_eq!(first, second);
}
