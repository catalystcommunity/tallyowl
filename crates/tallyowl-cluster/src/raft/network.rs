//! Many consensus groups over shared CSIL connections.
//!
//! `docs/STORAGE.md` section 7: "A storage node may host many independent
//! tablet groups, multiplexed over shared CSIL connections and runtime
//! threads." This module is that sentence.
//!
//! # What multiplexed means here, concretely
//!
//! One connection for each *peer*, not for each group. A node that holds two
//! hundred tablets with a given peer opens one connection to it, and every
//! group's messages travel over that one, each carrying the group it belongs to
//! in its envelope. Two hundred connections for two hundred groups would put
//! the connection count at the product of the node count and the tablet count,
//! which is the shape that stops this design scaling.
//!
//! **What this is not, yet.** The connection to a peer carries one call at a
//! time: a [`tallyowl_rpc::Client`] holds its connection for a whole round
//! trip. Every group that shares a peer therefore waits behind whichever one is
//! sending. [`tallyowl_rpc::Pipeline`] keeps several calls outstanding on one
//! connection, and moving this module onto it, with a reader that hands each
//! reply to the call it answers, is the change that would make the sharing
//! free. Until then three things keep one slow call from costing every group:
//! a forwarded proposal has a connection of its own, because it waits for a
//! commit; every socket call has a deadline; and a peer holds a few waiting
//! calls and refuses the rest, so a peer that stops answering cannot take every
//! blocking thread on the node.
//!
//! # Blocking calls under an async runtime
//!
//! TallyOwl's transport is synchronous, and openraft's network trait is async.
//! Every call here goes through `spawn_blocking`, so a blocking socket read
//! never occupies a runtime worker. That is the whole of the bridge, and it is
//! in one place on purpose.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openraft::error::{InstallSnapshotError, RPCError, RaftError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::BasicNode;

use tallyowl_cluster_api::codec::{decode_consensus_reply, encode_consensus_message};
use tallyowl_cluster_api::types::{ConsensusKind, ConsensusMessage, ConsensusReply, GroupRef};
use tallyowl_rpc::Client;

use super::{NodeId, TypeConfig};
use crate::groups::GroupKey;

/// The CSIL service and operation a consensus message travels on.
pub const REPLICATION_SERVICE: &str = "TallyOwlReplication";
pub const DELIVER_CONSENSUS: &str = "deliver-consensus";

/// How large one consensus frame may be.
///
/// An append with a full batch of telemetry is the largest thing that travels
/// here. D27 measured 64 KiB entries at 12,543 each second; this bounds one
/// message rather than one entry, and a receiver refuses an oversized frame
/// before it allocates for it.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// How long a consensus message waits to connect, and for one read or write.
///
/// openraft gives up on an append after one heartbeat interval and sends
/// another, but the thread that carried the first one stays in its socket call
/// until the socket lets go. These bound that. They are short because a
/// consensus message is answered from memory and one fsync, and long enough
/// for a full append on a slow device.
pub const CONSENSUS_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
pub const CONSENSUS_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// How many consensus calls to one peer may be waiting at once.
///
/// One connection carries one call at a time, so a call behind this many is
/// already late. Refusing it costs openraft one retry on its next tick; taking
/// it costs a blocking thread, and a dark peer used to take them until the pool
/// of 512 was full and no group on the node could reach any peer.
pub const MAX_WAITING_FOR_ONE_PEER: usize = 4;

/// One peer: its connections, and how many calls are waiting on the first.
pub struct Peer {
    /// Consensus messages: appends, votes, and snapshot chunks.
    pub consensus: Arc<Client>,
    waiting: AtomicUsize,
}

/// A place in one peer's short queue. Dropping it gives the place back.
pub struct Waiting(Arc<Peer>);

impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Peer {
    /// Take a place in this peer's queue, or learn that it is full.
    pub fn wait(self: &Arc<Peer>) -> Option<Waiting> {
        let taken = self
            .waiting
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |now| {
                (now < MAX_WAITING_FOR_ONE_PEER).then_some(now + 1)
            });
        taken.ok().map(|_| Waiting(Arc::clone(self)))
    }
}

