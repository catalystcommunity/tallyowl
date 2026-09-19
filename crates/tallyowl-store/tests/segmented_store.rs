//! The native store against its own contract.
//!
//! These are the promises `crates/tallyowl-store/src/lib.rs` says must keep
//! holding when Phase 3 lands, plus the ones Phase 3 adds:
//!
//! - a commit that returned survives an abrupt process kill;
//! - a repeated commit of one batch ID gives one logical commit;
//! - a tombstone hides matching rows immediately, in every tier, including rows
//!   that arrive after the erasure;
//! - a query that cannot see all of its data says so rather than returning a
//!   smaller answer.

use std::path::PathBuf;
use std::sync::Arc;

use tallyowl_store::catalog::Tombstone;
use tallyowl_store::row::{EventRow, PropertyValue};
use tallyowl_store::segmented::{Sealing, SegmentedStore};
use tallyowl_store::wal::GroupCommit;
use tallyowl_store::{Store, StoreError, TimeBasis};

const WORKSPACE: [u8; 16] = [8; 16];
const PROJECT: [u8; 16] = [9; 16];
const BASE_TIME: i64 = 1_785_628_800_000;

fn directory(name: &str) -> PathBuf {
    // A real directory on real storage. Section 9 of the implementation prompt
    // forbids benchmarking storage on tmpfs, and a correctness test uses the
    // same path shape so the two stay honest.
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("segmented-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// A store that seals often, so a test sees several segments without writing a
/// benchmark's worth of data.
fn eager(place: &PathBuf) -> SegmentedStore {
    SegmentedStore::open_with(
        place,
        Sealing {
            max_open_rows: 50,
            max_open_ms: i64::MAX,
            verify_on_read: true,
            // Space is not what these tests are about, and a workstation
            // with less than the default reserve free would otherwise fail them.
            reserve_bytes: 0,
        },
        GroupCommit {
            // A test decides when a batch goes, so a linger would only add
            // waiting to every case.
            linger: std::time::Duration::from_millis(0),
            ..GroupCommit::default()
        },
    )
    .expect("the store opens")
}

fn row(n: u8, name: &str, at: i64) -> EventRow {
    let mut row = EventRow::new([n; 16], "event", name, at);
    row.workspace_id = WORKSPACE;
    row.project_id = PROJECT;
    row.received_at = at + 5;
    row
}

fn rows(count: usize, from: u16) -> Vec<EventRow> {
    (0..count)
        .map(|index| {
            let n = from + index as u16;
            let mut row = row(1, "checkout-started", BASE_TIME + i64::from(n));
            row.event_id[14..16].copy_from_slice(&n.to_be_bytes());
            row
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The commit boundary
// ---------------------------------------------------------------------------

#[test]
fn a_commit_returns_a_watermark_and_a_count() {
    let store = eager(&directory("commit"));
    let outcome = store
        .commit([1; 16], [2; 16], vec![row(1, "a", 100), row(2, "b", 200)])
        .unwrap();
    assert_eq!(outcome.accepted, 2);
    assert_eq!(outcome.commit_watermark, 1);
    assert!(!outcome.deduplicated);
    assert_eq!(store.row_count(), 2);
}

#[test]
fn a_repeated_batch_id_gives_one_logical_commit() {
    // DELIVERY.md section 5: if the connection drops after the commit but
    // before the receipt arrives, the collector retries the same batch ID.
    let store = eager(&directory("dedup"));
    let first = store
        .commit([1; 16], [2; 16], vec![row(1, "a", 100)])
        .unwrap();
    let second = store
        .commit([1; 16], [2; 16], vec![row(1, "a", 100)])
        .unwrap();

    assert!(!first.deduplicated);
    assert!(second.deduplicated);
    assert_eq!(second.commit_watermark, first.commit_watermark);
    assert_eq!(second.committed_at, first.committed_at);
    assert_eq!(store.row_count(), 1, "the rows are written once");
}

#[test]
fn one_batch_id_from_two_sources_is_two_batches() {
    let store = eager(&directory("scope"));
    store
        .commit([1; 16], [7; 16], vec![row(1, "a", 100)])
        .unwrap();
    let other = store
        .commit([2; 16], [7; 16], vec![row(2, "a", 100)])
        .unwrap();
    assert!(!other.deduplicated);
    assert_eq!(store.row_count(), 2);
}

#[test]
fn a_committed_batch_survives_a_reopen() {
    // The durability claim, minus the process kill the integration test does
    // for real. D53: a restart loses nothing that TallyOwl acknowledged.
    let place = directory("reopen");
    {
        let store = eager(&place);
        store
            .commit([1; 16], [2; 16], vec![row(1, "a", 100), row(2, "b", 200)])
            .unwrap();
    }
    let store = eager(&place);
    assert_eq!(store.row_count(), 2);
    assert_eq!(store.commit_watermark(), 1);
    assert!(store.receipt([1; 16], [2; 16]).is_some());
    // And the deduplication index survived with it.
    assert!(
        store
            .commit([1; 16], [2; 16], vec![row(1, "a", 100)])
            .unwrap()
            .deduplicated
    );
}

#[test]
fn an_unsealed_commit_survives_a_reopen_through_the_append_log() {
    // The case the append log exists for. The rows were acknowledged and no
    // segment holds them yet, so recovery has to replay them.
    let place = directory("replay");
    {
        let store = eager(&place);
        store.commit([1; 16], [2; 16], rows(10, 0)).unwrap();
        assert_eq!(store.segment_count(), 0, "ten rows do not fill a segment");
    }
    let store = eager(&place);
    assert_eq!(store.row_count(), 10);
    let found = store
        .scan(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 1_000,
            TimeBasis::OccurredAt,
        )
        .unwrap()
        .rows;
    assert_eq!(found.len(), 10);
}

#[test]
fn a_sealed_segment_and_an_open_buffer_answer_one_query_together() {
    // STORAGE.md section 9: a query can read recent data from a committed-log
    // read view, so dashboard freshness does not depend on the segment size.
    let place = directory("mixed");
    let store = eager(&place);
    store.commit([1; 16], [1; 16], rows(60, 0)).unwrap();
    assert_eq!(store.segment_count(), 1, "sixty rows filled a segment");
    store.commit([1; 16], [2; 16], rows(5, 100)).unwrap();

    let found = store
        .scan(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 10_000,
            TimeBasis::OccurredAt,
        )
        .unwrap()
        .rows;
    assert_eq!(
        found.len(),
        65,
        "the segment and the open buffer both answered"
    );
}

#[test]
fn a_segment_survives_a_reopen_and_the_log_does_not_replay_it_twice() {
    let place = directory("checkpoint");
    {
        let store = eager(&place);
        store.commit([1; 16], [1; 16], rows(60, 0)).unwrap();
        assert_eq!(store.segment_count(), 1);
    }
    let store = eager(&place);
    assert_eq!(store.segment_count(), 1);
    assert_eq!(
        store.row_count(),
        60,
        "the log range a segment covers is not replayed on top of it"
    );
    let found = store
        .scan(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 10_000,
            TimeBasis::OccurredAt,
        )
        .unwrap()
        .rows;
    assert_eq!(found.len(), 60);
}

#[test]
fn an_empty_batch_commits_and_still_records_a_receipt() {
    // A batch whose every item was rejected still needs a receipt, or the
    // collector retries it forever.
    let store = eager(&directory("empty"));
    let outcome = store.commit([1; 16], [2; 16], vec![]).unwrap();
    assert_eq!(outcome.accepted, 0);
    assert!(store.receipt([1; 16], [2; 16]).is_some());
}

#[test]
fn the_watermark_rises_once_for_each_logical_commit() {
    let store = eager(&directory("watermark"));
    assert_eq!(store.commit_watermark(), 0);
    store
        .commit([1; 16], [1; 16], vec![row(1, "a", 1)])
        .unwrap();
    store
        .commit([1; 16], [2; 16], vec![row(2, "a", 2)])
        .unwrap();
    assert_eq!(store.commit_watermark(), 2);
    store
        .commit([1; 16], [2; 16], vec![row(2, "a", 2)])
        .unwrap();
    assert_eq!(
        store.commit_watermark(),
        2,
        "a deduplicated commit does not move it"
    );
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

#[test]
fn a_property_survives_a_seal_with_its_type_and_origin() {
    let place = directory("properties");
    let store = eager(&place);
    let mut batch = rows(60, 0);
    batch[0] = batch[0]
        .clone()
        .with_property("value", PropertyValue::Decimal("19.99".into()), "client")
        .with_property(
            "region",
            PropertyValue::Text("us-west2".into()),
            "collector",
        )
        .with_property("count", PropertyValue::Integer(-3), "driver")
        .with_property("attempts", PropertyValue::Unsigned(4), "client");
    let target = batch[0].event_id;
    store.commit([1; 16], [1; 16], batch).unwrap();
    assert_eq!(store.segment_count(), 1);

    let found = store.lookup_event(target).unwrap().expect("the event");
    assert_eq!(
        found.properties["value"].0,
        PropertyValue::Decimal("19.99".into())
    );
    assert_eq!(found.properties["region"].1, "collector");
    assert_eq!(found.properties["count"].0, PropertyValue::Integer(-3));
    // An unsigned value stays unsigned through the whole path.
    assert_eq!(found.properties["attempts"].0, PropertyValue::Unsigned(4));
}

#[test]
fn an_exact_lookup_finds_one_event_across_several_segments() {
    // Phase 3's exit criterion: mostly-unique request IDs remain exactly
    // retrievable. The block filter rules out the segments that cannot hold the
    // value, and the rows then decide.
    let place = directory("lookup");
    let store = eager(&place);
    for batch in 0..5u8 {
        store
            .commit([1; 16], [batch; 16], rows(60, u16::from(batch) * 100))
            .unwrap();
    }
    assert!(store.segment_count() >= 4, "several segments hold the data");

    for n in [0u16, 55, 150, 402] {
        let mut event_id = [1u8; 16];
        event_id[14..16].copy_from_slice(&n.to_be_bytes());
        let found = store.lookup_event(event_id).unwrap();
        assert!(
            found.is_some(),
            "an event in a sealed segment was not found"
        );
        assert_eq!(found.unwrap().event_id, event_id);
    }

    let mut absent = [1u8; 16];
    absent[14..16].copy_from_slice(&9_999u16.to_be_bytes());
    assert!(store.lookup_event(absent).unwrap().is_none());
}

#[test]
fn the_locator_names_the_segments_that_can_hold_a_value() {
    // Phase 3's exit criterion, and the property HIGH_CARDINALITY.md section 4
    // exists for: a mostly-unique value stays exactly retrievable **without
    // scanning every retained segment**.
    let place = directory("locator");
    let store = eager(&place);
    for batch in 0..6u8 {
        store
            .commit([1; 16], [batch; 16], rows(60, u16::from(batch) * 100))
            .unwrap();
    }
    let segments = store.segment_count();
    assert!(segments >= 5, "several segments hold the data");

    let locator = store.catalog().locator().unwrap();
    assert!(!locator.is_empty(), "sealing built no locator run");

    // One event identifier is in exactly one segment, whatever the retention
    // window holds.
    let mut event_id = [1u8; 16];
    event_id[14..16].copy_from_slice(&250u16.to_be_bytes());
    let candidates = locator.candidates(tallyowl_store::segment::schema::EVENT_ID, &event_id, None);
    assert_eq!(
        candidates.len(),
        1,
        "a unique value named {} of {segments} segments",
        candidates.len()
    );

    // And the count is available before the query runs, so an unbounded
    // high-cardinality lookup is never accidental.
    assert_eq!(
        locator.candidate_count(tallyowl_store::segment::schema::EVENT_ID, &event_id, None),
        1
    );
    // A value the tablet never saw names nothing at all.
    assert_eq!(
        locator.candidate_count(tallyowl_store::segment::schema::EVENT_ID, &[0xab; 16], None),
        0
    );
}

#[test]
fn a_repeated_value_names_every_segment_that_holds_it() {
    // A session spans segments, so a lookup on one has to reach all of them.
    // This is the pair count HIGH_CARDINALITY.md section 4 says sets the cost.
    let place = directory("locator-repeated");
    let store = eager(&place);
    for batch in 0..4u8 {
        let mut batch_rows = rows(60, u16::from(batch) * 100);
        for row in &mut batch_rows {
            row.session_id = Some("s-shared".into());
        }
        store.commit([1; 16], [batch; 16], batch_rows).unwrap();
    }

    let locator = store.catalog().locator().unwrap();
    let candidates = locator.candidates(
        tallyowl_store::segment::schema::SESSION_ID,
        b"s-shared",
        None,
    );
    assert_eq!(
        candidates.len(),
        store.segment_count(),
        "a value on every segment names every segment"
    );
}

#[test]
fn a_dynamic_property_is_exactly_retrievable_without_advance_administration() {
    // D20: a dynamic scalar field defaults to exact `lookup` indexing, so an
    // unexpected application-specific correlation identifier stays useful with
    // nobody configuring anything.
    let place = directory("locator-dynamic");
    let store = eager(&place);
    let mut batch = rows(60, 0);
    batch[7] = batch[7].clone().with_property(
        "order_id",
        PropertyValue::Text("ord-4711".into()),
        "client",
    );
    store.commit([1; 16], [1; 16], batch).unwrap();

    let locator = store.catalog().locator().unwrap();
    let candidates = locator.candidates("p:order_id", b"ord-4711", None);
    assert_eq!(
        candidates.len(),
        1,
        "an unexpected property was not indexed"
    );
}

#[test]
fn the_locator_survives_a_reopen() {
    let place = directory("locator-reopen");
    {
        let store = eager(&place);
        store.commit([1; 16], [1; 16], rows(60, 0)).unwrap();
        assert!(!store.catalog().locator().unwrap().is_empty());
    }
    let store = eager(&place);
    assert!(
        !store.catalog().locator().unwrap().is_empty(),
        "the locator did not survive a restart"
    );
}

#[test]
fn a_locator_run_and_its_segments_agree() {
    // FAILURE_MODES.md section 8.4 rule 3 makes this a required test: a
    // generation's locator runs and its segments must not disagree.
    let place = directory("locator-agreement");
    let store = eager(&place);
    for batch in 0..3u8 {
        store
            .commit([1; 16], [batch; 16], rows(60, u16::from(batch) * 100))
            .unwrap();
    }
    let live: Vec<[u8; 16]> = store
        .catalog()
        .manifests()
        .unwrap()
        .iter()
        .map(|manifest| manifest.segment_id)
        .collect();
    let locator = store.catalog().locator().unwrap();
    assert!(
        locator.disagreements(&|id| live.contains(id)).is_empty(),
        "a locator run names a segment the generation does not hold"
    );
}

#[test]
fn a_query_over_another_project_returns_nothing() {
    let store = eager(&directory("isolation"));
    store.commit([1; 16], [1; 16], rows(60, 0)).unwrap();
    let found = store
        .scan(
            [1; 16],
            BASE_TIME - 1,
            BASE_TIME + 10_000,
            TimeBasis::OccurredAt,
        )
        .unwrap()
        .rows;
    assert!(found.is_empty(), "one project cannot see another's rows");
}

#[test]
fn a_trend_counts_logical_events_by_the_basis_the_caller_asked_for() {
    let store = eager(&directory("trend"));
    store
        .commit(
            [1; 16],
            [1; 16],
            vec![row(1, "a", 1_000), row(2, "a", 1_500), row(3, "a", 2_500)],
        )
        .unwrap();

    let by_occurred = store
        .trend(PROJECT, 0, 10_000, TimeBasis::OccurredAt, 1_000, None)
        .unwrap();
    assert_eq!(by_occurred.total, 3);
    assert_eq!(by_occurred.buckets, vec![(1_000, 2), (2_000, 1)]);
    assert!(!by_occurred.incomplete);

    let by_received = store
        .trend(PROJECT, 0, 10_000, TimeBasis::ReceivedAt, 1_000, None)
        .unwrap();
    assert_eq!(by_received.basis, TimeBasis::ReceivedAt);
    assert_eq!(by_received.total, 3);
}

#[test]
fn a_count_is_over_logical_events_even_across_a_seal() {
    // DELIVERY.md section 6. A duplicate that landed in a segment and again in
    // the open buffer must count once.
    let place = directory("logical");
    let store = eager(&place);
    store.commit([1; 16], [1; 16], rows(60, 0)).unwrap();
    // The same event IDs again, under a different batch ID, which is what a
    // manual replay produces.
    store.commit([1; 16], [2; 16], rows(60, 0)).unwrap();

    let trend = store
        .trend(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 10_000,
            TimeBasis::OccurredAt,
            60_000,
            None,
        )
        .unwrap();
    assert_eq!(trend.total, 60, "the count is over logical events");

    let scanned = store
        .scan(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 10_000,
            TimeBasis::OccurredAt,
        )
        .unwrap()
        .rows;
    assert_eq!(scanned.len(), 120, "a scan returns every physical row");
}

#[test]
fn a_backwards_range_and_a_zero_bucket_are_refused() {
    let store = eager(&directory("refuse"));
    assert!(matches!(
        store.scan(PROJECT, 200, 100, TimeBasis::OccurredAt),
        Err(StoreError::InvalidArgument(_))
    ));
    assert!(matches!(
        store.trend(PROJECT, 0, 100, TimeBasis::OccurredAt, 0, None),
        Err(StoreError::InvalidArgument(_))
    ));
}

#[test]
fn a_time_range_excludes_its_end() {
    let store = eager(&directory("range"));
    store
        .commit([1; 16], [1; 16], vec![row(1, "a", 100), row(2, "a", 200)])
        .unwrap();
    let inside = store
        .scan(PROJECT, 100, 200, TimeBasis::OccurredAt)
        .unwrap()
        .rows;
    assert_eq!(inside.len(), 1, "the range takes its start and not its end");
}

// ---------------------------------------------------------------------------
// Erasure
// ---------------------------------------------------------------------------

fn erasure(user: &str) -> Tombstone {
    Tombstone {
        tombstone_id: [1; 16],
        generation: 0,
        project_id: PROJECT,
        event_ids: Vec::new(),
        property: Some(("end_user".to_string(), user.to_string())),
        range: None,
        requested_at: BASE_TIME,
        horizon: BASE_TIME + 30 * 86_400_000,
        reason: "The end user asked for their data to be removed.".into(),
        except_kinds: Vec::new(),
    }
}

fn with_user(mut row: EventRow, user: &str) -> EventRow {
    row.properties.insert(
        "end_user".into(),
        (PropertyValue::Text(user.into()), "client".into()),
    );
    row
}

#[test]
fn an_erasure_hides_matching_rows_immediately_in_a_segment_and_in_the_buffer() {
    // STORAGE.md section 11: queries at newer generations filter those rows
    // immediately, before any compaction has run.
    let place = directory("erase");
    let store = eager(&place);

    let mut batch: Vec<EventRow> = rows(60, 0)
        .into_iter()
        .enumerate()
        .map(|(index, row)| with_user(row, if index % 2 == 0 { "u-042" } else { "u-999" }))
        .collect();
    batch.truncate(60);
    store.commit([1; 16], [1; 16], batch).unwrap();
    assert_eq!(store.segment_count(), 1);

    // And some rows still in the open buffer.
    store
        .commit(
            [1; 16],
            [2; 16],
            rows(4, 500)
                .into_iter()
                .map(|row| with_user(row, "u-042"))
                .collect(),
        )
        .unwrap();

    let before = store
        .scan(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 10_000,
            TimeBasis::OccurredAt,
        )
        .unwrap()
        .rows;
    assert_eq!(before.len(), 64);

    let generation = store.erase(&erasure("u-042")).unwrap();
    assert_eq!(generation, 1);

    let after = store
        .scan(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 10_000,
            TimeBasis::OccurredAt,
        )
        .unwrap()
        .rows;
    assert_eq!(
        after.len(),
        30,
        "every row for that person is hidden at once"
    );
    assert!(after
        .iter()
        .all(|row| row.properties["end_user"].0.to_display() == "u-999"));
}

#[test]
fn a_tombstone_hides_a_row_that_arrives_after_the_erasure() {
    // The property that makes a tombstone a standing predicate. Telemetry for
    // an erased end user can still be in a collector queue when the erasure
    // lands, and it must not become visible when it arrives. See D28.
    let store = eager(&directory("late"));
    store.erase(&erasure("u-042")).unwrap();

    store
        .commit(
            [1; 16],
            [1; 16],
            vec![
                with_user(row(1, "a", BASE_TIME), "u-042"),
                with_user(row(2, "a", BASE_TIME), "u-999"),
            ],
        )
        .unwrap();

    let found = store
        .scan(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 1_000,
            TimeBasis::OccurredAt,
        )
        .unwrap()
        .rows;
    assert_eq!(found.len(), 1, "the late arrival never became visible");
    assert_eq!(found[0].properties["end_user"].0.to_display(), "u-999");
}

#[test]
fn an_erased_event_is_not_found_by_an_exact_lookup() {
    // A point lookup goes through the index rather than through a scan, so it
    // needs its own check. FAILURE_MODES.md section 8.2 rule 4: a tombstone
    // hides the data on read whatever an index said.
    let place = directory("erase-lookup");
    let store = eager(&place);
    let batch: Vec<EventRow> = rows(60, 0)
        .into_iter()
        .map(|row| with_user(row, "u-042"))
        .collect();
    let target = batch[3].event_id;
    store.commit([1; 16], [1; 16], batch).unwrap();
    assert!(store.lookup_event(target).unwrap().is_some());

    store.erase(&erasure("u-042")).unwrap();
    assert!(
        store.lookup_event(target).unwrap().is_none(),
        "an erased event is still reachable by its identifier"
    );
}

#[test]
fn an_erasure_survives_a_reopen() {
    let place = directory("erase-reopen");
    {
        let store = eager(&place);
        store
            .commit(
                [1; 16],
                [1; 16],
                vec![with_user(row(1, "a", BASE_TIME), "u-042")],
            )
            .unwrap();
        store.erase(&erasure("u-042")).unwrap();
    }
    let store = eager(&place);
    assert_eq!(store.tombstone_generation().unwrap(), 1);
    let found = store
        .scan(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 1_000,
            TimeBasis::OccurredAt,
        )
        .unwrap()
        .rows;
    assert!(
        found.is_empty(),
        "an acknowledged erasure did not survive a restart"
    );
}

#[test]
fn an_erasure_that_names_nothing_is_refused() {
    // One with no events, no property, and no range would hide a whole
    // project. That is a real operation and not one a request should reach by
    // accident.
    let store = eager(&directory("erase-empty"));
    let mut nothing = erasure("u-042");
    nothing.property = None;
    let failure = store.erase(&nothing).unwrap_err();
    assert!(matches!(failure, StoreError::InvalidArgument(_)));
    assert!(failure.to_string().contains("names nothing"));
}

// ---------------------------------------------------------------------------
// Damage
// ---------------------------------------------------------------------------

#[test]
fn a_missing_segment_file_makes_a_query_say_it_could_not_see_everything() {
    // D57 and D18: a query over damaged data returns `incomplete-result` and
    // names what it could not read. It never silently returns a smaller answer.
    let place = directory("missing");
    let store = eager(&place);
    store.commit([1; 16], [1; 16], rows(60, 0)).unwrap();
    assert_eq!(store.segment_count(), 1);
    drop(store);

    // The file goes away underneath the catalog, which is procedure 6 of
    // FAILURE_MODES.md section 11 with no second copy.
    let segments = place.join("segments");
    for entry in std::fs::read_dir(&segments).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }

    let store = eager(&place);
    let trend = store
        .trend(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 10_000,
            TimeBasis::OccurredAt,
            60_000,
            None,
        )
        .unwrap();
    assert!(
        trend.incomplete,
        "a query that lost a segment reported a complete answer"
    );
}

#[test]
fn a_damaged_segment_makes_a_query_say_it_could_not_see_everything() {
    let place = directory("damaged");
    let store = eager(&place);
    store.commit([1; 16], [1; 16], rows(60, 0)).unwrap();
    drop(store);

    let segments = place.join("segments");
    let path = std::fs::read_dir(&segments)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut bytes = std::fs::read(&path).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();

    let store = eager(&place);
    let trend = store
        .trend(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 10_000,
            TimeBasis::OccurredAt,
            60_000,
            None,
        )
        .unwrap();
    assert!(
        trend.incomplete,
        "a damaged segment was answered from silently"
    );
}

#[test]
fn many_concurrent_commits_all_survive() {
    // Group commit is what makes this fast, and correctness is what it must not
    // trade for that. Every batch that returned has to be there.
    let place = directory("concurrent");
    let store = Arc::new(eager(&place));
    let mut threads = Vec::new();
    for source in 0..8u8 {
        let store = Arc::clone(&store);
        threads.push(std::thread::spawn(move || {
            for batch in 0..10u8 {
                let mut id = [0u8; 16];
                id[0] = source;
                id[1] = batch;
                let mut single = row(1, "a", BASE_TIME + i64::from(batch));
                single.event_id[0] = source;
                single.event_id[1] = batch;
                store.commit([source; 16], id, vec![single]).unwrap();
            }
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }

    assert_eq!(store.commit_watermark(), 80);
    let found = store
        .scan(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 10_000,
            TimeBasis::OccurredAt,
        )
        .unwrap()
        .rows;
    assert_eq!(found.len(), 80, "a commit that returned was lost");
}

// ---------------------------------------------------------------------------
// What the store reports about itself
// ---------------------------------------------------------------------------

#[test]
fn the_capacity_report_follows_what_the_store_actually_holds() {
    // STORAGE.md section 14. An operator cannot act on a failure that produces
    // no signal, and a gauge that never moves is no signal.
    use tallyowl_obs::metrics::{labels, Registry};

    let place = directory("metrics");
    let store = eager(&place);
    let metrics = Registry::new();
    tallyowl_store::metrics::declare(&metrics);

    tallyowl_store::metrics::sample(&store, &metrics);
    let none = labels(&[]);
    assert_eq!(metrics.gauge_value("tallyowl_segments_count", &none), 0);
    assert_eq!(metrics.gauge_value("tallyowl_receipts_count", &none), 0);

    store.commit([1; 16], [1; 16], rows(60, 0)).unwrap();
    tallyowl_store::metrics::sample(&store, &metrics);

    assert_eq!(metrics.gauge_value("tallyowl_segments_count", &none), 1);
    assert_eq!(
        metrics.gauge_value("tallyowl_segment_rows_count", &none),
        60
    );
    assert!(metrics.gauge_value("tallyowl_segment_bytes", &none) > 0);
    assert_eq!(metrics.gauge_value("tallyowl_receipts_count", &none), 1);
    assert_eq!(
        metrics.gauge_value("tallyowl_manifest_generation_count", &none),
        1
    );
    // The locator has entries, and its size follows the pair count.
    assert!(metrics.gauge_value("tallyowl_locator_bytes", &none) > 0);
}

#[test]
fn the_pin_age_rises_so_a_held_generation_is_visible_before_it_is_a_problem() {
    // FAILURE_MODES.md section 13 names this as one of the two instruments that
    // predict a failure rather than reporting one. A leaked pin retains storage
    // until it expires, and this is how that becomes visible first.
    use tallyowl_obs::metrics::{labels, Registry};

    let place = directory("pin-age");
    let store = eager(&place);
    store.commit([1; 16], [1; 16], rows(60, 0)).unwrap();

    let metrics = Registry::new();
    tallyowl_store::metrics::declare(&metrics);
    let none = labels(&[]);

    tallyowl_store::metrics::sample(&store, &metrics);
    assert_eq!(
        metrics.gauge_value("tallyowl_generation_pins_count", &none),
        0
    );
    assert_eq!(
        metrics.gauge_value("tallyowl_generation_pin_age_seconds", &none),
        0
    );

    // A query took a pin ten minutes ago and never let go.
    let generation = store.catalog().generation().unwrap();
    store
        .catalog()
        .pin(generation, tallyowl_obs::time::now_ms() - 600_000)
        .unwrap();

    tallyowl_store::metrics::sample(&store, &metrics);
    assert_eq!(
        metrics.gauge_value("tallyowl_generation_pins_count", &none),
        1
    );
    assert!(
        metrics.gauge_value("tallyowl_generation_pin_age_seconds", &none) >= 600,
        "the age of a held generation is not visible"
    );
}

#[test]
fn an_erasure_shows_up_in_the_report() {
    use tallyowl_obs::metrics::{labels, Registry};

    let place = directory("metrics-erasure");
    let store = eager(&place);
    store.commit([1; 16], [1; 16], rows(60, 0)).unwrap();
    store.erase(&erasure("u-042")).unwrap();

    let metrics = Registry::new();
    tallyowl_store::metrics::declare(&metrics);
    tallyowl_store::metrics::sample(&store, &metrics);
    let none = labels(&[]);
    assert_eq!(metrics.gauge_value("tallyowl_tombstones_count", &none), 1);
    assert_eq!(
        metrics.gauge_value("tallyowl_tombstone_generation_count", &none),
        1
    );
}

// ---------------------------------------------------------------------------
// Reclaiming the append log
// ---------------------------------------------------------------------------

#[test]
fn a_seal_keeps_the_log_frames_no_segment_covers_yet() {
    // A seal publishes the rows it captured and reclaims the log prefix those
    // segments cover. A commit that lands while the seal is building is not in
    // that prefix, and its frame has to survive: the commit was acknowledged,
    // and the log is the only durable copy of it until a later seal runs.
    //
    // This is the property a whole-file truncation breaks. It was reachable
    // before the collector served correlated batches at the same time, and
    // reachable much more often after.
    let place = directory("log-reclaim-race");
    let store = Arc::new(
        SegmentedStore::open_with(
            &place,
            Sealing {
                // Never seal on its own. The test decides when.
                max_open_rows: usize::MAX,
                max_open_ms: i64::MAX,
                verify_on_read: true,
                reserve_bytes: 0,
            },
            GroupCommit {
                linger: std::time::Duration::from_millis(0),
                ..GroupCommit::default()
            },
        )
        .expect("the store opens"),
    );

    // One committer keeps writing while the other keeps sealing.
    let committer = {
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            for n in 0..200u16 {
                let mut batch = [0u8; 16];
                batch[0..2].copy_from_slice(&n.to_be_bytes());
                store
                    .commit([1; 16], batch, rows(5, n.wrapping_mul(5)))
                    .expect("the commit is accepted");
            }
        })
    };
    let sealer = {
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            for _ in 0..200 {
                store.seal().expect("the seal publishes");
                std::thread::yield_now();
            }
        })
    };
    committer.join().expect("the committer finishes");
    sealer.join().expect("the sealer finishes");

    let committed = store.row_count();
    assert_eq!(committed, 1_000, "every batch was accepted");
    drop(store);

    // The abrupt kill. Nothing acknowledged may be missing after it.
    let reopened = SegmentedStore::open_with(
        &place,
        Sealing {
            max_open_rows: usize::MAX,
            max_open_ms: i64::MAX,
            verify_on_read: true,
            reserve_bytes: 0,
        },
        GroupCommit::default(),
    )
    .expect("the store reopens");

    assert_eq!(
        reopened.row_count(),
        committed,
        "a row that was acknowledged went missing across a restart"
    );
}

#[test]
fn an_idle_store_seals_when_its_buffer_is_due() {
    // L078. Sealing used to happen only inside a commit, so the seal condition
    // was evaluated only when a batch arrived: traffic stopping was exactly
    // when sealing stopped.
    //
    // The shape here is the one that was observed. A head restarted, replayed
    // its append log into the open buffer, and then went idle with 55 percent
    // of a load run's rows sitting in the log. Nothing was ever going to seal
    // them, because nothing was ever going to commit again.
    let place = directory("idle-seal");
    let never = Sealing {
        max_open_rows: usize::MAX,
        max_open_ms: i64::MAX,
        verify_on_read: true,
        reserve_bytes: 0,
    };
    {
        let store = SegmentedStore::open_with(
            &place,
            never,
            GroupCommit {
                linger: std::time::Duration::from_millis(0),
                ..GroupCommit::default()
            },
        )
        .expect("the store opens");
        store
            .commit([1; 16], [1; 16], rows(5, 0))
            .expect("the commit is accepted");
        // Left unsealed on purpose, which is what a drained store looks like.
    }

    let store = SegmentedStore::open_with(
        &place,
        Sealing {
            max_open_ms: 50,
            ..never
        },
        GroupCommit::default(),
    )
    .expect("the store reopens");
    assert_eq!(store.row_count(), 5, "the log replayed into the buffer");

    // Nothing will ever commit again. Only a background segmenter can seal it.
    std::thread::sleep(std::time::Duration::from_millis(120));
    assert!(
        store.seal_if_due().expect("the seal publishes").is_some(),
        "a buffer past its age seals with no traffic to trigger it"
    );
    assert_eq!(
        store.seal_if_due().expect("the check runs"),
        None,
        "and it is a no-op afterwards rather than an empty segment each period"
    );
    assert_eq!(store.row_count(), 5, "the rows are still all there");
}

#[test]
fn a_buffer_that_is_not_due_stays_open() {
    let place = directory("not-due");
    let store = SegmentedStore::open_with(
        &place,
        Sealing {
            max_open_rows: usize::MAX,
            max_open_ms: i64::MAX,
            verify_on_read: true,
            reserve_bytes: 0,
        },
        GroupCommit {
            linger: std::time::Duration::from_millis(0),
            ..GroupCommit::default()
        },
    )
    .expect("the store opens");
    store
        .commit([1; 16], [1; 16], rows(5, 0))
        .expect("the commit is accepted");
    assert_eq!(
        store.seal_if_due().expect("the check runs"),
        None,
        "a segmenter never seals a buffer that has not reached a condition"
    );
}

#[test]
fn a_seal_never_covers_a_log_frame_whose_rows_it_did_not_take() {
    // The hazard the append gate closes. A seal computes the log range it
    // covers from the log's own position, and a commit appends its frame before
    // its rows reach the open buffer. A seal that drained inside that window
    // would publish a segment claiming the frame while holding none of its
    // rows, advance the checkpoint past it, and lose an **acknowledged** batch
    // if the process then died.
    //
    // The existing race test above cannot catch it: its sealer keeps sealing
    // after the committer stops, so everything reaches a segment before the
    // reopen. This one reopens while the buffer still holds rows.
    let place = directory("seal-covers-what-it-took");
    let store = Arc::new(
        SegmentedStore::open_with(
            &place,
            Sealing {
                max_open_rows: usize::MAX,
                max_open_ms: i64::MAX,
                verify_on_read: true,
                reserve_bytes: 0,
            },
            GroupCommit {
                linger: std::time::Duration::from_millis(0),
                ..GroupCommit::default()
            },
        )
        .expect("the store opens"),
    );

    // Four committers against one sealer, which widens the window the gate has
    // to close rather than relying on one thread hitting it.
    let mut committers = Vec::new();
    for thread in 0..4u8 {
        let store = Arc::clone(&store);
        committers.push(std::thread::spawn(move || {
            for n in 0..100u16 {
                let mut batch = [0u8; 16];
                batch[0] = thread;
                batch[1..3].copy_from_slice(&n.to_be_bytes());
                store
                    .commit([1; 16], batch, rows(5, n.wrapping_mul(5)))
                    .expect("the commit is accepted");
            }
        }));
    }
    let sealer = {
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            for _ in 0..400 {
                store.seal().expect("the seal publishes");
                std::thread::yield_now();
            }
        })
    };
    for committer in committers {
        committer.join().expect("the committer finishes");
    }
    sealer.join().expect("the sealer finishes");

    let acknowledged = store.row_count();
    assert_eq!(acknowledged, 2_000, "every batch was accepted");

    // Reopen **without** a final seal, so anything the checkpoint wrongly
    // covered is gone rather than rescued by one last drain.
    drop(store);
    let reopened = SegmentedStore::open_with(
        &place,
        Sealing {
            max_open_rows: usize::MAX,
            max_open_ms: i64::MAX,
            verify_on_read: true,
            reserve_bytes: 0,
        },
        GroupCommit::default(),
    )
    .expect("the store reopens");
    assert_eq!(
        reopened.row_count(),
        acknowledged,
        "a segment covered a log frame whose rows it did not hold, and the \
         acknowledged batch in that frame was lost"
    );
}
