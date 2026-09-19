//! The two CSIL services this crate answers.
//!
//! `TallyOwlReplication` is node to node: consensus messages, snapshot chunks,
//! segment copies, partial aggregates, and health reports. It is never reached
//! by an application and never by a browser, and `AGENTS.md` puts every native
//! server-to-server hop on CSIL over TLS over TCP.
//!
//! `TallyOwlCluster` is topology: an operator changes placement here and a
//! gateway asks where a write belongs.
//!
//! # What a receiver checks before it does anything
//!
//! A consensus message names its group, its sender, and the placement
//! generation the sender routed with. Two of those are checked before the
//! algorithm sees the message:
//!
//! - a group this node does not hold is refused by name, so a message meant for
//!   a tablet that moved does not silently start one here;
//! - a generation older than this node's is refused with the current one in the
//!   answer, so the sender updates and retries instead of backing off blindly.
//!
//! The sender name is **not** what proves identity. The peer certificate is.
//! The name says which member the sender believes it is, and a mismatch is a
//! refusal that names both.

use std::sync::{Arc, Mutex};

use tallyowl_cluster_api::codec::{
    decode_consensus_message, decode_partial_query_request, decode_replica_status_request,
    encode_consensus_reply, encode_partial_query_response, encode_replica_status,
};
use tallyowl_cluster_api::types::{
    ConsensusKind, ConsensusReply, MissingRange as WireMissingRange, PartialQueryResponse,
    ReplicaStatus, TrendBucket,
};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_rpc::{
    error_outcome, malformed, reply, unknown_operation, Dispatcher, Outcome, Request,
};

use crate::groups::{GroupKey, GroupRegistry};
use crate::query::Partial;
use crate::topology::Topology;
use crate::transfer::TabletSegments;

/// What a partial aggregate is answered from.
///
/// The service does not know how to run a query; the head does. This is the
/// seam between them, and it is a trait so that `tallyowl-cluster` does not
/// depend on the query executor and the executor does not depend on this.
pub trait PartialSource: Send + Sync + 'static {
    /// Compute one tablet's partial state.
    fn partial(
        &self,
        tablet: &str,
        request: &tallyowl_cluster_api::types::PartialQueryRequest,
    ) -> Result<Partial, TallyOwlError>;

    /// What this node holds and how current each part is.
    fn status(&self) -> ReplicaStatus;
}

/// The node-to-node service.
pub struct ReplicationService {
    registry: Arc<GroupRegistry>,
    topology: Arc<Mutex<Topology>>,
    source: Arc<dyn PartialSource>,
    /// The sealed segments this node can serve and adopt. A node that has none
    /// refuses by name rather than answering an empty list, because an empty
    /// list would tell a replica catching up that it had everything.
    segments: Option<Arc<dyn TabletSegments>>,
    /// Health reports arrive here and the controller reads them.
    reports: Arc<Mutex<Vec<crate::health::HealthReport>>>,
}

