//! Where the head meets replicated storage.
//!
//! A home installation never enters this file's second half. It has one node,
//! one tablet, one voter, and `replication.listen` is empty, so
//! [`start`] returns the local store unchanged and the head is exactly what it
//! was before Phase 7. That is the point of the [`tallyowl_store::Store`]
//! contract: the seam is the same at one node and at ten thousand.
//!
//! # What starting a cluster node does
//!
//! 1. build the group registry for this node, with durable consensus state
//!    beside the data directory;
//! 2. start this node's tablet group and its cell-controller group;
//! 3. serve `TallyOwlReplication` and `TallyOwlCluster` on
//!    `replication.listen`, which no application and no browser ever reaches;
//! 4. hand the rest of the head a [`tallyowl_cluster::replicated::ReplicatedStore`],
//!    so a commit goes through the tablet group and a read comes from the local
//!    replica.
//!
//! # Why the address is what decides
//!
//! An operator turns replication on by giving this node an address that peers
//! can reach. `AGENTS.md` says a port never opens because the binary contains a
//! feature, and this holds the same rule: no address, no listener, no cluster.

use std::sync::{Arc, Mutex};

use tallyowl_cluster::clusterservice::ClusterService;
use tallyowl_cluster::controller::{Controller, Mode, Thresholds};
use tallyowl_cluster::fanout::{ClusterReads, TabletReads};
use tallyowl_cluster::groups::{GroupKey, GroupRegistry};
use tallyowl_cluster::health::SlowAction;
use tallyowl_cluster::plane::ControlPlane;
use tallyowl_cluster::query::Partial;
use tallyowl_cluster::raft::machine::{ControllerMachine, TabletMachine};
use tallyowl_cluster::replicated::ReplicatedStore;
use tallyowl_cluster::service::{PartialSource, ReplicationService};
use tallyowl_cluster::topology::{ControllerCommand, Member, ReceiptPolicy, Topology};
use tallyowl_cluster::transfer::{StoreSegments, TabletSegments};
use tallyowl_cluster_api::types::ReplicaStatus;
use tallyowl_config::Config;
use tallyowl_obs::error::TallyOwlError;
use tallyowl_obs::log::Logger;
use tallyowl_rpc::{Dispatcher, Server};
use tallyowl_store::{SegmentedStore, Store};

/// The tablet a cluster node starts with.
///
/// A cell normally learns its tablets from its controller quorum. This is the
/// name the first one takes, so that a two-node or three-node installation
/// works before anybody has run a placement command.
const FIRST_TABLET: &str = "t0";

/// What the head got from `start`.
pub struct Cluster {
    /// The store the rest of the head uses. It is the local store in the home
    /// profile and the tablet group in a cluster.
    pub store: Arc<dyn Store>,
    /// The control plane, when this node is part of a cluster.
    pub plane: Option<Arc<ControlPlane>>,
    /// The replication listener. Held so that dropping the cluster stops it.
    pub server: Option<Server>,
    pub registry: Option<Arc<GroupRegistry>>,
}

impl Cluster {
    /// A home installation: the local store, and nothing else.
    fn alone(store: Arc<dyn Store>) -> Cluster {
        Cluster {
            store,
            plane: None,
            server: None,
            registry: None,
        }
    }

    pub fn is_replicated(&self) -> bool {
        self.plane.is_some()
    }
}

/// Answers a partial aggregate from the local replica.
struct LocalReplica {
    node: String,
    tablet: String,
    store: Arc<dyn Store>,
    registry: Arc<GroupRegistry>,
    /// The coordinator's own aggregation, over this node's own rows. See
    /// [`crate::query::QueryService::partial_aggregate`].
    ///
    /// It reads the **local** store rather than the replicated one, or a
    /// pushed-down aggregate would fan out again from every tablet it reached.
    query: Option<Arc<crate::query::QueryService>>,
}

