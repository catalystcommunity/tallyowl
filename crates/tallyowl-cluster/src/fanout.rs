//! Asking every readable tablet, and merging what they say.
//!
//! [`crate::query`] plans and merges. This is the part in between: it takes a
//! plan, asks each tablet through `partial-aggregate`, turns a tablet that did
//! not answer into a named missing range rather than into silence, and hands
//! the merge back.
//!
//! # Why this is a `Store`
//!
//! The head's query executor speaks [`tallyowl_store::Store`], and Phase 1 put
//! a real contract there so that a later phase could replace the implementation
//! without moving the seam. Phase 7 already did that for writes:
//! [`crate::replicated::ReplicatedStore`] commits through a tablet group and
//! every caller above it is unchanged. This does the same for reads.
//!
//! The alternative was a second read path inside the head, reached only when an
//! installation has more than one tablet. That path would be the one nobody
//! runs at home and everybody runs in production, which is the shape of every
//! defect this project has paid for.
//!
//! # What a missing tablet costs, and what it does not
//!
//! A tablet that cannot be reached makes the answer **incomplete**, and
//! [`tallyowl_store::Scanned`] and [`tallyowl_store::Trend`] both carry that
//! flag beside their data rather than beside it. The head then refuses with
//! `incomplete-result` unless the caller asked for a partial answer, which is
//! `docs/QUERY.md` section 9 and it is already built. [`ClusterReads::unreadable`]
//! names which tablets, because "part of this could not be read" is true and
//! unactionable on its own.
//!
//! # What is pushed down and what is not
//!
//! A count, a trend, **and a general aggregate** are computed on the tablet and
//! merged here, which is what `docs/QUERY.md` sections 5 and 10 ask for. L101
//! recorded the aggregate as the half that was not built and L136 built it.
//!
//! The plan travels as bytes and this module never reads it. A storage node
//! runs the coordinator's own aggregation over its own rows, so there is one
//! implementation of a sum rather than two — L102 is what a second one costs —
//! and the partial states come back as bytes for the same reason.
//!
//! **Two things still move rows and both are meant to.** A detail read and an
//! exact correlation lookup are two of the three forms `docs/QUERY.md` section
//! 10 names, and an aggregate over a *filter* is the third case here: a filter
//! is expression work, and pushing it down means pushing the expression
//! language into the storage contract, which D25 keeps small.

use std::sync::{Arc, Mutex};

use tallyowl_cluster_api::types::PartialQueryRequest;
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_rpc::Client;
use tallyowl_store::row::EventRow;
use tallyowl_store::{Scanned, StoreError, TimeBasis, Trend};

use crate::groups::{GroupKey, GroupRegistry};
use crate::query::{merge, Consistency, Merged, Partial, PartialKind, Plan};
use crate::raft::network::{PeerConnections, REPLICATION_SERVICE};
use crate::service::PartialSource;
use crate::topology::Topology;

/// How many rows one tablet may send for one detail read.
///
/// A scan that feeds the head's aggregate has to move rows, so this bounds one
/// tablet's contribution. A tablet that reaches it marks its part incomplete,
/// so the answer is refused rather than quietly smaller. `docs/QUERY.md`
/// section 10 makes the budget configurable and this is its default.
pub const DEFAULT_MAX_ROWS_FOR_EACH_TABLET: usize = 1_000_000;

/// Reads that cross tablets.
///
/// This is what [`crate::replicated::ReplicatedStore`] delegates to when an
/// installation has more than one tablet. A one-tablet installation has none of
/// it, and reads its own replica exactly as before.
pub trait TabletReads: Send + Sync + 'static {
    fn scan(
        &self,
        project_id: [u8; 16],
        range_start: i64,
        range_end: i64,
        basis: TimeBasis,
    ) -> Result<Scanned, StoreError>;

    #[allow(clippy::too_many_arguments)]
    fn trend(
        &self,
        project_id: [u8; 16],
        range_start: i64,
        range_end: i64,
        basis: TimeBasis,
        bucket_ms: i64,
        name: Option<&str>,
    ) -> Result<Trend, StoreError>;

    fn lookup_correlated(&self, column: &str, value: &[u8]) -> Result<Scanned, StoreError>;

    /// Ask every tablet for the partial state of one aggregate.
    ///
    /// The plan and the states are bytes this module never reads. See
    /// [`crate::query::PartialKind::Aggregate`].
    fn partial_aggregates(
        &self,
        plan: &[u8],
        project_id: [u8; 16],
        range_start: i64,
        range_end: i64,
        basis: TimeBasis,
    ) -> Result<tallyowl_store::store::PartialAggregates, StoreError>;

    /// Which tablets the last read could not reach, in the words an operator
    /// needs. A refusal that says "part of this is missing" and not which part
    /// is true and unactionable.
    fn unreadable(&self) -> Vec<String>;
}

