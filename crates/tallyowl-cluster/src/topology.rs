//! What the cell controller quorum knows, and the only way it changes.
//!
//! `docs/STORAGE.md` section 7 lists what the cell controllers store: node
//! membership and health leases, virtual-shard and tablet definitions and
//! placement generations, split, move, and merge intents, configuration
//! versions, and fencing epochs. This module is that list as a state machine.
//!
//! # Why a command rather than a setter
//!
//! Every change here is replicated, so it has to be a value that can be written
//! to a log, sent to a peer, and applied again on restart with the same result.
//! A method that mutates in place cannot be any of those. [`ControllerCommand`]
//! is the whole change surface, [`Topology::apply`] is the only thing that
//! moves state, and the controller quorum's state machine is exactly this type.
//!
//! **Applying a command twice gives what applying it once gives.** The consensus
//! layer can hand the same entry to the state machine again after a restart, so
//! anything that is not idempotent here would corrupt the topology in a way no
//! test at a higher level would find. Each arm below says how it holds that.
//!
//! # The generation, and what it is for
//!
//! Every change raises the placement generation. A gateway caches a route with
//! the generation it saw, and a tablet leader refuses a write that carries an
//! older one, so a gateway that missed a movement is redirected rather than
//! writing to a node that no longer owns the data. The generation is a fence,
//! not a version number for people.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A node's name. The control plane assigns it at enrollment and a certificate
/// carries it, so it is never a value a peer chooses for itself.
pub type NodeName = String;
pub type CellName = String;
pub type RegionName = String;
pub type TabletName = String;
/// A failure domain: a rack, a zone, or a host. Placement spreads voters across
/// these, so one domain's loss never takes a quorum.
pub type DomainName = String;

/// A stable routing bucket. `routing.rs` derives it from tenancy and an
/// affinity key, and it never changes for a given input.
pub type VirtualShard = u64;

/// The placement generation. See the module note.
pub type Generation = u64;

/// The write-region epoch. A fenced failover raises it.
pub type Epoch = u64;

/// How many virtual shards an installation has, at every size.
///
/// The count is fixed for the life of an installation, because it is the input
/// to the routing hash: changing it would move existing data. Growth adds
/// tablets and moves shard ranges between them, which moves no shard.
///
/// 4,096 is enough that a 400-node cell has ten shards for each node at the
/// smallest useful tablet, and small enough that the whole map fits in a
/// controller's memory many times over.
pub const VIRTUAL_SHARDS: VirtualShard = 4096;

/// Whether a group member votes.
///
/// A learner follows the log and answers reads. It never enlarges a write
/// quorum, which is what makes a read replica free to a writer. See
/// `docs/STORAGE.md` section 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemberRole {
    Voter,
    Learner,
}

/// One member of one group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub node: NodeName,
    pub role: MemberRole,
    pub address: String,
    pub region: RegionName,
    pub domain: DomainName,
}

impl Member {
    pub fn voter(node: impl Into<NodeName>, address: impl Into<String>) -> Member {
        Member {
            node: node.into(),
            role: MemberRole::Voter,
            address: address.into(),
            region: "home".into(),
            domain: "default".into(),
        }
    }

    pub fn learner(node: impl Into<NodeName>, address: impl Into<String>) -> Member {
        Member {
            role: MemberRole::Learner,
            ..Member::voter(node, address)
        }
    }

    pub fn in_region(mut self, region: impl Into<RegionName>) -> Member {
        self.region = region.into();
        self
    }

    pub fn in_domain(mut self, domain: impl Into<DomainName>) -> Member {
        self.domain = domain.into();
        self
    }
}

/// What a tablet is doing. A tablet that is not `Active` still serves reads and
/// writes; the state says what else is happening to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TabletState {
    Active,
    Splitting,
    Merging,
    Moving,
    /// Ownership moved away and old requests are still draining. It answers
    /// reads and refuses writes. `docs/STORAGE.md` section 6 step 7.
    Draining,
    Retired,
}

/// The receipt policy for one tablet.
///
/// `LocalOne` is legal only on a tablet with one voter, and the controller
/// refuses it rather than quietly selecting another. `AGENTS.md`: "Never
/// acknowledge an uncommitted entry in a multi-voter group."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptPolicy {
    LocalOne,
    LocalQuorum,
    RemoteOne,
}

