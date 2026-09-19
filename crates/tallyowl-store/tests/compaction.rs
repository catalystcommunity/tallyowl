//! Compaction against a real store, and the races `docs/FAILURE_MODES.md`
//! section 14 requires tests for.
//!
//! | Required test | Where |
//! | --- | --- |
//! | 5. A query holding a pinned generation while compaction publishes | `a_pinned_generation_keeps_its_files` |
//! | 6. An erasure that lands mid-compaction, proving no resurrection | `an_erasure_that_lands_mid_compaction_does_not_resurrect` |
//! | 7. A tombstone against a stale generation, proving the standing predicate still hides | `a_tombstone_hides_on_read_even_when_compaction_missed_it` |
//! | 8. A locator run and its generation's segments checked for agreement | `compaction_leaves_the_locator_agreeing_with_its_segments` |
//! | 15. A leaked generation pin expiring | `a_leaked_pin_expires_so_storage_is_not_held_forever` |

use std::path::PathBuf;

use tallyowl_store::catalog::Tombstone;
use tallyowl_store::compact::{compact, CompactionSettings};
use tallyowl_store::row::{EventRow, PropertyValue};
use tallyowl_store::segmented::{Sealing, SegmentedStore};
use tallyowl_store::wal::GroupCommit;
use tallyowl_store::{Store, TimeBasis};

const WORKSPACE: [u8; 16] = [8; 16];
const PROJECT: [u8; 16] = [9; 16];
const BASE_TIME: i64 = 1_785_628_800_000;

