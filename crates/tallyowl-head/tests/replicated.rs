//! The head, unchanged, over a replicated store.
//!
//! Phase 7's last exit criterion is that "the reference application runs
//! unchanged against the replicated installation and produces identical ledger
//! results". The thing that makes that possible is one seam: the head talks to
//! [`tallyowl_store::Store`], and a replicated tablet is one. If it were not,
//! every caller above it would need a second path.
//!
//! These tests put a real tablet group under the real ingest and query code and
//! assert that what comes out is what the local store produces. **Nothing about
//! the head changes between the two**, which is what the criterion is really
//! about: the same request, the same receipt, the same answer.
//!
//! What this covers, stated exactly: one node, one tablet, one voter, and a
//! real consensus group with a durable log. Three voters over sockets are
//! covered in `tallyowl-cluster`'s own tests. Running the whole reference
//! application against three heads needs three data directories and a
//! Corndogs, and `docs/ALPHA_REPORT.md` says plainly that it was not run.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tallyowl_cluster::groups::{GroupKey, GroupRegistry};
use tallyowl_cluster::raft::machine::TabletMachine;
use tallyowl_cluster::replicated::{policy_is_legal, ReplicatedStore};
use tallyowl_cluster::topology::{Member, ReceiptPolicy as TabletPolicy};
use tallyowl_collector_api::types::{
    Batch, CommitBatchRequest, Envelope, EventPayload, ReceiptPolicy, TelemetryItem, TelemetryKind,
};
use tallyowl_head::ingest::Ingest;
use tallyowl_obs::metrics::Registry;
use tallyowl_store::{SegmentedStore, Store};
use tallyowl_wire::collector_items_bridge as items;

const PROJECT: [u8; 16] = [9; 16];
const WORKSPACE: [u8; 16] = [8; 16];
const SOURCE: [u8; 16] = [7; 16];
const BASE_TIME: i64 = 1_785_628_800_000;

fn directory(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("head-replicated")
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

/// A head whose store is a real tablet group.
struct ReplicatedHead {
    ingest: Ingest,
    store: Arc<dyn Store>,
    local: Arc<dyn Store>,
    registry: Arc<GroupRegistry>,
}

fn replicated_head(name: &str) -> ReplicatedHead {
    let place = directory(name);
    let local: Arc<dyn Store> =
        Arc::new(SegmentedStore::open(place.join("data")).expect("the store opens"));
    let registry = GroupRegistry::new("solo", "127.0.0.1:0", Some(place.join("consensus")))
        .expect("a registry");
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
        .await_leader(&group, Duration::from_secs(20))
        .expect("one voter elects itself");

    let store: Arc<dyn Store> = Arc::new(ReplicatedStore::new(
        Arc::clone(&registry),
        "t0",
        Arc::clone(&local),
        TabletPolicy::LocalOne,
        "home",
    ));

    let metrics = Registry::new();
    Ingest::declare_metrics(&metrics);
    ReplicatedHead {
        ingest: Ingest {
            golden_signal_bucket_ms: 60_000,
            store: Arc::clone(&store),
            metrics,
            receipt_policy: ReceiptPolicy::LocalOne,
            open_traces: None,
            policy: None,
        },
        store,
        local,
        registry,
    }
}

#[test]
fn a_commit_through_a_tablet_group_gives_the_receipt_the_local_store_gives() {
    // The exit criterion, at the smallest size that still runs real consensus.
    // The head is not changed and does not know: the request is the same, the
    // receipt is the same, and the rows are where a query will find them.
    let head = replicated_head("commit");
    let receipt = head
        .ingest
        .commit(commit_request(
            1,
            vec![
                item(1, "checkout-started", BASE_TIME),
                item(2, "checkout-finished", BASE_TIME + 10),
            ],
        ))
        .expect("the tablet commits");

    assert_eq!(receipt.accepted, 2);
    assert_eq!(receipt.satisfied_policy, ReceiptPolicy::LocalOne);
    assert_eq!(receipt.deduplicated, Some(false));
    // The watermark counts commits rather than rows, exactly as it does
    // without replication. One batch is one commit.
    assert_eq!(receipt.commit_watermark, 1);
    // The rows are in the replica's own store, which is where a read finds
    // them. Segment construction proceeds from the committed log.
    assert_eq!(head.local.row_count(), 2);
    head.registry.shutdown();
}

#[test]
fn a_repeated_batch_through_a_tablet_group_is_still_one_logical_commit() {
    // The promise Phase 1 made and every phase since has kept. Consensus must
    // not break it: an entry that is committed twice must still be one commit.
    let head = replicated_head("dedup");
    let first = head
        .ingest
        .commit(commit_request(2, vec![item(3, "signed-up", BASE_TIME)]))
        .expect("the first commit");
    let again = head
        .ingest
        .commit(commit_request(2, vec![item(3, "signed-up", BASE_TIME)]))
        .expect("the retry is accepted");

    assert_eq!(again.deduplicated, Some(true));
    assert_eq!(again.commit_watermark, first.commit_watermark);
    assert_eq!(head.local.row_count(), 1, "the row was not written twice");
    head.registry.shutdown();
}

#[test]
fn a_scan_through_the_replicated_store_reads_what_the_commit_wrote() {
    let head = replicated_head("scan");
    head.ingest
        .commit(commit_request(
            3,
            vec![
                item(4, "checkout-started", BASE_TIME),
                item(5, "checkout-started", BASE_TIME + 1_000),
                item(6, "checkout-finished", BASE_TIME + 2_000),
            ],
        ))
        .expect("the commit");

    let scanned = head
        .store
        .scan(
            PROJECT,
            BASE_TIME - 1,
            BASE_TIME + 10_000,
            tallyowl_store::TimeBasis::OccurredAt,
        )
        .expect("the scan");
    assert_eq!(scanned.rows.len(), 3);
    assert!(!scanned.incomplete);
    head.registry.shutdown();
}

#[test]
fn an_exact_lookup_stays_exact_through_the_replicated_store() {
    // `AGENTS.md`: a high-cardinality value is never dropped, coalesced, or
    // rejected. Consensus is in front of the write path and must not change
    // what a lookup finds.
    let head = replicated_head("lookup");
    head.ingest
        .commit(commit_request(
            4,
            vec![item(7, "checkout-started", BASE_TIME)],
        ))
        .expect("the commit");

    let found = head
        .store
        .lookup_event([7u8; 16])
        .expect("the lookup")
        .expect("the row is there");
    assert_eq!(found.name, "checkout-started");
    head.registry.shutdown();
}

#[test]
fn a_replicated_store_reports_the_policy_a_replica_set_can_actually_satisfy() {
    // A receipt that named the configured policy rather than the satisfied one
    // would be worthless during a degradation, which is exactly when somebody
    // reads it.
    let one = vec![Member::voter("solo", "127.0.0.1:1")];
    assert!(policy_is_legal(TabletPolicy::LocalOne, &one).is_ok());

    let three = vec![
        Member::voter("a", "127.0.0.1:1"),
        Member::voter("b", "127.0.0.1:2"),
        Member::voter("c", "127.0.0.1:3"),
    ];
    let failure = policy_is_legal(TabletPolicy::LocalOne, &three)
        .expect_err("three voters cannot acknowledge before they commit");
    assert!(failure.message.contains("local-quorum"));
    assert!(policy_is_legal(TabletPolicy::LocalQuorum, &three).is_ok());

    let failure = policy_is_legal(TabletPolicy::RemoteOne, &three)
        .expect_err("every replica is in one region");
    assert!(failure.message.contains("outside the write region"));
}
