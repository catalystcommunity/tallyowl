//! `TallyOwlCluster` over a real socket, the way an operator reaches it.
//!
//! The rules these exercise all live in the state machine, and the tests beside
//! this one prove them there. What is proved here is the other half: that the
//! operation decodes, reaches the rule, and comes back as the typed answer the
//! contract declares. A rule that is right and unreachable is not a rule.

use std::sync::{Arc, Mutex};

use tallyowl_cluster_api::codec::*;
use tallyowl_cluster_api::types::{
    AssignProjectRequest, ClearDegradedRequest, DescribeTopologyRequest, RouteRequest,
    SetReceiptPolicyRequest, SplitTabletRequest, UnsafeRecoverRequest,
};
use tallyowl_rpc::{Client, Dispatcher, Server};

use crate::clusterservice::ClusterService;
use crate::controller::Controller;
use crate::groups::GroupRegistry;
use crate::plane::ControlPlane;
use crate::topology::{ControllerCommand, Member, ReceiptPolicy, Topology, VIRTUAL_SHARDS};

const MAX_FRAME: usize = 4 * 1024 * 1024;
const SERVICE: &str = "TallyOwlCluster";

/// One control plane on a socket, with a cell and one tablet already placed.
///
/// It has no controller quorum, which is the home shape: `ControlPlane` applies
/// a change to its local topology through the same state machine a quorum runs,
/// so the rules under test are the replicated ones either way.
struct Operator {
    _server: Server,
    client: Client,
    plane: Arc<ControlPlane>,
    /// The tablet's store, when this operator has one. `start` gives an empty
    /// one that nothing serves, so a test that does not move rows ignores it.
    store: Arc<dyn tallyowl_store::Store>,
    /// Where this operator keeps its snapshots.
    snapshots: std::path::PathBuf,
}

/// A control plane on a socket, with a real store behind its tablet.
///
/// Every operator here has a store, because `AGENTS.md` says not to mock the
/// storage interface and a real one costs a directory. The operations that only
/// reach the topology never touch it.
fn start() -> Operator {
    start_at("operator", None)
}

/// One with its snapshots in a named place, so two operators can share them:
/// one takes a snapshot and another restores it, which is the shape of a
/// permanent quorum loss.
fn start_holding_data(name: &str) -> Operator {
    start_at(name, None)
}

fn start_holding_data_at(name: &str, snapshots: std::path::PathBuf) -> Operator {
    start_at(name, Some(snapshots))
}

fn a_place(name: &str) -> std::path::PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("target"));
    let place = base
        .join("cluster-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&place);
    place
}

