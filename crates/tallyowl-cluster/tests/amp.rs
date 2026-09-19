//! Where the consensus log's bytes actually go.
//!
//! The question this answers is "why does a replicated tablet need so much more
//! disk than an unreplicated one", and the answer is three multipliers rather
//! than one. It prints the decomposition, and it asserts the one that was a
//! defect: **a replicated command must not be materially larger than the batch
//! it carries.** serde encodes `Vec<u8>` as a sequence, so a plain derive wrote
//! one CBOR integer for each byte and doubled every batch, twice. See L096.

use std::path::PathBuf;
use std::sync::Arc;

use tallyowl_cluster::groups::{GroupKey, GroupRegistry};
use tallyowl_cluster::raft::machine::TabletMachine;
use tallyowl_cluster::replicated::ReplicatedStore;
use tallyowl_cluster::topology::{Member, ReceiptPolicy};
use tallyowl_store::row::{EventRow, PropertyValue};
use tallyowl_store::{SegmentedStore, Store};

fn place(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("amp")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn directory_bytes(path: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let meta = entry.metadata().expect("metadata");
            total += if meta.is_dir() {
                directory_bytes(&entry.path())
            } else {
                meta.len()
            };
        }
    }
    total
}

/// A row the load harness would produce: an ID, a name, three times, a session,
/// a request ID, a trace, and a handful of properties.
fn row(n: u64) -> EventRow {
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&n.to_be_bytes());
    let mut row = EventRow::new(
        id,
        "event",
        "checkout-started",
        1_785_628_800_000 + n as i64,
    );
    row.project_id = [9u8; 16];
    row.workspace_id = [8u8; 16];
    row.source_id = [7u8; 16];
    row.received_at = row.occurred_at + 5;
    row.session_id = Some(format!("session-{:016x}", n / 7));
    row.request_id = Some(format!("request-{n:016x}"));
    row.trace_id = Some(id);
    row.service_name = Some("seedstore-web".into());
    row.release = Some("2026.8.4".into());
    for (key, value) in [
        ("route", PropertyValue::Text("/checkout".into())),
        ("plan", PropertyValue::Text("professional".into())),
        ("region", PropertyValue::Text("home".into())),
        ("items", PropertyValue::Integer(3)),
    ] {
        row.properties
            .insert(key.into(), (value, "client".to_string()));
    }
    row
}

const EVENTS_EACH_BATCH: usize = 173;
const BATCHES: usize = 200;

#[test]
fn a_replicated_command_is_not_materially_larger_than_the_batch_it_carries() {
    let events = EVENTS_EACH_BATCH * BATCHES;
    let root = place("amp");
    let data = root.join("data");
    let consensus = root.join("consensus");

    let local: Arc<dyn Store> = Arc::new(SegmentedStore::open(&data).expect("a store"));
    let registry =
        GroupRegistry::new("solo", "127.0.0.1:0", Some(consensus.clone())).expect("a registry");
    let group = GroupKey::Tablet("t0".into());
    let members = vec![Member::voter("solo", "127.0.0.1:1")];
    registry
        .start(
            group.clone(),
            Arc::new(TabletMachine::new(Arc::clone(&local))),
            members.clone(),
            0,
        )
        .expect("the group starts");
    registry.bootstrap(&group, &members).expect("one voter");
    registry
        .await_leader(&group, std::time::Duration::from_secs(20))
        .expect("it elects itself");

    let store = ReplicatedStore::new(
        Arc::clone(&registry),
        "t0",
        Arc::clone(&local),
        ReceiptPolicy::LocalOne,
        "home",
    );

    // What the caller handed over, and what the replicated command carried.
    let mut offered = 0u64;
    let mut command_bytes = 0u64;
    for batch in 0..BATCHES {
        let rows: Vec<EventRow> = (0..EVENTS_EACH_BATCH)
            .map(|n| row((batch * EVENTS_EACH_BATCH + n) as u64))
            .collect();
        let encoded = tallyowl_store::row_codec::encode_rows(&rows);
        offered += encoded.len() as u64;
        let command = tallyowl_cluster::raft::machine::TabletCommand::Commit {
            source_id: [7u8; 16],
            batch_id: {
                let mut id = [0u8; 16];
                id[..8].copy_from_slice(&(batch as u64).to_be_bytes());
                id
            },
            rows: encoded,
        };
        command_bytes += tallyowl_cluster::raft::encode(&command).unwrap().len() as u64;

        let mut batch_id = [0u8; 16];
        batch_id[..8].copy_from_slice(&(batch as u64).to_be_bytes());
        store
            .commit([7u8; 16], batch_id, rows)
            .expect("the tablet commits");
    }
    registry.shutdown();

    let log_file = directory_bytes(&consensus);
    let store_bytes = directory_bytes(&data);

    println!("\n=== where the bytes go, {events} events in {BATCHES} batches ===");
    println!(
        "encoded batch (what the store's own append log carries): {:>12} bytes, {:>8.1} each event",
        offered,
        offered as f64 / events as f64
    );
    println!(
        "replicated command (that, plus its envelope):             {:>12} bytes, {:>8.1} each event",
        command_bytes,
        command_bytes as f64 / events as f64
    );
    println!(
        "consensus directory on disk:                              {:>12} bytes, {:>8.1} each event",
        log_file,
        log_file as f64 / events as f64
    );
    println!(
        "store directory on disk:                                  {:>12} bytes, {:>8.1} each event",
        store_bytes,
        store_bytes as f64 / events as f64
    );
    println!(
        "\nredb amplification over the commands it was given: {:.1}x",
        log_file as f64 / command_bytes as f64
    );
    println!(
        "the command against what the store keeps:          {:.1}x",
        command_bytes as f64 / store_bytes as f64
    );
    println!(
        "the whole consensus directory against the store:   {:.1}x",
        log_file as f64 / store_bytes as f64
    );

    // The regression guard. Before L096 this ratio was 1.74, because every byte
    // of the batch became its own CBOR integer. The envelope itself is a group
    // reference and a variant tag, so a few percent is all it should ever be.
    let envelope = command_bytes as f64 / offered as f64;
    assert!(
        envelope < 1.05,
        "a replicated command is {envelope:.2} times the batch it carries. \
         A `Vec<u8>` encoded as a sequence rather than a byte string does this; \
         see the `byte_string` module and L096."
    );

    // What compressing an entry would give. A batch is CBOR full of repeated
    // keys and repeated dimension values, which is why the delivery queue
    // already compresses one; the same reasoning applies to a log entry that
    // has to be written, fsynced, and read back. This measures it rather than
    // assuming it. See L095 option 2.
    let rows: Vec<EventRow> = (0..EVENTS_EACH_BATCH).map(|n| row(n as u64)).collect();
    let one = tallyowl_store::row_codec::encode_rows(&rows);
    for level in [1, 3, 9] {
        let squeezed = zstd::encode_all(&one[..], level).expect("zstd");
        println!(
            "one batch at zstd level {level}: {} bytes from {}, {:.1}x smaller",
            squeezed.len(),
            one.len(),
            one.len() as f64 / squeezed.len() as f64
        );
    }
    println!();
}