impl ReceiptPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            ReceiptPolicy::LocalOne => "local-one",
            ReceiptPolicy::LocalQuorum => "local-quorum",
            ReceiptPolicy::RemoteOne => "remote-one",
        }
    }

    pub fn parse(text: &str) -> Option<ReceiptPolicy> {
        match text {
            "local-one" => Some(ReceiptPolicy::LocalOne),
            "local-quorum" => Some(ReceiptPolicy::LocalQuorum),
            "remote-one" => Some(ReceiptPolicy::RemoteOne),
            _ => None,
        }
    }
}

/// The mark unsafe recovery leaves. `docs/FAILURE_MODES.md` section 6.2: it
/// never expires on its own, and clearing it records who accepted the loss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DegradedMark {
    pub since: i64,
    pub range_start: i64,
    pub range_end: i64,
    /// The watermark the survivor held. Anything the lost voters committed past
    /// this is gone, and no component can say which writes those were.
    pub survivor_watermark: u64,
    pub reason: String,
    pub audit_id: String,
}

/// One tablet: what it owns, who holds it, and what has happened to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tablet {
    pub name: TabletName,
    pub cell: CellName,
    /// The one region that may accept a write for this tablet right now.
    pub write_region: RegionName,
    pub epoch: Epoch,
    pub state: TabletState,
    /// The half-open virtual-shard range this tablet owns.
    pub shard_start: VirtualShard,
    pub shard_end: VirtualShard,
    pub members: Vec<Member>,
    pub receipt_policy: ReceiptPolicy,
    pub degraded: Option<DegradedMark>,
}

impl Tablet {
    pub fn voters(&self) -> impl Iterator<Item = &Member> {
        self.members.iter().filter(|m| m.role == MemberRole::Voter)
    }

    pub fn voter_count(&self) -> usize {
        self.voters().count()
    }

    pub fn member(&self, node: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.node == node)
    }

    pub fn owns(&self, shard: VirtualShard) -> bool {
        shard >= self.shard_start && shard < self.shard_end
    }

    /// Whether this tablet accepts a write right now. A draining or retired
    /// tablet answers reads and refuses writes.
    pub fn writable(&self) -> bool {
        matches!(
            self.state,
            TabletState::Active
                | TabletState::Splitting
                | TabletState::Merging
                | TabletState::Moving
        )
    }

    /// How many voters must persist an entry before it is committed.
    pub fn quorum(&self) -> usize {
        self.voter_count() / 2 + 1
    }
}

/// One regional cell: its controllers and its generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub name: CellName,
    pub region: RegionName,
    pub controllers: Vec<Member>,
}

/// A node the control plane knows about, and what the controller last decided
/// about it. `health.rs` produces the condition; this holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub name: NodeName,
    pub address: String,
    pub region: RegionName,
    pub domain: DomainName,
    pub state: NodeState,
    /// Why the node is in this state, when the controller could establish it.
    pub cause: Option<String>,
    pub since: i64,
    /// False when the node reported that it cannot make a write durable. A
    /// voter that fails this stops accepting appends and the quorum continues
    /// without it. `docs/FAILURE_MODES.md` section 6.
    pub writable: bool,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeState {
    Healthy,
    Slow,
    Unreachable,
    ReadOnly,
    Draining,
}

impl NodeState {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeState::Healthy => "healthy",
            NodeState::Slow => "slow",
            NodeState::Unreachable => "unreachable",
            NodeState::ReadOnly => "read-only",
            NodeState::Draining => "draining",
        }
    }
}

/// Why a command was refused.
///
/// Each of these is a rule that a document states and that this module holds,
/// so the refusal names the rule rather than saying "invalid".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopologyError {
    /// A name the topology does not know.
    NotFound(String),
    /// The change would break a rule the design states.
    Refused(String),
    /// The change is already made. This is not an error at the log level: an
    /// applied command that repeats is how a restart replays. It is an error
    /// only when a person asked for it.
    AlreadyDone(String),
}