fn start_at(name: &str, snapshots: Option<std::path::PathBuf>) -> Operator {
    let place = a_place(name);
    let snapshots = snapshots.unwrap_or_else(|| place.join("snapshots"));
    let store =
        Arc::new(tallyowl_store::SegmentedStore::open(place.join("data")).expect("a store opens"));

    let topology = Arc::new(Mutex::new(Topology::new()));
    {
        let mut held = topology.lock().unwrap();
        for (index, node) in ["storage-a", "storage-b", "storage-c"].iter().enumerate() {
            held.apply(&ControllerCommand::RegisterNode {
                node: node.to_string(),
                address: format!("127.0.0.1:52{index:02}"),
                region: "west".into(),
                domain: format!("rack-{index}"),
            })
            .unwrap();
        }
        held.apply(&ControllerCommand::RegisterCell {
            cell: "west-1".into(),
            region: "west".into(),
            controllers: vec![Member::voter("storage-a", "127.0.0.1:5200").in_region("west")],
        })
        .unwrap();
        held.apply(&ControllerCommand::CreateTablet {
            tablet: "t1".into(),
            cell: "west-1".into(),
            region: "west".into(),
            shard_start: 0,
            shard_end: VIRTUAL_SHARDS,
            members: vec![
                Member::voter("storage-a", "127.0.0.1:5200")
                    .in_region("west")
                    .in_domain("rack-0"),
                Member::voter("storage-b", "127.0.0.1:5201")
                    .in_region("west")
                    .in_domain("rack-1"),
                Member::voter("storage-c", "127.0.0.1:5202")
                    .in_region("west")
                    .in_domain("rack-2"),
            ],
            receipt_policy: ReceiptPolicy::LocalQuorum,
        })
        .unwrap();
    }

    let registry = GroupRegistry::new("operator", "127.0.0.1:0", None).expect("a registry");
    let plane = Arc::new(ControlPlane::new(
        registry,
        Arc::clone(&topology),
        Controller::new("west-1", "west"),
    ));
    let segments: Arc<dyn crate::transfer::TabletSegments> = Arc::new(
        crate::transfer::StoreSegments::new("t1", Arc::clone(&store)),
    );
    let server = tallyowl_rpc::serve(
        "127.0.0.1:0",
        Arc::new(
            ClusterService::new(Arc::clone(&plane))
                .restoring_into("t1", Arc::clone(&segments))
                .keeping_snapshots_in(snapshots.clone()),
        ) as Arc<dyn Dispatcher>,
        MAX_FRAME,
    )
    .expect("it listens");
    let client = Client::new(server.local_address().to_string(), MAX_FRAME);
    Operator {
        _server: server,
        client,
        plane,
        store: store as Arc<dyn tallyowl_store::Store>,
        snapshots,
    }
}

fn a_row(n: u8) -> tallyowl_store::row::EventRow {
    let mut id = [0u8; 16];
    id[0] = n;
    let mut row = tallyowl_store::row::EventRow::new(id, "event", "checkout", 1_000 + n as i64);
    row.project_id = [7u8; 16];
    row
}

fn batch_ids(n: u8) -> ([u8; 16], [u8; 16]) {
    let mut source = [0u8; 16];
    source[0] = 1;
    let mut batch = [0u8; 16];
    batch[0] = n;
    (source, batch)
}

fn call(operator: &Operator, op: &str, payload: Vec<u8>) -> tallyowl_rpc::Response {
    operator
        .client
        .call(SERVICE, op, payload)
        .expect("the call reaches the service")
}

fn is_error(response: &tallyowl_rpc::Response) -> Option<String> {
    if response.variant.as_deref() != Some(tallyowl_rpc::SERVICE_ERROR_VARIANT) {
        return None;
    }
    decode_service_error(&response.payload)
        .ok()
        .map(|e| e.message)
}

#[test]
fn describe_topology_reports_the_cell_the_tablet_and_the_nodes() {
    let operator = start();
    let response = call(
        &operator,
        "describe-topology",
        encode_describe_topology_request(&DescribeTopologyRequest { cell: None }),
    );
    let topology = decode_topology_response(&response.payload).expect("a topology");
    assert_eq!(topology.cells.len(), 1);
    assert_eq!(topology.cells[0].cell, "west-1");
    assert_eq!(topology.tablets.len(), 1);
    assert_eq!(topology.tablets[0].tablet, "t1");
    assert_eq!(topology.tablets[0].members.len(), 3);
    assert_eq!(topology.tablets[0].receipt_policy, "local-quorum");
    assert_eq!(topology.nodes.len(), 3);
    // No controller quorum is running, and the answer says so rather than
    // implying one.
    assert!(!topology.cells[0].quorum_available);
}

#[test]
fn route_says_which_tablet_a_write_belongs_to_and_at_which_generation() {
    let operator = start();
    let response = call(
        &operator,
        "route",
        encode_route_request(&RouteRequest {
            project_id: vec![7u8; 16],
            affinity_key: b"trace-1".to_vec(),
        }),
    );
    let route = decode_route_response(&response.payload).expect("a route");
    assert_eq!(route.tablet, "t1");
    assert_eq!(route.cell, "west-1");
    assert!(route.shard < VIRTUAL_SHARDS);
    assert_eq!(route.receipt_policy, "local-quorum");
    assert_eq!(route.generation, operator.plane.topology().generation);
}