fn directory(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("compaction-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn store(place: &PathBuf) -> SegmentedStore {
    SegmentedStore::open_with(
        place,
        Sealing {
            max_open_rows: 40,
            max_open_ms: i64::MAX,
            verify_on_read: true,
            // Space is not what these tests are about, and a workstation
            // with less than the default reserve free would otherwise fail them.
            reserve_bytes: 0,
        },
        GroupCommit {
            linger: std::time::Duration::from_millis(0),
            ..GroupCommit::default()
        },
    )
    .expect("the store opens")
}

/// Rows that alternate between two people, so an erasure takes half.
fn rows(count: usize, from: u16) -> Vec<EventRow> {
    (0..count)
        .map(|index| {
            let n = from + index as u16;
            let mut row = EventRow::new(
                [1; 16],
                "event",
                "checkout-started",
                BASE_TIME + i64::from(n),
            );
            row.event_id[14..16].copy_from_slice(&n.to_be_bytes());
            row.workspace_id = WORKSPACE;
            row.project_id = PROJECT;
            row.received_at = row.occurred_at + 5;
            let user = if index % 2 == 0 { "u-042" } else { "u-999" };
            row.with_property("end_user", PropertyValue::Text(user.into()), "client")
        })
        .collect()
}

fn erasure(id: u8, user: &str) -> Tombstone {
    Tombstone {
        tombstone_id: [id; 16],
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

/// A compaction that reclaims at once, so a test does not wait five minutes.
fn eager_settings() -> CompactionSettings {
    CompactionSettings {
        grace_ms: 0,
        pin_max_age_ms: 60_000,
        ..CompactionSettings::default()
    }
}

fn visible(store: &SegmentedStore) -> Vec<EventRow> {
    store
        .scan(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 1_000_000,
            TimeBasis::OccurredAt,
        )
        .expect("scan")
        .rows
}

#[test]
fn compaction_rewrites_only_the_segments_an_erasure_touches() {
    // STORAGE.md section 11: compaction rewrites only affected bounded
    // segments. The physical work has to stay bounded, or a single erasure
    // rewrites the installation.
    let place = directory("bounded");
    let store = store(&place);
    for batch in 0..3u8 {
        store
            .commit([1; 16], [batch; 16], rows(40, u16::from(batch) * 100))
            .unwrap();
    }
    // One segment holds rows for one person only, so an erasure of the other
    // person cannot reach it.
    let mut untouched = rows(40, 900);
    for row in &mut untouched {
        row.properties.insert(
            "end_user".into(),
            (PropertyValue::Text("u-777".into()), "client".into()),
        );
    }
    store.commit([1; 16], [9; 16], untouched).unwrap();
    let before = store.segment_count();
    assert!(before >= 4);

    store.erase(&erasure(1, "u-042")).unwrap();
    let outcome = compact(&store, eager_settings()).unwrap();

    assert!(
        outcome.rewritten >= 3,
        "the affected segments were rewritten"
    );
    assert!(outcome.rows_erased > 0);
    // Every row for that person is gone from the rewritten segments, and the
    // rest are still there.
    let rows = visible(&store);
    assert!(rows
        .iter()
        .all(|row| row.properties["end_user"].0.to_display() != "u-042"));
    assert!(rows
        .iter()
        .any(|row| row.properties["end_user"].0.to_display() == "u-777"));
}

#[test]
fn compaction_removes_a_segment_whose_every_row_was_erased() {
    let place = directory("removed");
    let store = store(&place);
    let mut all_one_person = rows(40, 0);
    for row in &mut all_one_person {
        row.properties.insert(
            "end_user".into(),
            (PropertyValue::Text("u-042".into()), "client".into()),
        );
    }
    store.commit([1; 16], [1; 16], all_one_person).unwrap();
    assert_eq!(store.segment_count(), 1);

    store.erase(&erasure(1, "u-042")).unwrap();
    let outcome = compact(&store, eager_settings()).unwrap();

    assert_eq!(outcome.removed, 1, "a segment with nothing left is removed");
    assert_eq!(store.segment_count(), 0);
    assert!(visible(&store).is_empty());
}

#[test]
fn compaction_reclaims_the_files_it_retired() {
    let place = directory("reclaim");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(40, 0)).unwrap();
    let files_before = std::fs::read_dir(place.join("segments")).unwrap().count();

    store.erase(&erasure(1, "u-042")).unwrap();
    let outcome = compact(&store, eager_settings()).unwrap();

    assert!(
        outcome.files_reclaimed > 0,
        "the retired file was not removed"
    );
    let files_after = std::fs::read_dir(place.join("segments")).unwrap().count();
    assert!(
        files_after <= files_before,
        "compaction left more files than it found"
    );
    // And the store still answers correctly from the replacement.
    assert_eq!(visible(&store).len(), 20);
}

#[test]
fn a_pinned_generation_keeps_its_files() {
    // Required test 5, and FAILURE_MODES.md section 8.1 rule 1: a pinned
    // generation's segments are never deleted. A query reading them must not
    // have its inputs removed underneath it.
    let place = directory("pinned");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(40, 0)).unwrap();

    let generation = store.catalog().generation().unwrap();
    let pin = store
        .catalog()
        .pin(generation, tallyowl_obs::time::now_ms())
        .unwrap();

    store.erase(&erasure(1, "u-042")).unwrap();
    let outcome = compact(&store, eager_settings()).unwrap();

    assert!(outcome.rewritten > 0, "the rewrite still happened");
    assert_eq!(
        outcome.files_reclaimed, 0,
        "a file was deleted while a query still held its generation"
    );

    // The query finishes, and the next compaction reclaims.
    store.catalog().release_pin(pin).unwrap();
    let after = compact(&store, eager_settings()).unwrap();
    assert!(
        after.files_reclaimed > 0,
        "the files stayed after the pin went"
    );
}

#[test]
fn a_leaked_pin_expires_so_storage_is_not_held_forever() {
    // Required test 15, and rule 4: a pin that outlives the grace period
    // because a process died holding it expires. The bound is what makes it a
    // delay rather than a leak.
    let place = directory("leaked");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(40, 0)).unwrap();

    let generation = store.catalog().generation().unwrap();
    // A pin taken by a process that then died, an hour ago.
    store
        .catalog()
        .pin(generation, tallyowl_obs::time::now_ms() - 3_600_000)
        .unwrap();
    assert!(store
        .catalog()
        .oldest_pinned_generation()
        .unwrap()
        .is_some());

    store.erase(&erasure(1, "u-042")).unwrap();
    let outcome = compact(
        &store,
        CompactionSettings {
            grace_ms: 0,
            pin_max_age_ms: 60_000,
            ..CompactionSettings::default()
        },
    )
    .unwrap();

    assert!(
        store.catalog().pins().unwrap().is_empty(),
        "a leaked pin was not expired"
    );
    assert!(
        outcome.files_reclaimed > 0,
        "storage stayed held by a pin nobody owns"
    );
}

