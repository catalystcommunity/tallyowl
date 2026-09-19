//! Head tests: the commit boundary and the query contract.

use std::path::PathBuf;
use std::sync::Arc;

use tallyowl_collector_api::types::{
    Batch, CommitBatchRequest, Envelope, EventPayload, ReceiptPolicy, TelemetryItem, TelemetryKind,
};
use tallyowl_control_api::codec::{decode_query_node_box, encode_query_node_box};
use tallyowl_control_api::types::{
    AggregateNode, Dataset, Interval, MeasureKind, QueryNodeKind, QueryRequest, ScanNode,
    TimeBasis, TimeRange,
};
use tallyowl_obs::metrics::Registry;
use tallyowl_store::{SegmentedStore, Store};
use tallyowl_wire::{collector_items_bridge as items, control as wire, query, Value};

use crate::ingest::Ingest;
use crate::query::QueryService;

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

fn item(id: u8, name: &str, at: i64) -> TelemetryItem {
    items::event(
        Envelope {
            event_id: vec![id; 16],
            kind: TelemetryKind::Event,
            schema_version: 1,
            occurred_at: at,
            observed_at: None,
            received_at: Some(at + 5),
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
        },
        EventPayload {
            name: name.into(),
            route: None,
            page_title: None,
        },
    )
}

fn commit_request(batch_id: u8, items: Vec<TelemetryItem>) -> CommitBatchRequest {
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

fn head(name: &str) -> (Ingest, QueryService, Arc<dyn Store>) {
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
        },
        QueryService {
            store: Arc::clone(&store),
            max_runtime_ms: 30_000,
            max_expression_depth: crate::expr::DEFAULT_MAX_DEPTH,
            guards: crate::analysis::Guards::default(),
            attribution: Default::default(),
            policy: Default::default(),
            identity: Default::default(),
        },
        store,
    )
}

/// Reach into the encoded scan under an aggregate, change it, and encode it
/// again. A child node travels encoded, so a test edits it the same way the
/// dashboard builds it.
fn edit_scan(request: &mut QueryRequest, change: impl FnOnce(&mut ScanNode)) {
    let mut aggregate = top_aggregate(request);
    let mut boxed = decode_query_node_box(&aggregate.input).expect("the child decodes");
    assert_eq!(boxed.node, QueryNodeKind::Scan, "a trend query scans");
    change(boxed.scan.as_mut().expect("the scan"));
    aggregate.input = encode_query_node_box(&boxed);
    put_aggregate(request, aggregate);
}

/// The aggregate at the top of a trend request.
fn top_aggregate(request: &QueryRequest) -> AggregateNode {
    let encoded = request.node.as_ref().expect("a node query");
    let boxed = decode_query_node_box(encoded).expect("the node decodes");
    assert_eq!(boxed.node, QueryNodeKind::Aggregate, "a trend query counts");
    boxed.aggregate.expect("the aggregate")
}

/// Put an edited aggregate back into the request.
fn put_aggregate(request: &mut QueryRequest, aggregate: AggregateNode) {
    request.node = Some(query::node_ref(&query::node::aggregate(aggregate)));
}

/// Change the aggregate at the top of a trend request.
fn edit_aggregate(request: &mut QueryRequest, change: impl FnOnce(&mut AggregateNode)) {
    let mut aggregate = top_aggregate(request);
    change(&mut aggregate);
    put_aggregate(request, aggregate);
}

fn trend_query(bucket_ms: i64, range: (i64, i64)) -> QueryRequest {
    query::trend(
        1,
        query::events(
            &PROJECT,
            TimeRange {
                range_start: range.0,
                range_end: range.1,
                basis: TimeBasis::OccurredAt,
                timezone: None,
            },
        ),
        bucket_ms,
        "events",
    )
}

// ---------------------------------------------------------------------------
// The commit boundary
// ---------------------------------------------------------------------------