impl PartialSource for LocalReplica {
    fn partial(
        &self,
        tablet: &str,
        request: &tallyowl_cluster_api::types::PartialQueryRequest,
    ) -> Result<Partial, TallyOwlError> {
        let project = to_id(&request.project_id)?;
        let basis = match request.basis.as_str() {
            "received_at" => tallyowl_store::TimeBasis::ReceivedAt,
            "committed_at" => tallyowl_store::TimeBasis::CommittedAt,
            _ => tallyowl_store::TimeBasis::OccurredAt,
        };
        let mut partial = Partial::for_tablet(tablet);
        partial.commit_watermark = self
            .registry
            .applied_index(&GroupKey::Tablet(tablet.to_string()));

        match (request.column.as_deref(), request.value.as_deref()) {
            // An exact lookup on a high-cardinality value. The tablet prunes
            // with its own locator; the rows decide.
            (Some(column), Some(value)) => {
                let found = self
                    .store
                    .lookup_correlated(column, value)
                    .map_err(|e| TallyOwlError::internal(e.to_string()))?;
                partial.complete = !found.incomplete;
                partial.count = found.rows.len() as u64;
                partial.rows = found.rows;
            }
            // A detail read. `docs/QUERY.md` section 10 names the three forms
            // that move rows and this is one of them; the coordinator asks for
            // it when the operator above it reads fields rather than a
            // pre-aggregated state.
            // **A general aggregate, computed here.** L101 recorded that this
            // did not exist and that the coordinator therefore asked for rows.
            // The plan is the coordinator's own query node, and this runs the
            // coordinator's own aggregation over local rows, so there is one
            // implementation of a sum rather than two.
            _ if request.kind == tallyowl_cluster_api::types::PartialKind::Aggregate => {
                let Some(plan) = request.aggregate_plan.as_deref() else {
                    return Err(TallyOwlError::invalid_argument(
                        "A pushed-down aggregate arrived with no plan to run.",
                    ));
                };
                let Some(query) = self.query.as_ref() else {
                    return Err(TallyOwlError::new(
                        tallyowl_obs::ErrorCode::FailedPrecondition,
                        "This node cannot compute a partial aggregate, because it was started without a query service.".to_string(),
                    ));
                };
                partial.aggregate_state = query.partial_aggregate(plan)?;
                partial.complete = partial.aggregate_state.is_some();
                if partial.aggregate_state.is_none() {
                    // A measure with no partial state. Saying so rather than
                    // sending rows keeps the contract: this form never moves a
                    // row, and the coordinator falls back on its own.
                    partial.missing.push(tallyowl_cluster::query::MissingRange {
                        tablet: tablet.to_string(),
                        range_start: request.range_start,
                        range_end: request.range_end,
                    });
                }
            }
            _ if request.kind == tallyowl_cluster_api::types::PartialKind::Rows => {
                let scanned = self
                    .store
                    .scan(project, request.range_start, request.range_end, basis)
                    .map_err(|e| TallyOwlError::internal(e.to_string()))?;
                let wanted = request
                    .event_name
                    .as_deref()
                    .filter(|name| !name.is_empty());
                let mut rows: Vec<tallyowl_store::row::EventRow> = scanned
                    .rows
                    .into_iter()
                    .filter(|row| wanted.is_none_or(|name| row.name == name))
                    .collect();
                partial.complete = !scanned.incomplete;
                // A tablet that filled its bound marks its own part
                // incomplete. The coordinator then refuses rather than
                // returning a number that is smaller than the truth.
                if let Some(max_rows) = request.max_rows {
                    if rows.len() as u64 > max_rows {
                        rows.truncate(max_rows as usize);
                        partial.complete = false;
                        partial.missing.push(tallyowl_cluster::query::MissingRange {
                            tablet: tablet.to_string(),
                            range_start: request.range_start,
                            range_end: request.range_end,
                        });
                    }
                }
                partial.count = rows.len() as u64;
                partial.rows = rows;
            }
            _ => match request.bucket_ms {
                // A trend. The partial state is a bucket map, and merging is
                // addition on a shared bucket start.
                Some(bucket_ms) if bucket_ms > 0 => {
                    let trend = self
                        .store
                        .trend(
                            project,
                            request.range_start,
                            request.range_end,
                            basis,
                            bucket_ms,
                            request.event_name.as_deref(),
                        )
                        .map_err(|e| TallyOwlError::internal(e.to_string()))?;
                    partial.complete = !trend.incomplete;
                    partial.count = trend.total;
                    partial.buckets = trend.buckets.into_iter().collect();
                }
                // A count. The coordinator never pulls rows for this.
                _ => {
                    let trend = self
                        .store
                        .trend(
                            project,
                            request.range_start,
                            request.range_end,
                            basis,
                            (request.range_end - request.range_start).max(1),
                            request.event_name.as_deref(),
                        )
                        .map_err(|e| TallyOwlError::internal(e.to_string()))?;
                    partial.complete = !trend.incomplete;
                    partial.count = trend.total;
                }
            },
        }
        Ok(partial)
    }

