//! Generated service traits from CSIL specification

use super::types::*;

/// TallyOwlReplication service trait
pub trait TallyOwlReplication {
    type Context;
    /// Hand one consensus message to the named group.
    fn deliver_consensus(
        &self,
        ctx: &Self::Context,
        input: ConsensusMessage,
    ) -> Result<ConsensusReply, ServiceError>;
    /// Read part of a snapshot, so a new or lagging replica can catch up.
    fn fetch_snapshot_chunk(
        &self,
        ctx: &Self::Context,
        input: SnapshotChunkRequest,
    ) -> Result<SnapshotChunk, ServiceError>;
    /// Read part of a sealed segment, for a tablet that is moving.
    fn fetch_segment(
        &self,
        ctx: &Self::Context,
        input: SegmentTransferRequest,
    ) -> Result<SegmentTransfer, ServiceError>;
    /// Compute a partial aggregate over one tablet. The coordinator merges.
    fn partial_aggregate(
        &self,
        ctx: &Self::Context,
        input: PartialQueryRequest,
    ) -> Result<PartialQueryResponse, ServiceError>;
    /// What this node holds, and how current each of it is.
    fn replica_status(
        &self,
        ctx: &Self::Context,
        input: ReplicaStatusRequest,
    ) -> Result<ReplicaStatus, ServiceError>;
    /// A node reports its own latency and device readings. The controller
    /// compares; the node never decides that it is slow.
    fn report_health(
        &self,
        ctx: &Self::Context,
        input: NodeHealthReport,
    ) -> Result<ClusterAck, ServiceError>;
    /// Which sealed segments this node holds for one tablet.
    fn list_segments(
        &self,
        ctx: &Self::Context,
        input: SegmentListRequest,
    ) -> Result<SegmentList, ServiceError>;
}

/// Wire-id ordinals for the TallyOwlReplication service (transport compact profiles).
pub mod tally_owl_replication_wire_ids {
    pub const SERVICE: u64 = 4;
    pub const OP_DELIVER_CONSENSUS: u64 = 0;
    pub const OP_FETCH_SNAPSHOT_CHUNK: u64 = 1;
    pub const OP_FETCH_SEGMENT: u64 = 2;
    pub const OP_PARTIAL_AGGREGATE: u64 = 3;
    pub const OP_REPLICA_STATUS: u64 = 4;
    pub const OP_REPORT_HEALTH: u64 = 5;
    pub const OP_LIST_SEGMENTS: u64 = 6;
}

/// TallyOwlCluster service trait
pub trait TallyOwlCluster {
    type Context;
    /// The whole topology, or one cell's part of it.
    fn describe_topology(
        &self,
        ctx: &Self::Context,
        input: DescribeTopologyRequest,
    ) -> Result<TopologyResponse, ServiceError>;
    /// Where a write for one project and affinity key belongs.
    fn route(
        &self,
        ctx: &Self::Context,
        input: RouteRequest,
    ) -> Result<RouteResponse, ServiceError>;
    /// Create a group. The only operation that creates a voter set.
    fn bootstrap_group(
        &self,
        ctx: &Self::Context,
        input: BootstrapGroupRequest,
    ) -> Result<ClusterAck, ServiceError>;
    /// Add a replica, voting or not. Online.
    fn add_replica(
        &self,
        ctx: &Self::Context,
        input: ChangeReplicaRequest,
    ) -> Result<ClusterAck, ServiceError>;
    /// Remove a replica. Online, and refused when it would end the quorum.
    fn remove_replica(
        &self,
        ctx: &Self::Context,
        input: RemoveReplicaRequest,
    ) -> Result<ClusterAck, ServiceError>;
    /// Split one tablet into two at a virtual-shard boundary.
    fn split_tablet(
        &self,
        ctx: &Self::Context,
        input: SplitTabletRequest,
    ) -> Result<ClusterAck, ServiceError>;
    /// Merge two adjacent tablets.
    fn merge_tablets(
        &self,
        ctx: &Self::Context,
        input: MergeTabletsRequest,
    ) -> Result<ClusterAck, ServiceError>;
    /// Move a tablet from one node to another, online and with parity checked.
    fn move_tablet(
        &self,
        ctx: &Self::Context,
        input: MoveTabletRequest,
    ) -> Result<ClusterAck, ServiceError>;
    /// Change a tablet's receipt policy. `local-one` is refused on a tablet
    /// with more than one voter, and the refusal says so rather than quietly
    /// selecting another policy.
    fn set_receipt_policy(
        &self,
        ctx: &Self::Context,
        input: SetReceiptPolicyRequest,
    ) -> Result<ClusterAck, ServiceError>;
    /// Move a tablet's write region behind a fence.
    fn fail_over_region(
        &self,
        ctx: &Self::Context,
        input: FailOverRegionRequest,
    ) -> Result<ClusterAck, ServiceError>;
    /// Force a single-voter membership from a survivor. It can lose an
    /// acknowledged write, it is audited, and it marks the range degraded.
    fn unsafe_recover(
        &self,
        ctx: &Self::Context,
        input: UnsafeRecoverRequest,
    ) -> Result<UnsafeRecoverResponse, ServiceError>;
    /// Clear a degraded mark. It records who accepted the loss.
    fn clear_degraded(
        &self,
        ctx: &Self::Context,
        input: ClearDegradedRequest,
    ) -> Result<ClusterAck, ServiceError>;
    /// Take a cluster snapshot.
    fn snapshot_cluster(
        &self,
        ctx: &Self::Context,
        input: ClusterSnapshotRequest,
    ) -> Result<ClusterSnapshotResponse, ServiceError>;
    /// Restore from a snapshot. The default answer to a permanent quorum loss.
    fn restore(
        &self,
        ctx: &Self::Context,
        input: RestoreRequest,
    ) -> Result<ClusterAck, ServiceError>;
    /// The global directory: which cells hold a project.
    fn directory(
        &self,
        ctx: &Self::Context,
        input: DirectoryRequest,
    ) -> Result<DirectoryResponse, ServiceError>;
    /// Assign a project to cells. Refused while the directory is unreachable,
    /// and existing cells keep working through that refusal.
    fn assign_project(
        &self,
        ctx: &Self::Context,
        input: AssignProjectRequest,
    ) -> Result<ClusterAck, ServiceError>;
}

/// Wire-id ordinals for the TallyOwlCluster service (transport compact profiles).
pub mod tally_owl_cluster_wire_ids {
    pub const SERVICE: u64 = 5;
    pub const OP_DESCRIBE_TOPOLOGY: u64 = 0;
    pub const OP_ROUTE: u64 = 1;
    pub const OP_BOOTSTRAP_GROUP: u64 = 2;
    pub const OP_ADD_REPLICA: u64 = 3;
    pub const OP_REMOVE_REPLICA: u64 = 4;
    pub const OP_SPLIT_TABLET: u64 = 5;
    pub const OP_MERGE_TABLETS: u64 = 6;
    pub const OP_MOVE_TABLET: u64 = 7;
    pub const OP_SET_RECEIPT_POLICY: u64 = 8;
    pub const OP_FAIL_OVER_REGION: u64 = 9;
    pub const OP_UNSAFE_RECOVER: u64 = 10;
    pub const OP_CLEAR_DEGRADED: u64 = 11;
    pub const OP_SNAPSHOT_CLUSTER: u64 = 12;
    pub const OP_RESTORE: u64 = 13;
    pub const OP_DIRECTORY: u64 = 14;
    pub const OP_ASSIGN_PROJECT: u64 = 15;
}