#[test]
fn a_commit_returns_a_receipt_that_names_the_policy_it_satisfied() {
    // A caller must never have to assume which durability it got.
    let (ingest, _, _) = head("receipt");
    let receipt = ingest
        .commit(commit_request(
            1,
            vec![item(1, "checkout-started", BASE_TIME)],
        ))
        .unwrap();
    assert_eq!(receipt.accepted, 1);
    assert_eq!(receipt.satisfied_policy, ReceiptPolicy::LocalOne);
    assert_eq!(receipt.commit_watermark, 1);
    assert_eq!(receipt.deduplicated, Some(false));
    assert_eq!(receipt.protocol_version, crate::ingest::PROTOCOL_VERSION);
}

#[test]
fn a_retried_batch_returns_the_prior_receipt_rather_than_committing_again() {
    // DELIVERY.md section 5. This is what makes at-least-once delivery give one
    // logical commit.
    let (ingest, _, store) = head("dedup");
    let first = ingest
        .commit(commit_request(1, vec![item(1, "a", BASE_TIME)]))
        .unwrap();
    let second = ingest
        .commit(commit_request(1, vec![item(1, "a", BASE_TIME)]))
        .unwrap();

    assert_eq!(second.deduplicated, Some(true));
    assert_eq!(second.committed_at, first.committed_at);
    assert_eq!(second.commit_watermark, first.commit_watermark);
    assert_eq!(store.commit_watermark(), 1);
}

#[test]
fn a_retried_batch_that_now_holds_different_items_still_returns_the_prior_receipt() {
    // A batch ID is the identity. A retry that somehow carried other items must
    // not commit them under an ID that already committed.
    let (ingest, _, store) = head("dedup-different");
    ingest
        .commit(commit_request(1, vec![item(1, "a", BASE_TIME)]))
        .unwrap();
    let second = ingest
        .commit(commit_request(
            1,
            vec![item(2, "b", BASE_TIME), item(3, "c", BASE_TIME)],
        ))
        .unwrap();
    assert_eq!(second.deduplicated, Some(true));
    assert_eq!(second.accepted, 1, "the prior count, not the new one");
    assert!(store.lookup_event([2; 16]).unwrap().is_none());
}

#[test]
fn an_item_that_cannot_be_projected_is_rejected_and_the_batch_still_commits() {
    let (ingest, _, store) = head("partial");
    let mut orphan = item(2, "no-tenancy", BASE_TIME);
    orphan.envelope.project_id = None;

    let receipt = ingest
        .commit(commit_request(
            1,
            vec![
                item(1, "good", BASE_TIME),
                orphan,
                item(3, "good", BASE_TIME),
            ],
        ))
        .unwrap();

    assert_eq!(receipt.accepted, 2);
    let rejected = receipt.rejected.expect("one item was rejected");
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].event_id, vec![2; 16]);
    assert!(store.lookup_event([2; 16]).unwrap().is_none());
}

#[test]
fn a_batch_with_no_identifier_is_refused() {
    let (ingest, _, _) = head("no-batch-id");
    let mut request = commit_request(1, vec![item(1, "a", BASE_TIME)]);
    request.batch.batch_id = vec![1, 2, 3];
    let failure = ingest.commit(request).unwrap_err();
    assert_eq!(failure.code, tallyowl_obs::ErrorCode::InvalidArgument);
    assert!(!failure.retryable);
}

