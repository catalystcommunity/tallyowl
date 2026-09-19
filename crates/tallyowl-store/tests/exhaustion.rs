//! Disk exhaustion at each point of `docs/FAILURE_MODES.md` section 10.
//!
//! This is required test 13.
//!
//! | Point of exhaustion | Stated behaviour | Test |
//! | --- | --- | --- |
//! | Append log | Stop accepting writes; never accept a write that cannot be made durable | `the_append_log_refuses_*` |
//! | Segment publish | Keep the log range; the log is the durable record | `a_segment_publish_*` |
//! | Compaction | Abandon the attempt, keep the source segments | `compaction_abandons_*` |
//! | Catalog | Stop accepting control writes; serve reads | `the_catalog_*` |
//! | Cold-tier cache | Evict cached copies; a cached copy is never the only copy | `the_cold_cache_*` |
//! | Export | Fail the export; never displace live data | in `tallyowl-export` |
//!
//! # Why these pretend
//!
//! Filling a real device would need a device to fill, would take minutes, and
//! would leave a workstation in a state a failed test cannot undo. The write
//! paths are what is under test here. The reading of the device itself is
//! covered by a unit test in `src/space.rs` that calls `statvfs` for real, so
//! the pretence cannot hide a broken reading.

use std::path::PathBuf;

use tallyowl_store::catalog::Tombstone;
use tallyowl_store::compact::{compact, CompactionSettings};
use tallyowl_store::row::{EventRow, PropertyValue};
use tallyowl_store::segmented::{Sealing, SegmentedStore};
use tallyowl_store::space::{Point, Space};
use tallyowl_store::store::StoreError;
use tallyowl_store::tier::{ColdCache, ColdStore, FilesystemColdStore};
use tallyowl_store::wal::GroupCommit;
use tallyowl_store::{Store, TimeBasis};

const PROJECT: [u8; 16] = [9; 16];
const BASE_TIME: i64 = 1_785_628_800_000;
const RESERVE: u64 = 1_000_000;

fn directory(name: &str) -> PathBuf {
    let path = PathBuf::from("target")
        .join("exhaustion-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a place to work");
    path
}

fn store(place: &PathBuf) -> SegmentedStore {
    SegmentedStore::open_with(
        place,
        Sealing {
            max_open_rows: 40,
            max_open_ms: i64::MAX,
            verify_on_read: true,
            reserve_bytes: RESERVE,
        },
        GroupCommit {
            linger: std::time::Duration::from_millis(0),
            ..GroupCommit::default()
        },
    )
    .expect("the store opens")
}

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
            row.project_id = PROJECT;
            row.received_at = row.occurred_at + 5;
            let user = if index % 2 == 0 { "u-042" } else { "u-999" };
            row.with_property("end_user", PropertyValue::Text(user.into()), "client")
        })
        .collect()
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

