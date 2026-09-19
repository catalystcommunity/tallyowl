//! Segment encryption, per D61.
//!
//! This closes two of the required tests the design listed as blocked:
//!
//! | Required test | Where |
//! | --- | --- |
//! | SEGMENT_FORMAT.md section 16 test 8: a read of an encrypted segment after key destruction | `a_read_after_key_destruction_recovers_nothing` |
//! | The Phase 3 exit criterion "a read of cold data after key destruction fails and cannot recover the value" | the same test |
//!
//! The property that matters most is the one about what stays readable: an
//! encrypted segment keeps its prologue, header, and footer in the clear, so a
//! query still prunes by project, kind, and time without a key, and the index
//! region does **not** stay in the clear, because it holds fingerprints of the
//! identifiers erasure exists to make unreadable.

use std::path::PathBuf;

use tallyowl_store::catalog::Catalog;
use tallyowl_store::keys::RootKey;
use tallyowl_store::row::{EventRow, PropertyValue};
use tallyowl_store::segment::{open, open_with_key, SegmentWriter};

const WORKSPACE: [u8; 16] = [8; 16];
const PROJECT: [u8; 16] = [9; 16];
const BASE_TIME: i64 = 1_785_628_800_000;

fn directory(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("encryption-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn rows(count: usize) -> Vec<EventRow> {
    (0..count)
        .map(|index| {
            let n = index as u16;
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
            row.session_id = Some(format!("s-{}", index % 10));
            row.with_property(
                "end_user",
                PropertyValue::Text(format!("u-{:04}", index % 50)),
                "client",
            )
        })
        .collect()
}

#[test]
fn an_encrypted_segment_round_trips_every_row() {
    let catalog = Catalog::open(directory("round-trip")).unwrap();
    let root = RootKey::generate().unwrap();
    let cipher = catalog.current_key(PROJECT, &root).unwrap();

    let original = rows(500);
    let mut writer = SegmentWriter::new([5; 16], WORKSPACE, PROJECT);
    writer.cipher = Some(cipher.clone());
    let segment = writer.write(&original).unwrap();
    assert!(segment.is_encrypted());

    let reopened = open_with_key(segment.bytes.clone(), true, cipher).unwrap();
    let back = reopened.rows(true).unwrap();
    assert_eq!(back.len(), original.len());
    for (a, b) in original.iter().zip(&back) {
        assert_eq!(a.event_id, b.event_id);
        assert_eq!(a.properties, b.properties);
    }
}

#[test]
fn an_encrypted_segment_still_prunes_without_a_key() {
    // SEGMENT_FORMAT.md section 11: an encrypted segment keeps its header
    // readable, so a reader can still prune by time, project, and kind without
    // a key. That is what lets a query skip a segment it does not need before
    // it asks for any key at all.
    let catalog = Catalog::open(directory("prune")).unwrap();
    let root = RootKey::generate().unwrap();
    let cipher = catalog.current_key(PROJECT, &root).unwrap();

    let mut writer = SegmentWriter::new([5; 16], WORKSPACE, PROJECT);
    writer.cipher = Some(cipher);
    let segment = writer.write(&rows(200)).unwrap();

    // No key at all.
    let without = open(segment.bytes.clone(), true).unwrap();
    assert_eq!(without.header.project_id, PROJECT);
    assert_eq!(without.header.workspace_id, WORKSPACE);
    assert_eq!(without.header.kinds, vec!["event".to_string()]);
    assert_eq!(without.header.row_count, 200);
    assert!(without.header.occurred_range.0 >= BASE_TIME);
    assert!(without.is_encrypted());

    use tallyowl_store::TimeBasis;
    assert!(without.overlaps(TimeBasis::OccurredAt, BASE_TIME, BASE_TIME + 1_000_000));
    assert!(!without.overlaps(TimeBasis::OccurredAt, 0, BASE_TIME - 1));

    // And the content address still verifies, so a backup can be checked
    // without holding the key.
    assert!(without.verify().is_ok());
}

#[test]
fn an_encrypted_segment_gives_up_no_row_without_a_key() {
    let catalog = Catalog::open(directory("no-key")).unwrap();
    let root = RootKey::generate().unwrap();
    let cipher = catalog.current_key(PROJECT, &root).unwrap();

    let mut writer = SegmentWriter::new([5; 16], WORKSPACE, PROJECT);
    writer.cipher = Some(cipher);
    let segment = writer.write(&rows(200)).unwrap();

    let without = open(segment.bytes.clone(), true).unwrap();
    assert!(without.rows(true).is_err(), "a row came back with no key");
    assert!(
        without.index("event_id", true).is_err(),
        "an index came back with no key"
    );
}

#[test]
fn the_index_region_is_not_readable_without_a_key() {
    // D61: the index holds fingerprints of end-user, session, request, and
    // trace identifiers, and D9 makes the end-user identifier an erasure key. A
    // readable fingerprint index in a bucket would let anyone with object-store
    // access enumerate and correlate exactly the values erasure exists to make
    // unreadable.
    let catalog = Catalog::open(directory("index")).unwrap();
    let root = RootKey::generate().unwrap();
    let cipher = catalog.current_key(PROJECT, &root).unwrap();

    let mut writer = SegmentWriter::new([5; 16], WORKSPACE, PROJECT);
    writer.cipher = Some(cipher.clone());
    let encrypted = writer.write(&rows(500)).unwrap();

    // The same rows without a key, so the two can be compared.
    let plain = SegmentWriter::new([5; 16], WORKSPACE, PROJECT)
        .write(&rows(500))
        .unwrap();

    // A session identifier appears in the clear in the unencrypted segment's
    // term dictionary, and must not in the encrypted one.
    let needle = b"s-7";
    assert!(
        plain.bytes.windows(3).any(|window| window == needle),
        "the test does not exercise what it claims: the value is not in the plain segment either"
    );
    assert!(
        !encrypted.bytes.windows(3).any(|window| window == needle),
        "an identifier appears in the clear in an encrypted segment"
    );

    // With the key, the index reads normally.
    let reopened = open_with_key(encrypted.bytes.clone(), true, cipher).unwrap();
    assert!(reopened.index("session_id", true).unwrap().is_some());
    assert!(reopened.may_hold("session_id", b"s-7", true));
}

#[test]
fn a_read_after_key_destruction_recovers_nothing() {
    // SEGMENT_FORMAT.md section 16 test 8, and the Phase 3 exit criterion. This
    // is the whole point of the cold tier's erasure story: destroying the key
    // erases the project and the bytes cannot be read back.
    let place = directory("destroyed");
    let catalog = Catalog::open(&place).unwrap();
    let root = RootKey::generate().unwrap();
    let cipher = catalog.current_key(PROJECT, &root).unwrap();

    let mut writer = SegmentWriter::new([5; 16], WORKSPACE, PROJECT);
    writer.cipher = Some(cipher.clone());
    let segment = writer.write(&rows(200)).unwrap();

    // Before: the rows read.
    assert_eq!(
        open_with_key(segment.bytes.clone(), true, cipher)
            .unwrap()
            .rows(true)
            .unwrap()
            .len(),
        200
    );

    let destroyed = catalog
        .destroy_project_keys(PROJECT, BASE_TIME, "The customer closed their account.")
        .unwrap();
    assert_eq!(destroyed, 1);

    // After: the key is gone, and nothing recovers it.
    assert!(catalog
        .key_at_generation(PROJECT, &root, None)
        .unwrap()
        .is_none());
    assert!(catalog.key_generations(PROJECT).unwrap().is_empty());

    // The bytes are still on disk and still say what they are. They give up no
    // value.
    let without = open(segment.bytes.clone(), true).unwrap();
    assert_eq!(without.header.row_count, 200);
    assert!(without.rows(true).is_err());

    // A fresh key for the same project does not open the old segment either.
    let replacement = catalog.current_key(PROJECT, &root).unwrap();
    assert!(open_with_key(segment.bytes, true, replacement)
        .unwrap()
        .rows(true)
        .is_err());
}

#[test]
fn a_destroyed_key_stays_destroyed_across_a_reopen() {
    let place = directory("destroyed-reopen");
    let root = RootKey::generate().unwrap();
    {
        let catalog = Catalog::open(&place).unwrap();
        catalog.current_key(PROJECT, &root).unwrap();
        catalog
            .destroy_project_keys(PROJECT, BASE_TIME, "asked")
            .unwrap();
    }
    let catalog = Catalog::open(&place).unwrap();
    assert!(catalog.key_generations(PROJECT).unwrap().is_empty());
    // And the destruction is in the ledger, which is durable independently of
    // the catalog, so a catalog restore cannot bring the key back.
    assert_eq!(catalog.erasure_ledger().unwrap().len(), 1);
}

#[test]
fn a_destruction_survives_a_lost_catalog() {
    // FAILURE_MODES.md section 9 rule 3: an erasure that a rebuild can undo is
    // not an erasure.
    let place = directory("destroyed-rebuild");
    let root = RootKey::generate().unwrap();
    {
        let catalog = Catalog::open(&place).unwrap();
        catalog.current_key(PROJECT, &root).unwrap();
        catalog
            .destroy_project_keys(PROJECT, BASE_TIME, "asked")
            .unwrap();
    }
    std::fs::remove_file(place.join("catalog.redb")).unwrap();

    let rebuilt = Catalog::open(&place).unwrap();
    assert!(
        rebuilt.key_generations(PROJECT).unwrap().is_empty(),
        "a lost catalog brought a destroyed key back"
    );
    assert_eq!(
        rebuilt.erasure_ledger().unwrap().len(),
        1,
        "the destruction is not in the independently durable ledger"
    );
}

#[test]
fn rotation_writes_a_new_generation_and_leaves_the_old_segment_readable() {
    // D61: rotation writes a new generation and leaves already-written objects
    // readable until retention expires them or compaction rewrites them.
    // Re-encrypting every cold object on the spot is the cost D28 refused.
    let catalog = Catalog::open(directory("rotate")).unwrap();
    let root = RootKey::generate().unwrap();

    let first = catalog.current_key(PROJECT, &root).unwrap();
    assert_eq!(first.generation, 1);
    let mut writer = SegmentWriter::new([5; 16], WORKSPACE, PROJECT);
    writer.cipher = Some(first.clone());
    let old_segment = writer.write(&rows(100)).unwrap();

    let second = catalog.rotate_key(PROJECT, &root).unwrap();
    assert_eq!(second.generation, 2);
    assert_eq!(catalog.current_key(PROJECT, &root).unwrap().generation, 2);

    // The old segment names the generation it used, so a reader knows which key
    // to ask for.
    assert_eq!(old_segment.key_reference().unwrap().generation, 1);
    let key = catalog
        .key_at_generation(PROJECT, &root, Some(1))
        .unwrap()
        .expect("the old generation is still there");
    assert_eq!(
        open_with_key(old_segment.bytes, true, key)
            .unwrap()
            .rows(true)
            .unwrap()
            .len(),
        100
    );
}

#[test]
fn destruction_takes_every_generation() {
    let catalog = Catalog::open(directory("rotate-destroy")).unwrap();
    let root = RootKey::generate().unwrap();
    catalog.current_key(PROJECT, &root).unwrap();
    catalog.rotate_key(PROJECT, &root).unwrap();
    catalog.rotate_key(PROJECT, &root).unwrap();
    assert_eq!(catalog.key_generations(PROJECT).unwrap().len(), 3);

    assert_eq!(
        catalog
            .destroy_project_keys(PROJECT, BASE_TIME, "asked")
            .unwrap(),
        3
    );
    assert!(catalog.key_generations(PROJECT).unwrap().is_empty());
}

#[test]
fn one_projects_key_never_opens_another_projects_segment() {
    // Tenant isolation holds at the bytes, not only at the query.
    let catalog = Catalog::open(directory("isolation")).unwrap();
    let root = RootKey::generate().unwrap();
    let ours = catalog.current_key(PROJECT, &root).unwrap();
    let theirs = catalog.current_key([1; 16], &root).unwrap();

    let mut writer = SegmentWriter::new([5; 16], WORKSPACE, PROJECT);
    writer.cipher = Some(ours);
    let segment = writer.write(&rows(50)).unwrap();

    assert!(open_with_key(segment.bytes, true, theirs)
        .unwrap()
        .rows(true)
        .is_err());
}

#[test]
fn the_wrong_installation_key_opens_nothing() {
    // A data directory restored without its root key holds unreadable
    // segments. That is the same property that makes erasure work, and
    // DEPLOYMENT.md states it as an operator requirement.
    let place = directory("wrong-root");
    let real = RootKey::generate().unwrap();
    let other = RootKey::generate().unwrap();
    let catalog = Catalog::open(&place).unwrap();
    catalog.current_key(PROJECT, &real).unwrap();

    assert!(catalog.key_at_generation(PROJECT, &other, None).is_err());
    assert!(catalog
        .key_at_generation(PROJECT, &real, None)
        .unwrap()
        .is_some());
}

#[test]
fn a_damaged_encrypted_page_reports_damage_rather_than_a_key_problem() {
    // The checksum is over what is on disk, so damage is caught before any
    // attempt to decrypt. An operator chasing a failing device must not be sent
    // to look at key management.
    let catalog = Catalog::open(directory("damaged")).unwrap();
    let root = RootKey::generate().unwrap();
    let cipher = catalog.current_key(PROJECT, &root).unwrap();

    let mut writer = SegmentWriter::new([5; 16], WORKSPACE, PROJECT);
    writer.cipher = Some(cipher.clone());
    let segment = writer.write(&rows(200)).unwrap();

    let first_page = segment.footer.row_groups[0]
        .pages
        .values()
        .flat_map(|refs| refs.iter())
        .map(|page| page.offset as usize)
        .min()
        .unwrap();
    let mut damaged = segment.bytes.clone();
    damaged[first_page + 30] ^= 0xff;

    let reopened = open_with_key(damaged, true, cipher).unwrap();
    let failure = reopened.rows(true).unwrap_err();
    assert!(
        failure.to_string().contains("did not read back"),
        "the failure blames the key rather than the device: {failure}"
    );
}

#[test]
fn encryption_costs_a_bounded_amount() {
    // A fixed overhead for each block, so the capacity envelope moves by a
    // stated amount rather than an unknown one.
    let catalog = Catalog::open(directory("cost")).unwrap();
    let root = RootKey::generate().unwrap();
    let cipher = catalog.current_key(PROJECT, &root).unwrap();

    let plain = SegmentWriter::new([5; 16], WORKSPACE, PROJECT)
        .write(&rows(2_000))
        .unwrap();
    let mut writer = SegmentWriter::new([5; 16], WORKSPACE, PROJECT);
    writer.cipher = Some(cipher);
    let encrypted = writer.write(&rows(2_000)).unwrap();

    let added = encrypted.bytes.len() as f64 / plain.bytes.len() as f64;
    assert!(
        added < 1.10,
        "encryption added {:.1} percent, which is more than a fixed per-block overhead explains",
        (added - 1.0) * 100.0
    );
}
