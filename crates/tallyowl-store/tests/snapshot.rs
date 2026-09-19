//! Snapshot, restore, and rebuild, and the recovery procedures they implement.
//!
//! | Required test | Where |
//! | --- | --- |
//! | FAILURE_MODES.md section 14 test 10: a catalog rebuild with snapshots and without, each asserting exactly what section 7 says survives | `a_rebuild_*` |
//! | FAILURE_MODES.md section 14 test 14: a restore that must not resurrect an erased end user | `a_restore_does_not_resurrect_an_erased_end_user` |
//! | SEGMENT_FORMAT.md section 16 test 9: a catalog rebuilt by scanning manifests only | `a_rebuild_finds_every_segment_by_scanning` |

use std::path::PathBuf;

use tallyowl_store::catalog::Tombstone;
use tallyowl_store::row::{EventRow, PropertyValue};
use tallyowl_store::segmented::{Sealing, SegmentedStore};
use tallyowl_store::snapshot;
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
        .join("snapshot-tests")
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
fn a_snapshot_pins_what_the_design_says_it_pins() {
    // STORAGE.md section 13: the catalog generation, the log checkpoint, the
    // required segment identifiers and checksums, and the tombstone generation.
    let place = directory("pins");
    let data = place.join("data");
    let into = place.join("snapshot");
    {
        let store = store(&data);
        store.commit([1; 16], [1; 16], rows(40, 0)).unwrap();
        store.commit([1; 16], [2; 16], rows(40, 100)).unwrap();
        store.erase(&erasure()).unwrap();
    }

    let taken = snapshot::take(&data, &into, BASE_TIME).unwrap();
    assert!(taken.manifest_generation > 0);
    assert_eq!(taken.tombstone_generation, 1);
    assert_eq!(taken.commit_watermark, 2);
    assert_eq!(taken.erasures, 1);
    assert_eq!(taken.segments.len(), 2, "two segments were sealed");

    let text = std::fs::read_to_string(snapshot::description_path(&into)).unwrap();
    assert!(text.contains("tombstone_generation: 1"));
    assert!(text.contains("segments: 2"));
    assert!(text.contains("segment: "), "the segments are not named");
}

#[test]
fn a_second_snapshot_copies_only_what_is_new() {
    // Only new segments need copying after the first, which is what makes an
    // incremental backup policy possible at all.
    let place = directory("incremental");
    let data = place.join("data");
    let into = place.join("snapshot");
    // A running node snapshots itself: it already holds the catalog lock.
    let store = store(&data);
    store.commit([1; 16], [1; 16], rows(40, 0)).unwrap();

    let first = store.snapshot(&into, BASE_TIME).unwrap();
    assert_eq!(first.segments.len(), 1);
    let written_at = std::fs::metadata(into.join(format!(
        "segments/{}.tos",
        tallyowl_store::row::hex(&first.segments[0].0)
    )))
    .unwrap()
    .modified()
    .unwrap();

    store.commit([1; 16], [2; 16], rows(40, 100)).unwrap();
    let second = store.snapshot(&into, BASE_TIME + 1_000).unwrap();
    assert_eq!(second.segments.len(), 2);

    // The first segment's file was not rewritten, because a segment is
    // immutable and content-addressed.
    let again = std::fs::metadata(into.join(format!(
        "segments/{}.tos",
        tallyowl_store::row::hex(&first.segments[0].0)
    )))
    .unwrap()
    .modified()
    .unwrap();
    assert_eq!(written_at, again, "an unchanged segment was copied again");
}

#[test]
fn a_restore_brings_back_every_row() {
    let place = directory("restore");
    let data = place.join("data");
    let into = place.join("snapshot");
    let fresh = place.join("restored");
    {
        let store = store(&data);
        store.commit([1; 16], [1; 16], rows(40, 0)).unwrap();
        store.commit([1; 16], [2; 16], rows(40, 100)).unwrap();
        assert_eq!(visible(&store).len(), 80);
    }
    snapshot::take(&data, &into, BASE_TIME).unwrap();

    let report = snapshot::restore(&into, &fresh).unwrap();
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(report.segments_restored, 2);

    let store = store(&fresh);
    assert_eq!(visible(&store).len(), 80);
    assert!(
        store.receipt([1; 16], [1; 16]).is_some(),
        "a restore lost the receipts, so an in-flight retry would duplicate"
    );
}