fn erasure() -> Tombstone {
    Tombstone {
        tombstone_id: [1; 16],
        generation: 0,
        project_id: PROJECT,
        event_ids: Vec::new(),
        property: Some(("end_user".to_string(), "u-042".to_string())),
        range: None,
        requested_at: BASE_TIME,
        horizon: BASE_TIME + 30 * 86_400_000,
        reason: "The end user asked for their data to be removed.".into(),
        except_kinds: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Append log
// ---------------------------------------------------------------------------

#[test]
fn the_append_log_refuses_a_write_it_cannot_make_durable() {
    let place = directory("append-log");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(10, 0)).unwrap();

    // Inside the reserve. Nothing bulk may be written.
    store.space().pretend_free_bytes(RESERVE / 2);

    let refused = store.commit([1; 16], [2; 16], rows(10, 100)).unwrap_err();
    assert!(
        matches!(refused, StoreError::Exhausted(_)),
        "a full device gave the wrong kind of failure: {refused:?}"
    );
    assert!(refused
        .to_string()
        .contains("Nothing already accepted is lost"));

    // Nothing acknowledged was lost, and no receipt claims a batch that is not
    // there.
    assert_eq!(visible(&store).len(), 10);
    assert!(store.receipt([1; 16], [2; 16]).is_none());
    assert_eq!(store.commit_watermark(), 1, "a refused batch took a number");
}

#[test]
fn the_append_log_accepts_again_when_space_comes_back() {
    let place = directory("append-log-recovers");
    let store = store(&place);
    store.space().pretend_free_bytes(0);
    assert!(store.commit([1; 16], [1; 16], rows(10, 0)).is_err());

    store.stop_pretending_the_device_is_full();
    store
        .commit([1; 16], [1; 16], rows(10, 0))
        .expect("space came back");
    assert_eq!(visible(&store).len(), 10);
}

#[test]
fn a_retry_of_a_committed_batch_still_answers_when_the_device_is_full() {
    // Deduplication answers from the receipt and writes nothing, so a full
    // device must not turn one logical commit into a failure the caller retries
    // for ever.
    let place = directory("dedup-when-full");
    let store = store(&place);
    let first = store.commit([1; 16], [1; 16], rows(10, 0)).unwrap();

    store.space().pretend_free_bytes(0);

    let again = store
        .commit([1; 16], [1; 16], rows(10, 0))
        .expect("the receipt answers");
    assert!(again.deduplicated);
    assert_eq!(again.commit_watermark, first.commit_watermark);
}

// ---------------------------------------------------------------------------
// Segment publish
// ---------------------------------------------------------------------------

#[test]
fn a_segment_publish_keeps_the_log_range_and_loses_nothing() {
    let place = directory("publish");
    let store = store(&place);

    // Enough rows to reach the seal threshold, with room for the log write and
    // not for the segment. The log write is the smaller of the two because the
    // seal only happens once the buffer is full.
    store.commit([1; 16], [1; 16], rows(39, 0)).unwrap();
    assert_eq!(store.segment_count(), 0);

    store.space().pretend_free_bytes(RESERVE + 1_000);
    store
        .commit([1; 16], [2; 16], rows(1, 100))
        .expect("the log write fits");

    // The publish was refused and the commit was not.
    assert_eq!(
        store.segment_count(),
        0,
        "a segment was published with no room"
    );
    assert_eq!(
        visible(&store).len(),
        40,
        "rows disappeared when a seal was refused"
    );
    assert!(store.receipt([1; 16], [2; 16]).is_some());

    // And it seals when there is room again, from the same log range.
    store.stop_pretending_the_device_is_full();
    store.seal().expect("the seal works now");
    assert_eq!(store.segment_count(), 1);
    assert_eq!(visible(&store).len(), 40);
}

#[test]
fn a_refused_seal_survives_a_restart_because_the_log_still_holds_it() {
    // "The log is the durable record until a segment replaces it" is only true
    // if a restart replays it, so this restarts.
    let place = directory("publish-restart");
    {
        let store = store(&place);
        store.commit([1; 16], [1; 16], rows(39, 0)).unwrap();
        store.space().pretend_free_bytes(RESERVE + 1_000);
        store.commit([1; 16], [2; 16], rows(1, 100)).unwrap();
        assert_eq!(store.segment_count(), 0);
    }

    let store = store(&place);
    assert_eq!(
        visible(&store).len(),
        40,
        "a refused seal lost data across a restart"
    );
}

// ---------------------------------------------------------------------------
// Compaction
// ---------------------------------------------------------------------------

#[test]
fn compaction_abandons_the_attempt_and_keeps_the_source_segments() {
    let place = directory("compaction");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(40, 0)).unwrap();
    store.commit([1; 16], [2; 16], rows(40, 100)).unwrap();
    assert_eq!(store.segment_count(), 2);
    store.erase(&erasure()).unwrap();

    store.space().pretend_free_bytes(RESERVE);

    let outcome =
        compact(&store, CompactionSettings::default()).expect("compaction stops, not fails");
    assert!(outcome.abandoned_for_space, "{outcome:?}");
    assert_eq!(outcome.rows_erased, 0);

    // The sources are exactly as they were, and no answer changed: the erased
    // rows stay hidden because a tombstone is a standing predicate.
    assert_eq!(store.segment_count(), 2);
    let rows = visible(&store);
    assert_eq!(rows.len(), 40);
    assert!(rows
        .iter()
        .all(|row| row.properties["end_user"].0.to_display() != "u-042"));

    // And it finishes when there is room.
    store.stop_pretending_the_device_is_full();
    let outcome = compact(&store, CompactionSettings::default()).unwrap();
    assert!(!outcome.abandoned_for_space);
    assert_eq!(outcome.rows_erased, 40);
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

#[test]
fn the_catalog_stops_control_writes_and_still_serves_reads() {
    let place = directory("catalog");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(40, 0)).unwrap();

    store
        .guard_control_write()
        .expect("room for a settings change");

    store.space().pretend_free_bytes(RESERVE / 2);

    let refused = store.guard_control_write().unwrap_err();
    assert!(matches!(refused, StoreError::Exhausted(_)));
    assert!(refused.to_string().contains("Queries still answer"));

    // Reads are unaffected, which is the half of the rule that is easy to lose.
    assert_eq!(visible(&store).len(), 40);
    assert_eq!(store.catalog().manifests().unwrap().len(), 1);
}

