//! The registry: many groups on one node, and the one place threads meet async.
//!
//! A storage node holds one cell-controller group at most, one global-directory
//! group at most, and as many tablet groups as the controller placed on it. D27
//! measured 600 instances at 21 MiB, which is what makes "as many as the
//! controller placed on it" a real answer rather than a hopeful one.
//!
//! # The boundary
//!
//! Everything above this crate is synchronous: the head serves on threads, the
//! store commits on the calling thread, and the query executor blocks. openraft
//! is async. **This module is the only place the two meet**, and it meets them
//! in one direction at a time:
//!
//! - a synchronous caller enters through [`GroupRegistry::propose`] and friends,
//!   which block on the runtime;
//! - an async consensus task leaves through
//!   [`crate::raft::network::PeerLink`], which uses `spawn_blocking`.
//!
//! Keeping the bridge in one file is deliberate. A runtime handle that spread
//! through the head would make every later change an async question.
//!
//! # Why a group is not started until it is placed
//!
//! A node builds a group when the controller says it holds that tablet, and
//! stops it when the controller says it does not. Starting every group on every
//! node would give the cluster-wide consensus group that `AGENTS.md` forbids.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openraft::error::{ClientWriteError, RaftError};
use openraft::{BasicNode, ChangeMembers, Raft};
use tallyowl_cluster_api::codec::{decode_consensus_reply, encode_consensus_message};
use tallyowl_cluster_api::types::GroupKind as WireGroupKind;
use tallyowl_cluster_api::types::{ConsensusKind, ConsensusMessage};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};

use crate::raft::machine::{read_outcome, GroupMachine, Outcome};
use crate::raft::network::{GroupNetwork, PeerConnections, DELIVER_CONSENSUS, REPLICATION_SERVICE};
use crate::raft::storage::GroupStorage;
use crate::raft::{config_with, node_id, GroupRequest, NodeId, RaftHandle, TypeConfig};
use crate::topology::{Generation, Member, MemberRole, NodeName};

/// Which group a message or a command is for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GroupKey {
    /// The one global directory. `docs/STORAGE.md` section 7.
    GlobalDirectory,
    /// One cell's controller quorum.
    CellController(String),
    /// One tablet's replica set.
    Tablet(String),
}

impl GroupKey {
    pub fn name(&self) -> Option<&str> {
        match self {
            GroupKey::GlobalDirectory => None,
            GroupKey::CellController(name) | GroupKey::Tablet(name) => Some(name),
        }
    }

    pub fn kind_wire(&self) -> WireGroupKind {
        match self {
            GroupKey::GlobalDirectory => WireGroupKind::GlobalDirectory,
            GroupKey::CellController(_) => WireGroupKind::CellController,
            GroupKey::Tablet(_) => WireGroupKind::Tablet,
        }
    }

    pub fn from_wire(kind: WireGroupKind, name: Option<&str>) -> Result<GroupKey, TallyOwlError> {
        match (kind, name) {
            (WireGroupKind::GlobalDirectory, _) => Ok(GroupKey::GlobalDirectory),
            (WireGroupKind::CellController, Some(name)) => {
                Ok(GroupKey::CellController(name.to_string()))
            }
            (WireGroupKind::Tablet, Some(name)) => Ok(GroupKey::Tablet(name.to_string())),
            (kind, None) => Err(TallyOwlError::invalid_argument(format!(
                "A `{}` group message must name its group.",
                match kind {
                    WireGroupKind::CellController => "cell-controller",
                    _ => "tablet",
                }
            ))),
        }
    }

    /// The directory one group's durable state lives in.
    pub fn directory_name(&self) -> String {
        match self {
            GroupKey::GlobalDirectory => "global-directory".to_string(),
            GroupKey::CellController(name) => format!("cell-{name}"),
            GroupKey::Tablet(name) => format!("tablet-{name}"),
        }
    }

    /// What an operator reads.
    pub fn label(&self) -> String {
        match self {
            GroupKey::GlobalDirectory => "the global directory".to_string(),
            GroupKey::CellController(name) => format!("the `{name}` cell controllers"),
            GroupKey::Tablet(name) => format!("tablet `{name}`"),
        }
    }
}

/// What one group looks like from this node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupHealth {
    /// Why the group stopped on this node, when it did. A stopped group takes
    /// no write and no message until the node restarts.
    pub fatal: Option<String>,
    pub leader: Option<NodeName>,
    pub term: u64,
    pub last_applied: u64,
    pub last_log: u64,
    pub snapshot_index: u64,
    /// The last entry purged from the log. A replica behind this cannot catch
    /// up from the log and is sent a snapshot.
    pub purged_index: u64,
    /// How many entries the slowest replica is behind, when this node leads.
    pub worst_replication_lag: Option<u64>,
    pub snapshot_failures: u64,
    pub last_snapshot_failure: Option<String>,
}

/// Every group on one node, added up. See [`GroupRegistry::health`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeGroupsHealth {
    pub groups: u64,
    pub led_here: u64,
    /// Running, and with no leader this node knows of.
    pub leaderless: u64,
    /// Stopped on this node after a storage failure.
    pub stopped: u64,
    pub stopped_names: Vec<String>,
    /// Led here, with a replica further behind than the leader keeps log for.
    pub lagging: u64,
    pub worst_replication_lag: u64,
    pub worst_apply_lag: u64,
    pub snapshot_failures: u64,
}