/// The fan-out over this cell's tablets.
pub struct ClusterReads {
    topology: Arc<Mutex<Topology>>,
    registry: Arc<GroupRegistry>,
    /// This node's own answer, with no socket in the way. A node asking itself
    /// over loopback would pay a connection and a codec for nothing, and would
    /// stop working the moment its own listener was busy.
    local: Arc<dyn PartialSource>,
    connections: Arc<PeerConnections>,
    max_fan_out: usize,
    max_rows_for_each_tablet: usize,
    /// What the last read could not reach.
    unreachable: Mutex<Vec<String>>,
}

impl ClusterReads {
    pub fn new(
        topology: Arc<Mutex<Topology>>,
        registry: Arc<GroupRegistry>,
        local: Arc<dyn PartialSource>,
    ) -> ClusterReads {
        ClusterReads {
            topology,
            registry,
            local,
            connections: Arc::new(PeerConnections::new()),
            max_fan_out: crate::query::DEFAULT_MAX_FAN_OUT,
            max_rows_for_each_tablet: DEFAULT_MAX_ROWS_FOR_EACH_TABLET,
            unreachable: Mutex::new(Vec::new()),
        }
    }

    pub fn with_max_fan_out(mut self, max: usize) -> ClusterReads {
        self.max_fan_out = max.max(1);
        self
    }

    pub fn with_max_rows_for_each_tablet(mut self, max: usize) -> ClusterReads {
        self.max_rows_for_each_tablet = max.max(1);
        self
    }

    /// How many tablets a read currently fans out to.
    pub fn tablet_count(&self) -> usize {
        self.topology
            .lock()
            .expect("topology")
            .readable_tablets()
            .len()
    }

    fn plan_for(
        &self,
        project_id: [u8; 16],
        range_start: i64,
        range_end: i64,
        basis: TimeBasis,
        kind: PartialKind,
        name: Option<&str>,
    ) -> Result<Plan, TallyOwlError> {
        let topology = self.topology.lock().expect("topology");
        crate::query::plan(
            &topology,
            project_id,
            range_start,
            range_end,
            basis,
            kind,
            name.map(str::to_string),
            // A read at this seam has already been checked against the caller's
            // consistency by the head. What crosses here is the fan-out.
            Consistency::Committed,
            // Partial mode is decided above: the store contract carries the
            // flag on the result, and the head refuses unless the caller asked.
            // Allowing it here is what lets the flag reach the head at all.
            true,
            self.max_fan_out,
        )
    }

    /// Ask every tablet, and merge.
    fn run(&self, plan: &Plan) -> Result<Merged, TallyOwlError> {
        let mut missed: Vec<String> = Vec::new();
        let mut parts: Vec<Partial> = Vec::with_capacity(plan.tablets.len());
        for tablet in &plan.tablets {
            match self.ask(plan, tablet) {
                Ok(partial) => parts.push(partial),
                Err(e) => {
                    // A tablet that did not answer becomes a **named** missing
                    // range. A caller that dropped it would get a smaller answer
                    // marked complete, which is the failure FAILURE_MODES.md
                    // section 2 ranks worst.
                    missed.push(format!("`{tablet}` did not answer: {}", e.message));
                    parts.push(Partial::unavailable(
                        tablet.clone(),
                        plan.range_start,
                        plan.range_end,
                    ));
                }
            }
        }
        *self.unreachable.lock().expect("unreachable") = missed;
        merge(plan, parts)
    }

