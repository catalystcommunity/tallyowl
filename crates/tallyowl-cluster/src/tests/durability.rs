//! What a group still holds after the things that go wrong on one node.
//!
//! The consensus tests next door ask whether a group agrees. These ask what one
//! replica is left holding after a restart, a full disk, and a snapshot that
//! could not be prepared, because agreeing on a log is not the same as holding
//! what the log says.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tallyowl_store::row::EventRow;
use tallyowl_store::{SegmentedStore, Store};

use crate::groups::{GroupKey, GroupRegistry};
use crate::raft::machine::{
    ControllerMachine, GroupMachine, Outcome, TabletCommand, TabletMachine,
};
use crate::raft::storage::GroupStorage;
use crate::topology::{ControllerCommand, Member, Topology};
use crate::transfer::{LocalReader, SegmentCatchUp, SegmentReader, StoreSegments, TabletSegments};

const ELECTION_PATIENCE: Duration = Duration::from_secs(20);

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

fn register(node: &str) -> Vec<u8> {
    crate::raft::encode(&ControllerCommand::RegisterNode {
        node: node.into(),
        address: format!("10.0.0.9:{}", 5200 + node.len()),
        region: "west".into(),
        domain: format!("rack-{node}"),
    })
    .expect("a command encodes")
}

/// One controller voter on a real directory, the way a restart finds it.
fn controllers_at(place: &std::path::Path) -> (Arc<GroupRegistry>, Arc<Mutex<Topology>>, GroupKey) {
    let registry =
        GroupRegistry::new("solo", "127.0.0.1:0", Some(place.to_path_buf())).expect("a registry");
    // **A fresh topology each time**, which is what the head builds on every
    // start: it seeds from configuration and knows nothing the group decided.
    let topology = Arc::new(Mutex::new(Topology::new()));
    let group = GroupKey::CellController("c1".into());
    registry
        .start(
            group.clone(),
            Arc::new(ControllerMachine::new(Arc::clone(&topology))),
            vec![Member::voter("solo", "127.0.0.1:1")],
            0,
        )
        .expect("the group starts");
    (registry, topology, group)
}

#[test]
fn a_controller_group_keeps_every_committed_command_across_a_restart() {
    // The applied position is durable and the topology is in memory. openraft
    // applies nothing at or below that position again, so before the machine's
    // state was written with it, a restart deleted every committed command
    // since the last snapshot — and a controller group never reaches one.
    let place = directory("controller-restart");
    {
        let (registry, topology, group) = controllers_at(&place);
        registry
            .bootstrap(&group, &[Member::voter("solo", "127.0.0.1:1")])
            .expect("the voter set is created once");
        registry
            .await_leader(&group, ELECTION_PATIENCE)
            .expect("one voter elects itself");
        for node in ["n7", "n8"] {
            let outcome = registry
                .propose(&group, register(node))
                .expect("it commits");
            assert!(matches!(outcome, Outcome::Applied { .. }), "{outcome:?}");
        }
        assert!(topology.lock().unwrap().node("n8").is_some());
        registry.shutdown();
    }

    let (registry, topology, _) = controllers_at(&place);
    let held = topology.lock().unwrap().clone();
    assert!(
        held.node("n7").is_some() && held.node("n8").is_some(),
        "a restart lost what the group had committed"
    );
    registry.shutdown();
}

#[test]
fn a_group_file_from_before_the_state_record_replays_from_its_snapshot() {
    // An installation that upgrades holds files with a position and no state.
    // Those must open, and must not claim the position their machine is not at.
    let place = directory("controller-upgrade");
    let generation = {
        let (registry, topology, group) = controllers_at(&place);
        registry
            .bootstrap(&group, &[Member::voter("solo", "127.0.0.1:1")])
            .expect("the voter set is created once");
        registry
            .await_leader(&group, ELECTION_PATIENCE)
            .expect("one voter elects itself");
        registry
            .propose(&group, register("n7"))
            .expect("it commits");
        registry.shutdown();
        let generation = topology.lock().unwrap().generation;
        generation
    };

    // Make the file what an older release left behind.
    let file = place.join("cell-c1").join("raft.redb");
    GroupStorage::forget_machine_state(&file).expect("the record is removed");

    let (registry, topology, _) = controllers_at(&place);
    let held = topology.lock().unwrap().clone();
    assert!(
        held.node("n7").is_some(),
        "the log after the snapshot was not applied again"
    );
    assert_eq!(
        held.generation, generation,
        "applying the log again reached a different topology"
    );
    registry.shutdown();
}