#[test]
fn a_committed_batch_survives_a_restart_of_the_head() {
    // The durability claim, exercised by reopening the same directory.
    let directory = temporary_directory("restart");
    {
        let store: Arc<dyn Store> = Arc::new(SegmentedStore::open(&directory).unwrap());
        let metrics = Registry::new();
        Ingest::declare_metrics(&metrics);
        let ingest = Ingest {
            golden_signal_bucket_ms: 60_000,
            store,
            metrics,
            receipt_policy: ReceiptPolicy::LocalOne,
            open_traces: None,
            policy: None,
        };
        ingest
            .commit(commit_request(
                1,
                vec![item(1, "checkout-started", BASE_TIME)],
            ))
            .unwrap();
    }

    let store: Arc<dyn Store> = Arc::new(SegmentedStore::open(&directory).unwrap());
    assert!(store.lookup_event([1; 16]).unwrap().is_some());
    // And a retry after the restart is still deduplicated, so a collector that
    // outlived the head does not double count.
    let metrics = Registry::new();
    Ingest::declare_metrics(&metrics);
    let ingest = Ingest {
        golden_signal_bucket_ms: 60_000,
        store,
        metrics,
        receipt_policy: ReceiptPolicy::LocalOne,
        open_traces: None,
        policy: None,
    };
    let retry = ingest
        .commit(commit_request(
            1,
            vec![item(1, "checkout-started", BASE_TIME)],
        ))
        .unwrap();
    assert_eq!(retry.deduplicated, Some(true));
}

// ---------------------------------------------------------------------------
// The query contract
// ---------------------------------------------------------------------------

#[test]
fn a_trend_counts_committed_events_by_bucket() {
    let (ingest, query, _) = head("trend");
    ingest
        .commit(commit_request(
            1,
            vec![
                item(1, "a", BASE_TIME),
                item(2, "a", BASE_TIME + 1_000),
                item(3, "a", BASE_TIME + 70_000),
            ],
        ))
        .unwrap();

    let response = query
        .run(trend_query(60_000, (BASE_TIME, BASE_TIME + 600_000)))
        .unwrap();

    assert_eq!(response.columns, vec!["bucket", "events"]);
    assert_eq!(response.rows.len(), 2);
    assert_eq!(
        wire::read(&response.rows[0].values[1]).unwrap(),
        Value::Unsigned(2)
    );
    assert_eq!(
        wire::read(&response.rows[1].values[1]).unwrap(),
        Value::Unsigned(1)
    );
    assert!(response.metadata.complete);
    assert_eq!(response.metadata.commit_watermark, 1);
}

#[test]
fn a_count_says_that_it_is_exact() {
    // D21: every query and result type says whether it is exact.
    let (ingest, query, _) = head("exactness");
    ingest
        .commit(commit_request(1, vec![item(1, "a", BASE_TIME)]))
        .unwrap();
    let response = query
        .run(trend_query(60_000, (BASE_TIME, BASE_TIME + 600_000)))
        .unwrap();
    let exactness = &response.metadata.exactness[0];
    assert_eq!(exactness.alias, "events");
    assert!(exactness.exact);
}

#[test]
fn a_query_over_another_project_returns_nothing() {
    // Tenant isolation. A project that did not send this data cannot read it.
    let (ingest, query, _) = head("isolation");
    ingest
        .commit(commit_request(1, vec![item(1, "a", BASE_TIME)]))
        .unwrap();

    let mut request = trend_query(60_000, (BASE_TIME, BASE_TIME + 600_000));
    edit_scan(&mut request, |scan| scan.project_id = vec![0xaa; 16]);
    let response = query.run(request).unwrap();
    assert!(response.rows.is_empty());
}

#[test]
fn a_backwards_time_range_is_refused_with_a_message_a_person_can_act_on() {
    let (_, query, _) = head("backwards");
    let failure = query
        .run(trend_query(60_000, (BASE_TIME + 1_000, BASE_TIME)))
        .unwrap_err();
    assert_eq!(failure.code, tallyowl_obs::ErrorCode::InvalidArgument);
    assert!(failure.message.contains("ends before it starts"));
}

#[test]
fn a_query_from_a_newer_client_says_so_plainly() {
    let (_, query, _) = head("version");
    let mut request = trend_query(60_000, (BASE_TIME, BASE_TIME + 1_000));
    request.algebra_version = 99;
    let failure = query.run(request).unwrap_err();
    assert_eq!(failure.code, tallyowl_obs::ErrorCode::SchemaUnsupported);
    assert!(failure.message.contains("newer version"));
    assert!(!failure.message.contains("algebra"), "no internal word");
}

