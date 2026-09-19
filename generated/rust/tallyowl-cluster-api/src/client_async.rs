//! Generated transport-agnostic service clients from CSIL specification

#![allow(async_fn_in_trait)]

use super::client::ClientError;
use super::codec::*;
use super::types::*;

/// The caller-supplied byte carrier: it performs the call named by `(service, op)`
/// with the already-encoded request bytes and returns the response bytes, or an
/// error. The generated client owns (de)serialization via the codec; the carrier
/// only moves bytes, so it can be HTTP, a queue, or an in-process loop.
pub trait AsyncTransport {
    async fn call(&self, service: &str, op: &str, req: &[u8]) -> Result<Vec<u8>, ClientError>;
}

/// Typed client for the TallyOwlReplication service.
pub struct TallyOwlReplicationAsyncClient<T: AsyncTransport> {
    #[allow(dead_code)]
    transport: T,
}

impl<T: AsyncTransport> TallyOwlReplicationAsyncClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Hand one consensus message to the named group.
    pub async fn deliver_consensus(
        &self,
        req: ConsensusMessage,
    ) -> Result<ConsensusReply, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlReplication",
                "deliver-consensus",
                &encode_consensus_message(&req),
            )
            .await?;
        decode_consensus_reply(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Read part of a snapshot, so a new or lagging replica can catch up.
    pub async fn fetch_snapshot_chunk(
        &self,
        req: SnapshotChunkRequest,
    ) -> Result<SnapshotChunk, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlReplication",
                "fetch-snapshot-chunk",
                &encode_snapshot_chunk_request(&req),
            )
            .await?;
        decode_snapshot_chunk(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Read part of a sealed segment, for a tablet that is moving.
    pub async fn fetch_segment(
        &self,
        req: SegmentTransferRequest,
    ) -> Result<SegmentTransfer, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlReplication",
                "fetch-segment",
                &encode_segment_transfer_request(&req),
            )
            .await?;
        decode_segment_transfer(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Compute a partial aggregate over one tablet. The coordinator merges.
    pub async fn partial_aggregate(
        &self,
        req: PartialQueryRequest,
    ) -> Result<PartialQueryResponse, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlReplication",
                "partial-aggregate",
                &encode_partial_query_request(&req),
            )
            .await?;
        decode_partial_query_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// What this node holds, and how current each of it is.
    pub async fn replica_status(
        &self,
        req: ReplicaStatusRequest,
    ) -> Result<ReplicaStatus, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlReplication",
                "replica-status",
                &encode_replica_status_request(&req),
            )
            .await?;
        decode_replica_status(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// A node reports its own latency and device readings. The controller
    /// compares; the node never decides that it is slow.
    pub async fn report_health(&self, req: NodeHealthReport) -> Result<ClusterAck, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlReplication",
                "report-health",
                &encode_node_health_report(&req),
            )
            .await?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Which sealed segments this node holds for one tablet.
    pub async fn list_segments(&self, req: SegmentListRequest) -> Result<SegmentList, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlReplication",
                "list-segments",
                &encode_segment_list_request(&req),
            )
            .await?;
        decode_segment_list(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }
}

/// Typed client for the TallyOwlCluster service.
pub struct TallyOwlClusterAsyncClient<T: AsyncTransport> {
    #[allow(dead_code)]
    transport: T,
}