/// One running group on this node.
struct RunningGroup {
    raft: RaftHandle,
    machine: Arc<dyn GroupMachine>,
    /// The group's durable state, kept so that unsafe recovery can rewrite it
    /// while the algorithm is stopped.
    storage: GroupStorage,
    generation: Generation,
    /// What went wrong on this group's storage path without stopping it.
    counters: Arc<crate::raft::storage::StorageCounters>,
    /// Set while this node must not vote. See [`GroupRegistry::hold_votes`].
    holding_votes: std::sync::atomic::AtomicBool,
    /// Set while a snapshot chunk is being taken. See
    /// [`GroupRegistry::deliver_snapshot`].
    installing: std::sync::atomic::AtomicBool,
    /// The members this node was told about, so a proposal can name a leader's
    /// address without asking the controller again.
    members: Mutex<Vec<Member>>,
}

/// Every group this node holds, and the runtime they share.
pub struct GroupRegistry {
    /// This node's name, as the control plane assigned it.
    node: NodeName,
    id: NodeId,
    address: String,
    /// Where durable consensus state lives. `None` keeps it in memory, which is
    /// what a test and a single-voter home tablet use.
    root: Option<PathBuf>,
    runtime: tokio::runtime::Runtime,
    connections: Arc<PeerConnections>,
    groups: Mutex<BTreeMap<GroupKey, Arc<RunningGroup>>>,
    /// Node names by their consensus number, so a collision is refused rather
    /// than silently merging two nodes into one identity.
    names: Mutex<BTreeMap<NodeId, NodeName>>,
    /// How long a proposal waits for its group to commit before it is refused.
    /// See [`GroupRegistry::propose`].
    write_timeout_ms: std::sync::atomic::AtomicU64,
    /// Committed entries between snapshots, and entries kept after one.
    ///
    /// L099 measured what these two bound: the consensus log on disk. They are
    /// read when a group starts, so a change reaches a group the next time this
    /// node runs it. See [`crate::raft::config_with`].
    snapshot_every: std::sync::atomic::AtomicU64,
    keep_after_snapshot: std::sync::atomic::AtomicU64,
    /// How much memory one group's log file may use as a cache.
    /// `replication.logCacheBytes`. See [`DEFAULT_LOG_CACHE_BYTES`].
    log_cache_bytes: std::sync::atomic::AtomicU64,
}

/// How long a write waits for a commit before it is refused.
///
/// It is longer than an election, so an ordinary leader change costs a retry
/// rather than a refusal, and short enough that an application is not left
/// holding a batch for a partition's lifetime. D27 measured 985 ms to replace
/// an isolated leader.
pub const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a membership change waits for the previous one to commit.
pub const MEMBERSHIP_PATIENCE: Duration = Duration::from_secs(20);

/// How much memory one group's log file may use as a cache.
///
/// The engine's own default is 1 GiB **for each file**, and a node holds one
/// file for each group, so a node with a few hundred tablets could be asked for
/// a few hundred gigabytes. A consensus log is appended at one end and purged
/// at the other; it has almost nothing worth caching.
pub const DEFAULT_LOG_CACHE_BYTES: u64 = 16 * 1024 * 1024;

/// How far behind the leader's log a replica may be before it counts as
/// lagging in [`GroupRegistry::health`]. One snapshot's worth: past this the
/// leader may have purged what the replica needs.
pub const LAGGING_AFTER_ENTRIES: u64 = crate::raft::KEEP_AFTER_SNAPSHOT;

impl GroupRegistry {
    /// Build a registry for one node.
    pub fn new(
        node: impl Into<NodeName>,
        address: impl Into<String>,
        root: Option<PathBuf>,
    ) -> Result<Arc<GroupRegistry>, TallyOwlError> {
        let node = node.into();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            // Consensus is not the throughput limit; D27 measured the storage
            // beneath it as the constraint. Two workers run many groups, and
            // every blocking call leaves the pool through `spawn_blocking`.
            .worker_threads(2)
            .thread_name("tallyowl-consensus")
            .enable_all()
            .build()
            .map_err(|e| {
                TallyOwlError::internal(format!("The consensus runtime would not start: {e}"))
            })?;
        let id = node_id(&node);
        let mut names = BTreeMap::new();
        names.insert(id, node.clone());
        Ok(Arc::new(GroupRegistry {
            node,
            id,
            address: address.into(),
            root,
            runtime,
            connections: Arc::new(PeerConnections::new()),
            groups: Mutex::new(BTreeMap::new()),
            names: Mutex::new(names),
            write_timeout_ms: std::sync::atomic::AtomicU64::new(
                DEFAULT_WRITE_TIMEOUT.as_millis() as u64
            ),
            snapshot_every: std::sync::atomic::AtomicU64::new(crate::raft::SNAPSHOT_EVERY_ENTRIES),
            keep_after_snapshot: std::sync::atomic::AtomicU64::new(
                crate::raft::KEEP_AFTER_SNAPSHOT,
            ),
            log_cache_bytes: std::sync::atomic::AtomicU64::new(DEFAULT_LOG_CACHE_BYTES),
        }))
    }

    pub fn node(&self) -> &str {
        &self.node
    }

    /// Use this transport to every peer. Call it before the first group starts:
    /// a connection already open keeps the transport it was opened with.
    pub fn set_security(&self, security: crate::security::PeerSecurity) {
        self.connections.set_security(security);
    }

    /// Which node answers at a member's address, so a client verifies the
    /// peer's certificate against that node's name.
    fn remember_address(&self, member: &Member) {
        self.connections
            .security()
            .remember(&member.address, &member.node);
    }

    /// The transport this node uses to its peers.
    pub fn security(&self) -> crate::security::PeerSecurity {
        self.connections.security()
    }