#[test]
fn an_unsupported_measure_is_refused_by_name_rather_than_answered_wrongly() {
    let (_, query, _) = head("measure");
    let mut request = trend_query(60_000, (BASE_TIME, BASE_TIME + 1_000));
    edit_aggregate(&mut request, |aggregate| {
        aggregate.measures[0].kind = MeasureKind::QuantileApprox;
    });
    let failure = query.run(request).unwrap_err();
    assert_eq!(failure.code, tallyowl_obs::ErrorCode::InvalidArgument);
    assert!(!failure.retryable, "asking again will not help");
    assert!(failure.message.contains("later release"));
}

#[test]
fn a_calendar_month_is_a_month_and_not_a_fixed_span() {
    // This test used to assert that a calendar month was refused. L110 deferred
    // it for want of a timezone database and L134 built it. The fixture is two
    // events a person can place by hand: one on the last day of January and one
    // on the first of February. A fixed 30-day bucket puts both in the same one.
    let (ingest, query, _) = head("month");
    let january = timestamp("2026-01-31T12:00:00Z");
    let february = timestamp("2026-02-01T12:00:00Z");
    ingest
        .commit(commit_request(
            1,
            vec![
                item(1, "checkout-started", january),
                item(2, "checkout-started", february),
            ],
        ))
        .unwrap();

    let mut request = trend_query(
        60_000,
        (
            timestamp("2026-01-01T00:00:00Z"),
            timestamp("2026-03-01T00:00:00Z"),
        ),
    );
    edit_aggregate(&mut request, |aggregate| {
        aggregate.interval = Some(Interval {
            fixed_ms: None,
            calendar: Some(tallyowl_control_api::types::Interval_calendar::Month),
        });
    });
    let result = query.run(request).expect("a calendar month is answered");
    let buckets = bucket_starts(&result);
    assert_eq!(
        buckets,
        vec![
            timestamp("2026-01-01T00:00:00Z"),
            timestamp("2026-02-01T00:00:00Z")
        ],
        "the two events landed in one bucket, so the month is still a fixed span"
    );
}

#[test]
fn a_calendar_day_is_the_local_day_the_query_asked_for() {
    // The same instant belongs to different days in different zones, and the
    // reader means their own. 22:30 UTC is already tomorrow in Berlin.
    let (ingest, query, _) = head("local-day");
    let at = timestamp("2026-08-09T22:30:00Z");
    ingest
        .commit(commit_request(1, vec![item(1, "checkout-started", at)]))
        .unwrap();

    let bucket_in = |zone: Option<&str>| {
        let mut request = trend_query(
            60_000,
            (
                timestamp("2026-08-08T00:00:00Z"),
                timestamp("2026-08-12T00:00:00Z"),
            ),
        );
        edit_scan(&mut request, |scan| {
            scan.range.timezone = zone.map(str::to_string)
        });
        edit_aggregate(&mut request, |aggregate| {
            aggregate.interval = Some(Interval {
                fixed_ms: None,
                calendar: Some(tallyowl_control_api::types::Interval_calendar::Day),
            });
        });
        bucket_starts(&query.run(request).expect("a calendar day is answered"))
    };

    assert_eq!(bucket_in(None), vec![timestamp("2026-08-09T00:00:00Z")]);
    assert_eq!(
        bucket_in(Some("Europe/Berlin")),
        vec![timestamp("2026-08-09T22:00:00Z")],
        "the Berlin day that holds this instant starts at 22:00 UTC on the ninth"
    );
}

#[test]
fn a_timezone_this_installation_does_not_know_is_refused_rather_than_answered_in_utc() {
    let (_, query, _) = head("bad-zone");
    let mut request = trend_query(60_000, (BASE_TIME, BASE_TIME + 1_000));
    edit_scan(&mut request, |scan| {
        scan.range.timezone = Some("Middle/Earth".to_string())
    });
    let failure = query.run(request).unwrap_err();
    assert_eq!(failure.code, tallyowl_obs::ErrorCode::InvalidArgument);
    assert!(failure.message.contains("Middle/Earth"));
}