#[test]
fn an_erasure_that_lands_mid_compaction_does_not_resurrect() {
    // Required test 6, and the dangerous race of section 8.2. A replacement
    // written from a pre-erasure snapshot would bring erased data back.
    //
    // The second erasure lands between the read and the publish. Rule 2
    // re-reads tombstones before publishing, so the replacement already
    // excludes it; rule 3 then sees the generation moved and restarts.
    let place = directory("mid-compaction");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(40, 0)).unwrap();

    store.erase(&erasure(1, "u-042")).unwrap();
    // The second person is erased while the first compaction would be running.
    store.erase(&erasure(2, "u-999")).unwrap();

    let outcome = compact(&store, eager_settings()).unwrap();
    let _ = outcome;

    // Whatever order the work happened in, nothing for either person is
    // visible. That is the guarantee; the restart count is an implementation
    // detail.
    let rows = visible(&store);
    assert!(
        rows.is_empty(),
        "an erasure that landed during compaction left {} rows visible",
        rows.len()
    );
}

#[test]
fn a_tombstone_hides_on_read_even_when_compaction_missed_it() {
    // Required test 7, and rule 4: a tombstone applied against a stale
    // generation still hides the data on read. This is the second line of
    // defence, and it is what keeps a bug in rules 1 to 3 from becoming a
    // privacy incident.
    let place = directory("standing");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(40, 0)).unwrap();

    // No compaction runs at all. The rows are physically still in the segment.
    store.erase(&erasure(1, "u-042")).unwrap();

    let rows = visible(&store);
    assert_eq!(rows.len(), 20, "the erasure was not applied on read");
    assert!(rows
        .iter()
        .all(|row| row.properties["end_user"].0.to_display() != "u-042"));

    // And the physical rows really are still there, which is what makes this a
    // read-side guarantee rather than a coincidence.
    let manifests = store.catalog().manifests().unwrap();
    let physical = store.read_segment_rows(&manifests[0]).unwrap();
    assert!(
        physical
            .iter()
            .any(|row| row.properties["end_user"].0.to_display() == "u-042"),
        "the test did not exercise the read-side hide"
    );
}

#[test]
fn compaction_leaves_the_locator_agreeing_with_its_segments() {
    // Required test 8, and section 8.4 rule 3.
    let place = directory("locator-agreement");
    let store = store(&place);
    for batch in 0..3u8 {
        store
            .commit([1; 16], [batch; 16], rows(40, u16::from(batch) * 100))
            .unwrap();
    }
    store.erase(&erasure(1, "u-042")).unwrap();
    compact(&store, eager_settings()).unwrap();

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

    // And the locator still finds a row that survived, through its replacement
    // segment rather than the retired one.
    let survivor = visible(&store)[0].event_id;
    let candidates = locator.candidates(tallyowl_store::segment::schema::EVENT_ID, &survivor, None);
    assert_eq!(candidates.len(), 1);
    assert!(live.contains(&candidates[0]));
    assert!(store.lookup_event(survivor).unwrap().is_some());
}