    fn status(&self) -> ReplicaStatus {
        // The exact high-water mark, which is what `docs/STORAGE.md` section 8
        // asks a read replica to report, and beside it how old this replica's
        // newest data is. The second is not "how far behind the leader", and a
        // caller that treated it as that would be wrong on an idle tablet. See
        // `TabletMarks::last_applied_at`.
        let marks = self
            .registry
            .machine(&GroupKey::Tablet(self.tablet.clone()))
            .ok()
            .and_then(|machine| {
                tallyowl_cluster::raft::decode::<tallyowl_cluster::raft::machine::TabletMarks>(
                    &machine.snapshot(),
                )
                .ok()
            });
        let lag_ms = marks
            .filter(|marks| marks.last_applied_at > 0)
            .map(|marks| tallyowl_obs::time::now_ms() - marks.last_applied_at);
        ReplicaStatus {
            node: self.node.clone(),
            tablets: Vec::new(),
            applied_watermark: self.store.commit_watermark(),
            lag_ms,
            writable: self.store.is_writable(),
        }
    }
}

fn to_id(bytes: &[u8]) -> Result<[u8; 16], TallyOwlError> {
    bytes.try_into().map_err(|_| {
        TallyOwlError::invalid_argument(format!(
            "An ID is 16 bytes and this one is {}.",
            bytes.len()
        ))
    })
}