impl ReplicationService {
    pub fn new(
        registry: Arc<GroupRegistry>,
        topology: Arc<Mutex<Topology>>,
        source: Arc<dyn PartialSource>,
    ) -> ReplicationService {
        ReplicationService {
            registry,
            topology,
            source,
            segments: None,
            reports: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Serve this node's sealed segments, so a replica can catch up from them.
    pub fn serving_segments(mut self, segments: Arc<dyn TabletSegments>) -> ReplicationService {
        self.segments = Some(segments);
        self
    }

    pub fn reports(&self) -> Arc<Mutex<Vec<crate::health::HealthReport>>> {
        Arc::clone(&self.reports)
    }

    fn segments(&self) -> Result<&Arc<dyn TabletSegments>, TallyOwlError> {
        self.segments.as_ref().ok_or_else(|| {
            TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                "This node does not serve sealed segments, so a replica cannot catch up from it. It was started without a store."
                    .to_string(),
            )
        })
    }

    fn list_segments(&self, payload: &[u8]) -> Outcome {
        use tallyowl_cluster_api::codec::{decode_segment_list_request, encode_segment_list};
        let request = match decode_segment_list_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let segments = match self.segments() {
            Ok(segments) => segments,
            Err(e) => return refusal(&e),
        };
        match segments.list(&request.tablet) {
            Err(e) => refusal(&e),
            Ok(list) => reply("SegmentList", encode_segment_list(&list)),
        }
    }

    fn fetch_segment(&self, payload: &[u8]) -> Outcome {
        use tallyowl_cluster_api::codec::{
            decode_segment_transfer_request, encode_segment_transfer,
        };
        let request = match decode_segment_transfer_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let segments = match self.segments() {
            Ok(segments) => segments,
            Err(e) => return refusal(&e),
        };
        match segments.read(
            &request.tablet,
            &request.segment_id,
            request.offset,
            request.max_bytes,
        ) {
            Err(e) => refusal(&e),
            Ok(transfer) => reply("SegmentTransfer", encode_segment_transfer(&transfer)),
        }
    }

    /// Serve part of a group's state-machine snapshot.
    ///
    /// Consensus installs a snapshot itself, over `deliver-consensus`. This is
    /// the pull an operator and a tool use, and it exists because the contract
    /// declares it: an operation that is declared and unanswered is worse than
    /// one that is not declared.
    fn fetch_snapshot_chunk(&self, payload: &[u8]) -> Outcome {
        use tallyowl_cluster_api::codec::{decode_snapshot_chunk_request, encode_snapshot_chunk};
        use tallyowl_cluster_api::types::SnapshotChunk;

        let request = match decode_snapshot_chunk_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let key = match GroupKey::from_wire(request.group.kind, request.group.name.as_deref()) {
            Ok(key) => key,
            Err(e) => return refusal(&e),
        };
        let machine = match self.registry.machine(&key) {
            Ok(machine) => machine,
            Err(e) => return refusal(&e),
        };
        let data = machine.snapshot();
        let total = data.len() as u64;
        if request.offset > total {
            return refusal(&TallyOwlError::invalid_argument(format!(
                "The snapshot of {} holds {total} bytes and the request starts at {}.",
                key.label(),
                request.offset
            )));
        }
        let take = request
            .max_bytes
            .clamp(1, crate::transfer::DEFAULT_CHUNK_BYTES)
            .min(total - request.offset);
        let from = request.offset as usize;
        let to = from + take as usize;
        let last = to as u64 >= total;
        reply(
            "SnapshotChunk",
            encode_snapshot_chunk(&SnapshotChunk {
                // The applied index names it, so a caller that fetched two
                // chunks can see that they came from one snapshot.
                snapshot_id: request
                    .snapshot_id
                    .unwrap_or_else(|| format!("{}-{}", key.directory_name(), total)),
                offset: request.offset,
                data: data[from..to].to_vec(),
                total_bytes: total,
                last,
                whole_digest: last.then(|| crate::movement::digest(&data).to_vec()),
            }),
        )
    }

    /// Answer one consensus message.
    fn consensus(&self, payload: &[u8]) -> Outcome {
        let message = match decode_consensus_message(payload) {
            Ok(message) => message,
            Err(e) => return malformed(e),
        };
        let key = match GroupKey::from_wire(message.group.kind, message.group.name.as_deref()) {
            Ok(key) => key,
            Err(e) => return refusal(&e),
        };
        if !self.registry.holds(&key) {
            return encode_refusal(
                format!(
                    "This node does not hold {}. It moved, or it was never placed here.",
                    key.label()
                ),
                None,
            );
        }
        let current = self.topology.lock().expect("topology").generation;
        if message.generation > 0 && message.generation < current {
            // A message from before a membership change. Say what the current
            // generation is, so the sender updates rather than retrying blind.
            return encode_refusal(
                format!(
                    "`{}` sent a message for {} at placement generation {} and this cell is at {current}.",
                    message.sender,
                    key.label(),
                    message.generation
                ),
                Some(current),
            );
        }

        let answered = match message.kind {
            ConsensusKind::AppendEntries => crate::raft::decode(&message.payload)
                .map_err(TallyOwlError::invalid_argument)
                .and_then(|request| self.registry.deliver_append(&key, request))
                .and_then(|response| {
                    crate::raft::encode(&response).map_err(TallyOwlError::internal)
                }),
            ConsensusKind::Vote => crate::raft::decode(&message.payload)
                .map_err(TallyOwlError::invalid_argument)
                .and_then(|request| self.registry.deliver_vote(&key, request))
                .and_then(|response| {
                    crate::raft::encode(&response).map_err(TallyOwlError::internal)
                }),
            ConsensusKind::InstallSnapshot => crate::raft::decode(&message.payload)
                .map_err(TallyOwlError::invalid_argument)
                .and_then(|request| self.registry.deliver_snapshot(&key, request))
                .and_then(|response| {
                    crate::raft::encode(&response).map_err(TallyOwlError::internal)
                }),
            // A client write a peer voter took while this node leads. It is
            // proposed here with no further forward, so a proposal travels at
            // most one hop and a leadership change mid-flight is a retryable
            // refusal rather than a loop.
            ConsensusKind::Proposal => self.registry.propose_local(&key, message.payload.clone()),
        };

        match answered {
            Ok(answer) => reply(
                "ConsensusReply",
                encode_consensus_reply(&ConsensusReply {
                    accepted: true,
                    payload: Some(answer),
                    current_generation: Some(current),
                    refusal: None,
                }),
            ),
            Err(e) => encode_refusal(e.message, Some(current)),
        }
    }

    fn partial_aggregate(&self, payload: &[u8]) -> Outcome {
        let request = match decode_partial_query_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let key = GroupKey::Tablet(request.tablet.clone());
        if !self.registry.holds(&key) {
            return refusal(&TallyOwlError::new(
                ErrorCode::NotFound,
                format!("This node does not hold `{}`.", request.tablet),
            ));
        }
        match self.source.partial(&request.tablet, &request) {
            Err(e) => refusal(&e),
            Ok(partial) => reply(
                "PartialQueryResponse",
                encode_partial_query_response(&to_wire_partial(&partial)),
            ),
        }
    }

    fn replica_status(&self, payload: &[u8]) -> Outcome {
        if let Err(e) = decode_replica_status_request(payload) {
            return malformed(e);
        }
        reply(
            "ReplicaStatus",
            encode_replica_status(&self.source.status()),
        )
    }
}

impl Dispatcher for ReplicationService {
    fn dispatch(&self, request: &Request) -> Outcome {
        match request.op.as_str() {
            "deliver-consensus" => self.consensus(&request.payload),
            "partial-aggregate" => self.partial_aggregate(&request.payload),
            "replica-status" => self.replica_status(&request.payload),
            "report-health" => self.health(&request.payload),
            "list-segments" => self.list_segments(&request.payload),
            "fetch-segment" => self.fetch_segment(&request.payload),
            "fetch-snapshot-chunk" => self.fetch_snapshot_chunk(&request.payload),
            other => unknown_operation(&request.service, other),
        }
    }
}

impl ReplicationService {
    fn health(&self, payload: &[u8]) -> Outcome {
        use tallyowl_cluster_api::codec::{decode_node_health_report, encode_cluster_ack};
        use tallyowl_cluster_api::types::ClusterAck;
        let report = match decode_node_health_report(payload) {
            Ok(report) => report,
            Err(e) => return malformed(e),
        };
        self.reports
            .lock()
            .expect("health reports")
            .push(from_wire_report(&report));
        let generation = self.topology.lock().expect("topology").generation;
        reply(
            "ClusterAck",
            encode_cluster_ack(&ClusterAck {
                accepted: true,
                generation,
                message: None,
            }),
        )
    }
}

/// Turn one error into the typed refusal the contract declares.
pub fn refusal(error: &TallyOwlError) -> Outcome {
    use tallyowl_cluster_api::codec::encode_service_error;
    use tallyowl_cluster_api::types::{ErrorCode as Wire, ServiceError};
    error_outcome(encode_service_error(&ServiceError {
        code: match error.code {
            ErrorCode::InvalidArgument => Wire::InvalidArgument,
            ErrorCode::Unauthenticated => Wire::Unauthenticated,
            ErrorCode::PermissionDenied => Wire::PermissionDenied,
            ErrorCode::NotFound => Wire::NotFound,
            ErrorCode::AlreadyExists => Wire::AlreadyExists,
            ErrorCode::ResourceExhausted => Wire::ResourceExhausted,
            ErrorCode::FailedPrecondition => Wire::FailedPrecondition,
            ErrorCode::Unavailable => Wire::Unavailable,
            ErrorCode::SchemaUnsupported => Wire::SchemaUnsupported,
            ErrorCode::BudgetExceeded => Wire::BudgetExceeded,
            ErrorCode::IncompleteResult => Wire::IncompleteResult,
            ErrorCode::Internal => Wire::Internal,
        },
        message: error.message.clone(),
        retryable: error.retryable,
        detail: None,
    }))
}

/// A refusal the sender can act on: it says what happened, and when the cause
/// was a stale route it says what the current generation is.
fn encode_refusal(refusal: String, current_generation: Option<u64>) -> Outcome {
    reply(
        "ConsensusReply",
        encode_consensus_reply(&ConsensusReply {
            accepted: false,
            payload: None,
            current_generation,
            refusal: Some(refusal),
        }),
    )
}

/// One tablet's partial state, in the shape the contract declares.
pub fn to_wire_partial(partial: &Partial) -> PartialQueryResponse {
    PartialQueryResponse {
        tablet: partial.tablet.clone(),
        commit_watermark: partial.commit_watermark,
        freshness_ms: partial.freshness_ms,
        complete: partial.complete,
        missing: (!partial.missing.is_empty()).then(|| {
            partial
                .missing
                .iter()
                .map(|m| WireMissingRange {
                    tablet_id: m.tablet.clone(),
                    range_start: m.range_start,
                    range_end: m.range_end,
                })
                .collect()
        }),
        count: partial.count,
        buckets: (!partial.buckets.is_empty()).then(|| {
            partial
                .buckets
                .iter()
                .map(|(start, count)| TrendBucket {
                    bucket_start: *start,
                    count: *count,
                })
                .collect()
        }),
        rows: (!partial.rows.is_empty())
            .then(|| vec![tallyowl_store::row_codec::encode_rows(&partial.rows)]),
        aggregate_state: partial.aggregate_state.clone(),
        scanned_segments: partial.scanned_segments,
        scanned_bytes: partial.scanned_bytes,
        degraded: partial.degraded.then_some(true),
    }
}

/// Read one tablet's partial state back at the coordinator.
pub fn from_wire_partial(wire: &PartialQueryResponse) -> Result<Partial, TallyOwlError> {
    let mut rows = Vec::new();
    for frame in wire.rows.iter().flatten() {
        rows.extend(tallyowl_store::row_codec::decode_rows(frame).map_err(|e| {
            TallyOwlError::internal(format!("A partial answer could not be read: {e}"))
        })?);
    }
    Ok(Partial {
        tablet: wire.tablet.clone(),
        aggregate_state: wire.aggregate_state.clone(),
        commit_watermark: wire.commit_watermark,
        freshness_ms: wire.freshness_ms,
        complete: wire.complete,
        missing: wire
            .missing
            .iter()
            .flatten()
            .map(|m| crate::query::MissingRange {
                tablet: m.tablet_id.clone(),
                range_start: m.range_start,
                range_end: m.range_end,
            })
            .collect(),
        count: wire.count,
        buckets: wire
            .buckets
            .iter()
            .flatten()
            .map(|b| (b.bucket_start, b.count))
            .collect(),
        rows,
        scanned_segments: wire.scanned_segments,
        scanned_bytes: wire.scanned_bytes,
        degraded: wire.degraded.unwrap_or(false),
    })
}

fn from_wire_report(
    wire: &tallyowl_cluster_api::types::NodeHealthReport,
) -> crate::health::HealthReport {
    crate::health::HealthReport {
        node: wire.node.clone(),
        reported_at: wire.reported_at,
        append_latency_us: wire.append_latency_us,
        fsync_latency_us: wire.fsync_latency_us,
        queue_depth: wire.queue_depth,
        accepted_bytes_each_second: wire.accepted_bytes_each_second,
        device_errors: wire.device_errors,
        device_service_time_us: wire.device_service_time_us,
        compaction_backlog_bytes: wire.compaction_backlog_bytes,
        memory_reclaim_events: wire.memory_reclaim_events,
        peer_round_trip_us: wire.peer_round_trip_us,
        writable: wire.writable,
        free_bytes: wire.free_bytes,
    }
}
