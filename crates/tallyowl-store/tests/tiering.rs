//! Cold tiering, and the rule that keeps a tier move from losing data.
//!
//! The Phase 3 exit criterion is the whole of this file:
//!
//! > an interrupted cold-tier upload never evicts the only valid segment copy.
//!
//! Every test here is a way for an upload to go wrong. In each one the local
//! file has to still be there afterwards.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tallyowl_store::catalog::Manifest;
use tallyowl_store::row::EventRow;
use tallyowl_store::segment::SegmentWriter;
use tallyowl_store::store::StoreError;
use tallyowl_store::tier::{
    evict_local, object_key, upload, ColdStore, FilesystemColdStore, Pressure, Tier, TierPolicy,
};

const WORKSPACE: [u8; 16] = [8; 16];
const PROJECT: [u8; 16] = [9; 16];
const BASE_TIME: i64 = 1_785_628_800_000;

fn directory(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("tiering-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a place to work");
    path
}

/// A real segment, so the content address is a real one.
fn segment() -> (Manifest, Vec<u8>) {
    let rows: Vec<EventRow> = (0..500u16)
        .map(|n| {
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
            row
        })
        .collect();
    let written = SegmentWriter::new([5; 16], WORKSPACE, PROJECT)
        .write(&rows)
        .expect("the segment writes");

    let manifest = Manifest {
        segment_id: [5; 16],
        content_address: written.content_address,
        tablet_id: 0,
        virtual_shard: 0,
        workspace_id: WORKSPACE,
        project_id: PROJECT,
        kinds: written.header.kinds.clone(),
        occurred_range: written.header.occurred_range,
        received_range: written.header.received_range,
        committed_range: written.header.committed_range,
        log_range: (0, 10),
        row_count: written.header.row_count,
        byte_count: written.bytes.len() as u64,
        generation: 1,
        tier: "local".into(),
        relative_path: "segments/5.tos".into(),
    };
    (manifest, written.bytes)
}

fn write_local(place: &Path, bytes: &[u8]) -> PathBuf {
    let path = place.join("segment.tos");
    std::fs::write(&path, bytes).expect("the local copy");
    path
}

/// A cold store that fails in a chosen way.
///
/// This is not a mock of TallyOwl's own storage interface, which `AGENTS.md`
/// forbids. It is a bucket that misbehaves, which is the thing under test: a
/// real bucket that truncates, acknowledges early, or loses an object is
/// exactly the failure this rule exists for.
struct AwkwardColdStore {
    inner: FilesystemColdStore,
    behaviour: Mutex<Behaviour>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Behaviour {
    Honest,
    /// The upload stops part way through and the bucket keeps what arrived.
    Truncate,
    /// The put returns and nothing lands, which is a bucket acknowledging early.
    SwallowSilently,
    /// The object is there and holds something else.
    Corrupt,
    /// The object was there and is not any more.
    LoseAfterUpload,
}

impl AwkwardColdStore {
    fn new(root: &Path, behaviour: Behaviour) -> AwkwardColdStore {
        AwkwardColdStore {
            inner: FilesystemColdStore::new(root).expect("the cold store opens"),
            behaviour: Mutex::new(behaviour),
        }
    }

    fn set(&self, behaviour: Behaviour) {
        *self.behaviour.lock().unwrap() = behaviour;
    }

    fn behaviour(&self) -> Behaviour {
        *self.behaviour.lock().unwrap()
    }
}

impl ColdStore for AwkwardColdStore {
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), StoreError> {
        match self.behaviour() {
            Behaviour::Honest | Behaviour::LoseAfterUpload => self.inner.put(key, bytes),
            Behaviour::Truncate => self.inner.put(key, &bytes[..bytes.len() / 2]),
            Behaviour::SwallowSilently => Ok(()),
            Behaviour::Corrupt => {
                let mut damaged = bytes.to_vec();
                let middle = damaged.len() / 2;
                damaged[middle] ^= 0xff;
                self.inner.put(key, &damaged)
            }
        }
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        self.inner.get(key)
    }

    fn exists(&self, key: &str) -> Result<bool, StoreError> {
        self.inner.exists(key)
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.inner.delete(key)
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }
}

#[test]
fn an_upload_that_arrives_whole_verifies_and_the_local_copy_can_go() {
    let place = directory("honest");
    let (manifest, bytes) = segment();
    let local = write_local(&place, &bytes);
    let cold = FilesystemColdStore::new(place.join("cold")).unwrap();

    assert!(
        upload(&cold, &manifest, &bytes).unwrap(),
        "a whole upload did not verify"
    );
    assert!(
        local.is_file(),
        "the upload removed the local copy on its own"
    );

    assert!(evict_local(&cold, &manifest, &local).unwrap());
    assert!(
        !local.is_file(),
        "the local copy stayed after a verified eviction"
    );

    // And the cold copy is the segment, byte for byte.
    assert_eq!(cold.get(&object_key(&manifest)).unwrap(), bytes);
}

#[test]
fn an_interrupted_upload_never_evicts_the_only_valid_copy() {
    // The Phase 3 exit criterion. An upload that stopped part way through must
    // not be mistaken for a copy.
    let place = directory("truncated");
    let (manifest, bytes) = segment();
    let local = write_local(&place, &bytes);
    let cold = AwkwardColdStore::new(&place.join("cold"), Behaviour::Truncate);

    assert!(
        !upload(&cold, &manifest, &bytes).unwrap(),
        "a truncated upload verified"
    );
    assert!(!evict_local(&cold, &manifest, &local).unwrap());
    assert!(
        local.is_file(),
        "the only valid copy was removed after a truncated upload"
    );
}