/// Start replicated storage, or do nothing.
///
/// The local store comes back unchanged when this installation has one node,
/// which is what makes the home profile pay nothing for a feature it does not
/// use.
pub fn start(
    config: &Config,
    segmented: Arc<SegmentedStore>,
    logger: &Logger,
) -> Result<Cluster, Box<dyn std::error::Error>> {
    let local: Arc<dyn Store> = Arc::clone(&segmented) as Arc<dyn Store>;
    let listen = config.text("replication.listen");
    let voters = config.integer("storage.tabletVoters").max(1) as usize;
    if listen.is_empty() {
        if voters > 1 {
            // The configuration check refuses this before we reach here. Saying
            // it again costs nothing and covers a code path reached another way.
            logger.warning(
                "This node is configured for more than one voter and has no replication address, so it has no peers. Writes will not commit.",
                &[("voters", &voters.to_string())],
            );
        }
        return Ok(Cluster::alone(local));
    }

    let node = {
        let configured = config.text("node.name");
        if configured.is_empty() {
            // A node with no name takes one from its address, which is stable
            // for as long as the address is. An enrolled node is given a name
            // by the control plane and sets it here.
            format!("node-{}", listen.replace(['.', ':'], "-"))
        } else {
            configured.to_string()
        }
    };
    let region = config.text("cell.region").to_string();
    let cell = config.text("cell.id").to_string();
    let domain = config.text("node.failureDomain").to_string();

    let root = std::path::Path::new(config.text("head.dataDir")).join("consensus");
    let registry = GroupRegistry::new(node.clone(), listen, Some(root))?;
    registry.set_write_timeout(std::time::Duration::from_millis(
        config.duration_ms("replication.writeTimeout").max(1) as u64,
    ));
    // L099 made these settings rather than constants. A group reads them when it
    // starts, so a change takes effect the next time this node runs the group.
    registry.set_log_bounds(
        config.integer("replication.snapshotEvery").max(1) as u64,
        config.integer("replication.keepAfterSnapshot").max(0) as u64,
    );

    let topology = Arc::new(Mutex::new(Topology::new()));
    let policy = match config.text("storage.receiptPolicy") {
        "local-quorum" => ReceiptPolicy::LocalQuorum,
        "remote-one" => ReceiptPolicy::RemoteOne,
        _ => ReceiptPolicy::LocalOne,
    };

    // This node, and whatever peers configuration named. A cell normally learns
    // its peers from its controller quorum; these are what a first bootstrap
    // uses.
    let mut members = vec![Member::voter(node.clone(), listen)
        .in_region(&region)
        .in_domain(&domain)];
    for (index, peer) in config.list("replication.peers").iter().enumerate() {
        let peer = peer.trim();
        if peer.is_empty() {
            continue;
        }
        members.push(
            Member::voter(format!("node-{}", peer.replace(['.', ':'], "-")), peer)
                .in_region(&region)
                // Without a told domain, each peer counts as its own. Assuming
                // they share one would make placement believe a spread it does
                // not have.
                .in_domain(format!("peer-{index}")),
        );
    }

    {
        let mut held = topology.lock().expect("topology");
        for member in &members {
            held.apply(&ControllerCommand::RegisterNode {
                node: member.node.clone(),
                address: member.address.clone(),
                region: member.region.clone(),
                domain: member.domain.clone(),
            })
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        }
        held.apply(&ControllerCommand::RegisterCell {
            cell: cell.clone(),
            region: region.clone(),
            controllers: members.clone(),
        })
        .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        held.apply(&ControllerCommand::CreateTablet {
            tablet: FIRST_TABLET.into(),
            cell: cell.clone(),
            region: region.clone(),
            shard_start: 0,
            shard_end: tallyowl_cluster::topology::VIRTUAL_SHARDS,
            members: members.clone(),
            receipt_policy: policy,
        })
        .map_err(|e| TallyOwlError::internal(e.to_string()))?;
    }

    // The cell controller quorum. It holds topology and configuration and no
    // telemetry, and it is a different group from the tablet: a 400-node cell
    // still has three or five controller voters, and every storage node votes
    // only in the tablet groups it holds. `docs/STORAGE.md` section 7.
    //
    // A one-node installation starts it too. The group has one voter and does
    // the same thing a quorum does, so the code path that applies a topology
    // change is one path at every size.
    let controllers = GroupKey::CellController(cell.clone());
    registry.start(
        controllers.clone(),
        Arc::new(ControllerMachine::new(Arc::clone(&topology))),
        members.clone(),
        0,
    )?;

    let group = GroupKey::Tablet(FIRST_TABLET.into());
    registry.start(
        group.clone(),
        Arc::new(TabletMachine::new(Arc::clone(&local))),
        members.clone(),
        0,
    )?;

    let controller = Controller {
        mode: Mode::parse(config.text("placement.mode")).unwrap_or(Mode::RecommendOnly),
        thresholds: Thresholds {
            split_above_bytes: config.bytes("placement.splitAbove").max(0) as u64,
            merge_below_bytes: config.bytes("placement.mergeBelow").max(0) as u64,
            concurrent_changes: config.integer("placement.concurrentChanges").max(1) as usize,
            ..Thresholds::default()
        },
        slow_action: SlowAction::parse(config.text("placement.slowNode.action"))
            .unwrap_or(SlowAction::Alert),
        voters,
        ..Controller::new(cell.clone(), region.clone())
    };
    let plane = Arc::new(ControlPlane::new(
        Arc::clone(&registry),
        Arc::clone(&topology),
        controller,
    ));

    let source = Arc::new(LocalReplica {
        node: node.clone(),
        tablet: FIRST_TABLET.to_string(),
        store: Arc::clone(&local),
        registry: Arc::clone(&registry),
        // **Over the local store, never the replicated one.** A pushed-down
        // aggregate that read the replicated store would fan out again from
        // every tablet it reached, and the fan-out would not terminate.
        query: Some(Arc::new(crate::query::QueryService {
            store: Arc::clone(&local),
            max_runtime_ms: config.duration_ms("query.maxRuntime"),
            max_expression_depth: config.integer("query.maxExpressionDepth").max(1) as u32,
            guards: crate::analysis::Guards::default(),
            attribution: Default::default(),
            policy: Default::default(),
            identity: Default::default(),
        })),
    });
    // The sealed segments this node serves and adopts. A replica behind a
    // purged log catches up through these, and a tablet movement copies them.
    // See `tallyowl_cluster::transfer` and L087.
    let reporting = Arc::clone(&registry);
    let reporting_group = group.clone();
    let segments: Arc<dyn TabletSegments> = Arc::new(
        StoreSegments::new(FIRST_TABLET, Arc::clone(&segmented))
            .reporting_applied(Arc::new(move || reporting.applied_index(&reporting_group))),
    );
    let replication = ReplicationService::new(
        Arc::clone(&registry),
        Arc::clone(&topology),
        source.clone() as Arc<dyn PartialSource>,
    )
    .serving_segments(Arc::clone(&segments));
    let cluster_service = ClusterService::new(Arc::clone(&plane))
        .restoring_into(FIRST_TABLET, Arc::clone(&segments))
        .keeping_snapshots_in(std::path::Path::new(config.text("head.dataDir")).join("snapshots"));
    let dispatcher = Arc::new(Both {
        replication,
        cluster: cluster_service,
    }) as Arc<dyn Dispatcher>;

    let server = tallyowl_rpc::serve(
        listen,
        dispatcher,
        tallyowl_cluster::raft::network::MAX_FRAME_BYTES,
    )?;
    logger.info(
        "Serving replication and cluster control. An application never reaches this address.",
        &[
            ("address", &server.local_address().to_string()),
            ("node", &node),
            ("cell", &cell),
            ("tablet", FIRST_TABLET),
            ("voters", &members.len().to_string()),
        ],
    );

    // A group that already has a log keeps the membership it committed. One
    // that does not is created here, which is the only place a voter set is
    // created and which a role token can never reach.
    for (key, what) in [(&controllers, "cell controller quorum"), (&group, "tablet")] {
        if let Err(e) = registry.bootstrap(key, &members) {
            logger.info(
                "This group already has a voter set, so it was not created again.",
                &[("group", what), ("reason", &e.message)],
            );
        }
    }

    // Reads cross tablets. A cell with one tablet reads its own replica and
    // pays nothing for this; a cell with two would otherwise answer from one of
    // them and not know it was short. See `tallyowl_cluster::fanout`.
    let across: Arc<dyn TabletReads> = Arc::new(
        ClusterReads::new(
            Arc::clone(&topology),
            Arc::clone(&registry),
            source.clone() as Arc<dyn PartialSource>,
        )
        .with_max_fan_out(config.integer("query.maxFanOut").max(1) as usize),
    );

    let replicated = Arc::new(
        ReplicatedStore::new(
            Arc::clone(&registry),
            FIRST_TABLET,
            Arc::clone(&local),
            policy,
            region,
        )
        .with_remote_copy_timeout(std::time::Duration::from_millis(
            config.duration_ms("replication.writeTimeout").max(1) as u64,
        ))
        .reading_across(across),
    ) as Arc<dyn Store>;

    Ok(Cluster {
        store: replicated,
        plane: Some(plane),
        server: Some(server),
        registry: Some(registry),
    })
}

/// One listener, two services.
///
/// Both are node-to-node or operator surfaces and neither is reachable by an
/// application, so they share one address rather than opening two ports for a
/// feature an operator turned on once.
struct Both {
    replication: ReplicationService,
    cluster: ClusterService,
}

impl Dispatcher for Both {
    fn dispatch(&self, request: &tallyowl_rpc::Request) -> tallyowl_rpc::Outcome {
        match request.service.as_str() {
            "TallyOwlCluster" => self.cluster.dispatch(request),
            _ => self.replication.dispatch(request),
        }
    }
}
