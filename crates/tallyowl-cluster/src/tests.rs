//! What Phase 7 has to hold, as tests.
//!
//! The rules here are the ones that would be expensive to discover in
//! production: a policy that acknowledged an uncommitted write, a query that
//! answered smaller than the truth without saying so, a movement that published
//! before its copy was proved, and a recovery that lost data quietly.
//!
//! The consensus tests in `consensus` run real openraft groups over the real
//! CSIL transport, on real sockets, with durable storage. D27 measured the
//! library against in-memory storage and an in-process network and named that
//! gap; these close the part of it that does not need more machines.

use std::collections::BTreeMap;

use crate::controller::{choose_voters, default_policy, split_point, Controller, Mode, TabletLoad};
use crate::directory::{Directory, DirectoryCache, DirectoryCommand};
use crate::health::{diagnose, HealthReport, HealthWatch, SlowCause, SlowPolicy};
use crate::movement::{digest, Movement, SegmentCopy, Stage};
use crate::query::{merge, plan, Consistency, Partial, PartialKind, Plan};
use crate::recovery::{
    clear_degraded, overlaps_degraded, snapshot_digest, take_snapshot, unsafe_recover,
    verify_snapshot, Recovery, UnsafeRequest,
};
use crate::routing::{check_fence, route_write, shard_of, Affinity, Fence, RouteCache};
use crate::simulate::{controller_outage, measure, multi_cell, one_cell};
use crate::topology::{
    ControllerCommand, Member, MemberRole, NodeState, ReceiptPolicy, TabletState, Topology,
    TopologyError, VIRTUAL_SHARDS,
};

mod consensus;
mod operator;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn three_node_cell() -> Topology {
    let mut topology = Topology::new();
    for (index, name) in ["storage-a", "storage-b", "storage-c"].iter().enumerate() {
        topology
            .apply(&ControllerCommand::RegisterNode {
                node: name.to_string(),
                address: format!("127.0.0.1:520{index}"),
                region: "west".into(),
                domain: format!("rack-{index}"),
            })
            .expect("a node registers");
    }
    topology
        .apply(&ControllerCommand::RegisterCell {
            cell: "west-1".into(),
            region: "west".into(),
            controllers: vec![
                Member::voter("storage-a", "127.0.0.1:5200").in_region("west"),
                Member::voter("storage-b", "127.0.0.1:5201").in_region("west"),
                Member::voter("storage-c", "127.0.0.1:5202").in_region("west"),
            ],
        })
        .expect("a cell registers");
    topology
}

fn three_voters() -> Vec<Member> {
    vec![
        Member::voter("storage-a", "127.0.0.1:5200")
            .in_region("west")
            .in_domain("rack-0"),
        Member::voter("storage-b", "127.0.0.1:5201")
            .in_region("west")
            .in_domain("rack-1"),
        Member::voter("storage-c", "127.0.0.1:5202")
            .in_region("west")
            .in_domain("rack-2"),
    ]
}

fn with_one_tablet() -> Topology {
    let mut topology = three_node_cell();
    topology
        .apply(&ControllerCommand::CreateTablet {
            tablet: "t1".into(),
            cell: "west-1".into(),
            region: "west".into(),
            shard_start: 0,
            shard_end: VIRTUAL_SHARDS,
            members: three_voters(),
            receipt_policy: ReceiptPolicy::LocalQuorum,
        })
        .expect("a tablet is created");
    topology
}

// ---------------------------------------------------------------------------
// The rule with no exception
// ---------------------------------------------------------------------------

#[test]
fn local_one_is_refused_on_a_tablet_with_more_than_one_voter() {
    // `AGENTS.md`: "Never acknowledge an uncommitted entry in a multi-voter
    // group. `local-one` is therefore legal only for a single-voter tablet."
    let mut topology = three_node_cell();
    let failure = topology
        .apply(&ControllerCommand::CreateTablet {
            tablet: "t1".into(),
            cell: "west-1".into(),
            region: "west".into(),
            shard_start: 0,
            shard_end: VIRTUAL_SHARDS,
            members: three_voters(),
            receipt_policy: ReceiptPolicy::LocalOne,
        })
        .expect_err("three voters and local-one cannot go together");
    assert!(matches!(failure, TopologyError::Refused(_)));
    assert!(
        failure.to_string().contains("local-quorum"),
        "the refusal has to say what to use instead: {failure}"
    );
}

#[test]
fn a_policy_change_to_local_one_on_a_multi_voter_tablet_is_refused_and_the_policy_does_not_move() {
    // Refusing and then quietly leaving the tablet in some other state would be
    // worse than either. The policy must be exactly what it was.
    let mut topology = with_one_tablet();
    let failure = topology
        .apply(&ControllerCommand::SetReceiptPolicy {
            tablet: "t1".into(),
            policy: ReceiptPolicy::LocalOne,
        })
        .expect_err("the change is refused");
    assert!(matches!(failure, TopologyError::Refused(_)));
    assert_eq!(
        topology.tablet("t1").unwrap().receipt_policy,
        ReceiptPolicy::LocalQuorum
    );
}

#[test]
fn local_one_is_legal_on_a_single_voter_tablet_which_is_the_home_profile() {
    let mut topology = Topology::new();
    topology
        .apply(&ControllerCommand::RegisterNode {
            node: "home".into(),
            address: "127.0.0.1:5200".into(),
            region: "home".into(),
            domain: "default".into(),
        })
        .unwrap();
    topology
        .apply(&ControllerCommand::CreateTablet {
            tablet: "home".into(),
            cell: "home".into(),
            region: "home".into(),
            shard_start: 0,
            shard_end: VIRTUAL_SHARDS,
            members: vec![Member::voter("home", "127.0.0.1:5200")],
            receipt_policy: ReceiptPolicy::LocalOne,
        })
        .expect("one voter and local-one is the home profile");
    assert_eq!(default_policy(1), ReceiptPolicy::LocalOne);
    assert_eq!(default_policy(3), ReceiptPolicy::LocalQuorum);
}