#[test]
fn a_restore_does_not_resurrect_an_erased_end_user() {
    // FAILURE_MODES.md section 14 test 14, and section 9 rule 4: the erasure
    // ledger travels with a snapshot and with a restore.
    let place = directory("erasure");
    let data = place.join("data");
    let into = place.join("snapshot");
    let fresh = place.join("restored");
    {
        let store = store(&data);
        store.commit([1; 16], [1; 16], rows(40, 0)).unwrap();
        store.commit([1; 16], [2; 16], rows(40, 100)).unwrap();
        store.erase(&erasure()).unwrap();
        assert_eq!(visible(&store).len(), 40);
    }
    snapshot::take(&data, &into, BASE_TIME).unwrap();

    let report = snapshot::restore(&into, &fresh).unwrap();
    assert!(report.is_complete());

    let store = store(&fresh);
    let rows = visible(&store);
    assert_eq!(rows.len(), 40, "a restore brought erased rows back");
    assert!(rows
        .iter()
        .all(|row| row.properties["end_user"].0.to_display() != "u-042"));
    assert_eq!(store.tombstone_generation().unwrap(), 1);
}

#[test]
fn a_snapshot_keeps_rows_that_are_not_yet_in_a_segment() {
    // A row that is acknowledged but still only in the log is acknowledged
    // data. A snapshot that copied segments alone would drop it and still look
    // complete, which is the failure FAILURE_MODES.md section 2 ranks worst.
    let place = directory("open-rows");
    let data = place.join("data");
    let into = place.join("snapshot");
    let fresh = place.join("restored");
    {
        let store = store(&data);
        // Below the seal threshold on purpose, so nothing is in a segment.
        store.commit([1; 16], [1; 16], rows(10, 0)).unwrap();
        assert_eq!(
            store.segment_count(),
            0,
            "the test seals when it should not"
        );
        assert_eq!(visible(&store).len(), 10);
    }

    let taken = snapshot::take(&data, &into, BASE_TIME).unwrap();
    assert!(taken.segments.is_empty());

    let report = snapshot::restore(&into, &fresh).unwrap();
    assert!(report.is_complete());

    let store = store(&fresh);
    assert_eq!(
        visible(&store).len(),
        10,
        "a snapshot lost rows that were acknowledged but not yet in a segment"
    );
}

