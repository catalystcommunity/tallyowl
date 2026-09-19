//! Real consensus, over real sockets, against real durable storage.
//!
//! D27 selected openraft and measured it, and named what the measurement left
//! open:
//!
//! > - behaviour against durable storage, where every append pays an fsync;
//! > - behaviour over a real network with loss, reordering, and delay;
//! > - a partition that splits a group other than by isolating one node;
//! > - recovery from a corrupt or truncated log.
//!
//! These tests close the first of those and part of the second. Every node here
//! runs its own [`crate::groups::GroupRegistry`], its own durable consensus log
//! on a real directory, its own [`tallyowl_store::SegmentedStore`], and its own
//! CSIL listener on a real loopback socket. Nothing is in process and nothing
//! is a mock; `AGENTS.md` says not to mock the storage interface and the same
//! reasoning applies to the transport.
//!
//! **What is still not covered here, stated plainly.** Loss, reordering, and
//! delay are what loopback does not do, so the second item is only partly
//! closed. The third and fourth are not covered at all. `docs/ALPHA_REPORT.md`
//! says so rather than letting a passing suite imply more than it proves.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tallyowl_cluster_api::types::ReplicaStatus;
use tallyowl_obs::error::TallyOwlError;
use tallyowl_rpc::{Dispatcher, Server};
use tallyowl_store::row::EventRow;
use tallyowl_store::{SegmentedStore, Store};

use crate::groups::{GroupKey, GroupRegistry};
use crate::query::Partial;
use crate::raft::machine::TabletMachine;
use crate::replicated::ReplicatedStore;
use crate::service::{PartialSource, ReplicationService};
use crate::topology::{Member, ReceiptPolicy, Topology};

/// How long a test waits for an election. D27 measured 985 ms to replace an
/// isolated leader, so this is a generous multiple rather than a guess.
const ELECTION_PATIENCE: Duration = Duration::from_secs(20);

/// One node: its store, its registry, and its listener.
struct Node {
    name: String,
    registry: Arc<GroupRegistry>,
    store: Arc<dyn Store>,
    /// The same store, as the type the segment transfer needs. A copy reads and
    /// publishes sealed segments, which is below the `Store` contract.
    segments: Arc<SegmentedStore>,
    topology: Arc<Mutex<Topology>>,
    server: Option<Server>,
    address: String,
}

/// A fresh directory on real storage, the way the store's own tests make one.
///
/// Section 9 of the implementation prompt forbids benchmarking storage on
/// tmpfs, and a correctness test over durable consensus uses the same path
/// shape so the two stay honest: an fsync here is a real fsync.
fn directory(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("cluster-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// A `PartialSource` that answers from the local store. The head owns the real
/// one; this is enough to serve the two operations these tests exercise.
struct LocalSource {
    node: String,
    store: Arc<dyn Store>,
}

impl PartialSource for LocalSource {
    fn partial(
        &self,
        tablet: &str,
        request: &tallyowl_cluster_api::types::PartialQueryRequest,
    ) -> Result<Partial, TallyOwlError> {
        use tallyowl_cluster_api::types::PartialKind as Wire;

        let mut project = [0u8; 16];
        let taking = request.project_id.len().min(16);
        project[..taking].copy_from_slice(&request.project_id[..taking]);

        // An exact lookup goes to the locator; everything else is a scan. The
        // head owns the real one, and this answers the same four forms so a
        // fan-out test exercises the fan-out rather than a stub.
        if let (Some(column), Some(value)) = (request.column.as_deref(), request.value.as_deref()) {
            let found = self
                .store
                .lookup_correlated(column, value)
                .map_err(|e| TallyOwlError::internal(e.to_string()))?;
            return Ok(Partial {
                count: found.rows.len() as u64,
                complete: !found.incomplete,
                rows: found.rows,
                commit_watermark: self.store.commit_watermark(),
                ..Partial::for_tablet(tablet)
            });
        }

        let scanned = self
            .store
            .scan(
                project,
                request.range_start,
                request.range_end,
                tallyowl_store::TimeBasis::OccurredAt,
            )
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        let rows = if request.kind == Wire::Rows {
            scanned.rows.clone()
        } else {
            Vec::new()
        };
        Ok(Partial {
            count: scanned.rows.len() as u64,
            complete: !scanned.incomplete,
            buckets: if request.kind == Wire::Trend {
                [(request.range_start, scanned.rows.len() as u64)]
                    .into_iter()
                    .collect()
            } else {
                Default::default()
            },
            rows,
            commit_watermark: self.store.commit_watermark(),
            ..Partial::for_tablet(tablet)
        })
    }

    fn status(&self) -> ReplicaStatus {
        ReplicaStatus {
            node: self.node.clone(),
            tablets: Vec::new(),
            applied_watermark: self.store.commit_watermark(),
            lag_ms: None,
            writable: self.store.is_writable(),
        }
    }
}

impl Node {
    fn start(name: &str) -> Node {
        Node::start_holding(name, "t1")
    }

    fn start_holding(name: &str, tablet: &str) -> Node {
        let place = directory(name);
        let segments = Arc::new(SegmentedStore::open(place.join("data")).expect("a store opens"));
        let store: Arc<dyn Store> = Arc::clone(&segments) as Arc<dyn Store>;
        let registry = GroupRegistry::new(name, "127.0.0.1:0", Some(place.join("consensus")))
            .expect("a registry");
        let topology = Arc::new(Mutex::new(Topology::new()));
        let held = GroupKey::Tablet(tablet.to_string());
        let reporting = Arc::clone(&registry);
        let service = ReplicationService::new(
            Arc::clone(&registry),
            Arc::clone(&topology),
            Arc::new(LocalSource {
                node: name.to_string(),
                store: Arc::clone(&store),
            }),
        )
        .serving_segments(Arc::new(
            crate::transfer::StoreSegments::new(tablet, Arc::clone(&segments))
                .reporting_applied(Arc::new(move || reporting.applied_index(&held))),
        ));
        let server = tallyowl_rpc::serve(
            "127.0.0.1:0",
            Arc::new(service) as Arc<dyn Dispatcher>,
            crate::raft::network::MAX_FRAME_BYTES,
        )
        .expect("a listener");
        let address = server.local_address().to_string();
        Node {
            name: name.to_string(),
            registry,
            store,
            segments,
            topology,
            server: Some(server),
            address,
        }
    }

    fn member(&self) -> Member {
        Member::voter(self.name.clone(), self.address.clone())
            .in_region("west")
            .in_domain(format!("rack-{}", self.name))
    }

    /// Take this node away, the way losing a host does: it stops holding the
    /// group and it stops answering.
    fn lose(&mut self, group: &GroupKey) {
        self.registry.stop(group);
        self.server = None;
    }
}

fn start_group(nodes: &[Node], group: &GroupKey, members: &[Member]) {
    for node in nodes {
        node.registry
            .start(
                group.clone(),
                Arc::new(TabletMachine::new(Arc::clone(&node.store))),
                members.to_vec(),
                0,
            )
            .expect("the group starts on this node");
    }
    nodes[0]
        .registry
        .bootstrap(group, members)
        .expect("the voter set is created once");
}

fn leader_of<'a>(nodes: &'a [Node], group: &GroupKey) -> &'a Node {
    let name = nodes[0]
        .registry
        .await_leader(group, ELECTION_PATIENCE)
        .expect("the group elects a leader");
    nodes
        .iter()
        .find(|n| n.name == name)
        .expect("the leader is one of these nodes")
}