    pub fn node_id(&self) -> NodeId {
        self.id
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    /// How many peer connections are open. One for each peer, not one for each
    /// group; a test asserts that, because it is the property that makes the
    /// multi-group design scale.
    pub fn open_connections(&self) -> usize {
        self.connections.open_count()
    }

    pub fn holds(&self, group: &GroupKey) -> bool {
        self.groups.lock().expect("groups").contains_key(group)
    }

    pub fn group_count(&self) -> usize {
        self.groups.lock().expect("groups").len()
    }

    pub fn group_keys(&self) -> Vec<GroupKey> {
        self.groups
            .lock()
            .expect("groups")
            .keys()
            .cloned()
            .collect()
    }

    /// Remember a node's name against its consensus number.
    ///
    /// Two names that hash to one number would be one identity to consensus,
    /// which would let one node vote twice. This refuses the second name rather
    /// than letting that happen.
    pub fn learn(&self, name: &str) -> Result<NodeId, TallyOwlError> {
        let id = node_id(name);
        let mut names = self.names.lock().expect("node names");
        match names.get(&id) {
            Some(existing) if existing == name => Ok(id),
            Some(existing) => Err(TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                format!(
                    "`{name}` and `{existing}` reduce to the same consensus identity, so this cluster cannot hold both. Rename one of them."
                ),
            )),
            None => {
                names.insert(id, name.to_string());
                Ok(id)
            }
        }
    }

    pub fn name_of(&self, id: NodeId) -> Option<NodeName> {
        self.names.lock().expect("node names").get(&id).cloned()
    }

    /// Start one group on this node.
    ///
    /// `members` is what the controller placed. It is used to build the
    /// network; the group's own membership comes from its log, so a restart
    /// takes the membership it last committed rather than the one an argument
    /// happened to carry.
    pub fn start(
        &self,
        group: GroupKey,
        machine: Arc<dyn GroupMachine>,
        members: Vec<Member>,
        generation: Generation,
    ) -> Result<(), TallyOwlError> {
        if self.holds(&group) {
            return Ok(());
        }
        for member in &members {
            self.learn(&member.node)?;
            self.remember_address(member);
        }
        let storage = match &self.root {
            Some(root) => {
                let path = root.join(group.directory_name()).join("raft.redb");
                GroupStorage::open_with_cache(
                    &path,
                    Arc::clone(&machine),
                    self.log_cache_bytes
                        .load(std::sync::atomic::Ordering::Relaxed) as usize,
                )
            }
            None => GroupStorage::in_memory(Arc::clone(&machine)),
        }
        .map_err(|e| TallyOwlError::new(ErrorCode::FailedPrecondition, e))?;

        let counters = storage.counters();
        let raft = self.launch(&group, storage.clone(), generation)?;
        // A hold outlives a restart. A node that restarted part-way through
        // catching up has some log, so it no longer looks new, and it is no
        // safer to ask for its vote than it was before.
        let held = self
            .hold_marker(&group)
            .is_some_and(|marker| marker.exists());

        self.groups.lock().expect("groups").insert(
            group,
            Arc::new(RunningGroup {
                raft,
                machine,
                storage,
                generation,
                counters,
                holding_votes: std::sync::atomic::AtomicBool::new(held),
                installing: std::sync::atomic::AtomicBool::new(false),
                members: Mutex::new(members),
            }),
        );
        Ok(())
    }

    /// Run the algorithm over one group's storage.
    fn launch(
        &self,
        group: &GroupKey,
        storage: GroupStorage,
        generation: Generation,
    ) -> Result<RaftHandle, TallyOwlError> {
        let network = GroupNetwork {
            group: group.clone(),
            sender: self.node.clone(),
            generation,
            connections: Arc::clone(&self.connections),
        };
        self.runtime
            .block_on(Raft::new(
                self.id,
                {
                    let (every, keep) = self.log_bounds();
                    config_with(every, keep)
                },
                network,
                storage.clone(),
                storage,
            ))
            .map_err(|e| {
                TallyOwlError::internal(format!(
                    "{} could not be started on this node: {e}",
                    group.label()
                ))
            })
    }

    /// Stop one group on this node, because the controller moved it away.
    pub fn stop(&self, group: &GroupKey) {
        let removed = self.groups.lock().expect("groups").remove(group);
        if let Some(running) = removed {
            // Shut down inside the runtime that started it. A group dropped
            // without this leaves its tasks to be cancelled at runtime
            // shutdown, which is later than the caller believes.
            let _ = self.runtime.block_on(running.raft.shutdown());
        }
    }

    fn group(&self, key: &GroupKey) -> Result<Arc<RunningGroup>, TallyOwlError> {
        self.groups
            .lock()
            .expect("groups")
            .get(key)
            .cloned()
            .ok_or_else(|| {
                TallyOwlError::new(
                    ErrorCode::NotFound,
                    format!("This node does not hold {}.", key.label()),
                )
            })
    }

    /// One group's durable state, for a test that reads the log directly.
    #[cfg(test)]
    pub(crate) fn storage_of(&self, key: &GroupKey) -> Result<GroupStorage, TallyOwlError> {
        Ok(self.group(key)?.storage.clone())
    }

    pub fn machine(&self, key: &GroupKey) -> Result<Arc<dyn GroupMachine>, TallyOwlError> {
        Ok(Arc::clone(&self.group(key)?.machine))
    }

    /// Whether this node holds any consensus state for a group: a log entry, or
    /// a voter set it learned.
    ///
    /// A node that holds none is either new to the group or has lost its disk,
    /// and it cannot tell which. See [`GroupRegistry::hold_votes`].
    pub fn has_history(&self, key: &GroupKey) -> bool {
        let Ok(running) = self.group(key) else {
            return false;
        };
        let metrics = running.raft.metrics();
        let metrics = metrics.borrow();
        metrics.last_log_index.is_some() || metrics.membership_config.voter_ids().next().is_some()
    }

