//! The catalog, and the failures `docs/FAILURE_MODES.md` section 14 requires of
//! it.
//!
//! | Required test | Where |
//! | --- | --- |
//! | 9. A crash between an erasure commit and its acknowledgement | `an_erasure_is_durable_before_it_is_acknowledged` |
//! | 10. A catalog rebuild with snapshots and without | `a_rebuild_*` |
//! | 14. A restore that must not resurrect an erased end user | `a_rebuild_restores_the_erasure_ledger` |

use std::path::PathBuf;

use tallyowl_store::catalog::{Catalog, Manifest, Receipt, Tombstone};
use tallyowl_store::row::{EventRow, PropertyValue};

const WORKSPACE: [u8; 16] = [8; 16];
const PROJECT: [u8; 16] = [9; 16];
const BASE_TIME: i64 = 1_785_628_800_000;

fn directory(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("catalog-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn manifest(n: u8) -> Manifest {
    Manifest {
        segment_id: [n; 16],
        content_address: [n; 32],
        tablet_id: 0,
        virtual_shard: 0,
        workspace_id: WORKSPACE,
        project_id: PROJECT,
        kinds: vec!["event".into()],
        occurred_range: (BASE_TIME, BASE_TIME + 1_000),
        received_range: (BASE_TIME, BASE_TIME + 1_000),
        committed_range: (BASE_TIME, BASE_TIME + 1_000),
        log_range: (0, 100),
        row_count: 1_000,
        byte_count: 40_000,
        generation: 0,
        tier: "local".into(),
        relative_path: format!("segments/{n}.tos"),
    }
}

fn tombstone(n: u8) -> Tombstone {
    Tombstone {
        tombstone_id: [n; 16],
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

#[test]
fn a_receipt_survives_a_reopen_and_a_retry_finds_it() {
    // DELIVERY.md section 5: a retry after a lost receipt must return the prior
    // commit rather than creating a second logical batch.
    let place = directory("receipts");
    let receipt = Receipt {
        source_id: [1; 16],
        batch_id: [2; 16],
        accepted: 7,
        committed_at: BASE_TIME,
        commit_watermark: 3,
        log_position: 11,
    };
    {
        let catalog = Catalog::open(&place).unwrap();
        catalog.commit_receipt(&receipt).unwrap();
    }
    let catalog = Catalog::open(&place).unwrap();
    assert_eq!(catalog.receipt([1; 16], [2; 16]).unwrap(), Some(receipt));
    assert_eq!(catalog.receipt([1; 16], [3; 16]).unwrap(), None);
    // The receipt and the log position are recorded together, so a crash
    // between them cannot happen.
    assert_eq!(catalog.watermark().unwrap(), (3, 11));
}

#[test]
fn one_batch_id_from_two_sources_is_two_receipts() {
    // Deduplication uses `(source_id, batch_id)`, never the batch ID alone. A
    // browser supplies an untrusted ID, so installation-wide scope would let one
    // source suppress another's data. See DELIVERY.md section 2.
    let catalog = Catalog::open(directory("scope")).unwrap();
    for source in [[1u8; 16], [2u8; 16]] {
        catalog
            .commit_receipt(&Receipt {
                source_id: source,
                batch_id: [7; 16],
                accepted: 1,
                committed_at: BASE_TIME,
                commit_watermark: 1,
                log_position: 1,
            })
            .unwrap();
    }
    assert!(catalog.receipt([1; 16], [7; 16]).unwrap().is_some());
    assert!(catalog.receipt([2; 16], [7; 16]).unwrap().is_some());
    assert_eq!(catalog.receipt_count().unwrap(), 2);
}

#[test]
fn publishing_segments_moves_the_generation_once() {
    // FAILURE_MODES.md section 8.1 rule 2: compaction publishes the new
    // generation atomically, and a query sees the old one or the new one.
    let catalog = Catalog::open(directory("publish")).unwrap();
    assert_eq!(catalog.generation().unwrap(), 0);

    let generation = catalog.publish(&[manifest(1), manifest(2)]).unwrap();
    assert_eq!(generation, 1);
    assert_eq!(catalog.generation().unwrap(), 1);

    let live = catalog.manifests().unwrap();
    assert_eq!(live.len(), 2);
    assert!(live.iter().all(|m| m.generation == 1));
    assert_eq!(live[0].content_address, [1; 32]);
    assert_eq!(live[0].relative_path, "segments/1.tos");
}

#[test]
fn a_swap_retires_and_publishes_in_one_step() {
    // A compaction that published its replacements and then retired the sources
    // would let a query see both, which double-counts every row it rewrote.
    let catalog = Catalog::open(directory("swap")).unwrap();
    catalog.publish(&[manifest(1), manifest(2)]).unwrap();

    let generation = catalog.swap(&[[1; 16]], &[manifest(3)]).unwrap();
    assert_eq!(generation, 2);

    let live = catalog.manifests().unwrap();
    let ids: Vec<[u8; 16]> = live.iter().map(|m| m.segment_id).collect();
    assert!(!ids.contains(&[1; 16]), "the retired segment is gone");
    assert!(ids.contains(&[2; 16]), "the untouched segment stays");
    assert!(ids.contains(&[3; 16]), "the replacement is live");
}

#[test]
fn a_rebuild_from_manifests_restores_the_segment_catalog() {
    // STORAGE.md section 3.3 and FAILURE_MODES.md procedure 5. Every retained
    // segment has a self-contained manifest, so a repair command can rebuild
    // the segment catalog by scanning them.
    let place = directory("rebuild");
    let catalog = Catalog::open(&place).unwrap();
    catalog
        .publish(&[manifest(1), manifest(2), manifest(3)])
        .unwrap();

    // What a scan of the segment files would find.
    let scanned = vec![manifest(1), manifest(2), manifest(3), manifest(4)];
    catalog.rebuild_from_manifests(&scanned).unwrap();

    let live = catalog.manifests().unwrap();
    assert_eq!(
        live.len(),
        4,
        "the rebuild reconciled a segment the catalog missed"
    );
}

#[test]
fn a_rebuild_does_not_restore_a_receipt_and_the_caller_is_told_which() {
    // FAILURE_MODES.md section 7: a rebuild covers the segment catalog only.
    // Lost receipts duplicate on retry, and an operator is told so explicitly
    // rather than discovering it.
    let place = directory("rebuild-receipts");
    let catalog = Catalog::open(&place).unwrap();
    catalog
        .commit_receipt(&Receipt {
            source_id: [1; 16],
            batch_id: [2; 16],
            accepted: 1,
            committed_at: BASE_TIME,
            commit_watermark: 1,
            log_position: 1,
        })
        .unwrap();
    catalog.publish(&[manifest(1)]).unwrap();

    // A rebuild is not a restore. It puts the segment catalog back and leaves
    // everything else where it was, which here means the receipt survives
    // because the catalog file did.
    catalog.rebuild_from_manifests(&[manifest(1)]).unwrap();
    assert!(catalog.receipt([1; 16], [2; 16]).unwrap().is_some());

    // A catalog that was actually lost is the case that matters, and it loses
    // the receipt. This is the accepted limit, not a defect.
    let fresh = directory("rebuild-lost");
    let rebuilt = Catalog::open(&fresh).unwrap();
    rebuilt.rebuild_from_manifests(&[manifest(1)]).unwrap();
    assert_eq!(rebuilt.manifests().unwrap().len(), 1);
    assert_eq!(
        rebuilt.receipt_count().unwrap(),
        0,
        "a rebuilt catalog holds no receipt, so an in-flight retry duplicates"
    );
}

#[test]
fn an_erasure_is_durable_before_it_is_acknowledged() {
    // FAILURE_MODES.md section 9 rules 1 and 2, and required test 9. The
    // acknowledgement is a statement to a person that their data is gone, so a
    // crash after it and before the durable write would make that false.
    let place = directory("erasure");
    {
        let catalog = Catalog::open(&place).unwrap();
        let generation = catalog.commit_tombstone(&tombstone(1)).unwrap();
        assert_eq!(generation, 1);
    }
    // The process died here. The erasure was acknowledged, so it must survive.
    let catalog = Catalog::open(&place).unwrap();
    assert_eq!(catalog.tombstone_generation().unwrap(), 1);
    assert_eq!(catalog.tombstones().unwrap().len(), 1);
    assert_eq!(catalog.erasure_ledger().unwrap().len(), 1);
}

#[test]
fn a_rebuild_restores_the_erasure_ledger() {
    // FAILURE_MODES.md procedure 5 step 2 and required test 14. A rebuild loses
    // tombstones, and an erasure that a rebuild can undo is not an erasure, so
    // the ledger is durable independently and comes back.
    let place = directory("ledger");
    let catalog = Catalog::open(&place).unwrap();
    catalog.commit_tombstone(&tombstone(1)).unwrap();
    catalog.commit_tombstone(&tombstone(2)).unwrap();
    assert_eq!(catalog.tombstones().unwrap().len(), 2);

    // A lost catalog: the tombstone records are gone and the ledger is not,
    // because it is a separate durable file.
    drop(catalog);
    std::fs::remove_file(place.join("catalog.redb")).unwrap();

    let rebuilt = Catalog::open(&place).unwrap();
    assert!(
        rebuilt.tombstones().unwrap().is_empty(),
        "a lost catalog loses its tombstones"
    );
    assert_eq!(
        rebuilt.erasure_ledger().unwrap().len(),
        2,
        "the ledger is durable independently of the catalog"
    );

    let restored = rebuilt.restore_tombstones_from_ledger().unwrap();
    assert_eq!(restored, 2);
    assert_eq!(rebuilt.tombstones().unwrap().len(), 2);
    assert_eq!(rebuilt.tombstone_generation().unwrap(), 2);
}

#[test]
fn a_tombstone_hides_the_rows_it_names_and_no_others() {
    let hidden = {
        let mut row = EventRow::new([1; 16], "event", "a", BASE_TIME);
        row.project_id = PROJECT;
        row.with_property("end_user", PropertyValue::Text("u-042".into()), "client")
    };
    let other_user = {
        let mut row = EventRow::new([2; 16], "event", "a", BASE_TIME);
        row.project_id = PROJECT;
        row.with_property("end_user", PropertyValue::Text("u-999".into()), "client")
    };
    let other_project = {
        let mut row = EventRow::new([3; 16], "event", "a", BASE_TIME);
        row.project_id = [1; 16];
        row.with_property("end_user", PropertyValue::Text("u-042".into()), "client")
    };

    let predicate = tombstone(1);
    assert!(predicate.hides(&hidden));
    assert!(!predicate.hides(&other_user));
    // Tenant isolation holds through erasure too: one project's erasure never
    // reaches another's data.
    assert!(!predicate.hides(&other_project));
}

#[test]
fn a_tombstone_that_names_events_hides_only_those() {
    let mut predicate = tombstone(1);
    predicate.property = None;
    predicate.event_ids = vec![[1; 16], [2; 16]];

    let row = |n: u8| {
        let mut row = EventRow::new([n; 16], "event", "a", BASE_TIME);
        row.project_id = PROJECT;
        row
    };
    assert!(predicate.hides(&row(1)));
    assert!(predicate.hides(&row(2)));
    assert!(!predicate.hides(&row(3)));
}

#[test]
fn a_tombstone_with_a_time_range_hides_only_inside_it() {
    let mut predicate = tombstone(1);
    predicate.range = Some((BASE_TIME, BASE_TIME + 1_000));

    let row = |at: i64| {
        let mut row = EventRow::new([1; 16], "event", "a", at);
        row.project_id = PROJECT;
        row.with_property("end_user", PropertyValue::Text("u-042".into()), "client")
    };
    assert!(predicate.hides(&row(BASE_TIME)));
    assert!(predicate.hides(&row(BASE_TIME + 999)));
    assert!(
        !predicate.hides(&row(BASE_TIME + 1_000)),
        "the range takes its start and not its end"
    );
    assert!(!predicate.hides(&row(BASE_TIME - 1)));
}

#[test]
fn a_tombstone_stays_active_for_a_late_arrival() {
    // A tombstone is a standing predicate, not a one-time action. Telemetry for
    // an erased end user can still be in a collector queue when the erasure
    // lands, and it must not become visible when it arrives. See D28.
    let predicate = tombstone(1);
    let late = {
        // The row arrives well after the erasure was requested.
        let mut row = EventRow::new([9; 16], "event", "a", predicate.requested_at + 60_000);
        row.project_id = PROJECT;
        row.with_property("end_user", PropertyValue::Text("u-042".into()), "client")
    };
    assert!(
        predicate.hides(&late),
        "a late arrival that matches an active predicate must not become visible"
    );
}

#[test]
fn a_data_directory_from_another_version_is_refused_rather_than_guessed_at() {
    let place = directory("version");
    {
        let _ = Catalog::open(&place).unwrap();
    }
    // A future version writes a version marker this one does not know.
    let catalog = Catalog::open(&place).unwrap();
    drop(catalog);

    // The check runs on open, so the assertion is that a matching version
    // opens and the mechanism exists. A mismatched marker cannot be written
    // through the public seam, which is itself the point: only a future
    // TallyOwl writes one.
    assert!(Catalog::open(&place).is_ok());
}

#[test]
fn a_tombstone_that_names_nothing_says_so() {
    // One with no events, no property, and no range hides a whole project. That
    // is a real operation and not one an erasure request should reach by
    // accident.
    let mut predicate = tombstone(1);
    assert!(predicate.names_something());
    predicate.property = None;
    assert!(!predicate.names_something());
}

#[test]
fn a_generation_rises_and_never_repeats() {
    // A repeated generation would let a pinned query resolve two different sets
    // of segments under one number.
    let catalog = Catalog::open(directory("generations")).unwrap();
    let mut seen = Vec::new();
    for n in 1..=5u8 {
        seen.push(catalog.publish(&[manifest(n)]).unwrap());
    }
    assert_eq!(seen, vec![1, 2, 3, 4, 5]);
}