/// A timestamp a person can read in the test beside the answer it produces.
fn timestamp(text: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(text)
        .expect("a timestamp")
        .timestamp_millis()
}

/// The `bucket` column of a trend answer, in order.
fn bucket_starts(result: &tallyowl_control_api::types::QueryResponse) -> Vec<i64> {
    let at = result
        .columns
        .iter()
        .position(|column| column == "bucket")
        .expect("a trend answer has a bucket column");
    result
        .rows
        .iter()
        .filter_map(|row| row.values.get(at).and_then(|value| value.int_value))
        .collect()
}

#[test]
fn a_dataset_this_release_cannot_answer_is_refused() {
    // Every dataset that is a view over the stored rows now answers. Identity
    // edges are not: they need the identity graph that Phase 8 builds, and an
    // empty answer would read as "this end user has no aliases" rather than
    // "this installation cannot answer that yet".
    let (_, query, _) = head("dataset");
    let mut request = trend_query(60_000, (BASE_TIME, BASE_TIME + 1_000));
    edit_scan(&mut request, |scan| scan.scan = Dataset::IdentityEdges);
    let failure = query.run(request).unwrap_err();
    assert!(!failure.retryable);
    assert!(
        failure.message.contains("identity edges"),
        "{}",
        failure.message
    );
}

#[test]
fn a_dataset_selects_the_kind_it_names() {
    let (ingest, query, _) = head("dataset-view");
    let mut span = item(2, "GET /orders", BASE_TIME + 1);
    span.envelope.kind = TelemetryKind::Span;
    span.event = None;
    span.span = Some(tallyowl_collector_api::types::SpanPayload {
        operation: "GET /orders".into(),
        kind: tallyowl_collector_api::types::SpanKind::Server,
        start_at: BASE_TIME + 1,
        duration_ms: 12,
        status: tallyowl_collector_api::types::SpanPayload_status::Ok,
        resource: None,
        parent_span_id: None,
        links: None,
        error_event_id: None,
        sampling_reason: None,
    });
    ingest
        .commit(commit_request(
            1,
            vec![item(1, "checkout-started", BASE_TIME), span],
        ))
        .expect("both commit");

    let mut request = trend_query(60_000, (BASE_TIME - 1, BASE_TIME + 1_000));
    edit_scan(&mut request, |scan| scan.scan = Dataset::Spans);
    let response = query.run(request).expect("a span query answers");
    let total: u64 = response
        .rows
        .iter()
        .filter_map(|row| match row.values.get(1).map(wire::read) {
            Some(Ok(Value::Unsigned(count))) => Some(count),
            _ => None,
        })
        .sum();
    assert_eq!(total, 1, "one span, and not the event beside it");
}

#[test]
fn a_scan_on_its_own_lists_the_events() {
    let (ingest, query, _) = head("list");
    ingest
        .commit(commit_request(
            1,
            vec![item(1, "a", BASE_TIME), item(2, "b", BASE_TIME + 1)],
        ))
        .unwrap();

    let request = query::request(
        1,
        &query::node::scan(query::events(
            &PROJECT,
            TimeRange {
                range_start: BASE_TIME,
                range_end: BASE_TIME + 600_000,
                basis: TimeBasis::OccurredAt,
                timezone: None,
            },
        )),
    );
    let response = query.run(request).unwrap();
    assert_eq!(response.rows.len(), 2);
    assert_eq!(response.columns[0], "event_id");
}

