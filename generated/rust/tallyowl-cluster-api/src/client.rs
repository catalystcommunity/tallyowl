//! Generated transport-agnostic service clients from CSIL specification

use super::codec::*;
use super::types::*;

/// Error from a generated client call: a structured error the service returned,
/// or a transport-level failure. The caller-supplied `Transport` decides how an
/// error response maps onto `Service`. `Encode` means the request has no valid
/// wire form, so the client did not call the transport.
#[derive(Debug, Clone)]
pub enum ClientError {
    Service { code: i64, message: String },
    Transport(String),
    Encode(String),
    ApplicationServiceError(Box<ServiceError>),
    Decode(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Service { code, message } => write!(f, "service error {code}: {message}"),
            Self::Encode(msg) => write!(f, "invalid request: {msg}"),
            Self::ApplicationServiceError(e) => write!(f, "application error: {e:?}"),
            Self::Decode(msg) => write!(f, "invalid reply: {msg}"),
            ClientError::Transport(msg) => write!(f, "transport error: {msg}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// A status-zero CSIL-RPC reply: `variant` names the declared output arm that
/// `payload` encodes. A transport returns it from `call_reply`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireReply {
    pub variant: Option<String>,
    pub payload: Vec<u8>,
}

/// The caller-supplied byte carrier: it performs the call named by `(service, op)`
/// with the already-encoded request bytes and returns the response bytes, or an
/// error. The generated client owns (de)serialization via the codec; the carrier
/// only moves bytes, so it can be HTTP, a queue, or an in-process loop.
pub trait Transport {
    fn call(&self, service: &str, op: &str, req: &[u8]) -> Result<Vec<u8>, ClientError>;

    /// Perform a call whose operation declares several reply arms and return
    /// the status-zero reply `variant` with its payload, so the client can
    /// return the success arm or the declared error arm.
    fn call_reply(&self, service: &str, op: &str, req: &[u8]) -> Result<WireReply, ClientError>;
}
impl From<CsilCborError> for ClientError {
    fn from(error: CsilCborError) -> Self {
        Self::Decode(error.0)
    }
}

/// Resolve a status-zero RPC reply into its success payload or its declared error
/// arm. Generated clients call it with the reply from `Transport::call_reply`.
pub fn resolve_rpc_reply(
    service: &str,
    op: &str,
    variant: Option<&str>,
    payload: Vec<u8>,
) -> Result<Vec<u8>, ClientError> {
    match (service, op) {
        ("TallyOwlReplication", "deliver-consensus") => match variant {
            Some("ConsensusReply") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlReplication", "fetch-snapshot-chunk") => match variant {
            Some("SnapshotChunk") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlReplication", "fetch-segment") => match variant {
            Some("SegmentTransfer") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlReplication", "partial-aggregate") => match variant {
            Some("PartialQueryResponse") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlReplication", "replica-status") => match variant {
            Some("ReplicaStatus") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlReplication", "report-health") => match variant {
            Some("ClusterAck") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlReplication", "list-segments") => match variant {
            Some("SegmentList") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "describe-topology") => match variant {
            Some("TopologyResponse") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "route") => match variant {
            Some("RouteResponse") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "bootstrap-group") => match variant {
            Some("ClusterAck") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "add-replica") => match variant {
            Some("ClusterAck") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "remove-replica") => match variant {
            Some("ClusterAck") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "split-tablet") => match variant {
            Some("ClusterAck") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "merge-tablets") => match variant {
            Some("ClusterAck") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "move-tablet") => match variant {
            Some("ClusterAck") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "set-receipt-policy") => match variant {
            Some("ClusterAck") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "fail-over-region") => match variant {
            Some("ClusterAck") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "unsafe-recover") => match variant {
            Some("UnsafeRecoverResponse") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "clear-degraded") => match variant {
            Some("ClusterAck") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "snapshot-cluster") => match variant {
            Some("ClusterSnapshotResponse") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "restore") => match variant {
            Some("ClusterAck") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "directory") => match variant {
            Some("DirectoryResponse") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlCluster", "assign-project") => match variant {
            Some("ClusterAck") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        _ => Ok(payload),
    }
}

/// Typed client for the TallyOwlReplication service.
pub struct TallyOwlReplicationClient<T: Transport> {
    #[allow(dead_code)]
    transport: T,
}

impl<T: Transport> TallyOwlReplicationClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Hand one consensus message to the named group.
    pub fn deliver_consensus(&self, req: ConsensusMessage) -> Result<ConsensusReply, ClientError> {
        let csil_req = match encode_consensus_message(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlReplication", "deliver-consensus", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlReplication",
            "deliver-consensus",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_consensus_reply(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Read part of a snapshot, so a new or lagging replica can catch up.
    pub fn fetch_snapshot_chunk(
        &self,
        req: SnapshotChunkRequest,
    ) -> Result<SnapshotChunk, ClientError> {
        let csil_req = match encode_snapshot_chunk_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlReplication", "fetch-snapshot-chunk", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlReplication",
            "fetch-snapshot-chunk",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_snapshot_chunk(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Read part of a sealed segment, for a tablet that is moving.
    pub fn fetch_segment(
        &self,
        req: SegmentTransferRequest,
    ) -> Result<SegmentTransfer, ClientError> {
        let csil_req = match encode_segment_transfer_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlReplication", "fetch-segment", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlReplication",
            "fetch-segment",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_segment_transfer(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Compute a partial aggregate over one tablet. The coordinator merges.
    pub fn partial_aggregate(
        &self,
        req: PartialQueryRequest,
    ) -> Result<PartialQueryResponse, ClientError> {
        let csil_req = match encode_partial_query_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlReplication", "partial-aggregate", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlReplication",
            "partial-aggregate",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_partial_query_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// What this node holds, and how current each of it is.
    pub fn replica_status(&self, req: ReplicaStatusRequest) -> Result<ReplicaStatus, ClientError> {
        let csil_req = match encode_replica_status_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlReplication", "replica-status", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlReplication",
            "replica-status",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_replica_status(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// A node reports its own latency and device readings. The controller
    /// compares; the node never decides that it is slow.
    pub fn report_health(&self, req: NodeHealthReport) -> Result<ClusterAck, ClientError> {
        let csil_req = match encode_node_health_report(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlReplication", "report-health", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlReplication",
            "report-health",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Which sealed segments this node holds for one tablet.
    pub fn list_segments(&self, req: SegmentListRequest) -> Result<SegmentList, ClientError> {
        let csil_req = match encode_segment_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlReplication", "list-segments", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlReplication",
            "list-segments",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_segment_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }
}

/// Typed client for the TallyOwlCluster service.
pub struct TallyOwlClusterClient<T: Transport> {
    #[allow(dead_code)]
    transport: T,
}

impl<T: Transport> TallyOwlClusterClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// The whole topology, or one cell's part of it.
    pub fn describe_topology(
        &self,
        req: DescribeTopologyRequest,
    ) -> Result<TopologyResponse, ClientError> {
        let csil_req = match encode_describe_topology_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlCluster", "describe-topology", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "describe-topology",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_topology_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Where a write for one project and affinity key belongs.
    pub fn route(&self, req: RouteRequest) -> Result<RouteResponse, ClientError> {
        let csil_req = match encode_route_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlCluster", "route", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "route",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_route_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Create a group. The only operation that creates a voter set.
    pub fn bootstrap_group(&self, req: BootstrapGroupRequest) -> Result<ClusterAck, ClientError> {
        let csil_req = match encode_bootstrap_group_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlCluster", "bootstrap-group", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "bootstrap-group",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Add a replica, voting or not. Online.
    pub fn add_replica(&self, req: ChangeReplicaRequest) -> Result<ClusterAck, ClientError> {
        let csil_req = match encode_change_replica_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlCluster", "add-replica", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "add-replica",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Remove a replica. Online, and refused when it would end the quorum.
    pub fn remove_replica(&self, req: RemoveReplicaRequest) -> Result<ClusterAck, ClientError> {
        let csil_req = match encode_remove_replica_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlCluster", "remove-replica", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "remove-replica",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Split one tablet into two at a virtual-shard boundary.
    pub fn split_tablet(&self, req: SplitTabletRequest) -> Result<ClusterAck, ClientError> {
        let csil_req = match encode_split_tablet_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlCluster", "split-tablet", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "split-tablet",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Merge two adjacent tablets.
    pub fn merge_tablets(&self, req: MergeTabletsRequest) -> Result<ClusterAck, ClientError> {
        let csil_req = match encode_merge_tablets_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlCluster", "merge-tablets", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "merge-tablets",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Move a tablet from one node to another, online and with parity checked.
    pub fn move_tablet(&self, req: MoveTabletRequest) -> Result<ClusterAck, ClientError> {
        let csil_req = match encode_move_tablet_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlCluster", "move-tablet", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "move-tablet",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Change a tablet's receipt policy. `local-one` is refused on a tablet
    /// with more than one voter, and the refusal says so rather than quietly
    /// selecting another policy.
    pub fn set_receipt_policy(
        &self,
        req: SetReceiptPolicyRequest,
    ) -> Result<ClusterAck, ClientError> {
        let csil_req = match encode_set_receipt_policy_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlCluster", "set-receipt-policy", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "set-receipt-policy",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Move a tablet's write region behind a fence.
    pub fn fail_over_region(&self, req: FailOverRegionRequest) -> Result<ClusterAck, ClientError> {
        let csil_req = match encode_fail_over_region_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlCluster", "fail-over-region", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "fail-over-region",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Force a single-voter membership from a survivor. It can lose an
    /// acknowledged write, it is audited, and it marks the range degraded.
    pub fn unsafe_recover(
        &self,
        req: UnsafeRecoverRequest,
    ) -> Result<UnsafeRecoverResponse, ClientError> {
        let csil_req = match encode_unsafe_recover_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlCluster", "unsafe-recover", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "unsafe-recover",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_unsafe_recover_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Clear a degraded mark. It records who accepted the loss.
    pub fn clear_degraded(&self, req: ClearDegradedRequest) -> Result<ClusterAck, ClientError> {
        let csil_req = match encode_clear_degraded_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlCluster", "clear-degraded", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "clear-degraded",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Take a cluster snapshot.
    pub fn snapshot_cluster(
        &self,
        req: ClusterSnapshotRequest,
    ) -> Result<ClusterSnapshotResponse, ClientError> {
        let csil_req = match encode_cluster_snapshot_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlCluster", "snapshot-cluster", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "snapshot-cluster",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_cluster_snapshot_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Restore from a snapshot. The default answer to a permanent quorum loss.
    pub fn restore(&self, req: RestoreRequest) -> Result<ClusterAck, ClientError> {
        let csil_req = match encode_restore_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlCluster", "restore", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "restore",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// The global directory: which cells hold a project.
    pub fn directory(&self, req: DirectoryRequest) -> Result<DirectoryResponse, ClientError> {
        let csil_req = match encode_directory_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlCluster", "directory", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "directory",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_directory_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Assign a project to cells. Refused while the directory is unreachable,
    /// and existing cells keep working through that refusal.
    pub fn assign_project(&self, req: AssignProjectRequest) -> Result<ClusterAck, ClientError> {
        let csil_req = match encode_assign_project_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlCluster", "assign-project", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlCluster",
            "assign-project",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }
}