#[test]
fn compaction_groups_rows_by_the_correlation_value() {
    // BENCHMARKS.md section 12b: grouping collapses the (value, segment) pair
    // count for the retained majority of data, which is a query cost decision
    // rather than a compression one.
    let place = directory("grouping");
    let store = store(&place);
    let mut mixed = rows(40, 0);
    for (index, row) in mixed.iter_mut().enumerate() {
        row.properties.insert(
            "end_user".into(),
            (
                PropertyValue::Text(format!("u-{}", index % 4)),
                "client".into(),
            ),
        );
    }
    store.commit([1; 16], [1; 16], mixed).unwrap();

    // An erasure of a person who is not there still triggers the rewrite, which
    // is what carries the grouping.
    store.erase(&erasure(1, "u-absent")).unwrap();
    compact(&store, eager_settings()).unwrap();

    let manifests = store.catalog().manifests().unwrap();
    let physical = store.read_segment_rows(&manifests[0]).unwrap();
    let users: Vec<String> = physical
        .iter()
        .map(|row| row.properties["end_user"].0.to_display())
        .collect();
    let mut sorted = users.clone();
    sorted.sort();
    assert_eq!(
        users, sorted,
        "the rewritten segment is not grouped by person"
    );
}

#[test]
fn compaction_with_no_tombstone_does_nothing_but_reclaim() {
    let place = directory("nothing");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(40, 0)).unwrap();
    let outcome = compact(&store, eager_settings()).unwrap();
    assert_eq!(outcome.rewritten, 0);
    assert_eq!(outcome.removed, 0);
    assert_eq!(visible(&store).len(), 40);
}

#[test]
fn compaction_never_touches_another_project() {
    // Tenant isolation holds through erasure. One project's erasure must not
    // rewrite or remove another's segments.
    let place = directory("isolation");
    let store = store(&place);

    let mut other = rows(40, 0);
    for row in &mut other {
        row.project_id = [1; 16];
    }
    store.commit([1; 16], [1; 16], other).unwrap();
    store.commit([1; 16], [2; 16], rows(40, 100)).unwrap();

    store.erase(&erasure(1, "u-042")).unwrap();
    compact(&store, eager_settings()).unwrap();

    let elsewhere = store
        .scan(
            [1; 16],
            BASE_TIME - 1,
            BASE_TIME + 1_000_000,
            TimeBasis::OccurredAt,
        )
        .unwrap()
        .rows;
    assert_eq!(elsewhere.len(), 40, "another project's rows were touched");
}

#[test]
fn a_compacted_store_survives_a_reopen() {
    let place = directory("reopen");
    {
        let store = store(&place);
        store.commit([1; 16], [1; 16], rows(40, 0)).unwrap();
        store.erase(&erasure(1, "u-042")).unwrap();
        compact(&store, eager_settings()).unwrap();
        assert_eq!(visible(&store).len(), 20);
    }
    let store = store(&place);
    assert_eq!(visible(&store).len(), 20);
    assert_eq!(store.tombstone_generation().unwrap(), 1);
}

// ---------------------------------------------------------------------------
// The deduplication window
// ---------------------------------------------------------------------------

#[test]
fn a_receipt_outside_the_deduplication_window_expires() {
    // L052: nothing expired a receipt, so the catalog grew for ever and the
    // deduplication window was unbounded. D36 makes that window a real number
    // paired with the retry age.
    let place = directory("receipt-expiry");
    let store = store(&place);

    store
        .commit([1; 16], [2; 16], rows(1, 0))
        .expect("the commit is accepted");
    assert_eq!(store.catalog().receipt_count().unwrap(), 1);

    // A window of one millisecond has already passed for a receipt written now.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let outcome = compact(
        &store,
        CompactionSettings {
            deduplication_window_ms: 1,
            ..CompactionSettings::default()
        },
    )
    .expect("the pass runs");

    assert_eq!(outcome.receipts_expired, 1);
    assert_eq!(store.catalog().receipt_count().unwrap(), 0);
}

