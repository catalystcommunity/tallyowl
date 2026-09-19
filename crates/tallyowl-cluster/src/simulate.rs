//! The scale simulations, and what they are for.
//!
//! `docs/PLAN.md` Phase 7 is explicit that these are **a milestone, not a
//! gate**: "A 400-node cell simulation and a 10,000-node multi-cell simulation
//! must show bounded controller and global directory work. Do not block the
//! phase on hardware that the project does not have."
//!
//! So this simulates the *control* work, not the data path. It builds a real
//! [`crate::topology::Topology`] and a real [`crate::directory::Directory`] at
//! those sizes and counts what the controller and the directory each have to
//! hold and decide. It does not pretend to measure throughput on hardware that
//! does not exist, and no number here is reported as a benchmark.
//!
//! # What "bounded" means, and what would fail it
//!
//! Two properties, and both would be visible here if they broke:
//!
//! **The controller quorum does not grow with the cell.** A 400-node cell has
//! three or five controller voters, the same as a four-node cell. If placement
//! had put every storage node in a consensus group, the voter count would track
//! the node count and this would show it.
//!
//! **The global directory does not grow with the tablets.** It holds one row
//! for each project and it never holds tablet placement, so a multi-cell
//! installation with tens of thousands of tablets has a directory the size of
//! its project list. If tablet placement had leaked into the directory, the
//! directory row count would track the tablet count and this would show it.

use std::collections::BTreeSet;

use crate::directory::{Directory, DirectoryCommand};
use crate::topology::{
    ControllerCommand, Member, MemberRole, NodeState, ReceiptPolicy, Topology, VIRTUAL_SHARDS,
};

/// What one simulation found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scale {
    pub cells: usize,
    pub nodes: usize,
    pub tablets: usize,
    /// Voters in every controller quorum, added up. This is the number that
    /// must not track the node count.
    pub controller_voters: usize,
    /// The largest number of voters in any one consensus group.
    pub largest_group: usize,
    /// Rows in the global directory. This is the number that must not track the
    /// tablet count.
    pub directory_rows: usize,
    /// How many tablet groups the busiest storage node holds.
    pub busiest_node_groups: usize,
    /// Distinct failure domains any one tablet's voters span. Three voters in
    /// one domain is one domain away from a lost quorum.
    pub smallest_domain_spread: usize,
}

impl Scale {
    /// Whether the controller work is bounded: a quorum is three or five
    /// voters, whatever the cell size.
    pub fn controller_is_bounded(&self) -> bool {
        self.cells > 0 && self.controller_voters <= self.cells * 5
    }

    /// Whether the directory work is bounded: one row for each project, and
    /// never one for each tablet.
    pub fn directory_is_bounded(&self, projects: usize) -> bool {
        self.directory_rows == projects
    }
}