    fn ask(&self, plan: &Plan, tablet: &str) -> Result<Partial, TallyOwlError> {
        let request = to_wire(plan, tablet, self.max_rows_for_each_tablet);
        if self.registry.holds(&GroupKey::Tablet(tablet.to_string())) {
            return self.local.partial(tablet, &request);
        }
        let address = self.address_of(tablet)?;
        let client = self.connections.to(&address);
        self.call(&client, &address, &request)
    }

    /// Where a tablet's answer comes from.
    ///
    /// The leader when there is one, and any member otherwise. A read does not
    /// need the leader — a replica that applied the entry holds the rows — so a
    /// tablet whose leader is being replaced still answers.
    fn address_of(&self, tablet: &str) -> Result<String, TallyOwlError> {
        let topology = self.topology.lock().expect("topology");
        let found = topology.tablet(tablet).ok_or_else(|| {
            TallyOwlError::new(
                ErrorCode::NotFound,
                format!("No tablet is named `{tablet}`."),
            )
        })?;
        let leader = self.registry.leader(&GroupKey::Tablet(tablet.to_string()));
        if let Some(leader) = leader {
            if let Some(member) = found.member(&leader) {
                return Ok(member.address.clone());
            }
        }
        found
            .members
            .first()
            .map(|member| member.address.clone())
            .ok_or_else(|| {
                TallyOwlError::unavailable(format!("`{tablet}` has no replica to read from."))
            })
    }

    fn call(
        &self,
        client: &Client,
        address: &str,
        request: &PartialQueryRequest,
    ) -> Result<Partial, TallyOwlError> {
        use tallyowl_cluster_api::codec::{
            decode_partial_query_response, encode_partial_query_request,
        };
        let response = client
            .call(
                REPLICATION_SERVICE,
                "partial-aggregate",
                encode_partial_query_request(request),
            )
            .map_err(|e| {
                // A broken connection is the ordinary case when a peer
                // restarts. Drop it so the next read opens a fresh one.
                self.connections.forget(address);
                TallyOwlError::unavailable(format!("{address} did not answer: {e}"))
            })?;
        if response.variant.as_deref() == Some(tallyowl_rpc::SERVICE_ERROR_VARIANT) {
            return Err(crate::transfer::read_refusal(&response.payload));
        }
        let wire = decode_partial_query_response(&response.payload).map_err(|e| {
            TallyOwlError::internal(format!("A partial answer could not be read: {e}"))
        })?;
        crate::service::from_wire_partial(&wire)
    }
}

fn basis_name(basis: TimeBasis) -> &'static str {
    match basis {
        TimeBasis::OccurredAt => "occurred_at",
        TimeBasis::ReceivedAt => "received_at",
        TimeBasis::CommittedAt => "committed_at",
    }
}

fn to_wire(plan: &Plan, tablet: &str, max_rows: usize) -> PartialQueryRequest {
    let (column, value) = match &plan.kind {
        PartialKind::Lookup { column, value } => (Some(column.clone()), Some(value.clone())),
        _ => (None, None),
    };
    PartialQueryRequest {
        tablet: tablet.to_string(),
        generation: plan.generation,
        kind: match &plan.kind {
            PartialKind::Count => tallyowl_cluster_api::types::PartialKind::Count,
            PartialKind::Trend { .. } => tallyowl_cluster_api::types::PartialKind::Trend,
            PartialKind::Rows { .. } => tallyowl_cluster_api::types::PartialKind::Rows,
            PartialKind::Lookup { .. } => tallyowl_cluster_api::types::PartialKind::Lookup,
            PartialKind::Aggregate { .. } => tallyowl_cluster_api::types::PartialKind::Aggregate,
        },
        project_id: plan.project_id.to_vec(),
        range_start: plan.range_start,
        range_end: plan.range_end,
        basis: basis_name(plan.basis).to_string(),
        bucket_ms: match plan.kind {
            PartialKind::Trend { bucket_ms } => Some(bucket_ms),
            _ => None,
        },
        event_name: plan.event_name.clone(),
        column,
        value,
        max_rows: plan.kind.moves_rows().then_some(max_rows as u64),
        aggregate_plan: match &plan.kind {
            PartialKind::Aggregate { plan } => Some(plan.clone()),
            _ => None,
        },
        require_watermark: (plan.require_watermark > 0).then_some(plan.require_watermark),
        max_staleness_ms: match plan.consistency {
            Consistency::BoundedStale { max_staleness_ms } => Some(max_staleness_ms),
            Consistency::Committed => None,
        },
    }
}