/// One connection for each peer, shared by every group that peer holds.
///
/// **And one more for forwarded proposals.** A proposal waits for a commit,
/// which is up to the write timeout, and the connection carries one call at a
/// time. On the shared connection a forwarded write held every heartbeat to
/// that peer behind it, the peer's election timer fired, and leadership
/// flapped under ingest load.
#[derive(Default)]
pub struct PeerConnections {
    peers: Mutex<HashMap<String, Arc<Peer>>>,
    proposals: Mutex<HashMap<String, Arc<Client>>>,
    /// The transport every connection here uses. D62.
    security: std::sync::RwLock<crate::security::PeerSecurity>,
}

impl PeerConnections {
    pub fn new() -> PeerConnections {
        PeerConnections::default()
    }

    /// Use this transport for every connection opened after this call.
    pub fn set_security(&self, security: crate::security::PeerSecurity) {
        let mut held = self.security.write().expect("peer security");
        security.adopt_names(&held);
        *held = security;
    }

    pub fn security(&self) -> crate::security::PeerSecurity {
        self.security.read().expect("peer security").clone()
    }

    /// The peer at one address, opening its connection if this is the first
    /// group to ask for it.
    pub fn peer(&self, address: &str) -> Arc<Peer> {
        let security = self.security();
        let mut peers = self.peers.lock().expect("peer connections");
        Arc::clone(peers.entry(address.to_string()).or_insert_with(|| {
            Arc::new(Peer {
                consensus: Arc::new(security.client(
                    address,
                    MAX_FRAME_BYTES,
                    CONSENSUS_CONNECT_TIMEOUT,
                    CONSENSUS_IO_TIMEOUT,
                )),
                waiting: AtomicUsize::new(0),
            })
        }))
    }

    /// The consensus connection to one address.
    pub fn to(&self, address: &str) -> Arc<Client> {
        Arc::clone(&self.peer(address).consensus)
    }

    /// The connection forwarded proposals use. `patience` is how long a
    /// proposal may wait for its commit, and the socket waits a little longer
    /// so that the leader's own refusal arrives rather than a timeout.
    pub fn for_proposals(&self, address: &str, patience: Duration) -> Arc<Client> {
        let security = self.security();
        let mut proposals = self.proposals.lock().expect("proposal connections");
        Arc::clone(proposals.entry(address.to_string()).or_insert_with(|| {
            Arc::new(security.client(
                address,
                MAX_FRAME_BYTES,
                CONSENSUS_CONNECT_TIMEOUT,
                patience + CONSENSUS_IO_TIMEOUT,
            ))
        }))
    }

    /// Drop one connection to one address. The next message opens a fresh one.
    ///
    /// **Only if it is still the one that failed.** A call that was abandoned
    /// long ago fails late, and by then another call may have opened a new
    /// connection that works. Forgetting by address alone threw that one away.
    pub fn forget(&self, address: &str, failed: &Arc<Client>) {
        let mut peers = self.peers.lock().expect("peer connections");
        if peers
            .get(address)
            .is_some_and(|peer| Arc::ptr_eq(&peer.consensus, failed))
        {
            peers.remove(address);
        }
        drop(peers);
        let mut proposals = self.proposals.lock().expect("proposal connections");
        if proposals
            .get(address)
            .is_some_and(|held| Arc::ptr_eq(held, failed))
        {
            proposals.remove(address);
        }
    }

    /// How many peers this node has a consensus connection to.
    pub fn open_count(&self) -> usize {
        self.peers.lock().expect("peer connections").len()
    }
}

/// Builds a network for one group.
#[derive(Clone)]
pub struct GroupNetwork {
    pub group: GroupKey,
    pub sender: String,
    pub generation: u64,
    pub connections: Arc<PeerConnections>,
}

/// One group's link to one peer.
#[derive(Clone)]
pub struct PeerLink {
    group: GroupKey,
    sender: String,
    generation: u64,
    target: NodeId,
    address: String,
    connections: Arc<PeerConnections>,
}