/// Build one cell of `nodes` storage nodes and `tablets` tablets, and count.
///
/// `voters` is three or five. `domains` is how many failure domains the cell
/// spreads across, which is what stops a rack taking a quorum.
pub fn one_cell(
    cell: &str,
    region: &str,
    nodes: usize,
    tablets: usize,
    voters: usize,
    domains: usize,
) -> (Topology, Scale) {
    let mut topology = Topology::new();
    let node_names: Vec<String> = (0..nodes)
        .map(|n| format!("{cell}-storage-{n:04}"))
        .collect();

    for (index, name) in node_names.iter().enumerate() {
        topology
            .apply(&ControllerCommand::RegisterNode {
                node: name.clone(),
                address: format!("10.0.{}.{}:5200", index / 250, index % 250),
                region: region.to_string(),
                domain: format!("{cell}-domain-{}", index % domains.max(1)),
            })
            .expect("a fresh node registers");
    }

    // The controller quorum. Three or five, and it does not grow with the cell.
    let controllers: Vec<Member> = node_names
        .iter()
        .take(voters)
        .enumerate()
        .map(|(index, name)| {
            Member::voter(name.clone(), format!("10.0.0.{index}:5200"))
                .in_region(region)
                .in_domain(format!("{cell}-domain-{}", index % domains.max(1)))
        })
        .collect();
    topology
        .apply(&ControllerCommand::RegisterCell {
            cell: cell.to_string(),
            region: region.to_string(),
            controllers: controllers.clone(),
        })
        .expect("a cell registers");

    // The tablets, spread over the shard space, each on its own three nodes.
    let shards_each = (VIRTUAL_SHARDS as usize / tablets.max(1)).max(1);
    for index in 0..tablets {
        let start = (index * shards_each) as u64;
        let end = if index + 1 == tablets {
            VIRTUAL_SHARDS
        } else {
            ((index + 1) * shards_each) as u64
        };
        if start >= end {
            break;
        }
        let members: Vec<Member> = (0..voters)
            .map(|v| {
                // Stride the replica set across nodes, so the tablets on one
                // node are not all held with the same peers.
                let which = (index * voters + v * 7) % nodes;
                Member::voter(
                    node_names[which].clone(),
                    format!("10.0.{}.{}:5200", which / 250, which % 250),
                )
                .in_region(region)
                .in_domain(format!("{cell}-domain-{}", which % domains.max(1)))
            })
            .collect();
        // A replica set with a repeated node is not three replicas. Skip the
        // tablet rather than counting it, because counting it would make the
        // simulation say the cell is safer than it is.
        let distinct: BTreeSet<&String> = members.iter().map(|m| &m.node).collect();
        if distinct.len() != voters {
            continue;
        }
        topology
            .apply(&ControllerCommand::CreateTablet {
                tablet: format!("{cell}-t{index:05}"),
                cell: cell.to_string(),
                region: region.to_string(),
                shard_start: start,
                shard_end: end,
                members,
                receipt_policy: if voters > 1 {
                    ReceiptPolicy::LocalQuorum
                } else {
                    ReceiptPolicy::LocalOne
                },
            })
            .expect("a tablet is created over a free shard range");
    }

    let scale = measure(&topology, &Directory::new(), 1);
    (topology, scale)
}

/// Count what one topology and one directory hold.
pub fn measure(topology: &Topology, directory: &Directory, cells: usize) -> Scale {
    let mut busiest = std::collections::BTreeMap::new();
    let mut largest_group = 0usize;
    let mut smallest_spread = usize::MAX;
    let mut tablets = 0usize;

    for tablet in topology.tablets() {
        tablets += 1;
        let voters = tablet.voter_count();
        largest_group = largest_group.max(voters);
        let domains: BTreeSet<&str> = tablet.voters().map(|m| m.domain.as_str()).collect();
        smallest_spread = smallest_spread.min(domains.len());
        for member in &tablet.members {
            *busiest.entry(member.node.clone()).or_insert(0usize) += 1;
        }
    }

    let controller_voters: usize = topology
        .cells()
        .map(|cell| {
            cell.controllers
                .iter()
                .filter(|m| m.role == MemberRole::Voter)
                .count()
        })
        .sum();
    largest_group = largest_group.max(
        topology
            .cells()
            .map(|c| c.controllers.len())
            .max()
            .unwrap_or(0),
    );

    Scale {
        cells: cells.max(topology.cells().count()),
        nodes: topology.nodes().count(),
        tablets,
        controller_voters,
        largest_group,
        directory_rows: directory.placements().count(),
        busiest_node_groups: busiest.values().copied().max().unwrap_or(0),
        smallest_domain_spread: if smallest_spread == usize::MAX {
            0
        } else {
            smallest_spread
        },
    }
}