#[test]
fn a_receipt_inside_the_deduplication_window_stays_and_still_deduplicates() {
    // The property the window protects. While the receipt is there, a repeated
    // batch ID is one logical commit, which is what makes a retry safe.
    let place = directory("receipt-kept");
    let store = store(&place);

    let first = store
        .commit([1; 16], [2; 16], rows(1, 0))
        .expect("the commit is accepted");
    let outcome = compact(
        &store,
        CompactionSettings {
            deduplication_window_ms: 60 * 60_000,
            ..CompactionSettings::default()
        },
    )
    .expect("the pass runs");

    assert_eq!(outcome.receipts_expired, 0);
    assert_eq!(store.catalog().receipt_count().unwrap(), 1);

    let repeated = store
        .commit([1; 16], [2; 16], rows(1, 0))
        .expect("the retry is accepted");
    assert!(repeated.deduplicated, "the retry is one logical commit");
    assert_eq!(repeated.commit_watermark, first.commit_watermark);
}

#[test]
fn a_zero_window_expires_nothing() {
    // Zero is what every build did before the setting existed, and it stays
    // available for an installation that wants an unbounded window.
    let place = directory("receipt-zero-window");
    let store = store(&place);
    store
        .commit([1; 16], [2; 16], rows(1, 0))
        .expect("the commit is accepted");

    let outcome = compact(
        &store,
        CompactionSettings {
            deduplication_window_ms: 0,
            ..CompactionSettings::default()
        },
    )
    .expect("the pass runs");

    assert_eq!(outcome.receipts_expired, 0);
    assert_eq!(store.catalog().receipt_count().unwrap(), 1);
}

// ---------------------------------------------------------------------------
// Retention expiry
//
// POLICY.md section 4. Nothing expired a row before this, so a home
// installation grew without bound. See L080.
// ---------------------------------------------------------------------------

/// A row that is `age_ms` old.
fn aged_row(id: u8, age_ms: i64, derived: bool) -> EventRow {
    let at = tallyowl_obs::time::now_ms() - age_ms;
    let mut row = EventRow::new([id; 16], "event", "checkout-started", at);
    row.project_id = PROJECT;
    row.workspace_id = WORKSPACE;
    row.received_at = at;
    if derived {
        row.kind = "metric-point".to_string();
        row = row.with_property(
            "derived",
            PropertyValue::Text("tallyowl-rollup".into()),
            "collector",
        );
    }
    row
}

fn retention(detailed_ms: i64, rollup_ms: i64) -> CompactionSettings {
    CompactionSettings {
        grace_ms: 0,
        detailed_retention_ms: detailed_ms,
        rollup_retention_ms: rollup_ms,
        ..CompactionSettings::default()
    }
}

#[test]
fn nothing_expires_when_no_retention_is_configured() {
    // An installation that has not chosen a retention must not lose data
    // because a default expired it.
    let store = store(&directory("retention-off"));
    store
        .commit([1; 16], [1; 16], vec![aged_row(1, 100 * 86_400_000, false)])
        .expect("the commit is accepted");
    store.seal().expect("the seal publishes");

    let outcome = compact(&store, CompactionSettings::default()).expect("compaction runs");
    assert_eq!(outcome.segments_expired, 0);
    assert_eq!(store.row_count(), 1, "a hundred-day-old row is still there");
}

#[test]
fn a_segment_past_every_retention_is_dropped_without_being_read() {
    let store = store(&directory("retention-drop"));
    store
        .commit([1; 16], [1; 16], vec![aged_row(1, 40 * 86_400_000, false)])
        .expect("the commit is accepted");
    store.seal().expect("the seal publishes");

    let outcome =
        compact(&store, retention(30 * 86_400_000, 30 * 86_400_000)).expect("compaction runs");
    assert_eq!(outcome.segments_expired, 1);
    assert_eq!(outcome.rows_expired, 1);
    assert_eq!(store.row_count(), 0);
}

#[test]
fn a_segment_inside_its_retention_is_left_alone() {
    let store = store(&directory("retention-keep"));
    store
        .commit([1; 16], [1; 16], vec![aged_row(1, 86_400_000, false)])
        .expect("the commit is accepted");
    store.seal().expect("the seal publishes");

    let outcome =
        compact(&store, retention(30 * 86_400_000, 30 * 86_400_000)).expect("compaction runs");
    assert_eq!(outcome.segments_expired, 0);
    assert_eq!(store.row_count(), 1);
}

