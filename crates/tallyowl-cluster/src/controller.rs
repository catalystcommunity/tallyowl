//! The cell controller: what places a tablet, and what refuses to.
//!
//! `docs/CELLS.md` section 6 lists what the controller measures — stored bytes,
//! ingest rate, query load, compaction debt, disk pressure, replica lag — and
//! says it can split, merge, or move a tablet, with hysteresis and a limit on
//! concurrent changes. It also gives three modes, and says which one the first
//! releases should use:
//!
//! > An operator can select one of these modes: automatic; recommendation only;
//! > paused. The normal production mode is automatic. **The first releases can
//! > use recommendation-only mode until tests prove safe automatic control.**
//!
//! This is the first release, so [`Mode::RecommendOnly`] is the default here.
//! The whole decision path runs in every mode and produces the same list; the
//! mode decides only whether the controller acts on it. That way the automatic
//! path is exercised by every test and by every installation, rather than being
//! code nobody ran until the day it was switched on.
//!
//! # Placement spreads across failure domains
//!
//! Three voters on one rack is one rack away from a lost quorum.
//! [`choose_voters`] takes at most one node from each failure domain until it
//! runs out of domains, and says so when it had to double up rather than
//! placing the group and staying quiet about it.

use std::collections::{BTreeMap, BTreeSet};

use tallyowl_obs::error::{ErrorCode, TallyOwlError};

use crate::health::{Condition, SlowAction};
use crate::topology::{
    ControllerCommand, DomainName, Member, MemberRole, NodeName, NodeState, ReceiptPolicy,
    TabletName, TabletState, Topology, VirtualShard,
};

/// What the controller is allowed to do on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Decide and act.
    Automatic,
    /// Decide and report. The default in this release. See the module note.
    #[default]
    RecommendOnly,
    /// Decide nothing.
    Paused,
}

impl Mode {
    pub fn parse(text: &str) -> Option<Mode> {
        match text {
            "automatic" => Some(Mode::Automatic),
            "recommendation-only" => Some(Mode::RecommendOnly),
            "paused" => Some(Mode::Paused),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Automatic => "automatic",
            Mode::RecommendOnly => "recommendation-only",
            Mode::Paused => "paused",
        }
    }
}

/// What the controller measures about one tablet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TabletLoad {
    pub stored_bytes: u64,
    pub accepted_bytes_each_second: u64,
    pub compaction_backlog_bytes: u64,
    pub replica_lag_ms: i64,
    pub query_milliseconds_each_second: u64,
}

/// The thresholds the controller decides against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// Split above this. `docs/DECISIONS.md` D16 sizes a tablet.
    pub split_above_bytes: u64,
    /// Merge two adjacent tablets when both are below this. It is deliberately
    /// far below half the split threshold: making it exactly half would make a
    /// tablet split and merge repeatedly around one size, which is the churn
    /// `docs/CELLS.md` section 6 means by hysteresis.
    pub merge_below_bytes: u64,
    /// Move work off a node with less free space than this.
    pub move_below_free_bytes: u64,
    /// How many changes may run at once in one cell.
    pub concurrent_changes: usize,
}

impl Default for Thresholds {
    fn default() -> Thresholds {
        Thresholds {
            split_above_bytes: 64 * 1024 * 1024 * 1024,
            merge_below_bytes: 8 * 1024 * 1024 * 1024,
            move_below_free_bytes: 16 * 1024 * 1024 * 1024,
            concurrent_changes: 1,
        }
    }
}

/// One thing the controller thinks should happen, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    pub command: ControllerCommand,
    /// What an operator reads. It always names the measurement that caused it.
    pub because: String,
}

/// The cell controller's decision logic.
///
/// It holds no state of its own beyond its settings: every decision is a
/// function of the topology and the measurements it is given. That is what lets
/// the same code run on a controller, in a test, and in the scale simulation.
#[derive(Debug, Clone)]
pub struct Controller {
    pub cell: String,
    pub region: String,
    pub mode: Mode,
    pub thresholds: Thresholds,
    pub slow_action: SlowAction,
    /// How many voters a new tablet gets. Three by default; an operator can
    /// select five. One is the home profile.
    pub voters: usize,
}

impl Controller {
    pub fn new(cell: impl Into<String>, region: impl Into<String>) -> Controller {
        Controller {
            cell: cell.into(),
            region: region.into(),
            mode: Mode::default(),
            thresholds: Thresholds::default(),
            slow_action: SlowAction::Alert,
            voters: 3,
        }
    }

    pub fn with_mode(mut self, mode: Mode) -> Controller {
        self.mode = mode;
        self
    }

    pub fn with_voters(mut self, voters: usize) -> Controller {
        self.voters = voters;
        self
    }

