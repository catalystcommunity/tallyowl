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
//! **This is why L058 had to be fixed first.** A connection that served one
//! request at a time would have serialized every group behind whichever one
//! happened to be sending, so the multiplexing would have been a fiction and
//! any measurement of the replicated write path would have measured the
//! carrier. [`tallyowl_rpc::Pipeline`] keeps several calls outstanding on one
//! connection and answers them as they finish.
//!
//! # Blocking calls under an async runtime
//!
//! TallyOwl's transport is synchronous, and openraft's network trait is async.
//! Every call here goes through `spawn_blocking`, so a blocking socket read
//! never occupies a runtime worker. That is the whole of the bridge, and it is
//! in one place on purpose.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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

/// One connection for each peer, shared by every group that peer holds.
#[derive(Default)]
pub struct PeerConnections {
    peers: Mutex<HashMap<String, Arc<Client>>>,
}

impl PeerConnections {
    pub fn new() -> PeerConnections {
        PeerConnections::default()
    }

    /// The connection to one address, opening it if this is the first group to
    /// ask for it.
    pub fn to(&self, address: &str) -> Arc<Client> {
        let mut peers = self.peers.lock().expect("peer connections");
        Arc::clone(
            peers
                .entry(address.to_string())
                .or_insert_with(|| Arc::new(Client::new(address.to_string(), MAX_FRAME_BYTES))),
        )
    }

    /// Drop the connection to one address. The next message opens a fresh one.
    pub fn forget(&self, address: &str) {
        self.peers.lock().expect("peer connections").remove(address);
    }

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
        let client = self.connections.to(&self.address);
        let address = self.address.clone();
        let connections = Arc::clone(&self.connections);

        let response = tokio::task::spawn_blocking(move || {
            client.call(REPLICATION_SERVICE, DELIVER_CONSENSUS, encoded)
        })
        .await
        .map_err(|e| format!("The consensus call could not be run: {e}"))?;

        let response = match response {
            Ok(response) => response,
            Err(e) => {
                // A broken connection is the ordinary case when a peer
                // restarts. Drop it so the next message opens a fresh one
                // rather than retrying into a dead socket forever.
                connections.forget(&address);
                return Err(e.to_string());
            }
        };
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