#[test]
fn a_restore_never_silently_skips_a_missing_file() {
    // STORAGE.md section 13: a missing or corrupt file causes a visible
    // failure. A partial restore would leave an installation that looks healthy
    // and answers wrongly.
    let place = directory("missing");
    let data = place.join("data");
    let into = place.join("snapshot");
    let fresh = place.join("restored");
    {
        let store = store(&data);
        store.commit([1; 16], [1; 16], rows(80, 0)).unwrap();
    }
    snapshot::take(&data, &into, BASE_TIME).unwrap();

    // One file goes missing from the snapshot.
    let first = std::fs::read_dir(into.join("segments"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::remove_file(&first).unwrap();

    let report = snapshot::restore(&into, &fresh).unwrap();
    assert!(!report.is_complete(), "a missing file was skipped quietly");
    assert_eq!(report.segments_missing.len(), 1);
    // And nothing was published, so the destination is not a half-restored
    // installation somebody might start.
    assert!(
        !fresh.join("segments").is_dir()
            || std::fs::read_dir(fresh.join("segments")).unwrap().count() == 0,
        "a failed restore published anyway"
    );
}

#[test]
fn a_restore_never_silently_skips_a_damaged_file() {
    let place = directory("damaged");
    let data = place.join("data");
    let into = place.join("snapshot");
    let fresh = place.join("restored");
    {
        let store = store(&data);
        store.commit([1; 16], [1; 16], rows(80, 0)).unwrap();
    }
    snapshot::take(&data, &into, BASE_TIME).unwrap();

    let first = std::fs::read_dir(into.join("segments"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut bytes = std::fs::read(&first).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    std::fs::write(&first, &bytes).unwrap();

    let report = snapshot::restore(&into, &fresh).unwrap();
    assert!(!report.is_complete(), "a damaged file restored silently");
    assert_eq!(report.segments_damaged.len(), 1);
}

#[test]
fn a_restore_from_something_that_is_not_a_snapshot_says_so() {
    let place = directory("not-a-snapshot");
    let failure = snapshot::restore(&place.join("nothing"), &place.join("fresh")).unwrap_err();
    assert!(failure.to_string().contains("no TallyOwl snapshot"));
}

#[test]
fn a_rebuild_finds_every_segment_by_scanning() {
    // SEGMENT_FORMAT.md section 16 test 9, and STORAGE.md section 3.3: a repair
    // command rebuilds the segment catalog by scanning manifests. A segment is
    // self-describing, which is what makes it possible.
    let place = directory("rebuild");
    {
        let store = store(&place);
        store.commit([1; 16], [1; 16], rows(80, 0)).unwrap();
        store.commit([1; 16], [2; 16], rows(80, 200)).unwrap();
    }
    let segments_on_disk = std::fs::read_dir(place.join("segments")).unwrap().count();

    // The catalog is lost entirely.
    std::fs::remove_file(place.join("catalog/catalog.redb")).unwrap();

    let report = snapshot::rebuild(&place).unwrap();
    assert_eq!(report.segments_found, segments_on_disk);
    assert!(report.segments_unreadable.is_empty());

    let store = store(&place);
    assert_eq!(
        visible(&store).len(),
        160,
        "the rebuilt catalog does not name every row"
    );
}

#[test]
fn a_rebuild_says_exactly_what_it_did_not_restore() {
    // FAILURE_MODES.md section 7: a rebuild covers one of the twelve things the
    // catalog holds. An operator is told which nine it does not rather than
    // discovering them.
    let place = directory("rebuild-report");
    {
        let store = store(&place);
        store.commit([1; 16], [1; 16], rows(80, 0)).unwrap();
    }
    std::fs::remove_file(place.join("catalog/catalog.redb")).unwrap();

    let report = snapshot::rebuild(&place).unwrap();
    let text = report.to_text();
    assert!(text.contains("This did not restore:"));
    assert!(text.contains("receipts") || text.contains("Batch receipts"));
    assert!(text.contains("Sign-in sessions"));
    assert!(text.contains("Saved dashboards"));
    // The message is for a person, so it never says "tombstone" or "fencing".
    assert!(!text.contains("tombstone"));
    assert!(!text.contains("fencing"));

    // And the receipts really are gone, which is the accepted limit rather
    // than a defect.
    let store = store(&place);
    assert!(
        store.receipt([1; 16], [1; 16]).is_none(),
        "the test does not exercise what it claims"
    );
}

#[test]
fn a_rebuild_recovers_erasures_from_the_independently_durable_ledger() {
    // Procedure 5 step 2. A rebuild loses tombstones, and an erasure that a
    // rebuild can undo is not an erasure.
    let place = directory("rebuild-erasure");
    {
        let store = store(&place);
        store.commit([1; 16], [1; 16], rows(80, 0)).unwrap();
        store.erase(&erasure()).unwrap();
        assert_eq!(visible(&store).len(), 40);
    }
    std::fs::remove_file(place.join("catalog/catalog.redb")).unwrap();

    let report = snapshot::rebuild(&place).unwrap();
    assert_eq!(report.erasures_recovered, 1);

    let store = store(&place);
    let rows = visible(&store);
    assert_eq!(rows.len(), 40, "a rebuild brought erased rows back");
    assert!(rows
        .iter()
        .all(|row| row.properties["end_user"].0.to_display() != "u-042"));
}

#[test]
fn a_rebuild_names_a_file_it_could_not_read() {
    let place = directory("rebuild-damaged");
    {
        let store = store(&place);
        store.commit([1; 16], [1; 16], rows(80, 0)).unwrap();
    }
    let first = std::fs::read_dir(place.join("segments"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(&first, b"this is not a segment any more").unwrap();
    std::fs::remove_file(place.join("catalog/catalog.redb")).unwrap();

    let report = snapshot::rebuild(&place).unwrap();
    assert_eq!(report.segments_unreadable.len(), 1);
    assert!(report.to_text().contains("could not be read"));
}