#[test]
fn removing_a_voter_that_would_break_the_policy_is_refused_before_it_happens() {
    // Going from two voters to one under `local-quorum` is legal. Going to zero
    // is not, and the refusal has to happen before the member is removed.
    let mut topology = with_one_tablet();
    for node in ["storage-b", "storage-c"] {
        topology
            .apply(&ControllerCommand::RemoveReplica {
                tablet: "t1".into(),
                node: node.into(),
            })
            .expect("two removals leave one voter");
    }
    let failure = topology
        .apply(&ControllerCommand::RemoveReplica {
            tablet: "t1".into(),
            node: "storage-a".into(),
        })
        .expect_err("the last voter cannot be removed");
    assert!(failure.to_string().contains("no voter"));
    assert_eq!(topology.tablet("t1").unwrap().voter_count(), 1);
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

#[test]
fn a_virtual_shard_has_exactly_one_owner() {
    let mut topology = with_one_tablet();
    let failure = topology
        .apply(&ControllerCommand::CreateTablet {
            tablet: "t2".into(),
            cell: "west-1".into(),
            region: "west".into(),
            shard_start: 100,
            shard_end: 200,
            members: three_voters(),
            receipt_policy: ReceiptPolicy::LocalQuorum,
        })
        .expect_err("that range already has an owner");
    assert!(failure.to_string().contains("t1"));
}

#[test]
fn a_split_leaves_both_sides_holding_shards_and_covers_the_whole_range() {
    let mut topology = with_one_tablet();
    let at = split_point(0, VIRTUAL_SHARDS).expect("a range of 4096 splits");
    topology
        .apply(&ControllerCommand::SplitTablet {
            tablet: "t1".into(),
            at_shard: at,
            right: "t1-b".into(),
        })
        .expect("the split applies");

    let left = topology.tablet("t1").unwrap();
    let right = topology.tablet("t1-b").unwrap();
    assert_eq!(left.shard_start, 0);
    assert_eq!(left.shard_end, at);
    assert_eq!(right.shard_start, at);
    assert_eq!(right.shard_end, VIRTUAL_SHARDS);

    // No shard lost an owner and none gained a second one.
    for shard in [0, at - 1, at, VIRTUAL_SHARDS - 1] {
        assert!(
            topology.owner_of(shard).is_some(),
            "shard {shard} has no owner after the split"
        );
    }
}

#[test]
fn a_split_at_the_edge_of_a_range_is_refused_rather_than_making_an_empty_tablet() {
    let mut topology = with_one_tablet();
    let failure = topology
        .apply(&ControllerCommand::SplitTablet {
            tablet: "t1".into(),
            at_shard: 0,
            right: "t1-b".into(),
        })
        .expect_err("a split at the start makes an empty left side");
    assert!(failure.to_string().contains("at least one shard"));
    assert!(topology.tablet("t1-b").is_none());
}

#[test]
fn two_tablets_that_are_not_adjacent_do_not_merge() {
    let mut topology = with_one_tablet();
    topology
        .apply(&ControllerCommand::SplitTablet {
            tablet: "t1".into(),
            at_shard: 1000,
            right: "t2".into(),
        })
        .unwrap();
    topology
        .apply(&ControllerCommand::SplitTablet {
            tablet: "t2".into(),
            at_shard: 2000,
            right: "t3".into(),
        })
        .unwrap();
    // t1 ends at 1000, t3 starts at 2000. Merging them would leave 1000..2000
    // with no owner.
    let failure = topology
        .apply(&ControllerCommand::MergeTablets {
            left: "t1".into(),
            right: "t3".into(),
        })
        .expect_err("a hole is not a merge");
    assert!(failure.to_string().contains("hole"));
}

#[test]
fn a_merge_joins_two_adjacent_ranges_and_the_right_hand_tablet_is_gone() {
    let mut topology = with_one_tablet();
    topology
        .apply(&ControllerCommand::SplitTablet {
            tablet: "t1".into(),
            at_shard: 2048,
            right: "t2".into(),
        })
        .unwrap();
    topology
        .apply(&ControllerCommand::MergeTablets {
            left: "t1".into(),
            right: "t2".into(),
        })
        .expect("adjacent tablets on the same nodes merge");
    assert_eq!(topology.tablet("t1").unwrap().shard_end, VIRTUAL_SHARDS);
    assert!(topology.tablet("t2").is_none());
}

#[test]
fn a_merge_that_swallows_a_degraded_range_keeps_the_mark() {
    // A mark that disappeared because two tablets became one would hide a loss
    // nobody accepted.
    let mut topology = with_one_tablet();
    topology
        .apply(&ControllerCommand::SplitTablet {
            tablet: "t1".into(),
            at_shard: 2048,
            right: "t2".into(),
        })
        .unwrap();
    let mark = crate::topology::DegradedMark {
        since: 10,
        range_start: 0,
        range_end: 100,
        survivor_watermark: 7,
        reason: "two voters were destroyed".into(),
        audit_id: "unsafe-t2-10".into(),
    };
    topology
        .apply(&ControllerCommand::MarkDegraded {
            tablet: "t2".into(),
            mark: mark.clone(),
        })
        .unwrap();
    topology
        .apply(&ControllerCommand::MergeTablets {
            left: "t1".into(),
            right: "t2".into(),
        })
        .unwrap();
    assert_eq!(topology.tablet("t1").unwrap().degraded, Some(mark));
}

#[test]
fn voters_spread_across_failure_domains_and_say_so_when_they_cannot() {
    let mut topology = three_node_cell();
    let (chosen, doubled) = choose_voters(&topology, 3).expect("three nodes, three domains");
    assert_eq!(chosen.len(), 3);
    let domains: std::collections::BTreeSet<&String> = chosen.iter().map(|m| &m.domain).collect();
    assert_eq!(domains.len(), 3, "each voter is in its own domain");
    assert!(doubled.is_none());

    // Now put every node in one domain. The placement still happens and it says
    // what it cost.
    for name in ["storage-a", "storage-b", "storage-c"] {
        topology
            .apply(&ControllerCommand::RegisterNode {
                node: name.into(),
                address: "127.0.0.1:5200".into(),
                region: "west".into(),
                domain: "one-rack".into(),
            })
            .unwrap();
    }
    let (chosen, doubled) = choose_voters(&topology, 3).expect("it still places");
    assert_eq!(chosen.len(), 3);
    let warning = doubled.expect("a single-domain placement has to be reported");
    assert!(warning.contains("quorum"), "{warning}");
}

#[test]
fn a_cell_with_too_few_nodes_refuses_rather_than_placing_a_smaller_group() {
    let mut topology = Topology::new();
    topology
        .apply(&ControllerCommand::RegisterNode {
            node: "only".into(),
            address: "127.0.0.1:5200".into(),
            region: "west".into(),
            domain: "rack-0".into(),
        })
        .unwrap();
    let failure = choose_voters(&topology, 3).expect_err("one node cannot hold three voters");
    assert!(failure.message.contains("Add nodes"));
}

// ---------------------------------------------------------------------------
// Routing and fencing
// ---------------------------------------------------------------------------

#[test]
fn one_project_and_one_affinity_key_always_land_in_one_shard() {
    let project = [7u8; 16];
    let trace = b"trace-abcdef";
    let first = shard_of(&project, trace);
    for _ in 0..100 {
        assert_eq!(shard_of(&project, trace), first);
    }
    assert!(first < VIRTUAL_SHARDS);
    // A different project is a different bucket for the same key, which is what
    // keeps two tenants from sharing a hot shard by accident.
    assert_ne!(first, shard_of(&[8u8; 16], trace));
}

#[test]
fn a_span_routes_by_its_trace_so_one_tablet_holds_a_whole_trace() {
    // D16 and STORAGE.md section 6. Two spans of one trace must land together,
    // or assembling a trace would fan out across the cell.
    let project = [1u8; 16];
    let trace = b"one-trace";
    let first = crate::routing::affinity_key(
        Affinity::Trace,
        Some(trace),
        None,
        None,
        b"event-one-aaaaaa",
    );
    let second = crate::routing::affinity_key(
        Affinity::Trace,
        Some(trace),
        None,
        None,
        b"event-two-bbbbbb",
    );
    assert_eq!(shard_of(&project, first), shard_of(&project, second));
}

#[test]
fn an_item_with_no_affinity_value_falls_back_to_its_event_id_rather_than_one_bucket() {
    // Falling back to a constant would put every such item in one shard, which
    // is the hot-shard failure. It falls back to the event ID, which spreads.
    let project = [1u8; 16];
    let a = crate::routing::affinity_key(Affinity::Session, None, None, None, b"event-aaaa");
    let b = crate::routing::affinity_key(Affinity::Session, None, None, None, b"event-bbbb");
    assert_ne!(a, b);
    let _ = shard_of(&project, a);
}

#[test]
fn a_write_with_a_stale_epoch_is_fenced_and_told_which_region_owns_the_write_now() {
    let mut topology = with_one_tablet();
    topology
        .apply(&ControllerCommand::AddReplica {
            tablet: "t1".into(),
            member: Member::learner("storage-d", "127.0.0.1:5203").in_region("east"),
        })
        .unwrap();
    let before = topology.tablet("t1").unwrap().epoch;
    topology
        .apply(&ControllerCommand::FailOverRegion {
            tablet: "t1".into(),
            onto_region: "east".into(),
        })
        .expect("a replica exists in the target region");
    let after = topology.tablet("t1").unwrap().epoch;
    assert!(after > before, "a fenced failover raises the epoch");

    match check_fence(&topology, "t1", topology.generation, before, None) {
        Fence::Fenced {
            current_epoch,
            write_region,
            ..
        } => {
            assert_eq!(current_epoch, after);
            assert_eq!(write_region, "east");
        }
        other => panic!("a write from the old epoch must be fenced, not {other:?}"),
    }
}

#[test]
fn a_failover_onto_a_region_with_no_replica_is_refused() {
    let mut topology = with_one_tablet();
    let failure = topology
        .apply(&ControllerCommand::FailOverRegion {
            tablet: "t1".into(),
            onto_region: "east".into(),
        })
        .expect_err("no replica lives there");
    assert!(failure.to_string().contains("Add a replica"));
    assert_eq!(topology.tablet("t1").unwrap().write_region, "west");
}

#[test]
fn a_write_from_the_wrong_region_is_fenced_even_at_the_current_epoch() {
    // Two writers in two regions is the failure a fence exists to stop, and an
    // epoch alone does not catch a writer that never saw the old one.
    let topology = with_one_tablet();
    let tablet = topology.tablet("t1").unwrap();
    match check_fence(
        &topology,
        "t1",
        topology.generation,
        tablet.epoch,
        Some("east"),
    ) {
        Fence::Fenced { write_region, .. } => assert_eq!(write_region, "west"),
        other => panic!("a write from another region must be fenced, not {other:?}"),
    }
}

#[test]
fn a_route_cache_forgets_everything_when_the_generation_moves() {
    // Expiring only what it was told about would keep a stale entry for a shard
    // nobody happened to mention.
    let topology = with_one_tablet();
    let cache = RouteCache::new();
    let route = route_write(&topology, &[1u8; 16], b"key", |_| Some("storage-a".into()))
        .expect("the tablet owns every shard");
    cache.put(route.clone());
    assert_eq!(cache.len(), 1);

    cache.invalidate_before(route.generation + 1);
    assert!(cache.is_empty(), "a newer generation drops the whole cache");
}

#[test]
fn a_draining_tablet_answers_reads_and_refuses_writes() {
    let mut topology = with_one_tablet();
    topology
        .apply(&ControllerCommand::SetTabletState {
            tablet: "t1".into(),
            state: TabletState::Draining,
        })
        .unwrap();
    assert!(!topology.tablet("t1").unwrap().writable());
    assert_eq!(
        topology.readable_tablets().len(),
        1,
        "a draining tablet stays in the read fan-out"
    );
    let failure = route_write(&topology, &[1u8; 16], b"key", |_| None)
        .expect_err("a draining tablet takes no write");
    assert!(failure.to_string().contains("not accepting writes"));
}

// ---------------------------------------------------------------------------
// The slow node
// ---------------------------------------------------------------------------

fn healthy_report(node: &str, at: i64) -> HealthReport {
    HealthReport {
        node: node.into(),
        reported_at: at,
        append_latency_us: 1_000,
        fsync_latency_us: 2_000,
        queue_depth: 2,
        accepted_bytes_each_second: 1_000_000,
        device_service_time_us: 500,
        peer_round_trip_us: 800,
        writable: true,
        free_bytes: 500 * 1024 * 1024 * 1024,
        ..HealthReport::default()
    }
}

#[test]
fn a_node_that_is_slower_than_the_group_for_long_enough_is_slow_and_the_others_are_not() {
    let mut watch = HealthWatch::new(SlowPolicy {
        factor: 4,
        duration_ms: 1_000,
        action: crate::health::SlowAction::Alert,
    });
    watch.observe(healthy_report("a", 0));
    watch.observe(healthy_report("b", 0));
    let mut sick = healthy_report("c", 0);
    sick.append_latency_us = 60_000;
    sick.fsync_latency_us = 90_000;
    watch.observe(sick);

    // Not yet: it has not been slow for long enough. A brief spike is not a
    // sick node.
    let early = watch.assess(500);
    assert!(early.iter().all(|c| c.state == NodeState::Healthy));

    let later = watch.assess(2_000);
    let c = later.iter().find(|c| c.node == "c").unwrap();
    assert_eq!(c.state, NodeState::Slow);
    assert!(later
        .iter()
        .filter(|c| c.node != "c")
        .all(|c| c.state == NodeState::Healthy));
    // The evidence travels with the verdict.
    assert_eq!(c.group_median_append_latency_us, 1_000);
    assert_eq!(c.append_latency_us, 60_000);
    assert!(c.action_taken.is_some());
}

#[test]
fn one_node_on_its_own_is_never_called_slow() {
    // A single-node installation compared against its own median would be
    // permanently sick.
    let mut watch = HealthWatch::new(SlowPolicy::default());
    let mut alone = healthy_report("only", 0);
    alone.append_latency_us = 9_000_000;
    watch.observe(alone);
    let conditions = watch.assess(10_000_000);
    assert_eq!(conditions[0].state, NodeState::Healthy);
}

#[test]
fn a_node_that_recovers_stops_being_slow() {
    let mut watch = HealthWatch::new(SlowPolicy {
        factor: 4,
        duration_ms: 1_000,
        action: crate::health::SlowAction::Alert,
    });
    watch.observe(healthy_report("a", 0));
    watch.observe(healthy_report("b", 0));
    let mut sick = healthy_report("c", 0);
    sick.append_latency_us = 60_000;
    watch.observe(sick);
    assert_eq!(
        watch
            .assess(2_000)
            .iter()
            .find(|c| c.node == "c")
            .unwrap()
            .state,
        NodeState::Slow
    );

    watch.observe(healthy_report("c", 3_000));
    assert_eq!(
        watch
            .assess(4_000)
            .iter()
            .find(|c| c.node == "c")
            .unwrap()
            .state,
        NodeState::Healthy
    );
}

#[test]
fn each_cause_is_reported_from_its_own_evidence_and_the_most_specific_one_wins() {
    let base = healthy_report("c", 0);

    let mut errors = base.clone();
    errors.device_errors = 3;
    errors.queue_depth = 100;
    errors.device_service_time_us = 90_000;
    assert_eq!(
        diagnose(&errors, 1_000_000, 800, 1_000),
        SlowCause::StorageErrors,
        "a device returning errors is also saturated, and the specific answer wins"
    );

    let mut saturated = base.clone();
    saturated.queue_depth = 64;
    saturated.device_service_time_us = 40_000;
    assert_eq!(
        diagnose(&saturated, 1_000_000, 800, 1_000),
        SlowCause::StorageSaturated
    );

    let mut compaction = base.clone();
    compaction.compaction_backlog_bytes = 8 * 1024 * 1024 * 1024;
    assert_eq!(
        diagnose(&compaction, 1_000_000, 800, 1_000),
        SlowCause::CompactionPressure
    );

    let mut memory = base.clone();
    memory.memory_reclaim_events = 5_000;
    assert_eq!(
        diagnose(&memory, 1_000_000, 800, 1_000),
        SlowCause::MemoryPressure
    );

    let mut volume = base.clone();
    volume.accepted_bytes_each_second = 10_000_000;
    assert_eq!(
        diagnose(&volume, 1_000_000, 800, 1_000),
        SlowCause::WriteVolume,
        "a node taking much more work than its peers explains its own latency"
    );

    let mut network = base.clone();
    network.peer_round_trip_us = 50_000;
    assert_eq!(
        diagnose(&network, 1_000_000, 800, 1_000),
        SlowCause::NetworkLatency
    );
}

#[test]
fn a_cause_that_cannot_be_established_is_reported_as_unknown_rather_than_guessed() {
    // FAILURE_MODES.md section 6.1: "`unknown` is a valid answer and must be
    // reported as one." A guessed cause sends an operator down a wrong path.
    let quiet = HealthReport {
        node: "c".into(),
        reported_at: 0,
        writable: true,
        ..HealthReport::default()
    };
    assert_eq!(diagnose(&quiet, 0, 0, 0), SlowCause::Unknown);
}

#[test]
fn a_voter_that_cannot_make_a_write_durable_becomes_read_only_and_not_slow() {
    // The disk-exhausted voter from FAILURE_MODES.md section 6. It is a
    // different failure and it needs a different answer: the quorum continues
    // without it rather than waiting for it.
    let mut watch = HealthWatch::new(SlowPolicy::default());
    watch.observe(healthy_report("a", 0));
    watch.observe(healthy_report("b", 0));
    let mut full = healthy_report("c", 0);
    full.writable = false;
    full.free_bytes = 0;
    watch.observe(full);

    let conditions = watch.assess(1_000);
    let c = conditions.iter().find(|c| c.node == "c").unwrap();
    assert_eq!(c.state, NodeState::ReadOnly);
    assert!(c
        .action_taken
        .as_ref()
        .unwrap()
        .contains("quorum continues without it"));
}

#[test]
fn a_health_condition_becomes_a_replicated_command_that_the_topology_takes() {
    let mut topology = with_one_tablet();
    let mut watch = HealthWatch::new(SlowPolicy::default());
    let mut full = healthy_report("storage-c", 0);
    full.writable = false;
    watch.observe(full);
    let conditions = watch.assess(1_000);
    for command in HealthWatch::commands(&conditions) {
        topology.apply(&command).expect("the topology takes it");
    }
    assert_eq!(
        topology.node("storage-c").unwrap().state,
        NodeState::ReadOnly
    );
}

// ---------------------------------------------------------------------------
// Distributed query
// ---------------------------------------------------------------------------

fn a_plan(allow_partial: bool) -> Plan {
    Plan {
        project_id: [1u8; 16],
        range_start: 0,
        range_end: 1_000,
        basis: tallyowl_store::TimeBasis::OccurredAt,
        kind: PartialKind::Count,
        event_name: None,
        consistency: Consistency::Committed,
        require_watermark: 0,
        allow_partial,
        tablets: vec!["t1".into(), "t2".into()],
        generation: 1,
    }
}

#[test]
fn a_missing_tablet_refuses_by_default_and_the_refusal_names_it() {
    // QUERY.md section 9: "A missing tablet causes a typed incomplete-result
    // error by default. A caller must request partial mode explicitly."
    let plan = a_plan(false);
    let parts = vec![
        Partial {
            count: 40,
            ..Partial::for_tablet("t1")
        },
        Partial::unavailable("t2", 0, 1_000),
    ];
    let failure = merge(&plan, parts).expect_err("a smaller answer is not returned quietly");
    assert_eq!(
        failure.code,
        tallyowl_obs::error::ErrorCode::IncompleteResult
    );
    assert!(failure.message.contains("t2"), "{}", failure.message);
}

#[test]
fn a_partial_result_can_never_mark_itself_complete() {
    let plan = a_plan(true);
    let parts = vec![
        Partial {
            count: 40,
            ..Partial::for_tablet("t1")
        },
        Partial::unavailable("t2", 0, 1_000),
    ];
    let merged = merge(&plan, parts).expect("partial mode was asked for");
    assert!(!merged.complete);
    assert_eq!(merged.count, 40);
    assert_eq!(merged.missing.len(), 1);
    assert_eq!(merged.missing[0].tablet, "t2");
    assert!(merged.warnings.iter().any(|w| w.contains("partial answer")));
}

#[test]
fn one_incomplete_part_makes_the_whole_incomplete_whatever_order_the_parts_arrive_in() {
    let plan = a_plan(true);
    let complete = Partial {
        count: 1,
        ..Partial::for_tablet("t1")
    };
    let incomplete = Partial::unavailable("t2", 0, 1_000);
    let forwards = merge(&plan, vec![complete.clone(), incomplete.clone()]).unwrap();
    let backwards = merge(&plan, vec![incomplete, complete]).unwrap();
    assert!(!forwards.complete);
    assert!(!backwards.complete);
}

#[test]
fn the_watermark_of_a_merged_answer_is_the_smallest_and_never_the_largest() {
    // A result is only as current as its least current part. Reporting the
    // largest would let a dashboard believe it had seen a write one tablet had
    // not applied.
    let plan = a_plan(false);
    let parts = vec![
        Partial {
            commit_watermark: 900,
            freshness_ms: 10,
            ..Partial::for_tablet("t1")
        },
        Partial {
            commit_watermark: 400,
            freshness_ms: 250,
            ..Partial::for_tablet("t2")
        },
    ];
    let merged = merge(&plan, parts).unwrap();
    assert_eq!(merged.commit_watermark, 400);
    assert_eq!(merged.freshness_ms, 250, "and the staleness is the largest");
}

#[test]
fn trend_buckets_merge_by_addition_on_a_shared_bucket_start() {
    let mut plan = a_plan(false);
    plan.kind = PartialKind::Trend { bucket_ms: 100 };
    let parts = vec![
        Partial {
            buckets: BTreeMap::from([(0, 3), (100, 5)]),
            count: 8,
            ..Partial::for_tablet("t1")
        },
        Partial {
            buckets: BTreeMap::from([(100, 2), (200, 7)]),
            count: 9,
            ..Partial::for_tablet("t2")
        },
    ];
    let merged = merge(&plan, parts).unwrap();
    assert_eq!(merged.buckets, vec![(0, 3), (100, 7), (200, 7)]);
    assert_eq!(merged.count, 17);
}

#[test]
fn an_aggregate_never_uses_rows_a_tablet_sent_and_says_that_it_did_not() {
    // QUERY.md section 10: the coordinator never pulls raw rows to compute an
    // aggregate. Using them anyway would give the right number for the wrong
    // reason, and the design would rot quietly.
    let mut plan = a_plan(false);
    plan.tablets = vec!["t1".into()];
    let row = tallyowl_store::row::EventRow::new([9u8; 16], "event", "checkout", 10);
    let parts = vec![Partial {
        count: 1,
        rows: vec![row],
        ..Partial::for_tablet("t1")
    }];
    let merged = merge(&plan, parts).unwrap();
    assert!(merged.rows.is_empty());
    assert!(merged
        .warnings
        .iter()
        .any(|w| w.contains("does not move rows")));
}

#[test]
fn an_exact_lookup_across_tablets_returns_every_row_and_never_a_sample() {
    // `AGENTS.md`: "Do not silently drop, coalesce, or reject a value because
    // it has high cardinality."
    let mut plan = a_plan(false);
    plan.kind = PartialKind::Lookup {
        column: "request_id".into(),
        value: b"r-1".to_vec(),
    };
    let parts = vec![
        Partial {
            rows: vec![tallyowl_store::row::EventRow::new(
                [1u8; 16], "span", "a", 1,
            )],
            ..Partial::for_tablet("t1")
        },
        Partial {
            rows: vec![
                tallyowl_store::row::EventRow::new([2u8; 16], "span", "b", 2),
                tallyowl_store::row::EventRow::new([3u8; 16], "span", "c", 3),
            ],
            ..Partial::for_tablet("t2")
        },
    ];
    let merged = merge(&plan, parts).unwrap();
    assert_eq!(merged.rows.len(), 3, "every matching row from every tablet");
}

#[test]
fn a_row_query_holds_its_bound_after_the_merge_as_well_as_at_each_tablet() {
    let mut plan = a_plan(false);
    plan.kind = PartialKind::Rows { max_rows: 2 };
    let parts = vec![
        Partial {
            rows: vec![
                tallyowl_store::row::EventRow::new([1u8; 16], "event", "a", 1),
                tallyowl_store::row::EventRow::new([2u8; 16], "event", "b", 2),
            ],
            ..Partial::for_tablet("t1")
        },
        Partial {
            rows: vec![tallyowl_store::row::EventRow::new(
                [3u8; 16], "event", "c", 3,
            )],
            ..Partial::for_tablet("t2")
        },
    ];
    assert_eq!(merge(&plan, parts).unwrap().rows.len(), 2);
}

#[test]
fn a_fan_out_past_the_limit_is_refused_and_the_refusal_names_both_numbers() {
    let mut topology = three_node_cell();
    for index in 0..8u64 {
        topology
            .apply(&ControllerCommand::CreateTablet {
                tablet: format!("t{index}"),
                cell: "west-1".into(),
                region: "west".into(),
                shard_start: index * 100,
                shard_end: (index + 1) * 100,
                members: three_voters(),
                receipt_policy: ReceiptPolicy::LocalQuorum,
            })
            .unwrap();
    }
    let failure = plan(
        &topology,
        [1u8; 16],
        0,
        10,
        tallyowl_store::TimeBasis::OccurredAt,
        PartialKind::Count,
        None,
        Consistency::Committed,
        false,
        4,
    )
    .expect_err("eight tablets is past a limit of four");
    assert!(failure.message.contains('8') && failure.message.contains('4'));
    assert!(failure.message.contains("query.maxFanOut"));
}

#[test]
fn a_committed_read_refuses_a_replica_that_does_not_vote_or_is_behind() {
    let mut plan = a_plan(false);
    plan.require_watermark = 500;
    assert!(crate::query::replica_satisfies(&plan, false, 900, 0).is_err());
    assert!(crate::query::replica_satisfies(&plan, true, 400, 0).is_err());
    assert!(crate::query::replica_satisfies(&plan, true, 500, 0).is_ok());
}

#[test]
fn a_bounded_stale_read_accepts_a_replica_inside_its_tolerance_and_refuses_one_outside() {
    let mut plan = a_plan(false);
    plan.consistency = Consistency::BoundedStale {
        max_staleness_ms: 5_000,
    };
    assert!(crate::query::replica_satisfies(&plan, false, 0, 4_000).is_ok());
    assert!(crate::query::replica_satisfies(&plan, false, 0, 6_000).is_err());
}

#[test]
fn a_degraded_range_reaches_the_result_and_names_the_tablet() {
    // FAILURE_MODES.md section 6.2: the mark reaches query and explain output.
    let mut plan = a_plan(false);
    plan.tablets = vec!["t1".into()];
    let parts = vec![Partial {
        count: 5,
        degraded: true,
        ..Partial::for_tablet("t1")
    }];
    let merged = merge(&plan, parts).unwrap();
    assert!(merged.degraded);
    assert!(merged
        .warnings
        .iter()
        .any(|w| w.contains("t1") && w.contains("unsafe recovery")));
}

// ---------------------------------------------------------------------------
// The global directory
// ---------------------------------------------------------------------------

#[test]
fn a_cell_keeps_working_when_the_global_directory_is_gone() {
    // CELLS.md section 9. The four operations that stop are named; everything
    // else continues from the cached assignments.
    let mut directory = Directory::new();
    directory
        .apply(&DirectoryCommand::RegisterCell {
            cell: "west-1".into(),
            region: "west".into(),
        })
        .unwrap();
    directory
        .apply(&DirectoryCommand::AssignProject {
            project_id: [1u8; 16],
            cells: vec!["west-1".into()],
        })
        .unwrap();

    let cache = DirectoryCache::new();
    cache.refresh(directory, 1_000);
    assert_eq!(cache.write_cell(&[1u8; 16]).as_deref(), Some("west-1"));

    cache.unreachable();
    // The write path is unaffected: the cell still answers from what it holds.
    assert_eq!(cache.write_cell(&[1u8; 16]).as_deref(), Some("west-1"));
    // Placement changes stop, and the refusal says the data path is fine.
    let failure = cache.may_change_placement().expect_err("placement stops");
    assert!(failure.to_string().contains("not affected"));

    let topology = with_one_tablet();
    let effect = crate::simulate::directory_outage(&topology, &[]);
    assert_eq!(effect.tablets_still_writable, 1);
    assert!(effect.queries_still_planned);
}

#[test]
fn a_project_cannot_be_assigned_to_a_cell_that_does_not_exist() {
    let mut directory = Directory::new();
    let failure = directory
        .apply(&DirectoryCommand::AssignProject {
            project_id: [1u8; 16],
            cells: vec!["nowhere".into()],
        })
        .expect_err("an unknown cell is refused");
    assert!(failure.to_string().contains("Register the cell"));
}

#[test]
fn a_project_that_is_moving_still_writes_to_its_source_cell_until_the_move_finishes() {
    // Two writers at once is what this prevents. CELLS.md section 11.
    let mut directory = Directory::new();
    for cell in ["west-1", "east-1"] {
        directory
            .apply(&DirectoryCommand::RegisterCell {
                cell: cell.into(),
                region: cell.into(),
            })
            .unwrap();
    }
    directory
        .apply(&DirectoryCommand::AssignProject {
            project_id: [1u8; 16],
            cells: vec!["west-1".into()],
        })
        .unwrap();
    directory
        .apply(&DirectoryCommand::StartProjectMove {
            project_id: [1u8; 16],
            onto_cell: "east-1".into(),
        })
        .unwrap();
    assert_eq!(
        directory.write_cell(&[1u8; 16]).map(|s| s.as_str()),
        Some("west-1")
    );

    directory
        .apply(&DirectoryCommand::FinishProjectMove {
            project_id: [1u8; 16],
        })
        .unwrap();
    assert_eq!(
        directory.write_cell(&[1u8; 16]).map(|s| s.as_str()),
        Some("east-1")
    );
}

// ---------------------------------------------------------------------------
// Movement
// ---------------------------------------------------------------------------

fn a_movement() -> (Movement, Vec<(SegmentCopy, Vec<u8>)>) {
    let bodies: Vec<Vec<u8>> = (0..3u8).map(|n| vec![n; 4096]).collect();
    let copies: Vec<SegmentCopy> = bodies
        .iter()
        .enumerate()
        .map(|(index, body)| SegmentCopy {
            segment_id: format!("seg-{index}"),
            total_bytes: body.len() as u64,
            digest: digest(body),
        })
        .collect();
    let movement = Movement::plan(
        "t1",
        "storage-a",
        Member::voter("storage-d", "127.0.0.1:5203")
            .in_region("west")
            .in_domain("rack-3"),
        &copies,
        42,
    );
    (movement, copies.into_iter().zip(bodies).collect())
}

#[test]
fn placement_changes_only_after_every_segment_is_proved_and_the_log_caught_up() {
    let (mut movement, parts) = a_movement();
    movement.start();
    assert!(movement.publish().is_err(), "nothing is proved yet");

    for (copy, body) in &parts {
        movement
            .segment_arrived(copy, body)
            .expect("it arrives intact");
    }
    assert_eq!(movement.stage, Stage::CatchingUp);
    assert!(
        movement.publish().is_err(),
        "segments are proved and the log has not caught up"
    );

    movement.caught_up_to(41);
    assert!(movement.publish().is_err(), "one entry short is short");

    movement.caught_up_to(42);
    assert_eq!(movement.stage, Stage::Verified);
    let commands = movement.publish().expect("the copy is proved");
    assert!(commands
        .iter()
        .any(|c| matches!(c, ControllerCommand::MoveReplica { .. })));
}

#[test]
fn a_segment_that_did_not_arrive_intact_stops_the_move_and_leaves_the_source_owning_the_data() {
    let (mut movement, parts) = a_movement();
    movement.start();
    let (copy, body) = &parts[0];
    let mut damaged = body.clone();
    damaged[10] ^= 0xff;

    let failure = movement
        .segment_arrived(copy, &damaged)
        .expect_err("a damaged segment is not installed");
    assert!(failure.message.contains("seg-0"));
    assert!(failure.message.contains("storage-a"), "{}", failure.message);
    assert_eq!(movement.stage, Stage::Stopped);
    assert!(movement.publish().is_err());
}

#[test]
fn a_segment_of_the_wrong_length_is_refused_even_when_nothing_else_looks_wrong() {
    let (mut movement, parts) = a_movement();
    movement.start();
    let (copy, body) = &parts[0];
    let short = &body[..body.len() - 1];
    let mut lying = copy.clone();
    lying.digest = digest(short);
    let failure = movement
        .segment_arrived(&lying, short)
        .expect_err("the length does not match what the source said");
    assert!(failure.message.contains("bytes"));
    assert_eq!(movement.stage, Stage::Stopped);
}

#[test]
fn the_move_applies_to_the_topology_and_the_tablet_keeps_its_voter_count() {
    let mut topology = with_one_tablet();
    let (mut movement, parts) = a_movement();
    movement.start();
    for (copy, body) in &parts {
        movement.segment_arrived(copy, body).unwrap();
    }
    movement.caught_up_to(42);
    for command in movement.publish().unwrap() {
        topology.apply(&command).expect("the topology takes it");
    }
    let tablet = topology.tablet("t1").unwrap();
    assert_eq!(tablet.voter_count(), 3);
    assert!(tablet.member("storage-a").is_none());
    assert!(tablet.member("storage-d").is_some());
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

#[test]
fn restore_is_what_a_permanent_quorum_loss_recommends() {
    assert_eq!(Recovery::recommended(), Recovery::Restore);
}

#[test]
fn a_snapshot_that_does_not_match_its_own_checksum_is_not_restored() {
    let parts = vec![
        ("t1".to_string(), vec![1u8; 100]),
        ("t2".to_string(), vec![2u8; 200]),
    ];
    let snapshot = take_snapshot("s1", 1_000, parts.clone());
    verify_snapshot(&snapshot, &parts).expect("the parts it was built from");

    let mut damaged = parts.clone();
    damaged[0].1[50] ^= 0xff;
    let failure = verify_snapshot(&snapshot, &damaged).expect_err("a damaged part is caught");
    assert!(failure.message.contains("s1"));
}

#[test]
fn two_snapshots_of_the_same_bytes_under_different_names_are_not_the_same_snapshot() {
    let a = snapshot_digest(&[("t1".into(), vec![1u8; 10])]);
    let b = snapshot_digest(&[("t2".into(), vec![1u8; 10])]);
    assert_ne!(a, b);
}

#[test]
fn unsafe_recovery_needs_the_tablet_named_twice() {
    let topology = with_one_tablet();
    let failure = unsafe_recover(
        &topology,
        &UnsafeRequest {
            tablet: "t1".into(),
            confirm_tablet: "t2".into(),
            survivor: "storage-a".into(),
            reason: "two voters were destroyed".into(),
        },
        7,
        (0, 1_000),
        5_000,
    )
    .expect_err("the confirmation does not match");
    assert!(failure.message.contains("can lose an acknowledged write"));
}

#[test]
fn unsafe_recovery_needs_a_reason_because_somebody_reads_it_a_year_later() {
    let topology = with_one_tablet();
    let failure = unsafe_recover(
        &topology,
        &UnsafeRequest {
            tablet: "t1".into(),
            confirm_tablet: "t1".into(),
            survivor: "storage-a".into(),
            reason: "   ".into(),
        },
        7,
        (0, 1_000),
        5_000,
    )
    .expect_err("a blank reason is refused");
    assert!(failure.message.contains("audit record"));
}

#[test]
fn unsafe_recovery_writes_an_audit_record_marks_the_range_and_leaves_one_voter() {
    // All four requirements of FAILURE_MODES.md section 6.2, in one test,
    // because they are one intention and applying some of them is the failure.
    let mut topology = with_one_tablet();
    let outcome = unsafe_recover(
        &topology,
        &UnsafeRequest {
            tablet: "t1".into(),
            confirm_tablet: "t1".into(),
            survivor: "storage-a".into(),
            reason: "the b and c hosts were destroyed with their disks".into(),
        },
        7,
        (0, 1_000),
        5_000,
    )
    .expect("a named survivor and a reason");

    for command in &outcome.commands {
        topology.apply(command).expect("the topology takes it");
    }

    let tablet = topology.tablet("t1").unwrap();
    assert_eq!(tablet.voter_count(), 1);
    assert_eq!(tablet.member("storage-a").unwrap().role, MemberRole::Voter);

    let mark = tablet.degraded.as_ref().expect("the range is marked");
    assert_eq!(mark.survivor_watermark, 7);
    assert_eq!(mark.audit_id, outcome.audit_id);

    let record = topology
        .audit()
        .iter()
        .find(|a| a.audit_id == outcome.audit_id)
        .expect("an audit record exists");
    assert!(record.detail.contains("destroyed"));
    assert!(record.detail.contains("watermark 7"));
}

#[test]
fn a_degraded_mark_never_expires_on_its_own_and_clearing_it_records_who_accepted_the_loss() {
    let mut topology = with_one_tablet();
    let outcome = unsafe_recover(
        &topology,
        &UnsafeRequest {
            tablet: "t1".into(),
            confirm_tablet: "t1".into(),
            survivor: "storage-a".into(),
            reason: "hosts destroyed".into(),
        },
        7,
        (0, 1_000),
        5_000,
    )
    .unwrap();
    for command in &outcome.commands {
        topology.apply(command).unwrap();
    }

    // Nothing here expires it. Only a deliberate clear does.
    assert!(topology.tablet("t1").unwrap().degraded.is_some());
    assert!(clear_degraded(&topology, "t1", "  ", "reviewed", 9_000).is_err());

    let command = clear_degraded(&topology, "t1", "tod", "the loss was reviewed", 9_000).unwrap();
    topology.apply(&command).unwrap();
    assert!(topology.tablet("t1").unwrap().degraded.is_none());
    assert!(topology
        .audit()
        .iter()
        .any(|a| a.detail.contains("tod accepted the loss")));
}

#[test]
fn a_degraded_range_is_half_open_at_both_ends() {
    let mark = crate::topology::DegradedMark {
        since: 0,
        range_start: 100,
        range_end: 200,
        survivor_watermark: 0,
        reason: String::new(),
        audit_id: String::new(),
    };
    assert!(
        !overlaps_degraded(&mark, 0, 100),
        "a query that ends where the mark starts"
    );
    assert!(overlaps_degraded(&mark, 99, 101));
    assert!(overlaps_degraded(&mark, 150, 160));
    assert!(!overlaps_degraded(&mark, 200, 300));
}

// ---------------------------------------------------------------------------
// The controller
// ---------------------------------------------------------------------------

#[test]
fn the_first_release_recommends_rather_than_acts_and_the_decision_is_the_same_either_way() {
    // CELLS.md section 6: "The first releases can use recommendation-only mode
    // until tests prove safe automatic control." The decision path still runs,
    // so it is exercised rather than being code nobody ran.
    let topology = with_one_tablet();
    let load = BTreeMap::from([(
        "t1".to_string(),
        TabletLoad {
            stored_bytes: 200 * 1024 * 1024 * 1024,
            ..TabletLoad::default()
        },
    )]);
    let advising = Controller::new("west-1", "west");
    assert_eq!(advising.mode, Mode::RecommendOnly);
    let recommendations = advising.decide(&topology, &load, &[]);
    assert_eq!(recommendations.len(), 1);
    assert!(recommendations[0].because.contains("split threshold"));
    assert!(
        advising.to_run(&recommendations).is_empty(),
        "recommendation-only runs nothing"
    );

    let acting = advising.clone().with_mode(Mode::Automatic);
    assert_eq!(acting.decide(&topology, &load, &[]), recommendations);
    assert_eq!(acting.to_run(&recommendations).len(), 1);
}

#[test]
fn a_paused_controller_decides_nothing_at_all() {
    let topology = with_one_tablet();
    let load = BTreeMap::from([(
        "t1".to_string(),
        TabletLoad {
            stored_bytes: 900 * 1024 * 1024 * 1024,
            ..TabletLoad::default()
        },
    )]);
    let paused = Controller::new("west-1", "west").with_mode(Mode::Paused);
    assert!(paused.decide(&topology, &load, &[]).is_empty());
}

#[test]
fn the_merge_threshold_is_far_below_half_the_split_threshold_so_a_tablet_does_not_oscillate() {
    // Hysteresis, in CELLS.md section 6. A tablet that split and then merged
    // around one size would rewrite data for ever.
    let thresholds = crate::controller::Thresholds::default();
    assert!(
        thresholds.merge_below_bytes * 2 < thresholds.split_above_bytes,
        "two merged tablets must not immediately be over the split threshold"
    );
}

#[test]
fn a_cell_already_changing_something_does_not_start_another_change() {
    let mut topology = with_one_tablet();
    topology
        .apply(&ControllerCommand::SplitTablet {
            tablet: "t1".into(),
            at_shard: 2048,
            right: "t2".into(),
        })
        .unwrap();
    topology
        .apply(&ControllerCommand::SetTabletState {
            tablet: "t2".into(),
            state: TabletState::Moving,
        })
        .unwrap();
    let load = BTreeMap::from([
        (
            "t1".to_string(),
            TabletLoad {
                stored_bytes: 900 * 1024 * 1024 * 1024,
                ..TabletLoad::default()
            },
        ),
        ("t2".to_string(), TabletLoad::default()),
    ]);
    let controller = Controller::new("west-1", "west").with_mode(Mode::Automatic);
    let placement: Vec<_> = controller
        .decide(&topology, &load, &[])
        .into_iter()
        .filter(|r| !matches!(r.command, ControllerCommand::SetNodeCondition { .. }))
        .collect();
    assert!(
        placement.is_empty(),
        "a move is already in flight, so nothing else starts: {placement:?}"
    );
}

#[test]
fn two_small_adjacent_tablets_are_recommended_for_a_merge() {
    let mut topology = with_one_tablet();
    topology
        .apply(&ControllerCommand::SplitTablet {
            tablet: "t1".into(),
            at_shard: 2048,
            right: "t2".into(),
        })
        .unwrap();
    let load = BTreeMap::from([
        ("t1".to_string(), TabletLoad::default()),
        ("t2".to_string(), TabletLoad::default()),
    ]);
    let controller = Controller::new("west-1", "west");
    let recommendations = controller.decide(&topology, &load, &[]);
    assert!(recommendations
        .iter()
        .any(|r| matches!(r.command, ControllerCommand::MergeTablets { .. })));
}

// ---------------------------------------------------------------------------
// Scale
// ---------------------------------------------------------------------------

#[test]
fn a_four_hundred_node_cell_still_has_three_controller_voters() {
    // The property the simulation exists to show. If placement had put every
    // storage node in a consensus group, this number would track the node
    // count.
    let (topology, scale) = one_cell("big", "west", 400, 600, 3, 20);
    assert_eq!(scale.nodes, 400);
    assert!(scale.tablets > 500, "{} tablets", scale.tablets);
    assert_eq!(scale.controller_voters, 3);
    assert!(scale.controller_is_bounded());
    assert_eq!(
        scale.largest_group, 3,
        "no group in a 400-node cell is larger than three"
    );
    assert!(
        scale.smallest_domain_spread >= 2,
        "no tablet has all of its voters in one failure domain"
    );

    // Losing the controller quorum stops placement and nothing else.
    let effect = controller_outage(&topology);
    assert_eq!(effect.tablets_still_writable, scale.tablets);
    assert!(effect.placement_changes_refused);
    assert!(effect.queries_still_planned);
}

#[test]
fn a_ten_thousand_node_installation_has_a_directory_the_size_of_its_project_list() {
    // The other property: the global directory holds one row for each project
    // and never one for each tablet. If tablet placement had leaked into the
    // directory, this would track the tablet count instead.
    let projects = 500;
    let (_, directory, scale) = multi_cell(25, 400, 200, projects);
    assert_eq!(scale.nodes, 10_000);
    assert!(scale.tablets >= 4_000, "{} tablets", scale.tablets);
    assert_eq!(scale.directory_rows, projects);
    assert!(scale.directory_is_bounded(projects));
    assert!(
        scale.controller_voters <= 25 * 5,
        "{} controller voters across 25 cells",
        scale.controller_voters
    );
    assert_eq!(scale.largest_group, 3);
    // The directory knows about cells and about projects, and about no tablet.
    assert_eq!(directory.cells().count(), 25);
}

#[test]
fn one_storage_node_holds_many_tablet_groups_and_that_is_the_design() {
    // D27 measured 600 instances at 21 MiB, which is what makes this a design
    // rather than a hope.
    let (topology, scale) = one_cell("dense", "west", 20, 200, 3, 5);
    assert!(
        scale.busiest_node_groups > 10,
        "one node holds {} groups",
        scale.busiest_node_groups
    );
    let counted = measure(&topology, &Directory::new(), 1);
    assert_eq!(counted.tablets, scale.tablets);
}

#[test]
fn a_tablet_whose_part_never_arrived_at_all_makes_the_answer_incomplete() {
    // The caller is meant to build a `Partial::unavailable` for a tablet that
    // did not answer. A caller that forgot would otherwise get a smaller answer
    // marked complete, which is the failure FAILURE_MODES.md section 2 ranks
    // worst, so the merge counts the parts against the plan.
    let plan = a_plan(false);
    let only_one = vec![Partial {
        count: 40,
        ..Partial::for_tablet("t1")
    }];
    let failure = merge(&plan, only_one).expect_err("t2 sent nothing and nobody said so");
    assert_eq!(
        failure.code,
        tallyowl_obs::error::ErrorCode::IncompleteResult
    );
    assert!(failure.message.contains("t2"), "{}", failure.message);
}