#[test]
fn an_erasure_may_use_the_reserve() {
    // An erasure is not a control change. It is small, it is an obligation, and
    // FAILURE_MODES.md section 9 makes its record durable independently. The
    // reserve exists for exactly this kind of write.
    let place = directory("erasure-reserve");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(40, 0)).unwrap();

    store.space().pretend_free_bytes(RESERVE / 2);
    assert!(
        store.guard_control_write().is_err(),
        "the test proves nothing"
    );

    store
        .erase(&erasure())
        .expect("an erasure may use the reserve");
    assert_eq!(visible(&store).len(), 20);

    // And a device with nothing at all left refuses even this, because there is
    // no way to write it.
    store.space().pretend_free_bytes(0);
    let mut second = erasure();
    second.tombstone_id = [2; 16];
    second.property = Some(("end_user".to_string(), "u-999".to_string()));
    assert!(matches!(
        store.erase(&second).unwrap_err(),
        StoreError::Exhausted(_)
    ));
}

// ---------------------------------------------------------------------------
// Cold-tier cache
// ---------------------------------------------------------------------------

#[test]
fn the_cold_cache_can_be_thrown_away_because_it_is_never_the_only_copy() {
    let place = directory("cold-cache");
    let cold = FilesystemColdStore::new(place.join("cold")).unwrap();
    let cache = ColdCache::new(place.join("cache"), 1024 * 1024).unwrap();

    cold.put("aaaa/one.tos", &vec![1u8; 4_096]).unwrap();
    cold.put("bbbb/two.tos", &vec![2u8; 4_096]).unwrap();

    assert_eq!(
        cache.read_through(&cold, "aaaa/one.tos").unwrap().len(),
        4_096
    );
    assert_eq!(
        cache.read_through(&cold, "bbbb/two.tos").unwrap().len(),
        4_096
    );
    assert_eq!(cache.bytes(), 8_192);
    assert!(
        cache.get("aaaa/one.tos").is_some(),
        "the second read did not hit the cache"
    );

    let removed = cache.evict_all().unwrap();
    assert_eq!(removed, 2);
    assert_eq!(cache.bytes(), 0);

    // Every evicted byte is still reachable. That is what makes the eviction
    // the one point of exhaustion with no refusal and no lost work.
    assert_eq!(
        cache.read_through(&cold, "aaaa/one.tos").unwrap().len(),
        4_096
    );
}

#[test]
fn the_cold_cache_stays_inside_its_bound() {
    let place = directory("cold-cache-bound");
    let cache = ColdCache::new(place.join("cache"), 10_000).unwrap();

    for index in 0..8 {
        cache
            .put(&format!("p/{index}.tos"), &vec![index as u8; 2_000])
            .unwrap();
    }
    assert!(
        cache.bytes() <= 10_000,
        "the cache grew past its bound: {} bytes",
        cache.bytes()
    );

    // An object bigger than the whole cache is not cached, rather than emptying
    // the cache to hold one thing.
    cache.put("p/huge.tos", &vec![7u8; 20_000]).unwrap();
    assert!(cache.get("p/huge.tos").is_none());
    assert!(cache.bytes() > 0, "one large object emptied the cache");
}

// ---------------------------------------------------------------------------
// Readiness
// ---------------------------------------------------------------------------

#[test]
fn readiness_says_so_before_the_device_is_full() {
    // Section 10, append log: "fail readiness before the device is full". A
    // node that only says so once it is full has already refused writes it
    // could have had somebody else take.
    let place = directory("readiness");
    let space = Space::new(&place, RESERVE);

    space.pretend_free_bytes(RESERVE + tallyowl_store::space::LOW_WATER_BYTES + 1);
    assert!(!space.is_low());

    space.pretend_free_bytes(RESERVE + tallyowl_store::space::LOW_WATER_BYTES / 2);
    assert!(space.is_low(), "readiness waited for the device to fill");
    // Still accepting writes at this point, which is the whole idea.
    space
        .check_bulk(Point::AppendLog, 1_000)
        .expect("still writable");
}

#[test]
fn every_point_of_exhaustion_names_itself_and_what_it_did() {
    // A refusal that says only "the disk is full" does not tell an operator
    // whether ingest stopped or a compaction gave up.
    let place = directory("named");
    let space = Space::new(&place, RESERVE);
    space.pretend_free_bytes(0);

    for point in [
        Point::AppendLog,
        Point::SegmentPublish,
        Point::Compaction,
        Point::Catalog,
        Point::ColdCache,
        Point::Export,
    ] {
        let refused = space.check_bulk(point, 1).unwrap_err().to_string();
        assert!(
            refused.contains(point.behaviour()),
            "{} did not say what happened: {refused}",
            point.as_str()
        );
    }
    assert_eq!(space.refusals().iter().sum::<u64>(), 6);
}