fn a_row(n: u8) -> EventRow {
    let mut id = [0u8; 16];
    id[0] = n;
    let mut row = EventRow::new(id, "event", "checkout", 1_000 + n as i64);
    row.project_id = [7u8; 16];
    row
}

fn ids(n: u8) -> ([u8; 16], [u8; 16]) {
    let mut source = [0u8; 16];
    source[0] = 1;
    let mut batch = [0u8; 16];
    batch[0] = n;
    (source, batch)
}

// ---------------------------------------------------------------------------

#[test]
fn three_voters_commit_a_write_and_it_survives_losing_one_of_them() {
    // Phase 7 exit criterion: "acknowledged batches survive the agreed number
    // of storage-node losses." Three voters survive one.
    let mut nodes = vec![Node::start("n1"), Node::start("n2"), Node::start("n3")];
    let group = GroupKey::Tablet("t1".into());
    let members: Vec<Member> = nodes.iter().map(|n| n.member()).collect();
    start_group(&nodes, &group, &members);

    let leader = leader_of(&nodes, &group);
    let store = ReplicatedStore::new(
        Arc::clone(&leader.registry),
        "t1",
        Arc::clone(&leader.store),
        ReceiptPolicy::LocalQuorum,
        "west",
    );
    let (source, batch) = ids(1);
    let outcome = store
        .commit(source, batch, vec![a_row(1), a_row(2)])
        .expect("a quorum of three commits");
    assert_eq!(outcome.accepted, 2);
    assert!(!outcome.deduplicated);

    // Every voter applied it, because a quorum commit is followed by the rest.
    // Wait rather than assume: the two followers apply just after the leader
    // returns.
    let applied = leader.registry.applied_index(&group);
    let mut caught_up = 0;
    for _ in 0..500 {
        caught_up = nodes
            .iter()
            .filter(|n| n.registry.applied_index(&group) >= applied)
            .count();
        if caught_up == 3 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(caught_up, 3, "every voter applied the committed entry");

    // Lose one. The remaining two are still a quorum of three.
    let leader_name = leader.name.clone();
    let doomed = nodes
        .iter()
        .position(|n| n.name != leader_name)
        .expect("a follower");
    nodes[doomed].lose(&group);

    let leader = nodes
        .iter()
        .find(|n| n.name == leader_name)
        .expect("the leader is still here");
    let store = ReplicatedStore::new(
        Arc::clone(&leader.registry),
        "t1",
        Arc::clone(&leader.store),
        ReceiptPolicy::LocalQuorum,
        "west",
    );
    let (source, batch) = ids(2);
    store
        .commit(source, batch, vec![a_row(3)])
        .expect("two of three still commit");

    // And the first write is still there, on a node that never left.
    assert!(leader.store.receipt(source, ids(1).1).is_some() || leader.store.row_count() >= 3);
}

#[test]
fn a_write_through_a_follower_commits_by_forwarding_to_the_leader() {
    // The Phase 11 soak found this in its first hour: which voter leads is an
    // election's choice, and the ingest head was a follower, so every batch
    // parked for ever and nothing was ever committed. A voter that is not the
    // leader hands the proposal to whoever is, one hop, and the caller cannot
    // tell the difference from writing to the leader itself.
    let nodes = vec![Node::start("n1"), Node::start("n2"), Node::start("n3")];
    let group = GroupKey::Tablet("t1".into());
    let members: Vec<Member> = nodes.iter().map(|n| n.member()).collect();
    start_group(&nodes, &group, &members);

    let leader_name = leader_of(&nodes, &group).name.clone();
    let follower = nodes
        .iter()
        .find(|n| n.name != leader_name)
        .expect("two of three are followers");
    // Until the first append arrives, a fresh follower does not know who
    // leads, and a proposal then is a retryable refusal rather than a forward.
    // The forwarder's retry covers that second in production; the test waits
    // for it, because the property under test is the forward.
    follower
        .registry
        .await_leader(&group, ELECTION_PATIENCE)
        .expect("the follower learns the leader");

    let store = ReplicatedStore::new(
        Arc::clone(&follower.registry),
        "t1",
        Arc::clone(&follower.store),
        ReceiptPolicy::LocalQuorum,
        "west",
    );
    let (source, batch) = ids(9);
    let outcome = store
        .commit(source, batch, vec![a_row(9)])
        .expect("a follower's proposal reaches the leader and commits");
    assert_eq!(outcome.accepted, 1);

    // The same batch through the third node is still one logical commit, so
    // the deduplication rule holds whichever voter a retry lands on.
    let other = nodes
        .iter()
        .find(|n| n.name != leader_name && n.name != follower.name)
        .expect("the third node");
    let store = ReplicatedStore::new(
        Arc::clone(&other.registry),
        "t1",
        Arc::clone(&other.store),
        ReceiptPolicy::LocalQuorum,
        "west",
    );
    let outcome = store
        .commit(source, batch, vec![a_row(9)])
        .expect("the duplicate commits as a replay");
    assert!(outcome.deduplicated, "one batch ID is one logical commit");
}

#[test]
fn a_retry_after_a_lost_acknowledgement_gives_one_logical_commit() {
    // Phase 7 exit criterion: "leader loss during write returns either a prior
    // receipt or one logical retry." TallyOwl never claims exactly-once
    // transport; what it claims is that a repeated batch ID is one commit, and
    // that has to keep holding through consensus.
    let nodes = vec![Node::start("n1"), Node::start("n2"), Node::start("n3")];
    let group = GroupKey::Tablet("t1".into());
    let members: Vec<Member> = nodes.iter().map(|n| n.member()).collect();
    start_group(&nodes, &group, &members);

    let leader = leader_of(&nodes, &group);
    let store = ReplicatedStore::new(
        Arc::clone(&leader.registry),
        "t1",
        Arc::clone(&leader.store),
        ReceiptPolicy::LocalQuorum,
        "west",
    );
    let (source, batch) = ids(9);
    let first = store
        .commit(source, batch, vec![a_row(1), a_row(2)])
        .expect("the first attempt commits");
    assert!(!first.deduplicated);

    let again = store
        .commit(source, batch, vec![a_row(1), a_row(2)])
        .expect("the retry is accepted");
    assert!(
        again.deduplicated,
        "a repeated batch ID is one logical commit"
    );
    assert_eq!(again.commit_watermark, first.commit_watermark);
    assert_eq!(
        leader.store.row_count(),
        2,
        "the rows were not written twice"
    );
}

#[test]
fn a_single_node_that_lost_its_quorum_commits_nothing() {
    // The minority never commits, and it never acknowledges. This is the rule
    // `AGENTS.md` states with no exception, seen from the losing side.
    let mut nodes = vec![Node::start("n1"), Node::start("n2"), Node::start("n3")];
    let group = GroupKey::Tablet("t1".into());
    let members: Vec<Member> = nodes.iter().map(|n| n.member()).collect();
    start_group(&nodes, &group, &members);

    let leader_name = leader_of(&nodes, &group).name.clone();
    // Take away everything except the leader. One of three is a minority.
    let doomed: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.name != leader_name)
        .map(|(index, _)| index)
        .collect();
    for index in doomed {
        nodes[index].lose(&group);
    }

    let leader = nodes
        .iter()
        .find(|n| n.name == leader_name)
        .expect("the leader");
    let store = ReplicatedStore::new(
        Arc::clone(&leader.registry),
        "t1",
        Arc::clone(&leader.store),
        ReceiptPolicy::LocalQuorum,
        "west",
    )
    .with_remote_copy_timeout(Duration::from_secs(1));

    let (source, batch) = ids(3);
    let before = leader.store.row_count();
    let failure = store
        .commit(source, batch, vec![a_row(4)])
        .expect_err("one of three commits nothing");
    // It is retryable: the group can come back, and the batch ID makes the
    // retry one logical commit.
    assert!(
        failure.to_string().contains("tablet `t1`"),
        "the refusal names the tablet: {failure}"
    );
    assert_eq!(
        leader.store.row_count(),
        before,
        "nothing was written by a write that was not acknowledged"
    );
}