    /// Stop this node voting in one group until it holds everything the
    /// group's leader has committed.
    ///
    /// **This is for a node that came back with no consensus state into a group
    /// that already exists.** It may be a voter that lost its disk. Such a
    /// node remembers neither the entries it acknowledged nor the vote it
    /// cast, and its empty log makes every candidate look up to date: it would
    /// grant its vote to a replica that lacks a committed write, that replica
    /// would lead with it, and the write would be truncated on the replica
    /// that held it. `docs/FAILURE_MODES.md` procedure 2 has a replaced node
    /// join as a learner for this reason.
    ///
    /// The hold ends on its own, in [`GroupRegistry::deliver_append`], once a
    /// leader's entries have brought this node's log up to that leader's
    /// commit position. From there its vote is as safe as any other.
    pub fn hold_votes(&self, key: &GroupKey) {
        if let Ok(running) = self.group(key) {
            if let Some(marker) = self.hold_marker(key) {
                // Best effort. Without the file the hold still stands until
                // this process stops, which is the common case.
                let _ = std::fs::write(marker, b"");
            }
            running
                .holding_votes
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The file that says a hold is in force, for a group with durable state.
    fn hold_marker(&self, key: &GroupKey) -> Option<PathBuf> {
        self.root
            .as_ref()
            .map(|root| root.join(key.directory_name()).join("holding-votes"))
    }

    pub fn is_holding_votes(&self, key: &GroupKey) -> bool {
        self.group(key)
            .map(|running| {
                running
                    .holding_votes
                    .load(std::sync::atomic::Ordering::SeqCst)
            })
            .unwrap_or(false)
    }

    /// Create the voter set for a group that does not have one yet.
    ///
    /// This is the only operation that creates a voter set, and `AGENTS.md`
    /// says a role token can never reach it.
    pub fn bootstrap(&self, key: &GroupKey, members: &[Member]) -> Result<(), TallyOwlError> {
        let running = self.group(key)?;
        let mut initial = BTreeMap::new();
        for member in members.iter().filter(|m| m.role == MemberRole::Voter) {
            self.remember_address(member);
            initial.insert(
                self.learn(&member.node)?,
                BasicNode {
                    addr: member.address.clone(),
                },
            );
        }
        if initial.is_empty() {
            return Err(TallyOwlError::invalid_argument(format!(
                "{} cannot be created with no voter.",
                key.label()
            )));
        }
        *running.members.lock().expect("members") = members.to_vec();
        self.runtime
            .block_on(running.raft.initialize(initial))
            .map_err(|e| {
                TallyOwlError::new(
                    ErrorCode::FailedPrecondition,
                    format!("{} could not be created: {e}", key.label()),
                )
            })
    }

    /// Wait until the last membership change has committed.
    ///
    /// Consensus permits one configuration change at a time, so a caller that
    /// changed membership and immediately changed it again is refused. That is
    /// correct and it is not what an operator meant, so every membership change
    /// here waits for the previous one first. Bootstrap counts as one: the
    /// initial voter set is a membership entry like any other.
    pub fn await_membership_settled(
        &self,
        key: &GroupKey,
        within: Duration,
    ) -> Result<(), TallyOwlError> {
        let running = self.group(key)?;
        let mut metrics = running.raft.metrics();
        let deadline = std::time::Instant::now() + within;
        loop {
            {
                let now = metrics.borrow();
                let settled = match (now.membership_config.log_id(), now.last_applied) {
                    (Some(config), Some(applied)) => applied.index >= config.index,
                    _ => false,
                };
                if settled {
                    return Ok(());
                }
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(TallyOwlError::unavailable(format!(
                    "{} has a membership change that has not committed yet.",
                    key.label()
                )));
            }
            if self
                .runtime
                .block_on(async { tokio::time::timeout(left, metrics.changed()).await })
                .is_err()
            {
                return Err(TallyOwlError::unavailable(format!(
                    "{} has a membership change that has not committed yet.",
                    key.label()
                )));
            }
        }
    }

    /// Add a member. A learner catches up first and is promoted afterwards,
    /// which is what makes the change online: a voter added cold would hold up
    /// the quorum until it caught up.
    pub fn add_member(&self, key: &GroupKey, member: &Member) -> Result<(), TallyOwlError> {
        let running = self.group(key)?;
        self.await_membership_settled(key, MEMBERSHIP_PATIENCE)?;
        let id = self.learn(&member.node)?;
        self.remember_address(member);
        self.within_patience(
            key,
            &format!("adding `{}`", member.node),
            running.raft.add_learner(
                id,
                BasicNode {
                    addr: member.address.clone(),
                },
                true,
            ),
        )?
        .map_err(|e| {
            TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                format!("`{}` could not join {}: {e}", member.node, key.label()),
            )
        })?;
        if member.role == MemberRole::Voter {
            let mut voters = self.voter_ids(&running);
            voters.insert(id);
            self.change_voters(&running, key, voters)?;
        }
        let mut members = running.members.lock().expect("members");
        members.retain(|m| m.node != member.node);
        members.push(member.clone());
        Ok(())
    }

    /// Remove a member.
    pub fn remove_member(&self, key: &GroupKey, node: &str) -> Result<(), TallyOwlError> {
        let running = self.group(key)?;
        self.await_membership_settled(key, MEMBERSHIP_PATIENCE)?;
        let id = node_id(node);
        let mut voters = self.voter_ids(&running);
        if voters.remove(&id) {
            if voters.is_empty() {
                return Err(TallyOwlError::new(
                    ErrorCode::FailedPrecondition,
                    format!(
                        "Removing `{node}` would leave {} with no voter, and a group with no voter accepts no write.",
                        key.label()
                    ),
                ));
            }
            self.change_voters(&running, key, voters)?;
        }
        let mut removing = std::collections::BTreeSet::new();
        removing.insert(id);
        self.within_patience(
            key,
            &format!("removing `{node}`"),
            running
                .raft
                .change_membership(ChangeMembers::RemoveNodes(removing), false),
        )?
        .map_err(|e| {
            TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                format!("`{node}` could not leave {}: {e}", key.label()),
            )
        })?;
        running
            .members
            .lock()
            .expect("members")
            .retain(|m| m.node != node);
        Ok(())
    }