impl RaftNetworkFactory<TypeConfig> for GroupNetwork {
    type Network = PeerLink;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> Self::Network {
        PeerLink {
            group: self.group.clone(),
            sender: self.sender.clone(),
            generation: self.generation,
            target,
            address: node.addr.clone(),
            connections: Arc::clone(&self.connections),
        }
    }
}

fn unreachable<E: std::error::Error + 'static>(
    address: &str,
    why: impl std::fmt::Display,
) -> RPCError<NodeId, BasicNode, E> {
    RPCError::Unreachable(Unreachable::new(&std::io::Error::other(format!(
        "{address} did not answer: {why}"
    ))))
}

impl PeerLink {
    /// Send one consensus message and bring back the answer's payload.
    ///
    /// The whole synchronous transport is behind `spawn_blocking`, so a peer
    /// that stops answering costs one blocking thread rather than a runtime
    /// worker.
    async fn deliver(&self, kind: ConsensusKind, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        let message = ConsensusMessage {
            group: self.group.to_wire(),
            kind,
            sender: self.sender.clone(),
            generation: self.generation,
            payload,
        };
        let encoded = encode_consensus_message(&message);
        let peer = self.connections.peer(&self.address);
        let Some(place) = peer.wait() else {
            return Err(format!(
                "{} already has {MAX_WAITING_FOR_ONE_PEER} consensus messages waiting on it, so this one was not queued behind them.",
                self.address
            ));
        };
        let client = Arc::clone(&peer.consensus);
        let address = self.address.clone();
        let connections = Arc::clone(&self.connections);

        let response = tokio::task::spawn_blocking(move || {
            // The place is held for as long as the socket call runs, which is
            // longer than openraft waits for it.
            let _place = place;
            let response = client.call(REPLICATION_SERVICE, DELIVER_CONSENSUS, encoded);
            if response.is_err() {
                // A broken connection is the ordinary case when a peer
                // restarts. Drop it so the next message opens a fresh one
                // rather than retrying into a dead socket forever.
                connections.forget(&address, &client);
            }
            response
        })
        .await
        .map_err(|e| format!("The consensus call could not be run: {e}"))?;

        let response = response.map_err(|e| e.to_string())?;
        let reply: ConsensusReply = decode_consensus_reply(&response.payload)
            .map_err(|e| format!("The consensus reply could not be read: {e}"))?;
        if !reply.accepted {
            return Err(reply.refusal.unwrap_or_else(|| {
                "The peer refused the message and gave no reason.".to_string()
            }));
        }
        reply
            .payload
            .ok_or_else(|| "The peer accepted the message and sent no answer.".to_string())
    }
}

impl RaftNetwork<TypeConfig> for PeerLink {
    async fn append_entries(
        &mut self,
        request: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        let payload = super::encode(&request).map_err(|e| unreachable(&self.address, e))?;
        let answer = self
            .deliver(ConsensusKind::AppendEntries, payload)
            .await
            .map_err(|e| unreachable(&self.address, e))?;
        super::decode(&answer).map_err(|e| unreachable(&self.address, e))
    }

    async fn vote(
        &mut self,
        request: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        let payload = super::encode(&request).map_err(|e| unreachable(&self.address, e))?;
        let answer = self
            .deliver(ConsensusKind::Vote, payload)
            .await
            .map_err(|e| unreachable(&self.address, e))?;
        super::decode(&answer).map_err(|e| unreachable(&self.address, e))
    }

    async fn install_snapshot(
        &mut self,
        request: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<NodeId, BasicNode, RaftError<NodeId, InstallSnapshotError>>,
    > {
        let payload = super::encode(&request).map_err(|e| unreachable(&self.address, e))?;
        let answer = self
            .deliver(ConsensusKind::InstallSnapshot, payload)
            .await
            .map_err(|e| unreachable(&self.address, e))?;
        super::decode(&answer).map_err(|e| unreachable(&self.address, e))
    }
}

impl PeerLink {
    pub fn target(&self) -> NodeId {
        self.target
    }
}

/// A group reference, in the shape the contract declares.
impl GroupKey {
    pub fn to_wire(&self) -> GroupRef {
        GroupRef {
            kind: self.kind_wire(),
            name: self.name().map(|n| n.to_string()),
        }
    }
}