fn a_row(n: u8) -> EventRow {
    let mut id = [0u8; 16];
    id[0] = n;
    let mut row = EventRow::new(id, "event", "checkout", 1_000 + n as i64);
    row.project_id = [7u8; 16];
    row
}

fn a_commit(n: u8) -> Vec<u8> {
    let mut batch_id = [0u8; 16];
    batch_id[0] = n;
    crate::raft::encode(&TabletCommand::Commit {
        source_id: [1u8; 16],
        batch_id,
        rows: crate::raft::machine::encode_rows(&[a_row(n)]),
    })
    .expect("a command encodes")
}

/// A real store whose device has no room: the reserve is larger than any disk.
fn a_store_with_no_room(name: &str) -> Arc<dyn Store> {
    let sealing = tallyowl_store::Sealing {
        reserve_bytes: u64::MAX,
        ..Default::default()
    };
    Arc::new(
        SegmentedStore::open_with(directory(name).join("data"), sealing, Default::default())
            .expect("a store opens"),
    )
}

#[test]
fn a_replica_whose_store_has_no_room_fails_the_entry_rather_than_skipping_it() {
    // Returning a refusal here marked the entry applied, and the batch then
    // existed on every replica but this one, for ever, with nothing to say so.
    let machine = TabletMachine::new(a_store_with_no_room("no-room"));
    let answer = machine.apply(1, &a_commit(1));
    assert!(
        answer.is_err(),
        "a full disk was reported as an outcome: {:?}",
        answer.map(|bytes| crate::raft::machine::read_outcome(&bytes))
    );
    assert_eq!(
        machine.marks().applied_index,
        0,
        "the entry reads as applied and it was not"
    );
}

#[test]
fn a_batch_no_replica_could_read_is_refused_and_not_a_failure() {
    // Every replica was handed the same bytes and reaches the same refusal, so
    // this one must not stop the group.
    let store: Arc<dyn Store> =
        Arc::new(SegmentedStore::open(directory("unreadable").join("data")).expect("a store"));
    let machine = TabletMachine::new(store);
    let command = crate::raft::encode(&TabletCommand::Commit {
        source_id: [1u8; 16],
        batch_id: [2u8; 16],
        rows: vec![0xff, 0x00, 0x13],
    })
    .unwrap();
    let answer = machine
        .apply(1, &command)
        .expect("a refusal, not a failure");
    assert!(matches!(
        crate::raft::machine::read_outcome(&answer),
        Outcome::Refused { .. }
    ));
    assert_eq!(machine.marks().applied_index, 1);
}

// ---------------------------------------------------------------------------
// A replica behind the purged log. See `crate::transfer::SegmentCatchUp`.
// ---------------------------------------------------------------------------

/// A replica's store and the segments side of it, reporting a fixed position.
fn a_replica(name: &str, applied: u64) -> (Arc<SegmentedStore>, Arc<dyn TabletSegments>) {
    let store =
        Arc::new(SegmentedStore::open(directory(name).join("data")).expect("a store opens"));
    let segments: Arc<dyn TabletSegments> = Arc::new(
        StoreSegments::new("t1", Arc::clone(&store)).reporting_applied(Arc::new(move || applied)),
    );
    (store, segments)
}