#[test]
fn setting_local_one_on_a_multi_voter_tablet_comes_back_as_a_typed_refusal() {
    // The rule is in the state machine. This proves an operator who asks for it
    // gets the reason rather than a transport failure or a silent change.
    let operator = start();
    let response = call(
        &operator,
        "set-receipt-policy",
        encode_set_receipt_policy_request(&SetReceiptPolicyRequest {
            tablet: "t1".into(),
            policy: "local-one".into(),
        }),
    );
    let message = is_error(&response).expect("a typed refusal");
    assert!(message.contains("local-quorum"), "{message}");
    assert_eq!(
        operator
            .plane
            .topology()
            .tablet("t1")
            .unwrap()
            .receipt_policy,
        ReceiptPolicy::LocalQuorum,
        "the policy did not move"
    );
}

#[test]
fn a_policy_that_is_not_a_policy_is_refused_and_the_message_lists_the_real_ones() {
    let operator = start();
    let response = call(
        &operator,
        "set-receipt-policy",
        encode_set_receipt_policy_request(&SetReceiptPolicyRequest {
            tablet: "t1".into(),
            policy: "whenever".into(),
        }),
    );
    let message = is_error(&response).expect("a typed refusal");
    assert!(
        message.contains("local-quorum") && message.contains("remote-one"),
        "{message}"
    );
}

#[test]
fn a_split_with_no_point_chooses_the_middle_and_both_children_own_shards() {
    let operator = start();
    let response = call(
        &operator,
        "split-tablet",
        encode_split_tablet_request(&SplitTabletRequest {
            tablet: "t1".into(),
            at_shard: None,
        }),
    );
    let ack = decode_cluster_ack(&response.payload).expect("an acknowledgement");
    assert!(ack.accepted);

    let topology = operator.plane.topology();
    let left = topology.tablet("t1").expect("the left side");
    let right = topology.tablet("t1-b").expect("the right side");
    assert_eq!(left.shard_start, 0);
    assert_eq!(left.shard_end, right.shard_start);
    assert_eq!(right.shard_end, VIRTUAL_SHARDS);
    assert!(left.shard_end > left.shard_start);
    assert!(right.shard_end > right.shard_start);
}

#[test]
fn unsafe_recovery_over_the_wire_needs_the_tablet_named_twice() {
    let operator = start();
    let response = call(
        &operator,
        "unsafe-recover",
        encode_unsafe_recover_request(&UnsafeRecoverRequest {
            tablet: "t1".into(),
            confirm_tablet: "t2".into(),
            survivor: "storage-a".into(),
            reason: "two hosts were destroyed".into(),
        }),
    );
    let message = is_error(&response).expect("a typed refusal");
    assert!(
        message.contains("can lose an acknowledged write"),
        "{message}"
    );
    assert!(operator
        .plane
        .topology()
        .tablet("t1")
        .unwrap()
        .degraded
        .is_none());
}

#[test]
fn unsafe_recovery_over_the_wire_leaves_one_voter_an_audit_record_and_a_degraded_mark() {
    let operator = start();
    let response = call(
        &operator,
        "unsafe-recover",
        encode_unsafe_recover_request(&UnsafeRecoverRequest {
            tablet: "t1".into(),
            confirm_tablet: "t1".into(),
            survivor: "storage-a".into(),
            reason: "the b and c hosts were destroyed with their disks".into(),
        }),
    );
    let outcome = decode_unsafe_recover_response(&response.payload).expect("an outcome");
    assert_eq!(outcome.tablet, "t1");
    assert!(!outcome.audit_id.is_empty());

    let topology = operator.plane.topology();
    let tablet = topology.tablet("t1").expect("the tablet");
    assert_eq!(tablet.voter_count(), 1);
    let mark = tablet.degraded.as_ref().expect("the range is marked");
    assert_eq!(mark.audit_id, outcome.audit_id);
    assert!(topology
        .audit()
        .iter()
        .any(|a| a.audit_id == outcome.audit_id));

    // And clearing it records who accepted the loss.
    let cleared = call(
        &operator,
        "clear-degraded",
        encode_clear_degraded_request(&ClearDegradedRequest {
            tablet: "t1".into(),
            accepted_by: "tod".into(),
            reason: "reviewed".into(),
        }),
    );
    assert!(
        decode_cluster_ack(&cleared.payload)
            .expect("an acknowledgement")
            .accepted
    );
    let topology = operator.plane.topology();
    assert!(topology.tablet("t1").unwrap().degraded.is_none());
    assert!(topology
        .audit()
        .iter()
        .any(|a| a.detail.contains("tod accepted the loss")));
}