    /// Force a single-voter membership from this node's own log.
    ///
    /// `docs/FAILURE_MODES.md` section 6.2. This can lose an acknowledged
    /// write. Nothing here hides that: the caller in [`crate::recovery`] writes
    /// the audit record and the degraded mark, and this only does the part that
    /// needs the consensus handle.
    pub fn force_single_voter(&self, key: &GroupKey) -> Result<u64, TallyOwlError> {
        let running = self.group(key)?;
        // **Not through the group.** A voter set is changed by committing the
        // change, and the case this exists for is a group that can commit
        // nothing: two of three voters are gone, the survivor is not the
        // leader, and consensus rightly refuses it. So the algorithm is
        // stopped, this node's own durable state is rewritten to name it as
        // the only voter, and the algorithm is started again over that.
        let _ = self.runtime.block_on(running.raft.shutdown());
        self.groups.lock().expect("groups").remove(key);

        let rewritten = running
            .storage
            .force_single_voter(self.id, &self.address)
            .map_err(|e| TallyOwlError::new(ErrorCode::FailedPrecondition, e));

        // Started again whether or not the rewrite worked, so a refusal leaves
        // the group as it was and not stopped.
        let raft = self.launch(key, running.storage.clone(), running.generation)?;
        let alone: Vec<Member> = running
            .members
            .lock()
            .expect("members")
            .iter()
            .filter(|member| member.node == self.node)
            .cloned()
            .collect();
        let members = match &rewritten {
            Ok(_) => alone,
            Err(_) => running.members.lock().expect("members").clone(),
        };
        self.groups.lock().expect("groups").insert(
            key.clone(),
            Arc::new(RunningGroup {
                raft,
                machine: Arc::clone(&running.machine),
                storage: running.storage.clone(),
                generation: running.generation,
                counters: Arc::clone(&running.counters),
                holding_votes: std::sync::atomic::AtomicBool::new(false),
                installing: std::sync::atomic::AtomicBool::new(false),
                members: Mutex::new(members),
            }),
        );
        rewritten?;
        // Not `await_leader`: this node still remembers the leader it last
        // voted for, and that one is gone. What is waited for is this node
        // electing itself.
        self.await_leading(key, MEMBERSHIP_PATIENCE).map_err(|e| {
            TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                format!(
                    "{} was rewritten to one voter and did not then elect itself. {}",
                    key.label(),
                    e.message
                ),
            )
        })?;
        Ok(self.applied_index(key))
    }

    /// Run one membership change, and stop waiting for it after
    /// [`MEMBERSHIP_PATIENCE`].
    ///
    /// A membership change commits through the group, so a group that has lost
    /// its quorum never finishes one, and the operator's call never returned.
    /// The change may still commit later; the message says so.
    fn within_patience<T>(
        &self,
        key: &GroupKey,
        what: &str,
        change: impl std::future::Future<Output = T>,
    ) -> Result<T, TallyOwlError> {
        self.runtime
            .block_on(async { tokio::time::timeout(MEMBERSHIP_PATIENCE, change).await })
            .map_err(|_| {
                TallyOwlError::unavailable(format!(
                    "{} did not finish {what} within {} seconds. It may have no quorum. The change was not cancelled and may still commit; check the group's members before you try again.",
                    key.label(),
                    MEMBERSHIP_PATIENCE.as_secs()
                ))
            })
    }

    fn voter_ids(&self, running: &RunningGroup) -> std::collections::BTreeSet<NodeId> {
        running
            .raft
            .metrics()
            .borrow()
            .membership_config
            .voter_ids()
            .collect()
    }

    fn change_voters(
        &self,
        running: &RunningGroup,
        key: &GroupKey,
        voters: std::collections::BTreeSet<NodeId>,
    ) -> Result<(), TallyOwlError> {
        self.within_patience(
            key,
            "changing the voter set",
            running
                .raft
                .change_membership(ChangeMembers::ReplaceAllVoters(voters), false),
        )?
        .map(|_| ())
        .map_err(|e| {
            TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                format!("The voter set of {} could not be changed: {e}", key.label()),
            )
        })
    }

    /// Propose one command and wait for it to commit and apply.
    ///
    /// **This returns only after the group committed the entry.** A tablet with
    /// more than one voter therefore cannot acknowledge an uncommitted write,
    /// which is the rule `AGENTS.md` states with no exception.
    ///
    /// It also returns **within a bounded time**. A leader that has lost its
    /// quorum can keep an append outstanding for as long as the partition
    /// lasts, and consensus is right to do that: the entry may yet commit. A
    /// caller cannot wait that long, so the wait is bounded here and the
    /// refusal is retryable, because the batch ID makes the retry one logical
    /// commit whichever way the original went.
    pub fn propose(&self, key: &GroupKey, payload: Vec<u8>) -> Result<Outcome, TallyOwlError> {
        self.propose_within(key, payload, self.write_timeout())
    }

    pub fn propose_within(
        &self,
        key: &GroupKey,
        payload: Vec<u8>,
        within: Duration,
    ) -> Result<Outcome, TallyOwlError> {
        let running = self.group(key)?;
        let waited = self.runtime.block_on(async {
            tokio::time::timeout(
                within,
                running.raft.client_write(GroupRequest {
                    payload: payload.clone(),
                }),
            )
            .await
        });
        let response = match waited {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                // A voter that is not the leader knows who is. The proposal
                // goes there, one hop, over the connection every other
                // consensus message already uses — so ingest works on every
                // voter rather than on whichever one an election chose. The
                // Phase 11 soak found the difference on its first hour: a
                // three-voter cell whose ingest head was a follower parked
                // every batch for ever.
                if let RaftError::APIError(ClientWriteError::ForwardToLeader(forward)) = &error {
                    if let Some(node) = forward.leader_node.as_ref() {
                        let outcome = self.forward_proposal(key, &node.addr, payload)?;
                        return Ok(read_outcome(&outcome));
                    }
                }
                return Err(write_failure(key, error));
            }
            Err(_) => {
                return Err(TallyOwlError::unavailable(format!(
                    "{} did not commit the write within {} seconds. It may have no quorum. Send it again; the batch ID makes a retry one logical commit.",
                    key.label(),
                    within.as_secs_f32()
                )))
            }
        };
        Ok(read_outcome(&response.data.outcome))
    }

    /// Propose on this node, with no forward, and answer the encoded outcome.
    ///
    /// The server side of a forwarded proposal calls this, which is what
    /// bounds a proposal to one hop: two nodes that disagree about who leads
    /// produce a retryable refusal rather than a loop, and the sender's next
    /// attempt asks whoever leads by then.
    pub fn propose_local(
        &self,
        key: &GroupKey,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, TallyOwlError> {
        let running = self.group(key)?;
        let within = self.write_timeout();
        let waited = self.runtime.block_on(async {
            tokio::time::timeout(within, running.raft.client_write(GroupRequest { payload })).await
        });
        let response = match waited {
            Ok(result) => result.map_err(|e| write_failure(key, e))?,
            Err(_) => {
                return Err(TallyOwlError::unavailable(format!(
                    "{} did not commit the write within {} seconds. It may have no quorum. Send it again; the batch ID makes a retry one logical commit.",
                    key.label(),
                    within.as_secs_f32()
                )))
            }
        };
        Ok(response.data.outcome)
    }

    /// Hand one proposal to the leader at `address` and bring back the encoded
    /// outcome.
    ///
    /// The generation travels as zero, which skips the placement fence on
    /// purpose: the sender is a member of the same group, and membership is
    /// what authorizes a proposal. Routing decided nothing here.
    fn forward_proposal(
        &self,
        key: &GroupKey,
        address: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, TallyOwlError> {
        let message = ConsensusMessage {
            group: key.to_wire(),
            kind: ConsensusKind::Proposal,
            sender: self.node.clone(),
            generation: 0,
            payload,
        };
        // Its own connection. A proposal waits for a commit, and on the shared
        // one it held every heartbeat to that peer behind it.
        let client = self
            .connections
            .for_proposals(address, self.write_timeout());
        let response = client
            .call(
                REPLICATION_SERVICE,
                DELIVER_CONSENSUS,
                encode_consensus_message(&message),
            )
            .map_err(|e| {
                // A broken connection is the ordinary case when the leader
                // restarts. Drop it so the retry opens a fresh one.
                self.connections.forget(address, &client);
                write_failure(key, format!("the leader at {address} did not answer: {e}"))
            })?;
        let reply = decode_consensus_reply(&response.payload).map_err(|e| {
            write_failure(key, format!("the leader's answer could not be read: {e}"))
        })?;
        if !reply.accepted {
            return Err(write_failure(
                key,
                reply
                    .refusal
                    .unwrap_or_else(|| "the leader refused the proposal and gave no reason".into()),
            ));
        }
        reply.payload.ok_or_else(|| {
            write_failure(key, "the leader accepted the proposal and sent no outcome")
        })
    }

    /// How long a proposal waits before it is refused.
    pub fn set_write_timeout(&self, within: Duration) {
        self.write_timeout_ms.store(
            within.as_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// What bounds the consensus log on disk. A group reads these when it
    /// starts.
    pub fn set_log_bounds(&self, snapshot_every: u64, keep_after_snapshot: u64) {
        self.snapshot_every
            .store(snapshot_every, std::sync::atomic::Ordering::Relaxed);
        self.keep_after_snapshot
            .store(keep_after_snapshot, std::sync::atomic::Ordering::Relaxed);
    }

    /// How much memory one group's log file may cache. A group reads it when
    /// it starts.
    pub fn set_log_cache_bytes(&self, bytes: u64) {
        self.log_cache_bytes
            .store(bytes.max(1024 * 1024), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn log_bounds(&self) -> (u64, u64) {
        (
            self.snapshot_every
                .load(std::sync::atomic::Ordering::Relaxed),
            self.keep_after_snapshot
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    fn write_timeout(&self) -> Duration {
        Duration::from_millis(
            self.write_timeout_ms
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Hand one message from a peer to the algorithm.
    pub fn deliver_append(
        &self,
        key: &GroupKey,
        request: openraft::raft::AppendEntriesRequest<TypeConfig>,
    ) -> Result<openraft::raft::AppendEntriesResponse<NodeId>, TallyOwlError> {
        let running = self.group(key)?;
        // **A replica that cannot write does not take an entry it would then
        // acknowledge.** An acknowledged entry counts towards the quorum, and a
        // receipt would go to a client on the strength of a copy this node
        // cannot make. A heartbeat carries no entry and is still answered, so a
        // full disk does not also start an election.
        if !request.entries.is_empty() && !running.machine.is_writable() {
            return Err(TallyOwlError::new(
                ErrorCode::ResourceExhausted,
                format!(
                    "This node cannot write to its store, so it did not take new entries for {}. Free space on its data volume; it catches up on its own afterwards.",
                    key.label()
                ),
            ));
        }
        let leader_commit = request.leader_commit.map(|id| id.index).unwrap_or(0);
        let response = self
            .runtime
            .block_on(running.raft.append_entries(request))
            .map_err(|e| delivery_failure(key, e))?;
        // The end of a vote hold: a leader's entries were accepted, and this
        // node's log now reaches what that leader had committed.
        if running
            .holding_votes
            .load(std::sync::atomic::Ordering::SeqCst)
            && matches!(response, openraft::raft::AppendEntriesResponse::Success)
        {
            let held = running.raft.metrics().borrow().last_log_index.unwrap_or(0);
            if leader_commit > 0 && held >= leader_commit {
                // The file too, or the hold would come back at the next start.
                // If it cannot be removed the hold stays, which is the safe way
                // for that to fail.
                let cleared = self
                    .hold_marker(key)
                    .is_none_or(|marker| !marker.exists() || std::fs::remove_file(marker).is_ok());
                if cleared {
                    running
                        .holding_votes
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        Ok(response)
    }

    pub fn deliver_vote(
        &self,
        key: &GroupKey,
        request: openraft::raft::VoteRequest<NodeId>,
    ) -> Result<openraft::raft::VoteResponse<NodeId>, TallyOwlError> {
        let running = self.group(key)?;
        if running
            .holding_votes
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(TallyOwlError::unavailable(format!(
                "This node came back with no consensus state for {} and is catching up, so it does not vote yet. It votes again once it holds everything the leader has committed.",
                key.label()
            )));
        }
        self.runtime
            .block_on(running.raft.vote(request))
            .map_err(|e| delivery_failure(key, e))
    }

    pub fn deliver_snapshot(
        &self,
        key: &GroupKey,
        request: openraft::raft::InstallSnapshotRequest<TypeConfig>,
    ) -> Result<openraft::raft::InstallSnapshotResponse<NodeId>, TallyOwlError> {
        let running = self.group(key)?;
        // **One at a time.** The last chunk of a tablet snapshot does not
        // answer until the replica has copied the segments the snapshot stands
        // for, which can take minutes. The leader gives up on the call long
        // before that and sends the snapshot again, and each of those would
        // wait here on a thread of its own until the first one finished.
        if running
            .installing
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(TallyOwlError::unavailable(format!(
                "This node is still installing the last snapshot it was sent for {}. Send it again afterwards.",
                key.label()
            )));
        }
        let answer = self
            .runtime
            .block_on(running.raft.install_snapshot(request))
            .map_err(|e| delivery_failure(key, e));
        running
            .installing
            .store(false, std::sync::atomic::Ordering::SeqCst);
        answer
    }

    /// The leader of one group, as this node last saw it.
    ///
    /// **A group that stopped on this node has no leader as far as this node
    /// is concerned.** openraft keeps the last leader it saw in its metrics
    /// after a fatal error, and a caller that read only that would report a
    /// writable tablet whose every write then fails.
    pub fn leader(&self, key: &GroupKey) -> Option<NodeName> {
        let running = self.group(key).ok()?;
        let id = {
            let metrics = running.raft.metrics();
            let metrics = metrics.borrow();
            if metrics.running_state.is_err() {
                return None;
            }
            metrics.current_leader?
        };
        self.name_of(id)
    }

    /// What an operator needs to know about one group on this node.
    pub fn health_of(&self, key: &GroupKey) -> Option<GroupHealth> {
        let running = self.group(key).ok()?;
        let last_snapshot_failure = running
            .counters
            .last_snapshot_failure
            .lock()
            .expect("snapshot failure")
            .clone();
        let metrics = running.raft.metrics();
        let metrics = metrics.borrow();
        let last_log = metrics.last_log_index.unwrap_or(0);
        // A leader knows how far each replica got. A follower does not, and
        // reports none rather than a guess.
        let worst_lag = metrics.replication.as_ref().map(|replication| {
            replication
                .values()
                .map(|matched| last_log.saturating_sub(matched.map(|id| id.index).unwrap_or(0)))
                .max()
                .unwrap_or(0)
        });
        Some(GroupHealth {
            fatal: metrics
                .running_state
                .as_ref()
                .err()
                .map(|fatal| fatal.to_string()),
            leader: metrics.current_leader.and_then(|id| self.name_of(id)),
            term: metrics.current_term,
            last_applied: metrics.last_applied.map(|id| id.index).unwrap_or(0),
            last_log,
            snapshot_index: metrics.snapshot.map(|id| id.index).unwrap_or(0),
            purged_index: metrics.purged.map(|id| id.index).unwrap_or(0),
            worst_replication_lag: worst_lag,
            snapshot_failures: running
                .counters
                .snapshot_failures
                .load(std::sync::atomic::Ordering::Relaxed),
            last_snapshot_failure,
        })
    }

    /// Every group on this node, added up.
    ///
    /// **Added up rather than one series for each group**, because the number
    /// of groups on a node is whatever placement made it — D27 measured 600 —
    /// and a label for each would put that many series for each gauge into an
    /// operator's own monitoring. The counts say whether something is wrong;
    /// [`GroupRegistry::health_of`] and the cluster status operation say which.
    pub fn health(&self) -> NodeGroupsHealth {
        let mut summary = NodeGroupsHealth::default();
        for key in self.group_keys() {
            let Some(group) = self.health_of(&key) else {
                continue;
            };
            summary.groups += 1;
            if group.fatal.is_some() {
                summary.stopped += 1;
                summary.stopped_names.push(key.label());
            } else if group.leader.is_none() {
                summary.leaderless += 1;
            }
            if group.leader.as_deref() == Some(self.node.as_str()) && group.fatal.is_none() {
                summary.led_here += 1;
            }
            let behind = group.last_log.saturating_sub(group.last_applied);
            summary.worst_apply_lag = summary.worst_apply_lag.max(behind);
            if let Some(lag) = group.worst_replication_lag {
                summary.worst_replication_lag = summary.worst_replication_lag.max(lag);
                if lag > LAGGING_AFTER_ENTRIES {
                    summary.lagging += 1;
                }
            }
            summary.snapshot_failures += group.snapshot_failures;
        }
        summary
    }

    pub fn is_leader(&self, key: &GroupKey) -> bool {
        self.leader(key).as_deref() == Some(self.node.as_str())
    }

    /// The last log index this node applied. A tablet reports it as its
    /// watermark, and a read replica reports it so a bounded-stale read can
    /// decide whether this replica is current enough.
    pub fn applied_index(&self, key: &GroupKey) -> u64 {
        self.group(key)
            .ok()
            .and_then(|running| {
                running
                    .raft
                    .metrics()
                    .borrow()
                    .last_applied
                    .map(|log_id| log_id.index)
            })
            .unwrap_or(0)
    }

    /// Whether at least one of these nodes has persisted up to `index`.
    ///
    /// This reads the leader's own replication state. Asking a follower would
    /// be asking the wrong party: what matters for a `remote-one` receipt is
    /// what the leader knows the follower persisted, because that is what
    /// survives the leader's loss.
    pub fn replicated_to(&self, key: &GroupKey, nodes: &[NodeName], index: u64) -> bool {
        let Ok(running) = self.group(key) else {
            return false;
        };
        let metrics = running.raft.metrics();
        let metrics = metrics.borrow();
        let Some(replication) = metrics.replication.as_ref() else {
            // Not the leader. A follower has no view of anybody else's
            // progress, so it cannot answer this and must not guess.
            return false;
        };
        nodes.iter().any(|name| {
            let id = node_id(name);
            // This node's own progress is not in the replication map, because a
            // leader does not replicate to itself. It has the entry by
            // definition once it is applied.
            if id == self.id {
                return metrics
                    .last_applied
                    .map(|log_id| log_id.index >= index)
                    .unwrap_or(false);
            }
            replication
                .get(&id)
                .and_then(|matched| *matched)
                .map(|log_id| log_id.index >= index)
                .unwrap_or(false)
        })
    }

    /// Whether a node name belongs to one group, as far as this node knows.
    ///
    /// Either source counts: the members this node was told when the group
    /// started or changed, and the voter set and learners the group's own log
    /// holds. A node that knows of no member at all accepts, because it is a
    /// new learner and the first message it gets is from a leader it has not
    /// heard of yet.
    pub fn knows_member(&self, key: &GroupKey, name: &str) -> bool {
        let Ok(running) = self.group(key) else {
            return false;
        };
        let told = running.members.lock().expect("members");
        if told.iter().any(|member| member.node == name) {
            return true;
        }
        let id = node_id(name);
        let metrics = running.raft.metrics();
        let metrics = metrics.borrow();
        let mut known = metrics
            .membership_config
            .nodes()
            .map(|(id, _)| *id)
            .peekable();
        if told.is_empty() && known.peek().is_none() {
            return true;
        }
        known.any(|member| member == id)
    }

    pub fn members(&self, key: &GroupKey) -> Vec<Member> {
        self.group(key)
            .map(|running| running.members.lock().expect("members").clone())
            .unwrap_or_default()
    }

    /// Wait until this group has a leader, or give up.
    ///
    /// A caller that needs to write immediately after bootstrap uses this
    /// rather than sleeping, which is the difference between a test that is
    /// stable and one that is stable on this machine.
    pub fn await_leader(
        &self,
        key: &GroupKey,
        within: Duration,
    ) -> Result<NodeName, TallyOwlError> {
        let running = self.group(key)?;
        let mut metrics = running.raft.metrics();
        let deadline = std::time::Instant::now() + within;
        loop {
            if let Some(id) = metrics.borrow().current_leader {
                if let Some(name) = self.name_of(id) {
                    return Ok(name);
                }
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(TallyOwlError::unavailable(format!(
                    "{} has no leader yet. A write cannot be accepted until it elects one.",
                    key.label()
                )));
            }
            let changed = self
                .runtime
                .block_on(async { tokio::time::timeout(left, metrics.changed()).await });
            if changed.is_err() {
                return Err(TallyOwlError::unavailable(format!(
                    "{} has no leader yet. A write cannot be accepted until it elects one.",
                    key.label()
                )));
            }
        }
    }

    /// Wait until **this node** leads the group, or give up.
    pub fn await_leading(&self, key: &GroupKey, within: Duration) -> Result<(), TallyOwlError> {
        let running = self.group(key)?;
        let mut metrics = running.raft.metrics();
        let deadline = std::time::Instant::now() + within;
        loop {
            if metrics.borrow().current_leader == Some(self.id) {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let changed = self
                .runtime
                .block_on(async { tokio::time::timeout(left, metrics.changed()).await });
            if left.is_zero() || changed.is_err() {
                return Err(TallyOwlError::unavailable(format!(
                    "This node did not become the leader of {} within {} seconds.",
                    key.label(),
                    within.as_secs()
                )));
            }
        }
    }

    /// Take a snapshot of one group now, rather than waiting for the policy.
    pub fn snapshot_now(&self, key: &GroupKey) -> Result<(), TallyOwlError> {
        let running = self.group(key)?;
        self.runtime
            .block_on(running.raft.trigger().snapshot())
            .map_err(|e| {
                TallyOwlError::internal(format!(
                    "A snapshot of {} could not be taken: {e}",
                    key.label()
                ))
            })
    }

    /// Stop every group. A process shutting down calls this so that a group's
    /// last write is flushed rather than cancelled.
    pub fn shutdown(&self) {
        let keys: Vec<GroupKey> = self.group_keys();
        for key in keys {
            self.stop(&key);
        }
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }
}

fn write_failure(key: &GroupKey, error: impl std::fmt::Display) -> TallyOwlError {
    // A write that could not commit is retryable: the group may elect a leader
    // a moment later, and the batch ID makes the retry one logical commit.
    TallyOwlError::unavailable(format!(
        "{} could not commit the write. {error}",
        key.label()
    ))
}

fn delivery_failure(key: &GroupKey, error: impl std::fmt::Display) -> TallyOwlError {
    TallyOwlError::unavailable(format!(
        "{} could not take the message. {error}",
        key.label()
    ))
}