fn read_failure(e: TallyOwlError) -> StoreError {
    match e.code {
        ErrorCode::InvalidArgument => StoreError::InvalidArgument(e.message),
        ErrorCode::ResourceExhausted | ErrorCode::BudgetExceeded => {
            StoreError::Exhausted(e.message)
        }
        _ => StoreError::Unavailable(e.message),
    }
}

impl TabletReads for ClusterReads {
    fn scan(
        &self,
        project_id: [u8; 16],
        range_start: i64,
        range_end: i64,
        basis: TimeBasis,
    ) -> Result<Scanned, StoreError> {
        let plan = self
            .plan_for(
                project_id,
                range_start,
                range_end,
                basis,
                PartialKind::Rows {
                    // The merge truncates to this, so the bound is the whole
                    // fan-out rather than one tablet. A tablet that filled its
                    // own bound has already marked its part incomplete.
                    max_rows: self.max_rows_for_each_tablet * self.max_fan_out,
                },
                None,
            )
            .map_err(read_failure)?;
        let merged = self.run(&plan).map_err(read_failure)?;
        Ok(Scanned {
            rows: merged.rows,
            incomplete: !merged.complete,
        })
    }

    fn trend(
        &self,
        project_id: [u8; 16],
        range_start: i64,
        range_end: i64,
        basis: TimeBasis,
        bucket_ms: i64,
        name: Option<&str>,
    ) -> Result<Trend, StoreError> {
        let plan = self
            .plan_for(
                project_id,
                range_start,
                range_end,
                basis,
                PartialKind::Trend { bucket_ms },
                name,
            )
            .map_err(read_failure)?;
        let merged = self.run(&plan).map_err(read_failure)?;
        Ok(Trend {
            basis,
            bucket_ms,
            buckets: merged.buckets,
            total: merged.count,
            commit_watermark: merged.commit_watermark,
            incomplete: !merged.complete,
        })
    }

    fn partial_aggregates(
        &self,
        plan: &[u8],
        project_id: [u8; 16],
        range_start: i64,
        range_end: i64,
        basis: TimeBasis,
    ) -> Result<tallyowl_store::store::PartialAggregates, StoreError> {
        // **This is the push-down L101 recorded as not built.** Before it, an
        // aggregate over more than one tablet asked for rows and computed the
        // measures in the coordinator, which is correctness-preserving and is
        // not what `docs/QUERY.md` section 5 asks for.
        let planned = self
            .plan_for(
                project_id,
                range_start,
                range_end,
                basis,
                PartialKind::Aggregate {
                    plan: plan.to_vec(),
                },
                None,
            )
            .map_err(read_failure)?;
        let merged = self.run(&planned).map_err(read_failure)?;
        Ok(tallyowl_store::store::PartialAggregates {
            states: merged.aggregate_states,
            complete: merged.complete,
            unreadable: merged.warnings,
        })
    }

    fn lookup_correlated(&self, column: &str, value: &[u8]) -> Result<Scanned, StoreError> {
        // An exact lookup goes to every tablet that could hold the value and
        // the answers are concatenated, never sampled. `AGENTS.md`: "Do not
        // silently drop, coalesce, or reject a value because it has high
        // cardinality." The whole time range, because a correlation value is
        // not a time predicate.
        let plan = self
            .plan_for(
                [0u8; 16],
                i64::MIN,
                i64::MAX,
                TimeBasis::OccurredAt,
                PartialKind::Lookup {
                    column: column.to_string(),
                    value: value.to_vec(),
                },
                None,
            )
            .map_err(read_failure)?;
        let merged = self.run(&plan).map_err(read_failure)?;
        let mut rows: Vec<EventRow> = merged.rows;
        rows.sort_by_key(|row| row.occurred_at);
        Ok(Scanned {
            rows,
            incomplete: !merged.complete,
        })
    }

    fn unreadable(&self) -> Vec<String> {
        self.unreachable.lock().expect("unreachable").clone()
    }
}