#[test]
fn a_snapshot_holds_the_rows_and_a_restore_puts_them_back() {
    // L090. Restore is the **default** answer to a permanent quorum loss, which
    // makes a false acknowledgement here worse than anywhere else in the
    // system. This is the whole round trip: take a snapshot of a tablet that
    // holds rows, restore it onto a node that holds none, and find the rows.
    use tallyowl_cluster_api::types::{ClusterSnapshotRequest, RestoreRequest};

    let source = start_holding_data("restore-source");
    for n in 1..=4u8 {
        let (s, b) = batch_ids(n);
        source
            .store
            .commit(s, b, vec![a_row(n)])
            .expect("a write commits");
    }

    let response = call(
        &source,
        "snapshot-cluster",
        encode_cluster_snapshot_request(&ClusterSnapshotRequest {
            tablet: Some("t1".into()),
        }),
    );
    assert!(is_error(&response).is_none(), "{:?}", is_error(&response));
    let taken = decode_cluster_snapshot_response(&response.payload).expect("a snapshot");
    assert!(
        taken.total_bytes > 0,
        "a snapshot that holds no bytes holds no rows"
    );

    // A second node, holding nothing, restores from the same place.
    let target = start_holding_data_at("restore-target", source.snapshots.clone());
    assert_eq!(target.store.row_count(), 0);
    let response = call(
        &target,
        "restore",
        encode_restore_request(&RestoreRequest {
            snapshot_id: taken.snapshot_id.clone(),
            onto_tablet: Some("t1".into()),
        }),
    );
    assert!(is_error(&response).is_none(), "{:?}", is_error(&response));
    let ack = decode_cluster_ack(&response.payload).expect("an acknowledgement");
    assert!(ack.accepted);
    let said = ack.message.unwrap_or_default();
    assert!(
        said.contains("Everything written after the snapshot is not here"),
        "the acknowledgement names what a restore loses: {said}"
    );
    assert_eq!(
        target.store.row_count(),
        4,
        "the rows came back rather than only the marks"
    );
}

#[test]
fn a_restore_onto_a_node_that_holds_data_refuses_and_names_the_offline_path() {
    // Restoring on top of live data can lose an acknowledged write and there is
    // no way to take that back, so the online command refuses and points at
    // FAILURE_MODES.md section 11 procedure 3.
    use tallyowl_cluster_api::types::{ClusterSnapshotRequest, RestoreRequest};

    let operator = start_holding_data("restore-occupied");
    let (s, b) = batch_ids(1);
    operator
        .store
        .commit(s, b, vec![a_row(1)])
        .expect("a write commits");
    let response = call(
        &operator,
        "snapshot-cluster",
        encode_cluster_snapshot_request(&ClusterSnapshotRequest { tablet: None }),
    );
    let taken = decode_cluster_snapshot_response(&response.payload).expect("a snapshot");

    let response = call(
        &operator,
        "restore",
        encode_restore_request(&RestoreRequest {
            snapshot_id: taken.snapshot_id,
            onto_tablet: None,
        }),
    );
    let message = is_error(&response).expect("a typed refusal");
    assert!(
        message.contains("tallyowl-head restore"),
        "the refusal names the procedure that does work: {message}"
    );
    assert_eq!(operator.store.row_count(), 1, "nothing was mixed together");
}