#[test]
fn a_query_over_damaged_data_refuses_rather_than_returning_a_smaller_number() {
    // D57 and D18: correctness is the default. A caller must ask for a partial
    // result; it is never what they get by accident.
    let directory = temporary_directory("incomplete");
    {
        // Seal at once, so the item lands in a segment rather than staying in
        // the open buffer where nothing on disk could be damaged.
        let store = tallyowl_store::SegmentedStore::open_with(
            &directory,
            tallyowl_store::Sealing {
                max_open_rows: 1,
                max_open_ms: i64::MAX,
                verify_on_read: true,
                reserve_bytes: 0,
            },
            Default::default(),
        )
        .unwrap();
        let store: Arc<dyn Store> = Arc::new(store);
        let metrics = Registry::new();
        Ingest::declare_metrics(&metrics);
        Ingest {
            golden_signal_bucket_ms: 60_000,
            store,
            metrics,
            receipt_policy: ReceiptPolicy::LocalOne,
            open_traces: None,
            policy: None,
        }
        .commit(commit_request(1, vec![item(1, "a", BASE_TIME)]))
        .unwrap();
    }

    // A segment damaged after it was written, which is what a failing device
    // produces and what `scrub` exists to find.
    let segments = directory.join("segments");
    let path = std::fs::read_dir(&segments)
        .expect("the segment directory")
        .next()
        .expect("one segment")
        .unwrap()
        .path();
    let mut bytes = std::fs::read(&path).unwrap();
    // Inside the footer, which carries its own checksum and is the part a
    // reader checks before it says what the file holds.
    let at = bytes.len() - 40;
    bytes[at] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();

    let store: Arc<dyn Store> = Arc::new(
        tallyowl_store::SegmentedStore::open_waiting(&directory, std::time::Duration::from_secs(5))
            .unwrap(),
    );
    let query = QueryService {
        store,
        max_runtime_ms: 30_000,
        max_expression_depth: crate::expr::DEFAULT_MAX_DEPTH,
        guards: crate::analysis::Guards::default(),
        attribution: Default::default(),
        policy: Default::default(),
        identity: Default::default(),
    };

    let failure = query
        .run(trend_query(60_000, (BASE_TIME, BASE_TIME + 600_000)))
        .unwrap_err();
    assert_eq!(failure.code, tallyowl_obs::ErrorCode::IncompleteResult);
    assert!(failure.message.contains("smaller than the truth"));

    // A caller that asks for a partial answer gets one, and it says so.
    let mut partial = trend_query(60_000, (BASE_TIME, BASE_TIME + 600_000));
    partial.allow_partial = true;
    let response = query.run(partial).unwrap();
    assert!(!response.metadata.complete);
}

#[test]
fn a_running_head_reports_its_storage_and_capacity_instruments() {
    // The check reads the exposition, not the source. A gauge that nothing
    // declared is dropped when it is set, so asserting that `declare` was
    // called would prove less than asking the endpoint what it says.
    let place = temporary_directory("metrics-declared");
    let store = SegmentedStore::open(&place).expect("the store opens");
    let metrics = Registry::new();
    crate::declare_metrics(&metrics);
    tallyowl_store::metrics::sample(&store, &metrics);

    let text = metrics.render_text();
    for name in [
        // STORAGE.md section 14: disk used and free.
        "tallyowl_disk_free_bytes",
        "tallyowl_disk_total_bytes",
        "tallyowl_storage_reserve_bytes",
        // FAILURE_MODES.md section 10: a refusal an operator can see.
        "tallyowl_storage_refusals_total",
        // The two that predict rather than report.
        "tallyowl_generation_pin_age_seconds",
        "tallyowl_catalog_snapshot_age_seconds",
        "tallyowl_segments_count",
        "tallyowl_wal_bytes",
    ] {
        assert!(text.contains(name), "the head reports no {name}");
    }

    // And the free-space gauge holds a real reading rather than a zero nobody
    // set, which is what a missing declaration used to look like.
    let free = text
        .lines()
        .find(|line| line.starts_with("tallyowl_disk_free_bytes "))
        .expect("the gauge has no sample");
    let value: u64 = free.rsplit(' ').next().unwrap().parse().unwrap();
    assert!(value > 0, "the device reports no free space at all: {free}");
}