#[test]
fn a_bucket_that_acknowledges_early_never_evicts_the_only_valid_copy() {
    // A put that returns is not proof. This is the failure mode a
    // verification-free implementation would never notice.
    let place = directory("swallowed");
    let (manifest, bytes) = segment();
    let local = write_local(&place, &bytes);
    let cold = AwkwardColdStore::new(&place.join("cold"), Behaviour::SwallowSilently);

    // The upload reports failure rather than success, because the read back
    // finds nothing.
    assert!(
        upload(&cold, &manifest, &bytes).is_err() || !upload(&cold, &manifest, &bytes).unwrap()
    );
    assert!(!evict_local(&cold, &manifest, &local).unwrap());
    assert!(local.is_file(), "the only valid copy was removed");
}

#[test]
fn a_corrupted_upload_never_evicts_the_only_valid_copy() {
    let place = directory("corrupt");
    let (manifest, bytes) = segment();
    let local = write_local(&place, &bytes);
    let cold = AwkwardColdStore::new(&place.join("cold"), Behaviour::Corrupt);

    assert!(
        !upload(&cold, &manifest, &bytes).unwrap(),
        "a corrupted upload verified"
    );
    assert!(!evict_local(&cold, &manifest, &local).unwrap());
    assert!(local.is_file(), "the only valid copy was removed");
}

#[test]
fn an_object_lost_between_the_upload_and_the_eviction_keeps_the_local_copy() {
    // The interval between an upload and an eviction is exactly where a bucket
    // can lose an object, which is why the verification runs again at eviction
    // rather than trusting a flag from earlier.
    let place = directory("lost");
    let (manifest, bytes) = segment();
    let local = write_local(&place, &bytes);
    let cold = AwkwardColdStore::new(&place.join("cold"), Behaviour::LoseAfterUpload);

    assert!(upload(&cold, &manifest, &bytes).unwrap());
    // The bucket loses it.
    cold.delete(&object_key(&manifest)).unwrap();

    assert!(!evict_local(&cold, &manifest, &local).unwrap());
    assert!(local.is_file(), "the only valid copy was removed");
}

#[test]
fn an_object_that_changed_after_the_upload_keeps_the_local_copy() {
    let place = directory("changed");
    let (manifest, bytes) = segment();
    let local = write_local(&place, &bytes);
    let cold = AwkwardColdStore::new(&place.join("cold"), Behaviour::Honest);

    assert!(upload(&cold, &manifest, &bytes).unwrap());

    // Something else writes over the object.
    cold.set(Behaviour::Corrupt);
    cold.put(&object_key(&manifest), &bytes).unwrap();

    assert!(!evict_local(&cold, &manifest, &local).unwrap());
    assert!(local.is_file(), "the only valid copy was removed");
}

#[test]
fn a_retried_upload_after_an_interruption_succeeds() {
    // A failed upload has to be recoverable rather than terminal, or a bucket
    // hiccup would strand a segment locally forever.
    let place = directory("retry");
    let (manifest, bytes) = segment();
    let local = write_local(&place, &bytes);
    let cold = AwkwardColdStore::new(&place.join("cold"), Behaviour::Truncate);

    assert!(!upload(&cold, &manifest, &bytes).unwrap());
    assert!(local.is_file());

    cold.set(Behaviour::Honest);
    assert!(
        upload(&cold, &manifest, &bytes).unwrap(),
        "the retry did not verify"
    );
    assert!(evict_local(&cold, &manifest, &local).unwrap());
    assert!(!local.is_file());
}

#[test]
fn a_cold_segment_reads_back_and_answers_a_query() {
    // Cold data stays part of the logical database. It does not become an
    // export, and a query over it returns rows.
    let place = directory("readable");
    let (manifest, bytes) = segment();
    let local = write_local(&place, &bytes);
    let cold = FilesystemColdStore::new(place.join("cold")).unwrap();

    upload(&cold, &manifest, &bytes).unwrap();
    evict_local(&cold, &manifest, &local).unwrap();
    assert!(!local.is_file());

    let fetched = cold.get(&object_key(&manifest)).unwrap();
    let segment = tallyowl_store::segment::open(fetched, true).unwrap();
    assert!(segment.verify().is_ok());
    assert_eq!(segment.rows(true).unwrap().len(), 500);
}

#[test]
fn an_obsolete_upload_cannot_overwrite_a_live_object() {
    // FAILURE_MODES.md section 8.3 rule 1: a segment is identified by its
    // content address, so a compaction that obsoleted a segment cannot have its
    // in-flight upload land on the replacement's key.
    let place = directory("obsolete");
    let (live, live_bytes) = segment();
    let mut obsolete = live.clone();
    obsolete.content_address = [0xab; 32];

    let cold = FilesystemColdStore::new(place.join("cold")).unwrap();
    upload(&cold, &live, &live_bytes).unwrap();

    // The obsolete segment's key is a different key, so its upload cannot
    // reach the live object at all.
    assert_ne!(object_key(&live), object_key(&obsolete));
    cold.put(&object_key(&obsolete), b"whatever the old one held")
        .unwrap();
    assert_eq!(cold.get(&object_key(&live)).unwrap(), live_bytes);
}

#[test]
fn the_tier_policy_reports_what_the_disk_is_asking_for() {
    let policy = TierPolicy::default();
    let day = 24 * 3_600_000;
    assert_eq!(policy.tier_for(BASE_TIME, BASE_TIME), Tier::Hot);
    assert_eq!(policy.tier_for(BASE_TIME - 3 * day, BASE_TIME), Tier::Warm);
    assert_eq!(policy.tier_for(BASE_TIME - 30 * day, BASE_TIME), Tier::Cold);
    assert_eq!(policy.pressure(0.5), Pressure::Rest);
    assert_eq!(policy.pressure(0.96), Pressure::StopIngest);
}