#[test]
fn a_restore_of_a_damaged_snapshot_changes_nothing_and_names_the_file() {
    // STORAGE.md section 13: "restore never silently skips a file." A restore
    // that published anyway would leave an installation that looks healthy and
    // answers wrongly.
    use tallyowl_cluster_api::types::{ClusterSnapshotRequest, RestoreRequest};

    let source = start_holding_data("damaged-snapshot-source");
    let (s, b) = batch_ids(1);
    source
        .store
        .commit(s, b, vec![a_row(1), a_row(2)])
        .expect("a write commits");
    let response = call(
        &source,
        "snapshot-cluster",
        encode_cluster_snapshot_request(&ClusterSnapshotRequest { tablet: None }),
    );
    let taken = decode_cluster_snapshot_response(&response.payload).expect("a snapshot");

    // Damage one stored file inside the snapshot, the way a bad device does.
    let segments = source
        .snapshots
        .join(&taken.snapshot_id)
        .join("t1")
        .join("segments");
    let file = std::fs::read_dir(&segments)
        .expect("the snapshot holds segments")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("tos"))
        .expect("one segment file");
    let mut bytes = std::fs::read(&file).expect("it reads");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&file, &bytes).expect("it writes");

    let target = start_holding_data_at("damaged-snapshot-target", source.snapshots.clone());
    let response = call(
        &target,
        "restore",
        encode_restore_request(&RestoreRequest {
            snapshot_id: taken.snapshot_id,
            onto_tablet: Some("t1".into()),
        }),
    );
    let message = is_error(&response).expect("a typed refusal");
    assert!(
        message.contains("is damaged"),
        "the refusal names the damage: {message}"
    );
    assert_eq!(
        target.store.row_count(),
        0,
        "nothing was published from a snapshot that could not be proved"
    );
}

#[test]
fn a_project_can_be_placed_when_the_directory_is_in_process() {
    // CELLS.md section 3 puts the global directory role inside the head for the
    // home profile. A cache that started as unreachable refused every placement
    // in an installation whose directory is a field on the control plane, which
    // is what this asserts is not the case.
    let operator = start();
    let registered =
        operator
            .plane
            .apply_directory(crate::directory::DirectoryCommand::RegisterCell {
                cell: "west-1".into(),
                region: "west".into(),
            });
    assert!(registered.is_ok(), "{registered:?}");

    let response = call(
        &operator,
        "assign-project",
        encode_assign_project_request(&AssignProjectRequest {
            project_id: vec![1u8; 16],
            cells: vec!["west-1".into()],
        }),
    );
    assert!(
        decode_cluster_ack(&response.payload)
            .expect("an acknowledgement")
            .accepted
    );
    assert_eq!(
        operator
            .plane
            .directory()
            .write_cell(&[1u8; 16])
            .map(|c| c.as_str()),
        Some("west-1")
    );
}

#[test]
fn assigning_a_project_to_a_cell_that_does_not_exist_is_refused() {
    let operator = start();
    let response = call(
        &operator,
        "assign-project",
        encode_assign_project_request(&AssignProjectRequest {
            project_id: vec![1u8; 16],
            cells: vec!["nowhere".into()],
        }),
    );
    let message = is_error(&response).expect("a typed refusal");
    assert!(message.contains("Register the cell"), "{message}");
}

#[test]
fn an_operation_this_service_does_not_have_is_a_transport_failure_and_not_an_error() {
    // The rule `tallyowl-rpc` exists to hold: no handler ran, so there is no
    // typed reply, and a caller must be able to tell that from a rejection.
    let operator = start();
    let failure = operator
        .client
        .call(SERVICE, "invent", Vec::new())
        .expect_err("no handler ran");
    assert_eq!(failure.code, tallyowl_obs::error::ErrorCode::Unavailable);
}