    /// Everything the controller thinks should happen right now.
    ///
    /// At most `concurrent_changes` placement changes come back, because a cell
    /// that started every change it wanted at once would move more data than it
    /// could carry. A health change is not a placement change and is not
    /// counted against that limit: it moves no data.
    pub fn decide(
        &self,
        topology: &Topology,
        load: &BTreeMap<TabletName, TabletLoad>,
        conditions: &[Condition],
    ) -> Vec<Recommendation> {
        if self.mode == Mode::Paused {
            return Vec::new();
        }
        let mut out = Vec::new();

        // Health first. It is what an operator most needs to see, and it is
        // never held back by the placement limit.
        for condition in conditions {
            if let Some(recommendation) = self.for_condition(topology, condition) {
                out.push(recommendation);
            }
        }

        let mut placement = Vec::new();
        // A cell already moving something does not start another movement.
        let in_flight = topology
            .tablets()
            .filter(|t| t.state != TabletState::Active && t.state != TabletState::Retired)
            .count();
        let room = self.thresholds.concurrent_changes.saturating_sub(in_flight);

        for tablet in topology.tablets() {
            if placement.len() >= room {
                break;
            }
            if tablet.state != TabletState::Active {
                continue;
            }
            let Some(measured) = load.get(&tablet.name) else {
                continue;
            };
            if measured.stored_bytes > self.thresholds.split_above_bytes
                && tablet.shard_end - tablet.shard_start > 1
            {
                let at = tablet.shard_start + (tablet.shard_end - tablet.shard_start) / 2;
                placement.push(Recommendation {
                    because: format!(
                        "`{}` holds {} bytes and the split threshold is {}. Splitting at virtual shard {at} halves the range and moves no shard.",
                        tablet.name, measured.stored_bytes, self.thresholds.split_above_bytes
                    ),
                    command: ControllerCommand::SplitTablet {
                        tablet: tablet.name.clone(),
                        at_shard: at,
                        right: format!("{}-b", tablet.name),
                    },
                });
                continue;
            }
        }

        // A merge needs two adjacent tablets that are both small and held by
        // the same nodes. Looking at pairs rather than at one tablet is why it
        // is a second pass.
        if placement.len() < room {
            let tablets: Vec<_> = topology.tablets().collect();
            for pair in tablets.windows(2) {
                if placement.len() >= room {
                    break;
                }
                let (left, right) = (pair[0], pair[1]);
                if left.shard_end != right.shard_start
                    || left.state != TabletState::Active
                    || right.state != TabletState::Active
                {
                    continue;
                }
                let (Some(left_load), Some(right_load)) =
                    (load.get(&left.name), load.get(&right.name))
                else {
                    continue;
                };
                if left_load.stored_bytes < self.thresholds.merge_below_bytes
                    && right_load.stored_bytes < self.thresholds.merge_below_bytes
                {
                    placement.push(Recommendation {
                        because: format!(
                            "`{}` and `{}` are adjacent and hold {} and {} bytes, both under the merge threshold of {}. One tablet costs one consensus group instead of two.",
                            left.name, right.name, left_load.stored_bytes, right_load.stored_bytes,
                            self.thresholds.merge_below_bytes
                        ),
                        command: ControllerCommand::MergeTablets {
                            left: left.name.clone(),
                            right: right.name.clone(),
                        },
                    });
                }
            }
        }

        out.extend(placement);
        out
    }

    fn for_condition(&self, topology: &Topology, condition: &Condition) -> Option<Recommendation> {
        let known = topology.node(&condition.node)?;
        if known.state == condition.state
            && known.writable == condition.writable
            && known.free_bytes == condition.free_bytes
        {
            return None;
        }
        let because = match condition.state {
            NodeState::Slow => format!(
                "`{}` took {} microseconds to append against a group median of {}, and {} to fsync against a median of {}, for longer than the configured period. Cause: {}. Action: {}.",
                condition.node,
                condition.append_latency_us,
                condition.group_median_append_latency_us,
                condition.fsync_latency_us,
                condition.group_median_fsync_latency_us,
                condition
                    .cause
                    .map(|c| c.as_str())
                    .unwrap_or("unknown"),
                self.slow_action_text()
            ),
            NodeState::ReadOnly => format!(
                "`{}` reported that it cannot make a write durable, with {} bytes free. It stops accepting appends and the quorum continues without it.",
                condition.node, condition.free_bytes
            ),
            NodeState::Unreachable => {
                format!("`{}` has not reported for longer than its lease.", condition.node)
            }
            _ => format!("`{}` is healthy again.", condition.node),
        };
        Some(Recommendation {
            because,
            command: ControllerCommand::SetNodeCondition {
                node: condition.node.clone(),
                state: condition.state,
                cause: condition.cause.map(|c| c.as_str().to_string()),
                at: condition.since,
                writable: condition.writable,
                free_bytes: condition.free_bytes,
            },
        })
    }