#[test]
fn a_learner_follows_the_log_without_enlarging_the_write_quorum() {
    // Read and export replicas are non-voting learners, and STORAGE.md section
    // 8 says they "do not slow the write quorum". A learner that counted
    // towards the quorum would make every read replica a write dependency.
    let nodes = vec![Node::start("n1"), Node::start("n2"), Node::start("n3")];
    let group = GroupKey::Tablet("t1".into());
    let voters: Vec<Member> = nodes[..2].iter().map(|n| n.member()).collect();

    for node in &nodes {
        node.registry
            .start(
                group.clone(),
                Arc::new(TabletMachine::new(Arc::clone(&node.store))),
                voters.clone(),
                0,
            )
            .expect("the group starts");
    }
    nodes[0]
        .registry
        .bootstrap(&group, &voters)
        .expect("two voters");
    let leader = leader_of(&nodes, &group);

    let learner = Member::learner(nodes[2].name.clone(), nodes[2].address.clone());
    leader
        .registry
        .add_member(&group, &learner)
        .expect("a learner joins online");

    let store = ReplicatedStore::new(
        Arc::clone(&leader.registry),
        "t1",
        Arc::clone(&leader.store),
        ReceiptPolicy::LocalQuorum,
        "west",
    );
    let (source, batch) = ids(4);
    store
        .commit(source, batch, vec![a_row(5)])
        .expect("the write commits on the voters");

    // The learner catches up, and it did not have to for the write to commit.
    let target = leader.registry.applied_index(&group);
    let mut reached = false;
    for _ in 0..500 {
        if nodes[2].registry.applied_index(&group) >= target {
            reached = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(reached, "the learner follows the log");
    assert_eq!(nodes[2].store.row_count(), 1);
}

#[test]
fn a_node_that_does_not_hold_a_tablet_refuses_its_messages_by_name() {
    // The multi-node topology property: "only controllers and each tablet's
    // replica set participate in their respective consensus groups." A node
    // that answered for a group it was never placed on would be the
    // cluster-wide group `AGENTS.md` forbids, arrived at by accident.
    use tallyowl_cluster_api::codec::{decode_consensus_reply, encode_consensus_message};
    use tallyowl_cluster_api::types::{ConsensusKind, ConsensusMessage, GroupKind, GroupRef};

    let nodes = [Node::start("n1"), Node::start("n2")];
    let group = GroupKey::Tablet("t1".into());
    let members = vec![nodes[0].member()];
    nodes[0]
        .registry
        .start(
            group.clone(),
            Arc::new(TabletMachine::new(Arc::clone(&nodes[0].store))),
            members.clone(),
            0,
        )
        .expect("the group starts on the node that holds it");
    nodes[0].registry.bootstrap(&group, &members).unwrap();

    // n2 was never placed on `t1`.
    assert!(!nodes[1].registry.holds(&group));

    let client = tallyowl_rpc::Client::new(
        nodes[1].address.clone(),
        crate::raft::network::MAX_FRAME_BYTES,
    );
    let message = ConsensusMessage {
        group: GroupRef {
            kind: GroupKind::Tablet,
            name: Some("t1".into()),
        },
        kind: ConsensusKind::Vote,
        sender: "n1".into(),
        generation: 0,
        payload: vec![0xf6],
    };
    let response = client
        .call(
            crate::raft::network::REPLICATION_SERVICE,
            crate::raft::network::DELIVER_CONSENSUS,
            encode_consensus_message(&message),
        )
        .expect("the call reaches the service");
    let reply = decode_consensus_reply(&response.payload).expect("a reply");
    assert!(!reply.accepted);
    let refusal = reply.refusal.expect("a reason");
    assert!(
        refusal.contains("tablet `t1`") && refusal.contains("does not hold"),
        "{refusal}"
    );
}

#[test]
fn a_message_from_an_older_placement_generation_is_refused_with_the_current_one() {
    // A gateway or a peer that missed a membership change is redirected rather
    // than left to retry blind. `routing::Fence` is the same idea on the write
    // path; this is it on the consensus path.
    use tallyowl_cluster_api::codec::{decode_consensus_reply, encode_consensus_message};
    use tallyowl_cluster_api::types::{ConsensusKind, ConsensusMessage, GroupKind, GroupRef};

    let node = Node::start("n1");
    let group = GroupKey::Tablet("t1".into());
    let members = vec![node.member()];
    node.registry
        .start(
            group.clone(),
            Arc::new(TabletMachine::new(Arc::clone(&node.store))),
            members.clone(),
            0,
        )
        .unwrap();
    node.registry.bootstrap(&group, &members).unwrap();

    // Move this node's view of the cell forward. Two changes, so that "one
    // generation behind" is still a real generation rather than zero, which
    // means "the sender did not carry one".
    {
        let mut topology = node.topology.lock().unwrap();
        for name in ["n1", "n2"] {
            topology
                .apply(&crate::topology::ControllerCommand::RegisterNode {
                    node: name.into(),
                    address: node.address.clone(),
                    region: "west".into(),
                    domain: "rack-0".into(),
                })
                .unwrap();
        }
    }
    let current = node.topology.lock().unwrap().generation;
    assert!(current >= 2, "generation is {current}");

    let client =
        tallyowl_rpc::Client::new(node.address.clone(), crate::raft::network::MAX_FRAME_BYTES);
    let response = client
        .call(
            crate::raft::network::REPLICATION_SERVICE,
            crate::raft::network::DELIVER_CONSENSUS,
            encode_consensus_message(&ConsensusMessage {
                group: GroupRef {
                    kind: GroupKind::Tablet,
                    name: Some("t1".into()),
                },
                kind: ConsensusKind::Vote,
                sender: "n2".into(),
                // One generation behind.
                generation: current,
                payload: vec![0xf6],
            }),
        )
        .expect("the call reaches the service");
    let reply = decode_consensus_reply(&response.payload).unwrap();
    // At the current generation it is not refused for being stale.
    assert_eq!(reply.current_generation, Some(current));

    let stale = client
        .call(
            crate::raft::network::REPLICATION_SERVICE,
            crate::raft::network::DELIVER_CONSENSUS,
            encode_consensus_message(&ConsensusMessage {
                group: GroupRef {
                    kind: GroupKind::Tablet,
                    name: Some("t1".into()),
                },
                kind: ConsensusKind::Vote,
                sender: "n2".into(),
                generation: current - 1,
                payload: vec![0xf6],
            }),
        )
        .expect("the call reaches the service");
    let stale = decode_consensus_reply(&stale.payload).unwrap();
    assert!(!stale.accepted);
    assert_eq!(stale.current_generation, Some(current));
    assert!(stale.refusal.unwrap().contains("placement generation"));
}

#[test]
fn many_groups_on_one_node_share_one_connection_for_each_peer() {
    // STORAGE.md section 7: "multiplexed over shared CSIL connections". One
    // connection for each peer, not one for each group. The other shape puts
    // the connection count at the product of the node count and the tablet
    // count, which is what stops this design scaling.
    let nodes = [Node::start("n1"), Node::start("n2")];
    let members: Vec<Member> = nodes.iter().map(|n| n.member()).collect();

    for index in 0..6 {
        let group = GroupKey::Tablet(format!("t{index}"));
        for node in &nodes {
            node.registry
                .start(
                    group.clone(),
                    Arc::new(TabletMachine::new(Arc::clone(&node.store))),
                    members.clone(),
                    0,
                )
                .expect("the group starts");
        }
        nodes[0].registry.bootstrap(&group, &members).unwrap();
    }
    assert_eq!(nodes[0].registry.group_count(), 6);

    // Let the groups elect and heartbeat, which is what opens the connections.
    nodes[0]
        .registry
        .await_leader(&GroupKey::Tablet("t0".into()), ELECTION_PATIENCE)
        .expect("a leader");
    std::thread::sleep(Duration::from_millis(300));

    assert!(
        nodes[0].registry.open_connections() <= 2,
        "six groups opened {} connections to one peer",
        nodes[0].registry.open_connections()
    );
}

#[test]
fn a_committed_write_is_still_there_after_the_whole_group_restarts() {
    // The durability D27 left unmeasured: every append pays an fsync, and the
    // point of paying it is this.
    let place = directory("restart");
    let data = place.join("data");
    let consensus = place.join("consensus");

    let watermark = {
        let store: Arc<dyn Store> = Arc::new(SegmentedStore::open(&data).expect("a store"));
        let registry =
            GroupRegistry::new("solo", "127.0.0.1:0", Some(consensus.clone())).expect("a registry");
        let group = GroupKey::Tablet("t1".into());
        let members = vec![Member::voter("solo", "127.0.0.1:1")];
        registry
            .start(
                group.clone(),
                Arc::new(TabletMachine::new(Arc::clone(&store))),
                members.clone(),
                0,
            )
            .unwrap();
        registry.bootstrap(&group, &members).unwrap();
        registry
            .await_leader(&group, ELECTION_PATIENCE)
            .expect("one voter elects itself");

        let replicated = ReplicatedStore::new(
            Arc::clone(&registry),
            "t1",
            Arc::clone(&store),
            ReceiptPolicy::LocalOne,
            "home",
        );
        let (source, batch) = ids(5);
        let outcome = replicated
            .commit(source, batch, vec![a_row(6), a_row(7)])
            .expect("one voter commits after its own fsync");
        registry.shutdown();
        outcome.commit_watermark
    };

    // Everything is dropped. Open it again from the same directories.
    let store: Arc<dyn Store> = Arc::new(SegmentedStore::open(&data).expect("it reopens"));
    assert_eq!(store.row_count(), 2, "the rows survived");
    assert!(store.commit_watermark() >= watermark);

    let registry = GroupRegistry::new("solo", "127.0.0.1:0", Some(consensus)).expect("a registry");
    let group = GroupKey::Tablet("t1".into());
    registry
        .start(
            group.clone(),
            Arc::new(TabletMachine::new(Arc::clone(&store))),
            vec![Member::voter("solo", "127.0.0.1:1")],
            0,
        )
        .expect("the consensus log reopens");
    // It remembers its own membership from the log rather than being bootstrapped
    // again, which is what makes a restart a restart.
    registry
        .await_leader(&group, ELECTION_PATIENCE)
        .expect("it elects itself from the log it kept");
    registry.shutdown();
}

#[test]
fn a_remote_one_policy_refuses_when_no_replica_lives_outside_the_write_region() {
    // A policy that quietly degraded to `local-quorum` under a missing replica
    // would make the receipt a lie, and a receipt is the only thing a caller
    // has.
    let nodes = vec![Node::start("n1"), Node::start("n2"), Node::start("n3")];
    let group = GroupKey::Tablet("t1".into());
    let members: Vec<Member> = nodes.iter().map(|n| n.member()).collect();
    start_group(&nodes, &group, &members);
    let leader = leader_of(&nodes, &group);
    // The refusal under test happens *after* the group commits, so the commit
    // itself must not be what fails. On a machine that is busy with something
    // else the default ten seconds is not always enough, and the test would
    // then fail on a timeout rather than on the policy. This was seen once.
    leader.registry.set_write_timeout(Duration::from_secs(60));

    let store = ReplicatedStore::new(
        Arc::clone(&leader.registry),
        "t1",
        Arc::clone(&leader.store),
        ReceiptPolicy::RemoteOne,
        // Every member is in `west`, so nothing is outside the write region.
        "west",
    )
    .with_remote_copy_timeout(Duration::from_millis(200));

    let (source, batch) = ids(6);
    let failure = store
        .commit(source, batch, vec![a_row(8)])
        .expect_err("no replica can satisfy `remote-one`");
    assert!(
        failure.to_string().contains("remote-one"),
        "the refusal names the policy: {failure}"
    );
}

#[test]
fn a_replicated_store_is_not_writable_when_its_group_has_no_leader() {
    // A head that accepted a write it could not commit would acknowledge data
    // it then discarded. Readiness has to ask the group, not only the device.
    let node = Node::start("n1");
    let group = GroupKey::Tablet("t1".into());
    let store = ReplicatedStore::new(
        Arc::clone(&node.registry),
        "t1",
        Arc::clone(&node.store),
        ReceiptPolicy::LocalOne,
        "home",
    );
    assert!(
        !store.is_writable(),
        "this node does not hold the group at all"
    );

    let members = vec![node.member()];
    node.registry
        .start(
            group.clone(),
            Arc::new(TabletMachine::new(Arc::clone(&node.store))),
            members.clone(),
            0,
        )
        .unwrap();
    node.registry.bootstrap(&group, &members).unwrap();
    node.registry
        .await_leader(&group, ELECTION_PATIENCE)
        .unwrap();
    assert!(store.is_writable());
}

#[test]
fn losing_the_leader_during_a_write_costs_one_retry_and_never_two_commits() {
    // Phase 7 exit criterion: "leader loss during write returns either a prior
    // receipt or one logical retry."
    //
    // TallyOwl never claims exactly-once transport. What it claims is that a
    // repeated batch ID is one logical commit, and consensus has to keep that
    // true across an election: the group may have committed the entry before
    // the leader went away, and the caller cannot tell.
    let mut nodes = vec![Node::start("n1"), Node::start("n2"), Node::start("n3")];
    let group = GroupKey::Tablet("t1".into());
    let members: Vec<Member> = nodes.iter().map(|n| n.member()).collect();
    start_group(&nodes, &group, &members);

    let first_leader = leader_of(&nodes, &group).name.clone();
    let (source, batch) = ids(11);
    {
        let leader = nodes
            .iter()
            .find(|n| n.name == first_leader)
            .expect("the leader");
        let store = ReplicatedStore::new(
            Arc::clone(&leader.registry),
            "t1",
            Arc::clone(&leader.store),
            ReceiptPolicy::LocalQuorum,
            "west",
        );
        store
            .commit(source, batch, vec![a_row(1)])
            .expect("the first write commits");
    }

    // The leader goes away with the write already committed. Two voters are
    // left, which is a quorum of three.
    let index = nodes
        .iter()
        .position(|n| n.name == first_leader)
        .expect("the leader is one of these");
    nodes[index].lose(&group);

    let survivors: Vec<&Node> = nodes.iter().filter(|n| n.name != first_leader).collect();
    // Wait for a leader that is not the one that went away. `await_leader`
    // answers as soon as this node believes in *a* leader, and for the first
    // moment after a loss that belief is still the old one.
    let deadline = std::time::Instant::now() + ELECTION_PATIENCE;
    let mut next = first_leader.clone();
    while std::time::Instant::now() < deadline {
        if let Some(name) = survivors[0].registry.leader(&group) {
            if name != first_leader {
                next = name;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_ne!(
        next, first_leader,
        "the remaining voters elected a new leader"
    );

    let leader = survivors
        .iter()
        .find(|n| n.name == next)
        .expect("the new leader is one of the survivors");
    let store = ReplicatedStore::new(
        Arc::clone(&leader.registry),
        "t1",
        Arc::clone(&leader.store),
        ReceiptPolicy::LocalQuorum,
        "west",
    );
    // The caller retries with the same batch ID, because that is all it can do.
    let again = store
        .commit(source, batch, vec![a_row(1)])
        .expect("the retry is accepted by the new leader");
    assert!(
        again.deduplicated,
        "the new leader recognised the batch the old one had already committed"
    );
    assert_eq!(
        leader.store.row_count(),
        1,
        "the row exists once, not twice"
    );
}

// ---------------------------------------------------------------------------
// The segment copy. L087, and what it unblocks.
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_replica_catches_up_from_sealed_segments_over_a_real_socket() {
    // L087's gap: "a replica added to a tablet whose log has been purged past
    // its position will not catch up on its own." This is the path that lets it.
    // The source seals what it holds, the target holds nothing, and every
    // segment crosses a real socket and is proved from the bytes that arrived.
    let source = Node::start_holding("copy-source", "t1");
    let target = Node::start_holding("copy-target", "t1");

    let group = GroupKey::Tablet("t1".into());
    let members = vec![source.member()];
    source
        .registry
        .start(
            group.clone(),
            Arc::new(TabletMachine::new(Arc::clone(&source.store))),
            members.clone(),
            0,
        )
        .expect("the group starts on the source");
    source
        .registry
        .bootstrap(&group, &members)
        .expect("one voter");
    source
        .registry
        .await_leader(&group, ELECTION_PATIENCE)
        .expect("a leader");

    let store = ReplicatedStore::new(
        Arc::clone(&source.registry),
        "t1",
        Arc::clone(&source.store),
        ReceiptPolicy::LocalOne,
        "west",
    );
    for n in 1..=6u8 {
        let (s, b) = ids(n);
        store.commit(s, b, vec![a_row(n)]).expect("a write commits");
    }
    // Only a sealed segment travels. This is exactly why a snapshot seals.
    assert!(
        source.store.seal_now().expect("the source seals"),
        "the source had rows to seal"
    );
    assert_eq!(source.store.row_count(), 6);
    assert_eq!(target.store.row_count(), 0);

    let reader = crate::transfer::PeerSegments::at(source.address.clone());
    let into = crate::transfer::StoreSegments::new("t1", Arc::clone(&target.segments));
    let report =
        crate::transfer::copy_tablet(&reader, "t1", &into, crate::transfer::DEFAULT_CHUNK_BYTES)
            .expect("the copy runs");

    assert_eq!(report.segments_copied, 1, "one sealed segment moved");
    assert_eq!(report.rows, 6);
    assert_eq!(
        target.store.row_count(),
        6,
        "the target holds the rows it copied"
    );

    // And the copied rows are findable by an exact lookup, which is what says
    // the locator was rebuilt rather than merely the file written.
    let found = target
        .store
        .lookup_correlated("event_id", &a_row(3).event_id)
        .expect("an exact lookup");
    assert_eq!(found.rows.len(), 1, "the copied locator answers");
    assert!(!found.incomplete);
}

#[test]
fn a_copy_that_arrives_damaged_is_discarded_rather_than_installed() {
    // The parity check is the point of the copy. A segment that did not arrive
    // intact must leave the target holding nothing, because a target holding
    // half a segment answers a query with a smaller number and says nothing.
    let source = Node::start_holding("damage-source", "t1");
    let target = Node::start_holding("damage-target", "t1");
    let (s, b) = ids(1);
    source
        .store
        .commit(s, b, vec![a_row(1), a_row(2)])
        .expect("a write");
    source.store.seal_now().expect("a seal");

    // A reader that corrupts one byte on the way past. Nothing else changes.
    struct Corrupting(crate::transfer::PeerSegments);
    impl crate::transfer::SegmentReader for Corrupting {
        fn list(
            &self,
            tablet: &str,
        ) -> Result<tallyowl_cluster_api::types::SegmentList, TallyOwlError> {
            self.0.list(tablet)
        }
        fn fetch(
            &self,
            tablet: &str,
            segment_id: &str,
            offset: u64,
            max_bytes: u64,
        ) -> Result<tallyowl_cluster_api::types::SegmentTransfer, TallyOwlError> {
            let mut chunk = self.0.fetch(tablet, segment_id, offset, max_bytes)?;
            if let Some(byte) = chunk.data.last_mut() {
                *byte ^= 0xff;
            }
            Ok(chunk)
        }
    }

    let reader = Corrupting(crate::transfer::PeerSegments::at(source.address.clone()));
    let into = crate::transfer::StoreSegments::new("t1", Arc::clone(&target.segments));
    let refused =
        crate::transfer::copy_tablet(&reader, "t1", &into, crate::transfer::DEFAULT_CHUNK_BYTES)
            .expect_err("a damaged copy stops");
    assert!(
        refused.message.contains("did not arrive intact"),
        "the refusal says what happened: {}",
        refused.message
    );
    assert_eq!(
        target.store.row_count(),
        0,
        "nothing was installed from a copy that could not be proved"
    );
}

#[test]
fn a_copy_onto_a_node_that_already_holds_the_tablet_counts_the_overlap_once() {
    // **This test asserted a refusal until L147.** Two replicas that applied
    // the same entries build differently shaped segments, so there is no honest
    // way to tell a copied *segment* from one the target built itself — and a
    // segment was the wrong unit to reconcile on. A row carries a
    // producer-assigned `event_id`, so the target keeps what it does not
    // already hold.
    //
    // The fixture is the case that matters: both nodes hold the same row, and
    // the source holds one the target does not.
    let source = Node::start_holding("merge-source", "t1");
    let target = Node::start_holding("merge-target", "t1");
    for node in [&source, &target] {
        let (s, b) = ids(1);
        node.store.commit(s, b, vec![a_row(1)]).expect("a write");
        node.store.seal_now().expect("a seal");
    }
    // One more, on the source alone.
    let (s, b) = ids(2);
    source.store.commit(s, b, vec![a_row(2)]).expect("a write");
    source.store.seal_now().expect("a seal");

    let reader = crate::transfer::PeerSegments::at(source.address.clone());
    let into = crate::transfer::StoreSegments::new("t1", Arc::clone(&target.segments));
    crate::transfer::copy_tablet(&reader, "t1", &into, crate::transfer::DEFAULT_CHUNK_BYTES)
        .expect("a copy onto a node that holds part of it reconciles");

    assert_eq!(
        target.store.row_count(),
        2,
        "the shared row was counted twice, or the new one did not arrive"
    );
}

#[test]
fn a_reconciling_copy_that_brings_nothing_new_changes_nothing() {
    // The two nodes hold exactly the same rows. Every row is already here, so
    // the copy is a copy that finished and ran again.
    let source = Node::start_holding("same-source", "t1");
    let target = Node::start_holding("same-target", "t1");
    for node in [&source, &target] {
        let (s, b) = ids(1);
        node.store.commit(s, b, vec![a_row(1)]).expect("a write");
        node.store.seal_now().expect("a seal");
    }

    let reader = crate::transfer::PeerSegments::at(source.address.clone());
    let into = crate::transfer::StoreSegments::new("t1", Arc::clone(&target.segments));
    crate::transfer::copy_tablet(&reader, "t1", &into, crate::transfer::DEFAULT_CHUNK_BYTES)
        .expect("it reconciles");
    assert_eq!(target.store.row_count(), 1, "a row was doubled");

    // And again, because a copy is safe to repeat.
    crate::transfer::copy_tablet(&reader, "t1", &into, crate::transfer::DEFAULT_CHUNK_BYTES)
        .expect("it reconciles again");
    assert_eq!(
        target.store.row_count(),
        1,
        "repeating the copy doubled a row"
    );
}

#[test]
fn a_copy_is_safe_to_run_again() {
    // A transfer that stopped halfway is finished by running it again rather
    // than by an operator working out where it stopped. A segment already held
    // under the same content address is not published twice.
    let source = Node::start_holding("repeat-source", "t1");
    let target = Node::start_holding("repeat-target", "t1");
    let (s, b) = ids(1);
    source
        .store
        .commit(s, b, vec![a_row(1), a_row(2), a_row(3)])
        .expect("a write");
    source.store.seal_now().expect("a seal");

    let reader = crate::transfer::PeerSegments::at(source.address.clone());
    let into = crate::transfer::StoreSegments::new("t1", Arc::clone(&target.segments));
    let first = crate::transfer::copy_tablet(&reader, "t1", &into, 64)
        .expect("the copy runs in small chunks");
    assert_eq!(first.segments_copied, 1);
    assert_eq!(target.store.row_count(), 3);

    // The second run has to be told to proceed, because the target now holds
    // the tablet. That is the refusal above, so the repeat is exercised through
    // `install` directly, which is what a resumed transfer calls.
    let bytes = source
        .segments
        .read_segment_bytes(
            &source
                .segments
                .manifests()
                .expect("manifests")
                .first()
                .expect("one segment")
                .segment_id,
        )
        .expect("the segment reads");
    let again = crate::transfer::TabletSegments::install(&into, "t1", bytes)
        .expect("installing it again is accepted");
    assert!(
        again.already_held,
        "the same content was recognised rather than published twice"
    );
    assert_eq!(target.store.row_count(), 3, "the rows were not doubled");
}

#[test]
fn a_snapshot_seals_first_so_everything_it_covers_can_be_copied() {
    // This is the sentence that makes purging the log safe. A snapshot says
    // "everything below here is covered", and for a tablet the cover is a
    // sealed segment. Rows applied and not yet sealed would be on no replica
    // that catches up by segment copy.
    let node = Node::start_holding("seal-on-snapshot", "t1");
    let group = GroupKey::Tablet("t1".into());
    let members = vec![node.member()];
    node.registry
        .start(
            group.clone(),
            Arc::new(TabletMachine::new(Arc::clone(&node.store))),
            members.clone(),
            0,
        )
        .expect("the group starts");
    node.registry
        .bootstrap(&group, &members)
        .expect("one voter");
    node.registry
        .await_leader(&group, ELECTION_PATIENCE)
        .expect("a leader");

    let store = ReplicatedStore::new(
        Arc::clone(&node.registry),
        "t1",
        Arc::clone(&node.store),
        ReceiptPolicy::LocalOne,
        "west",
    );
    let (s, b) = ids(1);
    store
        .commit(s, b, vec![a_row(1), a_row(2)])
        .expect("a write");

    assert!(
        node.segments.manifests().expect("manifests").is_empty(),
        "nothing is sealed yet, so nothing could be copied yet"
    );
    node.registry
        .snapshot_now(&group)
        .expect("the group asks for a snapshot");
    // A snapshot is built on the group's own task, so the request returns
    // before the seal. Wait for the segment rather than for a duration.
    let mut sealed = 0;
    for _ in 0..500 {
        sealed = node.segments.manifests().expect("manifests").len();
        if sealed > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        sealed, 1,
        "the snapshot sealed, so what it covers is in a segment a copy can carry"
    );
}

#[test]
fn a_consensus_log_entry_is_compressed_and_reads_back() {
    // L095's option 2. The log is a second full copy of every batch, and a
    // batch is the largest thing in it. What matters here is that the entry
    // reads back: a log this node cannot read is worse than a large one.
    let node = Node::start_holding("compressed-log", "t1");
    let group = GroupKey::Tablet("t1".into());
    let members = vec![node.member()];
    node.registry
        .start(
            group.clone(),
            Arc::new(TabletMachine::new(Arc::clone(&node.store))),
            members.clone(),
            0,
        )
        .expect("the group starts");
    node.registry
        .bootstrap(&group, &members)
        .expect("one voter");
    node.registry
        .await_leader(&group, ELECTION_PATIENCE)
        .expect("a leader");

    let store = ReplicatedStore::new(
        Arc::clone(&node.registry),
        "t1",
        Arc::clone(&node.store),
        ReceiptPolicy::LocalOne,
        "west",
    );
    // A batch large enough to be over the compression threshold, and repetitive
    // the way real telemetry from one route is.
    let rows: Vec<EventRow> = (0..200u8).map(a_row).collect();
    let (s, b) = ids(1);
    let outcome = store.commit(s, b, rows).expect("a large batch commits");
    assert_eq!(outcome.accepted, 200);
    assert_eq!(
        node.store.row_count(),
        200,
        "every row applied from the log"
    );
}

// ---------------------------------------------------------------------------
// The query fan-out.
// ---------------------------------------------------------------------------

/// Two nodes, each holding its own tablet, and a topology that names both.
///
/// This is the shape a fan-out exists for. A head that read its own replica
/// would answer with one tablet's rows and would not know it was short.
fn two_tablets() -> (Node, Node) {
    let one = Node::start_holding("fanout-a", "ta");
    let two = Node::start_holding("fanout-b", "tb");
    for node in [&one, &two] {
        let mut held = node.topology.lock().expect("topology");
        for other in [&one, &two] {
            held.apply(&crate::topology::ControllerCommand::RegisterNode {
                node: other.name.clone(),
                address: other.address.clone(),
                region: "west".into(),
                domain: format!("rack-{}", other.name),
            })
            .expect("a node registers");
        }
        held.apply(&crate::topology::ControllerCommand::RegisterCell {
            cell: "west-1".into(),
            region: "west".into(),
            controllers: vec![one.member()],
        })
        .expect("a cell registers");
        for (name, holder) in [("ta", &one), ("tb", &two)] {
            held.apply(&crate::topology::ControllerCommand::CreateTablet {
                tablet: name.into(),
                cell: "west-1".into(),
                region: "west".into(),
                shard_start: if name == "ta" {
                    0
                } else {
                    crate::topology::VIRTUAL_SHARDS / 2
                },
                shard_end: if name == "ta" {
                    crate::topology::VIRTUAL_SHARDS / 2
                } else {
                    crate::topology::VIRTUAL_SHARDS
                },
                members: vec![holder.member()],
                receipt_policy: ReceiptPolicy::LocalOne,
            })
            .expect("a tablet is created");
        }
    }
    // Each node holds only its own tablet's group, which is what makes the
    // other one a real remote call.
    for (node, name) in [(&one, "ta"), (&two, "tb")] {
        let group = GroupKey::Tablet(name.to_string());
        node.registry
            .start(
                group.clone(),
                Arc::new(TabletMachine::new(Arc::clone(&node.store))),
                vec![node.member()],
                0,
            )
            .expect("the group starts");
        node.registry
            .bootstrap(&group, &[node.member()])
            .expect("one voter");
        node.registry
            .await_leader(&group, ELECTION_PATIENCE)
            .expect("a leader");
    }
    (one, two)
}

fn reads_across(node: &Node) -> Arc<dyn crate::fanout::TabletReads> {
    Arc::new(crate::fanout::ClusterReads::new(
        Arc::clone(&node.topology),
        Arc::clone(&node.registry),
        Arc::new(LocalSource {
            node: node.name.clone(),
            store: Arc::clone(&node.store),
        }),
    ))
}

#[test]
fn a_read_asks_every_tablet_and_the_answer_is_the_union() {
    let (one, two) = two_tablets();
    let (s, b) = ids(1);
    one.store
        .commit(s, b, vec![a_row(1), a_row(2)])
        .expect("a write");
    let (s, b) = ids(2);
    two.store.commit(s, b, vec![a_row(3)]).expect("a write");

    let store = ReplicatedStore::new(
        Arc::clone(&one.registry),
        "ta",
        Arc::clone(&one.store),
        ReceiptPolicy::LocalOne,
        "west",
    )
    .reading_across(reads_across(&one));

    let scanned = store
        .scan([7u8; 16], 0, 10_000, tallyowl_store::TimeBasis::OccurredAt)
        .expect("a scan across both tablets");
    assert_eq!(
        scanned.rows.len(),
        3,
        "the answer is the union of both tablets, not one of them"
    );
    assert!(!scanned.incomplete, "every tablet answered");

    // And a count is computed on each tablet and added here, rather than by
    // moving rows.
    let trend = store
        .trend(
            [7u8; 16],
            0,
            10_000,
            tallyowl_store::TimeBasis::OccurredAt,
            10_000,
            None,
        )
        .expect("a trend across both tablets");
    assert_eq!(trend.total, 3);
    assert!(!trend.incomplete);
}

#[test]
fn a_tablet_that_does_not_answer_makes_the_result_incomplete_and_is_named() {
    // The failure FAILURE_MODES.md section 2 ranks worst is a smaller answer
    // presented as a complete one. A tablet that is gone must make the result
    // say so, and `unreadable` must name which tablet.
    let (one, mut two) = two_tablets();
    let (s, b) = ids(1);
    one.store
        .commit(s, b, vec![a_row(1), a_row(2)])
        .expect("a write");
    let (s, b) = ids(2);
    two.store.commit(s, b, vec![a_row(3)]).expect("a write");

    two.lose(&GroupKey::Tablet("tb".into()));

    let store = ReplicatedStore::new(
        Arc::clone(&one.registry),
        "ta",
        Arc::clone(&one.store),
        ReceiptPolicy::LocalOne,
        "west",
    )
    .reading_across(reads_across(&one));

    let scanned = store
        .scan([7u8; 16], 0, 10_000, tallyowl_store::TimeBasis::OccurredAt)
        .expect("a scan still answers");
    assert!(
        scanned.incomplete,
        "a tablet that did not answer makes the result incomplete"
    );
    assert_eq!(scanned.rows.len(), 2, "what did answer is still returned");
    let named = store.unreadable();
    assert!(
        named.iter().any(|reason| reason.contains("`tb`")),
        "the tablet that did not answer is named: {named:?}"
    );
}

#[test]
fn an_exact_lookup_asks_every_tablet_and_concatenates() {
    // `AGENTS.md`: "Do not silently drop, coalesce, or reject a value because
    // it has high cardinality." Across tablets that means every tablet that
    // could hold the value is asked and the answers are concatenated.
    let (one, two) = two_tablets();
    let mut left = a_row(1);
    left.trace_id = Some([9u8; 16]);
    let mut right = a_row(2);
    right.trace_id = Some([9u8; 16]);

    let (s, b) = ids(1);
    one.store.commit(s, b, vec![left]).expect("a write");
    let (s, b) = ids(2);
    two.store.commit(s, b, vec![right]).expect("a write");

    let store = ReplicatedStore::new(
        Arc::clone(&one.registry),
        "ta",
        Arc::clone(&one.store),
        ReceiptPolicy::LocalOne,
        "west",
    )
    .reading_across(reads_across(&one));

    let found = store
        .lookup_correlated("trace_id", &[9u8; 16])
        .expect("an exact lookup across tablets");
    assert_eq!(
        found.rows.len(),
        2,
        "both halves of the trace came back, not one"
    );
    assert!(!found.incomplete);
}

#[test]
fn an_erasure_is_proposed_so_every_replica_hides_the_rows() {
    // A replica that missed a tombstone would answer a query with data an
    // erasure removed, which is worse than any wrong number. `AGENTS.md` makes
    // a tombstone a standing predicate, and a predicate that reached one
    // replica is not one.
    let nodes = vec![Node::start("n1"), Node::start("n2"), Node::start("n3")];
    let group = GroupKey::Tablet("t1".into());
    let members: Vec<Member> = nodes.iter().map(|n| n.member()).collect();
    start_group(&nodes, &group, &members);

    let leader = leader_of(&nodes, &group);
    let store = ReplicatedStore::new(
        Arc::clone(&leader.registry),
        "t1",
        Arc::clone(&leader.store),
        ReceiptPolicy::LocalQuorum,
        "west",
    );

    let mut doomed = a_row(1);
    doomed.session_id = Some("session-to-erase".into());
    let kept = a_row(2);
    let (s, b) = ids(1);
    store
        .commit(s, b, vec![doomed.clone(), kept.clone()])
        .expect("a write commits");

    // Wait for every replica to hold both rows before erasing one of them.
    let applied = leader.registry.applied_index(&group);
    for _ in 0..500 {
        if nodes
            .iter()
            .all(|n| n.registry.applied_index(&group) >= applied)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let tombstone = tallyowl_store::catalog::Tombstone {
        tombstone_id: [42u8; 16],
        generation: 0,
        project_id: [7u8; 16],
        event_ids: Vec::new(),
        property: Some(("session_id".to_string(), "session-to-erase".to_string())),
        except_kinds: Vec::new(),
        range: None,
        requested_at: 1,
        horizon: i64::MAX,
        reason: "the person asked to be removed".into(),
    };
    let generation = store.erase(&tombstone).expect("the erasure commits");
    assert!(generation > 0);

    // Every replica hides it, not only the one that took the request.
    let applied = leader.registry.applied_index(&group);
    let mut hidden = 0;
    for _ in 0..500 {
        hidden = nodes
            .iter()
            .filter(|n| {
                n.registry.applied_index(&group) >= applied
                    && n.store
                        .scan([7u8; 16], 0, 10_000, tallyowl_store::TimeBasis::OccurredAt)
                        .map(|scanned| {
                            scanned.rows.len() == 1 && scanned.rows[0].event_id == kept.event_id
                        })
                        .unwrap_or(false)
            })
            .count();
        if hidden == 3 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        hidden, 3,
        "every replica applied the predicate, not only the leader"
    );

    // Applying it again is applying it once. A replayed log entry must not
    // produce a second erasure record.
    let again = store.erase(&tombstone).expect("a repeat is accepted");
    assert!(again >= generation);
    assert_eq!(
        leader
            .store
            .scan([7u8; 16], 0, 10_000, tallyowl_store::TimeBasis::OccurredAt)
            .expect("a scan")
            .rows
            .len(),
        1
    );
}

// ---------------------------------------------------------------------------
// What bounds the consensus log. L099, and now settings rather than constants.
// ---------------------------------------------------------------------------

#[test]
fn the_consensus_log_bounds_are_what_an_operator_set() {
    // L099 measured what these two bound: the log fell from 798.3 bytes for
    // each event to 36.6 once it was bounded. The number that is right depends
    // on how many small segments an installation can afford, because a tablet
    // snapshot seals, so it cannot be one constant for everybody.
    let registry = crate::groups::GroupRegistry::new("node-a", "127.0.0.1:0", None)
        .expect("a registry starts");
    assert_eq!(
        registry.log_bounds(),
        (
            crate::raft::SNAPSHOT_EVERY_ENTRIES,
            crate::raft::KEEP_AFTER_SNAPSHOT
        ),
        "the measured defaults are still the defaults"
    );

    registry.set_log_bounds(1_024, 64);
    assert_eq!(registry.log_bounds(), (1_024, 64));
}

#[test]
fn a_snapshot_policy_of_zero_is_refused_rather_than_stopping_the_node() {
    // openraft refuses a zero policy, and a configuration mistake must not be
    // the reason a node will not start. It is floored instead.
    let config = crate::raft::config_with(0, 0);
    assert!(
        matches!(
            config.snapshot_policy,
            openraft::SnapshotPolicy::LogsSinceLast(1)
        ),
        "a zero snapshot policy reached openraft"
    );
}
