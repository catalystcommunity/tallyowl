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

/// One running group on this node.
struct RunningGroup {
    raft: RaftHandle,
    machine: Arc<dyn GroupMachine>,
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
        }))
    }

    pub fn node(&self) -> &str {
        &self.node
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
        }
        let storage = match &self.root {
            Some(root) => {
                let path = root.join(group.directory_name()).join("raft.redb");
                GroupStorage::open(&path, Arc::clone(&machine))
            }
            None => GroupStorage::in_memory(Arc::clone(&machine)),
        }
        .map_err(|e| TallyOwlError::new(ErrorCode::FailedPrecondition, e))?;

        let network = GroupNetwork {
            group: group.clone(),
            sender: self.node.clone(),
            generation,
            connections: Arc::clone(&self.connections),
        };

        let raft = self
            .runtime
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
            })?;

        self.groups.lock().expect("groups").insert(
            group,
            Arc::new(RunningGroup {
                raft,
                machine,
                members: Mutex::new(members),
            }),
        );
        Ok(())
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

    pub fn machine(&self, key: &GroupKey) -> Result<Arc<dyn GroupMachine>, TallyOwlError> {
        Ok(Arc::clone(&self.group(key)?.machine))
    }

    /// Create the voter set for a group that does not have one yet.
    ///
    /// This is the only operation that creates a voter set, and `AGENTS.md`
    /// says a role token can never reach it.
    pub fn bootstrap(&self, key: &GroupKey, members: &[Member]) -> Result<(), TallyOwlError> {
        let running = self.group(key)?;
        let mut initial = BTreeMap::new();
        for member in members.iter().filter(|m| m.role == MemberRole::Voter) {
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
        self.runtime
            .block_on(running.raft.add_learner(
                id,
                BasicNode {
                    addr: member.address.clone(),
                },
                true,
            ))
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
        self.runtime
            .block_on(
                running
                    .raft
                    .change_membership(ChangeMembers::RemoveNodes(removing), false),
            )
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
        // A forced change does not wait for a quorum it does not have, so the
        // settle is best effort: it makes the ordinary case clean and never
        // blocks the case this exists for.
        let _ = self.await_membership_settled(key, Duration::from_millis(500));
        let mut alone = std::collections::BTreeSet::new();
        alone.insert(self.id);
        self.runtime
            .block_on(
                running
                    .raft
                    .change_membership(ChangeMembers::ReplaceAllVoters(alone), true),
            )
            .map_err(|e| {
                TallyOwlError::new(
                    ErrorCode::FailedPrecondition,
                    format!("{} could not be forced to one voter: {e}", key.label()),
                )
            })?;
        Ok(self.applied_index(key))
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
        self.runtime
            .block_on(
                running
                    .raft
                    .change_membership(ChangeMembers::ReplaceAllVoters(voters), false),
            )
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
        let client = self.connections.to(address);
        let response = client
            .call(
                REPLICATION_SERVICE,
                DELIVER_CONSENSUS,
                encode_consensus_message(&message),
            )
            .map_err(|e| {
                // A broken connection is the ordinary case when the leader
                // restarts. Drop it so the retry opens a fresh one.
                self.connections.forget(address);
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
        self.runtime
            .block_on(running.raft.append_entries(request))
            .map_err(|e| delivery_failure(key, e))
    }

    pub fn deliver_vote(
        &self,
        key: &GroupKey,
        request: openraft::raft::VoteRequest<NodeId>,
    ) -> Result<openraft::raft::VoteResponse<NodeId>, TallyOwlError> {
        let running = self.group(key)?;
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
        self.runtime
            .block_on(running.raft.install_snapshot(request))
            .map_err(|e| delivery_failure(key, e))
    }

    /// The leader of one group, as this node last saw it.
    pub fn leader(&self, key: &GroupKey) -> Option<NodeName> {
        let running = self.group(key).ok()?;
        let id = running.raft.metrics().borrow().current_leader?;
        self.name_of(id)
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