impl<T: AsyncTransport> TallyOwlClusterAsyncClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// The whole topology, or one cell's part of it.
    pub async fn describe_topology(
        &self,
        req: DescribeTopologyRequest,
    ) -> Result<TopologyResponse, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlCluster",
                "describe-topology",
                &encode_describe_topology_request(&req),
            )
            .await?;
        decode_topology_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Where a write for one project and affinity key belongs.
    pub async fn route(&self, req: RouteRequest) -> Result<RouteResponse, ClientError> {
        let csil_resp = self
            .transport
            .call("TallyOwlCluster", "route", &encode_route_request(&req))
            .await?;
        decode_route_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Create a group. The only operation that creates a voter set.
    pub async fn bootstrap_group(
        &self,
        req: BootstrapGroupRequest,
    ) -> Result<ClusterAck, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlCluster",
                "bootstrap-group",
                &encode_bootstrap_group_request(&req),
            )
            .await?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Add a replica, voting or not. Online.
    pub async fn add_replica(&self, req: ChangeReplicaRequest) -> Result<ClusterAck, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlCluster",
                "add-replica",
                &encode_change_replica_request(&req),
            )
            .await?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Remove a replica. Online, and refused when it would end the quorum.
    pub async fn remove_replica(
        &self,
        req: RemoveReplicaRequest,
    ) -> Result<ClusterAck, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlCluster",
                "remove-replica",
                &encode_remove_replica_request(&req),
            )
            .await?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Split one tablet into two at a virtual-shard boundary.
    pub async fn split_tablet(&self, req: SplitTabletRequest) -> Result<ClusterAck, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlCluster",
                "split-tablet",
                &encode_split_tablet_request(&req),
            )
            .await?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Merge two adjacent tablets.
    pub async fn merge_tablets(&self, req: MergeTabletsRequest) -> Result<ClusterAck, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlCluster",
                "merge-tablets",
                &encode_merge_tablets_request(&req),
            )
            .await?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Move a tablet from one node to another, online and with parity checked.
    pub async fn move_tablet(&self, req: MoveTabletRequest) -> Result<ClusterAck, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlCluster",
                "move-tablet",
                &encode_move_tablet_request(&req),
            )
            .await?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Change a tablet's receipt policy. `local-one` is refused on a tablet
    /// with more than one voter, and the refusal says so rather than quietly
    /// selecting another policy.
    pub async fn set_receipt_policy(
        &self,
        req: SetReceiptPolicyRequest,
    ) -> Result<ClusterAck, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlCluster",
                "set-receipt-policy",
                &encode_set_receipt_policy_request(&req),
            )
            .await?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Move a tablet's write region behind a fence.
    pub async fn fail_over_region(
        &self,
        req: FailOverRegionRequest,
    ) -> Result<ClusterAck, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlCluster",
                "fail-over-region",
                &encode_fail_over_region_request(&req),
            )
            .await?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Force a single-voter membership from a survivor. It can lose an
    /// acknowledged write, it is audited, and it marks the range degraded.
    pub async fn unsafe_recover(
        &self,
        req: UnsafeRecoverRequest,
    ) -> Result<UnsafeRecoverResponse, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlCluster",
                "unsafe-recover",
                &encode_unsafe_recover_request(&req),
            )
            .await?;
        decode_unsafe_recover_response(&csil_resp)
            .map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Clear a degraded mark. It records who accepted the loss.
    pub async fn clear_degraded(
        &self,
        req: ClearDegradedRequest,
    ) -> Result<ClusterAck, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlCluster",
                "clear-degraded",
                &encode_clear_degraded_request(&req),
            )
            .await?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Take a cluster snapshot.
    pub async fn snapshot_cluster(
        &self,
        req: ClusterSnapshotRequest,
    ) -> Result<ClusterSnapshotResponse, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlCluster",
                "snapshot-cluster",
                &encode_cluster_snapshot_request(&req),
            )
            .await?;
        decode_cluster_snapshot_response(&csil_resp)
            .map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Restore from a snapshot. The default answer to a permanent quorum loss.
    pub async fn restore(&self, req: RestoreRequest) -> Result<ClusterAck, ClientError> {
        let csil_resp = self
            .transport
            .call("TallyOwlCluster", "restore", &encode_restore_request(&req))
            .await?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// The global directory: which cells hold a project.
    pub async fn directory(&self, req: DirectoryRequest) -> Result<DirectoryResponse, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlCluster",
                "directory",
                &encode_directory_request(&req),
            )
            .await?;
        decode_directory_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Assign a project to cells. Refused while the directory is unreachable,
    /// and existing cells keep working through that refusal.
    pub async fn assign_project(
        &self,
        req: AssignProjectRequest,
    ) -> Result<ClusterAck, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlCluster",
                "assign-project",
                &encode_assign_project_request(&req),
            )
            .await?;
        decode_cluster_ack(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }
}