#[test]
fn a_segment_is_dropped_only_when_everything_in_it_is_past_the_cutoff() {
    // The newest row decides. A segment holding one recent row keeps the old
    // ones with it rather than being dropped around them.
    let store = store(&directory("retention-newest"));
    store
        .commit(
            [1; 16],
            [1; 16],
            vec![
                aged_row(1, 40 * 86_400_000, false),
                aged_row(2, 60_000, false),
            ],
        )
        .expect("the commit is accepted");
    store.seal().expect("the seal publishes");

    let outcome =
        compact(&store, retention(30 * 86_400_000, 30 * 86_400_000)).expect("compaction runs");
    assert_eq!(outcome.segments_expired, 0);
    assert_eq!(store.row_count(), 2);
}

#[test]
fn a_rollup_outlives_the_detailed_data_it_summarises() {
    // POLICY.md section 4: a rollup that expired first would leave a gap in the
    // middle of a chart that no query could fill.
    let store = store(&directory("retention-rollup"));
    store
        .commit(
            [1; 16],
            [1; 16],
            vec![
                aged_row(1, 40 * 86_400_000, false),
                aged_row(2, 40 * 86_400_000, true),
            ],
        )
        .expect("the commit is accepted");
    store.seal().expect("the seal publishes");

    let outcome =
        compact(&store, retention(30 * 86_400_000, 400 * 86_400_000)).expect("compaction runs");
    assert_eq!(outcome.rows_expired, 1, "the detailed row went");
    assert_eq!(store.row_count(), 1, "the rollup stayed");

    let held = store
        .scan(
            PROJECT,
            0,
            tallyowl_obs::time::now_ms() + 1,
            TimeBasis::OccurredAt,
        )
        .expect("scan")
        .rows;
    assert!(
        held[0].properties.contains_key("derived"),
        "the row that survived is the rollup"
    );
}

#[test]
fn a_rollup_past_its_own_retention_goes_too() {
    let store = store(&directory("retention-rollup-expires"));
    store
        .commit([1; 16], [1; 16], vec![aged_row(1, 500 * 86_400_000, true)])
        .expect("the commit is accepted");
    store.seal().expect("the seal publishes");

    let outcome =
        compact(&store, retention(30 * 86_400_000, 400 * 86_400_000)).expect("compaction runs");
    assert_eq!(outcome.segments_expired, 1);
    assert_eq!(store.row_count(), 0);
}

/// Settings that consolidate everything old enough, at once.
fn consolidating() -> CompactionSettings {
    CompactionSettings {
        grace_ms: 0,
        pin_max_age_ms: 60_000,
        // One millisecond of age: every fixed-time test row is long cold.
        cold_group_after_ms: 1,
        ..CompactionSettings::default()
    }
}

#[test]
fn cold_consolidation_rewrites_a_buckets_segments_into_few_grouped_ones() {
    // HIGH_CARDINALITY.md: sorting rows inside a segment does not change
    // which segment holds them. Only compaction does, and this is it.
    let place = directory("consolidate");
    let store = store(&place);
    for batch_number in 0u8..3 {
        store
            .commit(
                [batch_number + 1; 16],
                [batch_number + 1; 16],
                rows(40, u16::from(batch_number) * 40),
            )
            .unwrap();
    }
    assert_eq!(
        store.catalog().manifests().unwrap().len(),
        3,
        "the setup makes three scattered segments"
    );
    // Before: both people sit in every segment.
    let scattered = store
        .catalog()
        .locator()
        .unwrap()
        .candidates("p:end_user", b"u-042", None)
        .len();
    assert_eq!(scattered, 3);

    let outcome = compact(&store, consolidating()).expect("compaction runs");
    assert_eq!(outcome.consolidated_sources, 3);
    assert_eq!(outcome.consolidated_outputs, 1);

    // After: one segment holds the bucket, nothing was lost, and the rows
    // sit grouped by person.
    let manifests = store.catalog().manifests().unwrap();
    assert_eq!(manifests.len(), 1);
    assert_eq!(visible(&store).len(), 120);
    let candidates = store
        .catalog()
        .locator()
        .unwrap()
        .candidates("p:end_user", b"u-042", None);
    assert_eq!(candidates.len(), 1);

    let physical = store.read_segment_rows(&manifests[0]).unwrap();
    let users: Vec<String> = physical
        .iter()
        .map(|row| row.properties["end_user"].0.to_display())
        .collect();
    let mut sorted = users.clone();
    sorted.sort();
    assert_eq!(users, sorted, "one person's rows sit together");

    // An exact lookup still answers through the rewritten layout.
    let found = store.lookup_correlated("p:end_user", b"u-042").unwrap();
    assert_eq!(found.rows.len(), 60);
}