/// A multi-cell installation: many cells, one small directory.
///
/// The directory gets one row for each project, whatever the tablet count is.
/// That is the property this simulation exists to show.
pub fn multi_cell(
    cells: usize,
    nodes_each_cell: usize,
    tablets_each_cell: usize,
    projects: usize,
) -> (Vec<Topology>, Directory, Scale) {
    let mut topologies = Vec::with_capacity(cells);
    let mut directory = Directory::new();

    for index in 0..cells {
        let cell = format!("cell-{index:03}");
        let region = format!("region-{}", index % 8);
        directory
            .apply(&DirectoryCommand::RegisterCell {
                cell: cell.clone(),
                region: region.clone(),
            })
            .expect("a cell registers in the directory");
        let (topology, _) = one_cell(&cell, &region, nodes_each_cell, tablets_each_cell, 3, 8);
        topologies.push(topology);
    }

    for project in 0..projects {
        let mut project_id = [0u8; 16];
        project_id[..8].copy_from_slice(&(project as u64).to_be_bytes());
        directory
            .apply(&DirectoryCommand::AssignProject {
                project_id,
                cells: vec![format!("cell-{:03}", project % cells.max(1))],
            })
            .expect("a project is assigned to a cell that exists");
    }

    // Every cell has the same shape, so the whole installation's control cost
    // is the sum. Adding the parts is legitimate here, unlike a capacity
    // envelope, because these are counts of records rather than measurements
    // that interact.
    let mut total = Scale {
        cells,
        nodes: 0,
        tablets: 0,
        controller_voters: 0,
        largest_group: 0,
        directory_rows: directory.placements().count(),
        busiest_node_groups: 0,
        smallest_domain_spread: usize::MAX,
    };
    for topology in &topologies {
        let one = measure(topology, &Directory::new(), 1);
        total.nodes += one.nodes;
        total.tablets += one.tablets;
        total.controller_voters += one.controller_voters;
        total.largest_group = total.largest_group.max(one.largest_group);
        total.busiest_node_groups = total.busiest_node_groups.max(one.busiest_node_groups);
        total.smallest_domain_spread = total.smallest_domain_spread.min(one.smallest_domain_spread);
    }
    if total.smallest_domain_spread == usize::MAX {
        total.smallest_domain_spread = 0;
    }
    (topologies, directory, total)
}

/// What a cell keeps working through when its controller quorum is gone.
///
/// `docs/CELLS.md` section 10: "A cell-controller failure does not stop a
/// healthy tablet group. It prevents tablet placement changes until the
/// controller quorum returns."
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutageEffect {
    /// Tablets that still accept writes.
    pub tablets_still_writable: usize,
    /// Placement changes that are refused.
    pub placement_changes_refused: bool,
    /// Whether a query can still be planned from the cached topology.
    pub queries_still_planned: bool,
}

/// What losing the controller quorum costs this cell.
pub fn controller_outage(topology: &Topology) -> OutageEffect {
    OutageEffect {
        tablets_still_writable: topology
            .tablets()
            .filter(|t| t.writable() && t.voters().count() > 0)
            .count(),
        placement_changes_refused: true,
        queries_still_planned: true,
    }
}

/// What losing the global directory costs a cell.
///
/// `docs/CELLS.md` section 9: an existing cell keeps working from its current
/// assignments, and only new placement, project movement, global policy, and
/// new cell registration stop. In particular the directory never joins a tablet
/// write path, so nothing about the write path appears here.
pub fn directory_outage(topology: &Topology, nodes_marked_unreachable: &[&str]) -> OutageEffect {
    let unreachable: BTreeSet<&str> = nodes_marked_unreachable.iter().copied().collect();
    OutageEffect {
        tablets_still_writable: topology
            .tablets()
            .filter(|t| {
                t.writable()
                    && t.voters()
                        .filter(|m| !unreachable.contains(m.node.as_str()))
                        .count()
                        > t.voter_count() / 2
            })
            .count(),
        placement_changes_refused: true,
        queries_still_planned: true,
    }
}

/// Mark some nodes unreachable, the way a partition would.
pub fn mark_unreachable(topology: &mut Topology, nodes: &[&str], at: i64) {
    for node in nodes {
        let _ = topology.apply(&ControllerCommand::SetNodeCondition {
            node: node.to_string(),
            state: NodeState::Unreachable,
            cause: None,
            at,
            writable: false,
            free_bytes: 0,
        });
    }
}
