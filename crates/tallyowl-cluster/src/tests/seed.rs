//! The first voter set, read from configuration. See [`crate::seed`].

use std::collections::BTreeMap;

use crate::raft::node_id;
use crate::seed::{seed, SeedSettings};

/// The membership consensus is created with: identity to address.
fn membership(settings: &SeedSettings) -> BTreeMap<u64, String> {
    seed(settings)
        .expect("the settings are legal")
        .members
        .iter()
        .map(|member| (node_id(&member.node), member.address.clone()))
        .collect()
}

#[test]
fn three_nodes_that_share_one_peer_list_read_the_same_voter_set() {
    // A chart renders one list for every pod. Before the advertise address
    // existed each pod named itself after its pod name, named its peers after
    // their addresses, and listed itself as a peer, so no two pods agreed on
    // who was in the group and it never formed.
    let peers: Vec<String> = (0..3)
        .map(|n| format!("tallyowl-{n}.tallyowl:5200"))
        .collect();
    let views: Vec<BTreeMap<u64, String>> = (0..3)
        .map(|n| {
            let advertise = format!("tallyowl-{n}.tallyowl:5200");
            membership(&SeedSettings {
                // What a chart sets: every pod binds the same address.
                listen: "0.0.0.0:5200",
                advertise: &advertise,
                peers: &peers,
                region: "west",
                ..Default::default()
            })
        })
        .collect();
    assert_eq!(
        views[0].len(),
        3,
        "a node listed itself twice, or not at all"
    );
    assert_eq!(views[0], views[1]);
    assert_eq!(views[1], views[2]);
}

#[test]
fn a_named_node_is_the_same_node_to_itself_and_to_its_peers() {
    let peers: Vec<String> = (0..3)
        .map(|n| format!("owl-{n}@tallyowl-{n}.tallyowl:5200"))
        .collect();
    let views: Vec<BTreeMap<u64, String>> = (0..3)
        .map(|n| {
            let name = format!("owl-{n}");
            let advertise = format!("tallyowl-{n}.tallyowl:5200");
            membership(&SeedSettings {
                name: &name,
                listen: "0.0.0.0:5200",
                advertise: &advertise,
                peers: &peers,
                ..Default::default()
            })
        })
        .collect();
    assert_eq!(views[0], views[1]);
    assert_eq!(views[1], views[2]);
    assert!(views[0].contains_key(&node_id("owl-1")));
}

#[test]
fn a_node_whose_peers_would_call_it_something_else_does_not_start() {
    let peers = vec![
        "tallyowl-0.tallyowl:5200".to_string(),
        "tallyowl-1.tallyowl:5200".to_string(),
    ];
    let refused = seed(&SeedSettings {
        name: "tallyowl-0",
        listen: "0.0.0.0:5200",
        advertise: "tallyowl-0.tallyowl:5200",
        peers: &peers,
        ..Default::default()
    })
    .expect_err("its peers derive a different name for it");
    assert!(refused
        .message
        .contains("tallyowl-0@tallyowl-0.tallyowl:5200"));
}

#[test]
fn an_address_nobody_can_dial_is_refused_by_name() {
    let peers = vec!["10.0.0.2:5200".to_string()];
    let advertised = seed(&SeedSettings {
        listen: "0.0.0.0:5200",
        advertise: "0.0.0.0:5200",
        peers: &peers,
        ..Default::default()
    })
    .expect_err("an unspecified advertise address");
    assert!(advertised.message.contains("replication.advertise"));

    let bound = seed(&SeedSettings {
        listen: "[::]:5200",
        peers: &peers,
        ..Default::default()
    })
    .expect_err("peers would be told to dial the bind address");
    assert!(bound.message.contains("replication.advertise"));
}

#[test]
fn a_node_alone_and_a_list_of_the_others_still_work() {
    // The home profile with replication on, and the soak's layout: each node
    // is given the others, and no advertise address.
    let alone = seed(&SeedSettings {
        listen: "0.0.0.0:5200",
        ..Default::default()
    })
    .expect("a node alone is never dialled");
    assert_eq!(alone.members.len(), 1);

    let others = vec!["127.0.0.1:5202".to_string(), "127.0.0.1:5203".to_string()];
    let first = membership(&SeedSettings {
        listen: "127.0.0.1:5201",
        peers: &others,
        ..Default::default()
    });
    let all = vec![
        "127.0.0.1:5201".to_string(),
        "127.0.0.1:5202".to_string(),
        "127.0.0.1:5203".to_string(),
    ];
    let second = membership(&SeedSettings {
        listen: "127.0.0.1:5202",
        peers: &all,
        ..Default::default()
    });
    assert_eq!(first, second);
}