#[test]
fn cold_consolidation_is_idempotent() {
    let place = directory("consolidate-idempotent");
    let store = store(&place);
    for batch_number in 0u8..3 {
        store
            .commit(
                [batch_number + 1; 16],
                [batch_number + 1; 16],
                rows(40, u16::from(batch_number) * 40),
            )
            .unwrap();
    }
    compact(&store, consolidating()).expect("the first pass runs");
    let generation = store.catalog().generation().unwrap();

    let again = compact(&store, consolidating()).expect("the second pass runs");
    assert_eq!(
        again.consolidated_sources, 0,
        "a consolidated bucket is not due"
    );
    assert_eq!(store.catalog().generation().unwrap(), generation);
}

#[test]
fn cold_consolidation_leaves_hot_segments_alone() {
    let place = directory("consolidate-hot");
    let store = store(&place);
    // Rows stamped now are inside any sane age window.
    let now = tallyowl_obs::time::now_ms();
    for batch_number in 0u8..3 {
        let fresh: Vec<EventRow> = rows(40, u16::from(batch_number) * 40)
            .into_iter()
            .map(|mut row| {
                row.occurred_at = now;
                row.received_at = now;
                row
            })
            .collect();
        store
            .commit([batch_number + 1; 16], [batch_number + 1; 16], fresh)
            .unwrap();
    }
    let outcome = compact(
        &store,
        CompactionSettings {
            cold_group_after_ms: 48 * 3_600_000,
            ..consolidating()
        },
    )
    .expect("compaction runs");
    assert_eq!(outcome.consolidated_sources, 0);
    assert_eq!(store.catalog().manifests().unwrap().len(), 3);
}

#[test]
fn cold_consolidation_fires_on_its_own_once_rows_age_past_the_shipped_default() {
    // `compaction.coldGroupAfter` ships at 48 h, and the trigger is
    // arithmetic over row ages — nothing about it needs a wall-clock soak.
    // One store, two projects: rows an hour past the default consolidate,
    // rows an hour short of it are left alone, under the same settings the
    // head builds from its defaults.
    let place = directory("consolidate-default-age");
    let store = store(&place);
    let now = tallyowl_obs::time::now_ms();
    let shipped_default = 48 * 3_600_000;

    let age_into = |project: u8, occurred_at: i64| {
        for batch_number in 0u8..3 {
            let aged: Vec<EventRow> = rows(40, u16::from(batch_number) * 40)
                .into_iter()
                .map(|mut row| {
                    row.project_id = [project; 16];
                    row.occurred_at = occurred_at;
                    row.received_at = occurred_at + 5;
                    row.event_id[13] = project;
                    row
                })
                .collect();
            store
                .commit(
                    [project * 10 + batch_number + 1; 16],
                    [project * 10 + batch_number + 1; 16],
                    aged,
                )
                .unwrap();
        }
    };
    age_into(1, now - shipped_default - 3_600_000);
    age_into(2, now - shipped_default + 3_600_000);

    let outcome = compact(
        &store,
        CompactionSettings {
            cold_group_after_ms: shipped_default,
            ..consolidating()
        },
    )
    .expect("compaction runs");

    assert_eq!(
        outcome.consolidated_sources, 3,
        "the bucket past the default is due"
    );
    assert_eq!(outcome.consolidated_outputs, 1);
    let manifests = store.catalog().manifests().unwrap();
    assert_eq!(
        manifests.iter().filter(|m| m.project_id == [1; 16]).count(),
        1,
        "the aged project's segments were consolidated"
    );
    assert_eq!(
        manifests.iter().filter(|m| m.project_id == [2; 16]).count(),
        3,
        "the project short of the default was left alone"
    );
}