    fn slow_action_text(&self) -> &'static str {
        match self.slow_action {
            SlowAction::Alert => "the state was raised and an operator decides",
            SlowAction::Demote => "the controller demoted this voter",
        }
    }

    /// The commands to run, given the mode.
    ///
    /// In `recommendation-only` this is empty and the recommendations are still
    /// produced, which is the whole difference between the two modes.
    pub fn to_run(&self, recommendations: &[Recommendation]) -> Vec<ControllerCommand> {
        match self.mode {
            Mode::Automatic => recommendations.iter().map(|r| r.command.clone()).collect(),
            Mode::RecommendOnly | Mode::Paused => Vec::new(),
        }
    }

    /// Create the first tablet of a cell, over the whole shard space.
    pub fn create_first_tablet(
        &self,
        tablet: impl Into<TabletName>,
        members: Vec<Member>,
    ) -> Result<ControllerCommand, TallyOwlError> {
        let voters = members
            .iter()
            .filter(|m| m.role == MemberRole::Voter)
            .count();
        let policy = default_policy(voters);
        Ok(ControllerCommand::CreateTablet {
            tablet: tablet.into(),
            cell: self.cell.clone(),
            region: self.region.clone(),
            shard_start: 0,
            shard_end: crate::topology::VIRTUAL_SHARDS,
            members,
            receipt_policy: policy,
        })
    }
}

/// The receipt policy a tablet gets when nobody chose one.
///
/// `docs/CELLS.md` section 8: `local-one` is the default in the home and
/// embedded profiles, and a tablet with more than one voter uses
/// `local-quorum`. Defaulting a multi-voter tablet to `local-one` and refusing
/// it later would be the same rule stated twice; defaulting correctly means the
/// refusal only ever fires on a deliberate choice.
pub fn default_policy(voters: usize) -> ReceiptPolicy {
    if voters <= 1 {
        ReceiptPolicy::LocalOne
    } else {
        ReceiptPolicy::LocalQuorum
    }
}

/// Choose voters for a new tablet, spread across failure domains.
///
/// Returns the members and, when the cell could not spread them, the reason.
/// The reason is returned rather than logged: a caller that placed a group with
/// two voters in one rack should be able to put that in an operator's hands.
pub fn choose_voters(
    topology: &Topology,
    want: usize,
) -> Result<(Vec<Member>, Option<String>), TallyOwlError> {
    let mut healthy: Vec<_> = topology
        .nodes()
        .filter(|n| n.writable && n.state != NodeState::Draining)
        .collect();
    if healthy.len() < want {
        return Err(TallyOwlError::new(
            ErrorCode::FailedPrecondition,
            format!(
                "This cell has {} nodes that can hold a voter and the tablet needs {want}. Add nodes, or create the tablet with fewer voters.",
                healthy.len()
            ),
        ));
    }
    // Prefer a node with more free space when two are otherwise equal, so a
    // fresh tablet does not land on the fullest node in the cell.
    healthy.sort_by(|a, b| b.free_bytes.cmp(&a.free_bytes).then(a.name.cmp(&b.name)));

    let mut chosen: Vec<Member> = Vec::new();
    let mut used_domains: BTreeSet<&DomainName> = BTreeSet::new();
    for node in &healthy {
        if chosen.len() == want {
            break;
        }
        if used_domains.insert(&node.domain) {
            chosen.push(Member {
                node: node.name.clone(),
                role: MemberRole::Voter,
                address: node.address.clone(),
                region: node.region.clone(),
                domain: node.domain.clone(),
            });
        }
    }
    let mut doubled = None;
    if chosen.len() < want {
        // Not enough distinct domains. Fill from what is left and say so.
        let already: BTreeSet<&NodeName> = chosen.iter().map(|m| &m.node).collect();
        let extra: Vec<Member> = healthy
            .iter()
            .filter(|n| !already.contains(&n.name))
            .take(want - chosen.len())
            .map(|n| Member {
                node: n.name.clone(),
                role: MemberRole::Voter,
                address: n.address.clone(),
                region: n.region.clone(),
                domain: n.domain.clone(),
            })
            .collect();
        doubled = Some(format!(
            "This cell has {} failure domains and the tablet needs {want} voters, so {} of them share a domain with another. One domain's loss can end this tablet's quorum.",
            used_domains.len(),
            want - used_domains.len()
        ));
        chosen.extend(extra);
    }
    Ok((chosen, doubled))
}

/// Where a shard boundary should go when a tablet splits and nobody chose.
///
/// The middle of the range. A boundary chosen from the data would move data;
/// the middle of a shard range moves none, because a shard never changes tablet
/// during a split — the range does.
pub fn split_point(shard_start: VirtualShard, shard_end: VirtualShard) -> Option<VirtualShard> {
    if shard_end - shard_start < 2 {
        return None;
    }
    Some(shard_start + (shard_end - shard_start) / 2)
}