impl std::fmt::Display for TopologyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TopologyError::NotFound(m)
            | TopologyError::Refused(m)
            | TopologyError::AlreadyDone(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for TopologyError {}

/// Every change the controller quorum can make.
///
/// This is the replicated command type. Nothing else moves topology state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControllerCommand {
    /// A node enrolled or re-reported itself.
    RegisterNode {
        node: NodeName,
        address: String,
        region: RegionName,
        domain: DomainName,
    },
    /// The controller decided something about a node's health.
    SetNodeCondition {
        node: NodeName,
        state: NodeState,
        cause: Option<String>,
        at: i64,
        writable: bool,
        free_bytes: u64,
    },
    RegisterCell {
        cell: CellName,
        region: RegionName,
        controllers: Vec<Member>,
    },
    /// Create a tablet over a shard range. This is the only command that
    /// creates a voter set, and a role token can never reach it.
    CreateTablet {
        tablet: TabletName,
        cell: CellName,
        region: RegionName,
        shard_start: VirtualShard,
        shard_end: VirtualShard,
        members: Vec<Member>,
        receipt_policy: ReceiptPolicy,
    },
    AddReplica {
        tablet: TabletName,
        member: Member,
    },
    RemoveReplica {
        tablet: TabletName,
        node: NodeName,
    },
    /// Move a tablet from one node to another. The data copy happens outside
    /// consensus; this is the placement change that follows a verified copy.
    MoveReplica {
        tablet: TabletName,
        away_from: NodeName,
        onto: Member,
    },
    SetTabletState {
        tablet: TabletName,
        state: TabletState,
    },
    /// Split at a virtual-shard boundary. The right-hand child is a new tablet
    /// over the same members; the controller rebalances afterwards.
    SplitTablet {
        tablet: TabletName,
        at_shard: VirtualShard,
        right: TabletName,
    },
    MergeTablets {
        left: TabletName,
        right: TabletName,
    },
    SetReceiptPolicy {
        tablet: TabletName,
        policy: ReceiptPolicy,
    },
    /// Move the write region behind a fence. The epoch rises, so a writer in
    /// the old region is refused rather than accepted beside the new one.
    FailOverRegion {
        tablet: TabletName,
        onto_region: RegionName,
    },
    MarkDegraded {
        tablet: TabletName,
        mark: DegradedMark,
    },
    ClearDegraded {
        tablet: TabletName,
        accepted_by: String,
        reason: String,
        at: i64,
    },
}

/// One audit record. Unsafe recovery and a degraded clear both write one, and
/// both belong to the `audit` retention class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub audit_id: String,
    pub at: i64,
    pub what: String,
    pub detail: String,
}

/// The whole controller state. This is the state machine of the cell
/// controller quorum.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topology {
    pub generation: Generation,
    cells: BTreeMap<CellName, Cell>,
    tablets: BTreeMap<TabletName, Tablet>,
    nodes: BTreeMap<NodeName, Node>,
    audit: Vec<AuditRecord>,
}

impl Topology {
    pub fn new() -> Topology {
        Topology::default()
    }

    pub fn cells(&self) -> impl Iterator<Item = &Cell> {
        self.cells.values()
    }

    pub fn cell(&self, name: &str) -> Option<&Cell> {
        self.cells.get(name)
    }

    pub fn tablets(&self) -> impl Iterator<Item = &Tablet> {
        self.tablets.values()
    }