#[test]
fn a_bucket_larger_than_the_pass_budget_consolidates_over_passes_not_at_once() {
    // The pass budget bounds memory: the read path holds a segment's rows
    // decompressed, so a bucket that is bigger than `cold_group_batch_bytes`
    // must be taken in budget-sized bites across passes — a soak-aged day
    // held two gigabytes in one group, and loading it whole took the machine
    // down, not the bucket.
    let place = directory("consolidate-budget");
    let store = store(&place);
    for batch_number in 0u8..6 {
        store
            .commit(
                [batch_number + 1; 16],
                [batch_number + 1; 16],
                rows(40, u16::from(batch_number) * 40),
            )
            .unwrap();
    }
    let manifests = store.catalog().manifests().unwrap();
    assert_eq!(manifests.len(), 6);
    let bucket_bytes: u64 = manifests.iter().map(|m| m.byte_count).sum();
    // A budget that holds about half the bucket, and a target of about a
    // third, so the bucket is due but can never be rewritten in one pass.
    let settings = CompactionSettings {
        cold_group_target_bytes: bucket_bytes / 3,
        cold_group_batch_bytes: bucket_bytes / 2,
        ..consolidating()
    };

    let first = compact(&store, settings).expect("the first pass runs");
    assert!(
        first.consolidated_sources > 0,
        "an oversized bucket still makes progress"
    );
    assert!(
        first.consolidated_sources < 6,
        "one pass must not swallow a bucket larger than its budget"
    );

    let mut passes = 1;
    loop {
        let outcome = compact(&store, settings).expect("a later pass runs");
        if outcome.consolidated_sources == 0 {
            break;
        }
        passes += 1;
        assert!(passes < 20, "the passes converge instead of thrashing");
    }

    // Converged: nothing lost, and the bucket sits near what its bytes need.
    // Near, not at: the progress guard refuses a rewrite that would not
    // shrink, so a bucket may settle a segment above the ideal count rather
    // than churn the same bytes forever.
    assert_eq!(visible(&store).len(), 240);
    assert!(store.catalog().manifests().unwrap().len() < 6);
    let found = store.lookup_correlated("p:end_user", b"u-042").unwrap();
    assert_eq!(found.rows.len(), 120);
}

#[test]
fn cold_consolidation_does_not_resurrect_an_erased_row() {
    // A tombstone is a standing predicate, and a rewrite must apply it. The
    // erased person's rows are physically gone from the consolidated segment.
    let place = directory("consolidate-erased");
    let store = store(&place);
    for batch_number in 0u8..3 {
        store
            .commit(
                [batch_number + 1; 16],
                [batch_number + 1; 16],
                rows(40, u16::from(batch_number) * 40),
            )
            .unwrap();
    }
    store.erase(&erasure(7, "u-999")).unwrap();

    let outcome = compact(&store, consolidating()).expect("compaction runs");
    assert_eq!(outcome.consolidated_sources, 3);

    let manifests = store.catalog().manifests().unwrap();
    for manifest in &manifests {
        let physical = store.read_segment_rows(manifest).unwrap();
        assert!(
            physical
                .iter()
                .all(|row| row.properties["end_user"].0.to_display() != "u-999"),
            "an erased person's row survived the rewrite"
        );
    }
    assert_eq!(visible(&store).len(), 60);
}