fn marks_at(index: u64) -> Vec<u8> {
    crate::raft::encode(&crate::raft::machine::TabletMarks {
        applied_index: index,
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn a_tablet_snapshot_is_refused_when_the_rows_cannot_be_fetched() {
    // The snapshot holds marks and no rows. Installing it and fetching nothing
    // made a replica that reported itself level with the leader, counted
    // towards a quorum, and answered reads, while holding none of the rows in
    // between.
    let (store, _) = a_replica("no-catch-up", 0);
    let machine = TabletMachine::new(store as Arc<dyn Store>);
    let refused = machine.install_from_peer(&marks_at(9), 9, &["127.0.0.1:1".into()]);
    assert!(
        refused.is_err(),
        "a snapshot with no rows behind it installed"
    );
    assert_eq!(
        machine.marks().applied_index,
        0,
        "the replica reports a position it does not hold"
    );
}

#[test]
fn a_catch_up_copies_only_from_a_replica_that_reached_the_snapshot() {
    let (ahead_store, ahead) = a_replica("ahead", 9);
    ahead_store
        .commit([1u8; 16], [1u8; 16], vec![a_row(1), a_row(2), a_row(3)])
        .expect("the replica that kept up holds the rows");
    let (_, behind) = a_replica("behind", 3);
    let (target_store, target) = a_replica("target", 0);

    let sources: std::collections::HashMap<String, Arc<dyn TabletSegments>> = [
        ("behind:1".to_string(), behind),
        ("ahead:1".to_string(), ahead),
    ]
    .into_iter()
    .collect();
    let open = {
        let sources = sources.clone();
        Arc::new(move |address: &str| {
            Arc::new(LocalReader(Arc::clone(&sources[address]))) as Arc<dyn SegmentReader>
        })
    };

    let machine = TabletMachine::new(Arc::clone(&target_store) as Arc<dyn Store>).catching_up_with(
        Arc::new(SegmentCatchUp::new("t1", Arc::clone(&target)).opening_with(open.clone())),
    );

    // Only the replica that is behind can be reached: nothing qualifies.
    let refused = machine.install_from_peer(&marks_at(9), 9, &["behind:1".into()]);
    let reason = refused.expect_err("a replica that is behind cannot supply the snapshot's rows");
    assert!(reason.contains("entry 3"), "{reason}");
    assert_eq!(target_store.row_count(), 0);
    assert_eq!(machine.marks().applied_index, 0);

    // With the one that kept up, the rows arrive before the position moves.
    machine
        .install_from_peer(&marks_at(9), 9, &["behind:1".into(), "ahead:1".into()])
        .expect("the rows were copied");
    assert_eq!(
        target_store.row_count(),
        3,
        "the rows were sealed on the source because it was asked to list, and then copied"
    );
    assert_eq!(machine.marks().applied_index, 9);
}

#[test]
fn a_tablet_snapshot_carries_the_erasures_a_lagging_replica_missed() {
    // An erasure reaches a replica as a log entry. A replica that installs a
    // snapshot skipped the entries behind it, and the segments it copies still
    // hold the erased rows until a compaction rewrites them.
    let (source_store, _) = a_replica("erasure-source", 5);
    let tombstone = tallyowl_store::catalog::Tombstone {
        tombstone_id: [42u8; 16],
        generation: 0,
        project_id: [7u8; 16],
        event_ids: vec![a_row(1).event_id],
        property: None,
        except_kinds: Vec::new(),
        range: None,
        requested_at: 1,
        horizon: i64::MAX,
        reason: "the person asked to be removed".into(),
    };
    source_store.erase(&tombstone).expect("the source holds it");

    let reading = Arc::clone(&source_store);
    let source = TabletMachine::new(Arc::clone(&source_store) as Arc<dyn Store>)
        .carrying_erasures_from(Arc::new(move || {
            Ok(reading
                .catalog()
                .tombstones()
                .map_err(|e| e.to_string())?
                .iter()
                .map(tallyowl_store::catalog::encode_tombstone)
                .collect())
        }));
    let sent = source
        .snapshot_for_peer(source.snapshot())
        .expect("a snapshot for a peer");

    let (target_store, _) = a_replica("erasure-target", 0);
    target_store
        .commit([1u8; 16], [1u8; 16], vec![a_row(1), a_row(2)])
        .expect("the target holds the row the erasure names");
    let target = TabletMachine::new(Arc::clone(&target_store) as Arc<dyn Store>);
    // Position zero, so no rows are owed and only the erasure is at issue.
    target
        .install_from_peer(&sent, 0, &[])
        .expect("it installs");

    let visible = target_store
        .scan([7u8; 16], 0, 10_000, tallyowl_store::TimeBasis::OccurredAt)
        .expect("a scan")
        .rows;
    assert_eq!(visible.len(), 1, "the erased row is still answered");
    assert_eq!(visible[0].event_id, a_row(2).event_id);

    // A snapshot from before erasures travelled is the marks alone, and reads.
    target
        .install(&marks_at(0))
        .expect("the older shape still installs");
}

// ---------------------------------------------------------------------------
// What an operator can see, and what a failure does to the group.
// ---------------------------------------------------------------------------

/// A machine that fails the way a device does, on request.
#[derive(Default)]
struct Faulty {
    /// Fail the snapshot preparation, as a store that cannot seal does.
    no_snapshot: std::sync::atomic::AtomicBool,
    /// Fail an apply, as a store with no room does.
    no_apply: std::sync::atomic::AtomicBool,
    applied: std::sync::atomic::AtomicU64,
}

impl GroupMachine for Faulty {
    fn apply(&self, index: u64, _payload: &[u8]) -> Result<Vec<u8>, String> {
        if self.no_apply.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("the device has no room".into());
        }
        self.applied
            .store(index, std::sync::atomic::Ordering::SeqCst);
        Ok(Vec::new())
    }

    fn snapshot(&self) -> Vec<u8> {
        self.applied
            .load(std::sync::atomic::Ordering::SeqCst)
            .to_be_bytes()
            .to_vec()
    }

    fn install(&self, _snapshot: &[u8]) -> Result<(), String> {
        Ok(())
    }

    fn before_snapshot(&self) -> Result<(), String> {
        match self.no_snapshot.load(std::sync::atomic::Ordering::SeqCst) {
            true => Err("the store could not seal".into()),
            false => Ok(()),
        }
    }
}

fn one_voter(machine: Arc<Faulty>) -> (Arc<GroupRegistry>, GroupKey) {
    let registry = GroupRegistry::new("solo", "127.0.0.1:0", None).expect("a registry");
    let group = GroupKey::Tablet("t1".into());
    let members = vec![Member::voter("solo", "127.0.0.1:1")];
    registry
        .start(group.clone(), machine, members.clone(), 0)
        .expect("the group starts");
    registry.bootstrap(&group, &members).expect("one voter");
    registry
        .await_leader(&group, ELECTION_PATIENCE)
        .expect("it elects itself");
    (registry, group)
}

#[test]
fn a_snapshot_that_cannot_be_prepared_does_not_stop_the_group() {
    // openraft stops a whole group on a storage error, and every replica of a
    // tablet reaches the snapshot policy at the same log index. A disk near its
    // reserve therefore stopped every replica at once, and a restart stopped
    // them again because the policy was still exceeded.
    let machine = Arc::new(Faulty::default());
    machine
        .no_snapshot
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (registry, group) = one_voter(Arc::clone(&machine));
    registry.propose(&group, vec![1]).expect("a write");

    registry
        .snapshot_now(&group)
        .expect("a snapshot is asked for");
    let mut failures = 0;
    for _ in 0..500 {
        failures = registry
            .health_of(&group)
            .expect("health")
            .snapshot_failures;
        if failures > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        failures > 0,
        "the failure was not counted, so nobody would see it"
    );

    let health = registry.health_of(&group).expect("health");
    assert_eq!(health.fatal, None, "a failed snapshot stopped the group");
    assert_eq!(
        health.snapshot_index, 0,
        "a snapshot that failed was recorded"
    );
    assert!(health.last_snapshot_failure.is_some());
    registry
        .propose(&group, vec![2])
        .expect("the group still takes a write");
    registry.shutdown();
}

#[test]
fn a_group_that_stopped_has_no_leader_and_is_counted() {
    // openraft keeps the last leader it saw in its metrics after a fatal error.
    // A caller that read only that reported a writable tablet whose every
    // write then failed, and no gauge said a group had stopped.
    let machine = Arc::new(Faulty::default());
    let (registry, group) = one_voter(Arc::clone(&machine));
    assert!(registry.is_leader(&group));

    machine
        .no_apply
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(
        registry.propose(&group, vec![1]).is_err(),
        "an entry this replica could not apply was acknowledged"
    );
    let mut stopped = false;
    for _ in 0..500 {
        stopped = registry
            .health_of(&group)
            .is_some_and(|health| health.fatal.is_some());
        if stopped {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        stopped,
        "a replica that could not apply an entry kept running"
    );
    assert_eq!(registry.leader(&group), None);
    assert!(!registry.is_leader(&group));

    let metrics = Arc::new(tallyowl_obs::metrics::Registry::new());
    crate::metrics::declare(&metrics);
    let mut sampler = crate::metrics::Sampler::default();
    let newly = sampler.sample(&registry, &metrics);
    assert_eq!(newly, vec!["tablet `t1`".to_string()]);
    assert!(
        sampler.sample(&registry, &metrics).is_empty(),
        "a stopped group is named once, not on every sample"
    );
    let none = tallyowl_obs::metrics::labels(&[]);
    assert_eq!(
        metrics.gauge_value("tallyowl_consensus_groups_stopped_count", &none),
        1
    );
    assert_eq!(
        metrics.gauge_value("tallyowl_consensus_groups_led_count", &none),
        0
    );
    registry.shutdown();
}

#[test]
fn a_replica_that_cannot_write_takes_a_heartbeat_and_no_entries() {
    // An acknowledged entry counts towards the quorum, so a follower that
    // cannot hold a batch must not acknowledge it. A heartbeat is still
    // answered, or a full disk would also start an election.
    let store = a_store_with_no_room("follower-no-room");
    let machine = TabletMachine::new(store).writable_when(Arc::new(|| false));
    let registry = GroupRegistry::new("solo", "127.0.0.1:0", None).expect("a registry");
    let group = GroupKey::Tablet("t1".into());
    registry
        .start(group.clone(), Arc::new(machine), Vec::new(), 0)
        .expect("the group starts");

    let leader = crate::raft::node_id("somebody");
    let vote = openraft::Vote::new_committed(1, leader);
    let heartbeat = openraft::raft::AppendEntriesRequest {
        vote,
        prev_log_id: None,
        entries: Vec::new(),
        leader_commit: None,
    };
    registry
        .deliver_append(&group, heartbeat)
        .expect("a heartbeat is answered");

    let with_an_entry = openraft::raft::AppendEntriesRequest {
        vote,
        prev_log_id: None,
        entries: vec![openraft::Entry {
            log_id: openraft::LogId::new(openraft::CommittedLeaderId::new(1, leader), 0),
            payload: openraft::EntryPayload::Blank,
        }],
        leader_commit: None,
    };
    let refused = registry
        .deliver_append(&group, with_an_entry)
        .expect_err("a replica that cannot write acknowledged an entry");
    assert!(
        refused.message.contains("cannot write"),
        "{}",
        refused.message
    );
    registry.shutdown();
}

// ---------------------------------------------------------------------------
// Bounds on what one peer can cost.
// ---------------------------------------------------------------------------

#[test]
fn an_append_is_bounded_by_its_bytes_and_still_carries_one_entry() {
    use openraft::RaftLogReader;

    let (registry, group) = one_voter(Arc::new(Faulty::default()));
    // Incompressible, so the size on the wire is the size here.
    let big: Vec<u8> = (0..1_500_000u32)
        .map(|n| (n.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    for _ in 0..6 {
        registry.propose(&group, big.clone()).expect("a write");
    }
    let mut storage = registry.storage_of(&group).expect("storage");
    let reading = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime");

    let every = reading
        .block_on(storage.try_get_log_entries(0..1_000))
        .expect("the log reads");
    let bounded = reading
        .block_on(storage.limited_get_log_entries(0, 1_000))
        .expect("the log reads");
    assert!(every.len() >= 6);
    assert!(
        bounded.len() < every.len(),
        "one append would carry the whole log: {} entries",
        bounded.len()
    );
    assert!(!bounded.is_empty(), "the trait requires one entry at least");

    // An entry larger than the budget still travels, alone.
    let last = every.len() as u64 - 1;
    let alone = reading
        .block_on(storage.limited_get_log_entries(last, last + 1))
        .expect("the log reads");
    assert_eq!(alone.len(), 1);
    registry.shutdown();
}

#[test]
fn a_late_failure_does_not_throw_away_the_connection_that_replaced_it() {
    use crate::raft::network::{PeerConnections, MAX_WAITING_FOR_ONE_PEER};

    let connections = PeerConnections::new();
    let first = connections.to("127.0.0.1:1");
    connections.forget("127.0.0.1:1", &first);
    assert_eq!(connections.open_count(), 0);

    // A call that was abandoned long ago fails now. Another call has opened a
    // fresh connection since, and it is not the one that failed.
    let second = connections.to("127.0.0.1:1");
    connections.forget("127.0.0.1:1", &first);
    assert_eq!(
        connections.open_count(),
        1,
        "the working connection was dropped for an old one's failure"
    );
    assert!(Arc::ptr_eq(&second, &connections.to("127.0.0.1:1")));

    // A peer that stops answering holds a few threads and no more.
    let peer = connections.peer("127.0.0.1:1");
    let places: Vec<_> = (0..MAX_WAITING_FOR_ONE_PEER)
        .map(|_| peer.wait().expect("a place"))
        .collect();
    assert!(peer.wait().is_none(), "a dark peer can take every thread");
    drop(places);
    assert!(
        peer.wait().is_some(),
        "a place that was given back is not free"
    );
}