    pub fn tablet(&self, name: &str) -> Option<&Tablet> {
        self.tablets.get(name)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    pub fn node(&self, name: &str) -> Option<&Node> {
        self.nodes.get(name)
    }

    pub fn audit(&self) -> &[AuditRecord] {
        &self.audit
    }

    /// The tablet that owns one virtual shard.
    ///
    /// A shard belongs to exactly one tablet that is not retired. A draining
    /// tablet still owns its shards for reads, so this returns the writable one
    /// when both exist.
    pub fn owner_of(&self, shard: VirtualShard) -> Option<&Tablet> {
        let mut draining = None;
        for tablet in self.tablets.values() {
            if tablet.state == TabletState::Retired || !tablet.owns(shard) {
                continue;
            }
            if tablet.writable() {
                return Some(tablet);
            }
            draining = Some(tablet);
        }
        draining
    }

    /// Every tablet a query over one project must ask.
    ///
    /// A retired tablet holds nothing. A draining one still answers reads, so
    /// it is in the fan-out: leaving it out is how a movement would silently
    /// shorten an answer.
    pub fn readable_tablets(&self) -> Vec<&Tablet> {
        self.tablets
            .values()
            .filter(|t| t.state != TabletState::Retired)
            .collect()
    }

    /// Apply one command.
    ///
    /// The generation rises on every accepted change. A command that changes
    /// nothing does not raise it, so a replayed log ends at the generation the
    /// leader reached rather than past it.
    pub fn apply(&mut self, command: &ControllerCommand) -> Result<Generation, TopologyError> {
        let changed = self.apply_inner(command)?;
        if changed {
            self.generation += 1;
        }
        Ok(self.generation)
    }

    fn apply_inner(&mut self, command: &ControllerCommand) -> Result<bool, TopologyError> {
        match command {
            ControllerCommand::RegisterNode {
                node,
                address,
                region,
                domain,
            } => {
                let fresh = Node {
                    name: node.clone(),
                    address: address.clone(),
                    region: region.clone(),
                    domain: domain.clone(),
                    state: NodeState::Healthy,
                    cause: None,
                    since: 0,
                    writable: true,
                    free_bytes: 0,
                };
                match self.nodes.get(node) {
                    // Re-registering with the same coordinates changes nothing,
                    // which is what makes a replayed enrollment harmless. A
                    // node that moved keeps whatever health state it had.
                    Some(existing)
                        if existing.address == *address
                            && existing.region == *region
                            && existing.domain == *domain =>
                    {
                        Ok(false)
                    }
                    Some(existing) => {
                        let kept = existing.clone();
                        self.nodes.insert(
                            node.clone(),
                            Node {
                                address: address.clone(),
                                region: region.clone(),
                                domain: domain.clone(),
                                ..kept
                            },
                        );
                        Ok(true)
                    }
                    None => {
                        self.nodes.insert(node.clone(), fresh);
                        Ok(true)
                    }
                }
            }

            ControllerCommand::SetNodeCondition {
                node,
                state,
                cause,
                at,
                writable,
                free_bytes,
            } => {
                let existing = self.nodes.get_mut(node).ok_or_else(|| {
                    TopologyError::NotFound(format!("No node is named `{node}`."))
                })?;
                if existing.state == *state
                    && existing.cause == *cause
                    && existing.writable == *writable
                    && existing.free_bytes == *free_bytes
                {
                    return Ok(false);
                }
                // `since` moves only when the state itself moves, so an
                // operator reads "slow for 40 minutes" rather than "slow since
                // the last report".
                if existing.state != *state {
                    existing.since = *at;
                }
                existing.state = *state;
                existing.cause = cause.clone();
                existing.writable = *writable;
                existing.free_bytes = *free_bytes;
                Ok(true)
            }

            ControllerCommand::RegisterCell {
                cell,
                region,
                controllers,
            } => {
                let fresh = Cell {
                    name: cell.clone(),
                    region: region.clone(),
                    controllers: controllers.clone(),
                };
                if self.cells.get(cell) == Some(&fresh) {
                    return Ok(false);
                }
                self.cells.insert(cell.clone(), fresh);
                Ok(true)
            }

            ControllerCommand::CreateTablet {
                tablet,
                cell,
                region,
                shard_start,
                shard_end,
                members,
                receipt_policy,
            } => {
                if shard_start >= shard_end {
                    return Err(TopologyError::Refused(format!(
                        "A tablet's shard range must not be empty. `{tablet}` was asked for {shard_start}..{shard_end}."
                    )));
                }
                if self.tablets.contains_key(tablet) {
                    return Err(TopologyError::AlreadyDone(format!(
                        "A tablet named `{tablet}` already exists."
                    )));
                }
                if let Some(other) = self.tablets.values().find(|t| {
                    t.state != TabletState::Retired
                        && t.shard_start < *shard_end
                        && *shard_start < t.shard_end
                }) {
                    return Err(TopologyError::Refused(format!(
                        "Shards {shard_start}..{shard_end} already belong to `{}`. A shard has one owner.",
                        other.name
                    )));
                }
                let voters = members
                    .iter()
                    .filter(|m| m.role == MemberRole::Voter)
                    .count();
                check_policy(*receipt_policy, voters, tablet)?;
                self.tablets.insert(
                    tablet.clone(),
                    Tablet {
                        name: tablet.clone(),
                        cell: cell.clone(),
                        write_region: region.clone(),
                        epoch: 1,
                        state: TabletState::Active,
                        shard_start: *shard_start,
                        shard_end: *shard_end,
                        members: members.clone(),
                        receipt_policy: *receipt_policy,
                        degraded: None,
                    },
                );
                Ok(true)
            }

            ControllerCommand::AddReplica { tablet, member } => {
                let found = self.tablet_mut(tablet)?;
                if found.member(&member.node) == Some(member) {
                    return Ok(false);
                }
                found.members.retain(|m| m.node != member.node);
                found.members.push(member.clone());
                Ok(true)
            }

            ControllerCommand::RemoveReplica { tablet, node } => {
                let found = self.tablet_mut(tablet)?;
                let Some(leaving) = found.member(node).cloned() else {
                    return Ok(false);
                };
                if leaving.role == MemberRole::Voter {
                    let after = found.voter_count() - 1;
                    // Removing this voter must leave a group that can still
                    // commit. Going from three to two is legal and going to
                    // zero is not; two is a quorum of two, which survives no
                    // loss but still commits.
                    if after == 0 {
                        return Err(TopologyError::Refused(format!(
                            "Removing `{node}` would leave `{tablet}` with no voter, and a tablet with no voter accepts no write."
                        )));
                    }
                    check_policy(found.receipt_policy, after, tablet)?;
                }
                found.members.retain(|m| m.node != *node);
                Ok(true)
            }

            ControllerCommand::MoveReplica {
                tablet,
                away_from,
                onto,
            } => {
                let found = self.tablet_mut(tablet)?;
                if found.member(away_from).is_none() {
                    // The move already applied. A replayed entry lands here.
                    if found.member(&onto.node).is_some() {
                        return Ok(false);
                    }
                    return Err(TopologyError::NotFound(format!(
                        "`{away_from}` does not hold `{tablet}`."
                    )));
                }
                found.members.retain(|m| m.node != *away_from);
                found.members.retain(|m| m.node != onto.node);
                found.members.push(onto.clone());
                Ok(true)
            }

            ControllerCommand::SetTabletState { tablet, state } => {
                let found = self.tablet_mut(tablet)?;
                if found.state == *state {
                    return Ok(false);
                }
                found.state = *state;
                Ok(true)
            }

            ControllerCommand::SplitTablet {
                tablet,
                at_shard,
                right,
            } => {
                if self.tablets.contains_key(right) {
                    return Err(TopologyError::AlreadyDone(format!(
                        "A tablet named `{right}` already exists."
                    )));
                }
                let parent = self.tablet_mut(tablet)?;
                if *at_shard <= parent.shard_start || *at_shard >= parent.shard_end {
                    return Err(TopologyError::Refused(format!(
                        "`{tablet}` owns {}..{} and cannot split at {at_shard}. A split point must be inside the range and must leave both sides holding at least one shard.",
                        parent.shard_start, parent.shard_end
                    )));
                }
                let child = Tablet {
                    name: right.clone(),
                    shard_start: *at_shard,
                    shard_end: parent.shard_end,
                    epoch: parent.epoch,
                    state: TabletState::Active,
                    degraded: parent.degraded.clone(),
                    ..parent.clone()
                };
                parent.shard_end = *at_shard;
                parent.state = TabletState::Active;
                self.tablets.insert(right.clone(), child);
                Ok(true)
            }

            ControllerCommand::MergeTablets { left, right } => {
                let (left_end, right_start, right_end, right_members, right_degraded) = {
                    let r = self.tablet(right).ok_or_else(|| {
                        TopologyError::NotFound(format!("No tablet is named `{right}`."))
                    })?;
                    let l = self.tablet(left).ok_or_else(|| {
                        TopologyError::NotFound(format!("No tablet is named `{left}`."))
                    })?;
                    (
                        l.shard_end,
                        r.shard_start,
                        r.shard_end,
                        r.members.clone(),
                        r.degraded.clone(),
                    )
                };
                if left_end != right_start {
                    return Err(TopologyError::Refused(format!(
                        "`{left}` and `{right}` are not adjacent, so merging them would leave a hole. `{left}` ends at {left_end} and `{right}` starts at {right_start}."
                    )));
                }
                let left_members: Vec<NodeName> = self
                    .tablet(left)
                    .expect("checked above")
                    .members
                    .iter()
                    .map(|m| m.node.clone())
                    .collect();
                let right_names: Vec<NodeName> =
                    right_members.iter().map(|m| m.node.clone()).collect();
                if left_members != right_names {
                    return Err(TopologyError::Refused(format!(
                        "`{left}` and `{right}` are held by different nodes. Move one onto the other's replica set before merging."
                    )));
                }
                let merged = self.tablet_mut(left)?;
                merged.shard_end = right_end;
                // A merge that swallows a degraded range keeps the mark. A mark
                // that disappeared because two tablets became one would hide a
                // loss an operator has not accepted.
                if merged.degraded.is_none() {
                    merged.degraded = right_degraded;
                }
                self.tablets.remove(right);
                Ok(true)
            }

            ControllerCommand::SetReceiptPolicy { tablet, policy } => {
                let found = self.tablet_mut(tablet)?;
                if found.receipt_policy == *policy {
                    return Ok(false);
                }
                let voters = found.voter_count();
                check_policy(*policy, voters, tablet)?;
                found.receipt_policy = *policy;
                Ok(true)
            }

            ControllerCommand::FailOverRegion {
                tablet,
                onto_region,
            } => {
                let found = self.tablet_mut(tablet)?;
                if found.write_region == *onto_region {
                    return Ok(false);
                }
                if !found.members.iter().any(|m| m.region == *onto_region) {
                    return Err(TopologyError::Refused(format!(
                        "`{tablet}` has no replica in `{onto_region}`, so that region cannot become its write region. Add a replica there first."
                    )));
                }
                found.write_region = onto_region.clone();
                // The fence. A writer that still believes the old region is
                // refused by the epoch rather than accepted beside the new one.
                found.epoch += 1;
                Ok(true)
            }

            ControllerCommand::MarkDegraded { tablet, mark } => {
                let found = self.tablet_mut(tablet)?;
                if found.degraded.as_ref() == Some(mark) {
                    return Ok(false);
                }
                found.degraded = Some(mark.clone());
                let record = AuditRecord {
                    audit_id: mark.audit_id.clone(),
                    at: mark.since,
                    what: format!("unsafe recovery of `{tablet}`"),
                    detail: format!(
                        "The survivor held watermark {}. Writes the lost voters committed past it are gone. Reason: {}",
                        mark.survivor_watermark, mark.reason
                    ),
                };
                if !self.audit.iter().any(|a| a.audit_id == record.audit_id) {
                    self.audit.push(record);
                }
                Ok(true)
            }

            ControllerCommand::ClearDegraded {
                tablet,
                accepted_by,
                reason,
                at,
            } => {
                let audit_id = format!("cleared-{tablet}-{at}");
                let found = self.tablet_mut(tablet)?;
                if found.degraded.is_none() {
                    return Ok(false);
                }
                found.degraded = None;
                if !self.audit.iter().any(|a| a.audit_id == audit_id) {
                    self.audit.push(AuditRecord {
                        audit_id,
                        at: *at,
                        what: format!("degraded mark cleared on `{tablet}`"),
                        detail: format!("{accepted_by} accepted the loss. Reason: {reason}"),
                    });
                }
                Ok(true)
            }
        }
    }

    fn tablet_mut(&mut self, name: &str) -> Result<&mut Tablet, TopologyError> {
        self.tablets
            .get_mut(name)
            .ok_or_else(|| TopologyError::NotFound(format!("No tablet is named `{name}`.")))
    }
}

/// The one rule with no exception, in one place.
///
/// `AGENTS.md`: "Never acknowledge an uncommitted entry in a multi-voter group.
/// `local-one` is therefore legal only for a single-voter tablet." The refusal
/// says which rule it is, because an operator who reads "invalid policy" will
/// try again with the same value.
fn check_policy(policy: ReceiptPolicy, voters: usize, tablet: &str) -> Result<(), TopologyError> {
    if policy == ReceiptPolicy::LocalOne && voters > 1 {
        return Err(TopologyError::Refused(format!(
            "`{tablet}` has {voters} voters, and `local-one` acknowledges a write before the group commits it. Use `local-quorum` or `remote-one`."
        )));
    }
    Ok(())
}
