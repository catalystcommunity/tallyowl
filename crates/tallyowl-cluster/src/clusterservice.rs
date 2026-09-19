//! `TallyOwlCluster`: what an operator and a gateway say about topology.
//!
//! Every operation here is thin on purpose. It decodes, it builds a
//! [`crate::topology::ControllerCommand`], and it hands it to
//! [`crate::plane::ControlPlane`]. **No rule is checked here**, because a rule
//! checked at the door and not in the state machine is a rule a replayed log
//! can break, and the log is replayed on every restart.
//!
//! # The three operations that are not thin
//!
//! `unsafe-recover` produces several commands as one intention, and applying
//! part of it would leave a working tablet with no record that anything was
//! lost. `move-tablet` refuses until the copy is proved. `route` answers from
//! the cached topology, so it keeps working when the controller quorum does
//! not.

use std::sync::Arc;

use tallyowl_cluster_api::codec::*;
use tallyowl_cluster_api::types::{
    CellInfo, ClusterAck, DirectoryResponse, GroupMember, MemberRole as WireRole,
    NodeCondition as WireCondition, NodeState as WireNodeState, ProjectPlacement, RouteResponse,
    TabletInfo, TabletState as WireTabletState, TopologyResponse, UnsafeRecoverResponse,
};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_rpc::{malformed, reply, unknown_operation, Dispatcher, Outcome, Request};

use crate::directory::DirectoryCommand;
use crate::plane::ControlPlane;
use crate::recovery::{clear_degraded, unsafe_recover, UnsafeRequest};
use crate::routing::route_write;
use crate::topology::{
    ControllerCommand, Member, MemberRole, NodeState, ReceiptPolicy, TabletState, Topology,
};

/// The operator and gateway surface.
pub struct ClusterService {
    plane: Arc<ControlPlane>,
    /// The tablet this node holds, and the segments it can adopt. `restore`
    /// needs both: a snapshot is put back as segments, through the same path a
    /// catching-up replica uses.
    restoring: Option<(String, Arc<dyn crate::transfer::TabletSegments>)>,
    /// Where snapshots are kept. A snapshot writes here and a restore reads
    /// from here, so an operator names a snapshot rather than a path.
    snapshot_root: Option<std::path::PathBuf>,
}

impl ClusterService {
    pub fn new(plane: Arc<ControlPlane>) -> ClusterService {
        ClusterService {
            plane,
            restoring: None,
            snapshot_root: None,
        }
    }

    /// Say which tablet this node holds and where its segments live.
    pub fn restoring_into(
        mut self,
        tablet: impl Into<String>,
        segments: Arc<dyn crate::transfer::TabletSegments>,
    ) -> ClusterService {
        self.restoring = Some((tablet.into(), segments));
        self
    }

    /// Say where snapshots are kept.
    pub fn keeping_snapshots_in(mut self, root: impl Into<std::path::PathBuf>) -> ClusterService {
        self.snapshot_root = Some(root.into());
        self
    }

    fn ack(&self, generation: u64) -> Outcome {
        reply(
            "ClusterAck",
            encode_cluster_ack(&ClusterAck {
                accepted: true,
                generation,
                message: None,
            }),
        )
    }

    fn run(&self, command: ControllerCommand) -> Outcome {
        match self.plane.apply(command) {
            Ok(generation) => self.ack(generation),
            Err(e) => crate::service::refusal(&e),
        }
    }
}

impl Dispatcher for ClusterService {
    fn dispatch(&self, request: &Request) -> Outcome {
        match request.op.as_str() {
            "describe-topology" => self.describe(&request.payload),
            "route" => self.route(&request.payload),
            "bootstrap-group" => self.bootstrap(&request.payload),
            "add-replica" => self.add_replica(&request.payload),
            "remove-replica" => self.remove_replica(&request.payload),
            "split-tablet" => self.split(&request.payload),
            "merge-tablets" => self.merge(&request.payload),
            "move-tablet" => self.move_tablet(&request.payload),
            "set-receipt-policy" => self.set_policy(&request.payload),
            "fail-over-region" => self.fail_over(&request.payload),
            "unsafe-recover" => self.unsafe_recover(&request.payload),
            "clear-degraded" => self.clear_degraded(&request.payload),
            "snapshot-cluster" => self.snapshot(&request.payload),
            "restore" => self.restore(&request.payload),
            "directory" => self.directory(&request.payload),
            "assign-project" => self.assign_project(&request.payload),
            other => unknown_operation(&request.service, other),
        }
    }
}

impl ClusterService {
    fn describe(&self, payload: &[u8]) -> Outcome {
        let request = match decode_describe_topology_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let topology = self.plane.topology();
        let registry = self.plane.registry();
        let wanted = request.cell.as_deref();

        let cells = topology
            .cells()
            .filter(|c| wanted.is_none_or(|name| c.name == name))
            .map(|cell| CellInfo {
                cell: cell.name.clone(),
                region: cell.region.clone(),
                controllers: cell.controllers.iter().map(to_wire_member).collect(),
                generation: topology.generation,
                quorum_available: self.plane.has_quorum(),
            })
            .collect();

        let tablets = topology
            .tablets()
            .filter(|t| wanted.is_none_or(|name| t.cell == name))
            .map(|tablet| {
                let group = crate::groups::GroupKey::Tablet(tablet.name.clone());
                TabletInfo {
                    tablet: tablet.name.clone(),
                    cell: tablet.cell.clone(),
                    write_region: tablet.write_region.clone(),
                    epoch: tablet.epoch,
                    generation: topology.generation,
                    state: to_wire_state(tablet.state),
                    shard_start: tablet.shard_start,
                    shard_end: tablet.shard_end,
                    members: tablet.members.iter().map(to_wire_member).collect(),
                    receipt_policy: tablet.receipt_policy.as_str().to_string(),
                    leader: registry.leader(&group),
                    commit_watermark: registry.applied_index(&group),
                    stored_bytes: 0,
                    degraded_since: tablet.degraded.as_ref().map(|d| d.since),
                    degraded_range_start: tablet.degraded.as_ref().map(|d| d.range_start),
                    degraded_range_end: tablet.degraded.as_ref().map(|d| d.range_end),
                }
            })
            .collect();

        let nodes = topology.nodes().map(to_wire_condition).collect();

        reply(
            "TopologyResponse",
            encode_topology_response(&TopologyResponse {
                cells,
                tablets,
                nodes,
                generation: topology.generation,
                // A cell keeps working when the directory is gone, and the
                // answer says so rather than pretending everything is fine.
                directory_available: self.plane.directory_cache().reachable(),
            }),
        )
    }

    fn route(&self, payload: &[u8]) -> Outcome {
        let request = match decode_route_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let topology = self.plane.topology();
        let registry = self.plane.registry();
        let project = match sixteen(&request.project_id) {
            Ok(project) => project,
            Err(e) => return crate::service::refusal(&e),
        };
        match route_write(&topology, &project, &request.affinity_key, |tablet| {
            registry.leader(&crate::groups::GroupKey::Tablet(tablet.to_string()))
        }) {
            Err(e) => crate::service::refusal(&TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                e.to_string(),
            )),
            Ok(route) => reply(
                "RouteResponse",
                encode_route_response(&RouteResponse {
                    cell: route.cell,
                    shard: route.shard,
                    tablet: route.tablet,
                    leader: route.leader,
                    leader_address: route.leader_address,
                    generation: route.generation,
                    epoch: route.epoch,
                    receipt_policy: route.receipt_policy.as_str().to_string(),
                }),
            ),
        }
    }

    fn bootstrap(&self, payload: &[u8]) -> Outcome {
        let request = match decode_bootstrap_group_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let members: Vec<Member> = request.members.iter().map(from_wire_member).collect();
        let key = match crate::groups::GroupKey::from_wire(
            request.group.kind,
            request.group.name.as_deref(),
        ) {
            Ok(key) => key,
            Err(e) => return crate::service::refusal(&e),
        };
        match self.plane.registry().bootstrap(&key, &members) {
            Ok(()) => self.ack(self.plane.topology().generation),
            Err(e) => crate::service::refusal(&e),
        }
    }

    fn add_replica(&self, payload: &[u8]) -> Outcome {
        let request = match decode_change_replica_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let member = Member {
            node: request.node.clone(),
            role: from_wire_role(request.role),
            address: request.address.clone(),
            region: request.region.clone().unwrap_or_else(|| "home".into()),
            domain: request.domain.clone().unwrap_or_else(|| "default".into()),
        };
        // The group first, then the topology. A topology that named a member
        // the group did not have would send a gateway to a node that refuses.
        let key = crate::groups::GroupKey::Tablet(request.tablet.clone());
        if self.plane.registry().holds(&key) {
            if let Err(e) = self.plane.registry().add_member(&key, &member) {
                return crate::service::refusal(&e);
            }
        }
        self.run(ControllerCommand::AddReplica {
            tablet: request.tablet,
            member,
        })
    }

    fn remove_replica(&self, payload: &[u8]) -> Outcome {
        let request = match decode_remove_replica_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        // The topology first this time, because it holds the rule that a group
        // may not lose its last voter. Removing from the group first and then
        // being refused here would leave the two disagreeing.
        let outcome = self.run(ControllerCommand::RemoveReplica {
            tablet: request.tablet.clone(),
            node: request.node.clone(),
        });
        let key = crate::groups::GroupKey::Tablet(request.tablet.clone());
        if self.plane.registry().holds(&key) {
            if let Err(e) = self.plane.registry().remove_member(&key, &request.node) {
                return crate::service::refusal(&e);
            }
        }
        outcome
    }

    fn split(&self, payload: &[u8]) -> Outcome {
        let request = match decode_split_tablet_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let topology = self.plane.topology();
        let Some(tablet) = topology.tablet(&request.tablet) else {
            return crate::service::refusal(&TallyOwlError::new(
                ErrorCode::NotFound,
                format!("No tablet is named `{}`.", request.tablet),
            ));
        };
        let at = match request
            .at_shard
            .or_else(|| crate::controller::split_point(tablet.shard_start, tablet.shard_end))
        {
            Some(at) => at,
            None => {
                return crate::service::refusal(&TallyOwlError::new(
                    ErrorCode::FailedPrecondition,
                    format!(
                        "`{}` owns one virtual shard and cannot be split any further.",
                        request.tablet
                    ),
                ))
            }
        };
        self.run(ControllerCommand::SplitTablet {
            right: format!("{}-b", request.tablet),
            tablet: request.tablet,
            at_shard: at,
        })
    }

    fn merge(&self, payload: &[u8]) -> Outcome {
        let request = match decode_merge_tablets_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        self.run(ControllerCommand::MergeTablets {
            left: request.left,
            right: request.right,
        })
    }

    fn move_tablet(&self, payload: &[u8]) -> Outcome {
        let request = match decode_move_tablet_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let topology = self.plane.topology();
        let Some(tablet) = topology.tablet(&request.tablet) else {
            return crate::service::refusal(&TallyOwlError::new(
                ErrorCode::NotFound,
                format!("No tablet is named `{}`.", request.tablet),
            ));
        };
        let Some(target) = topology.node(&request.onto) else {
            return crate::service::refusal(&TallyOwlError::new(
                ErrorCode::NotFound,
                format!("No node is named `{}`.", request.onto),
            ));
        };
        if tablet.member(&request.away_from).is_none() {
            return crate::service::refusal(&TallyOwlError::new(
                ErrorCode::NotFound,
                format!(
                    "`{}` does not hold `{}`.",
                    request.away_from, request.tablet
                ),
            ));
        }
        // The state change starts the movement. The placement change happens
        // when `crate::movement::Movement::publish` says the copy is proved,
        // and never before: this operation begins a move rather than finishing
        // one.
        let onto = Member {
            node: target.name.clone(),
            role: MemberRole::Voter,
            address: target.address.clone(),
            region: target.region.clone(),
            domain: target.domain.clone(),
        };
        match self.plane.apply(ControllerCommand::SetTabletState {
            tablet: request.tablet.clone(),
            state: TabletState::Moving,
        }) {
            Err(e) => crate::service::refusal(&e),
            Ok(generation) => reply(
                "ClusterAck",
                encode_cluster_ack(&ClusterAck {
                    accepted: true,
                    generation,
                    message: Some(format!(
                        "`{}` is moving from `{}` to `{}`. It keeps serving reads and writes, and placement changes only after every segment is copied and verified.",
                        request.tablet, request.away_from, onto.node
                    )),
                }),
            ),
        }
    }

    fn set_policy(&self, payload: &[u8]) -> Outcome {
        let request = match decode_set_receipt_policy_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let Some(policy) = ReceiptPolicy::parse(&request.policy) else {
            return crate::service::refusal(&TallyOwlError::invalid_argument(format!(
                "`{}` is not a receipt policy. Use `local-one`, `local-quorum`, or `remote-one`.",
                request.policy
            )));
        };
        self.run(ControllerCommand::SetReceiptPolicy {
            tablet: request.tablet,
            policy,
        })
    }

    fn fail_over(&self, payload: &[u8]) -> Outcome {
        let request = match decode_fail_over_region_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        self.run(ControllerCommand::FailOverRegion {
            tablet: request.tablet,
            onto_region: request.onto_region,
        })
    }

    fn unsafe_recover(&self, payload: &[u8]) -> Outcome {
        let request = match decode_unsafe_recover_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let topology = self.plane.topology();
        let at = tallyowl_obs::time::now_ms();
        let group = crate::groups::GroupKey::Tablet(request.tablet.clone());
        let survivor_watermark = self.plane.registry().applied_index(&group);

        // The range the tablet holds. Everything the lost voters committed past
        // the survivor's watermark is inside it and cannot be identified, which
        // is exactly why the whole range is marked rather than a part of it.
        let range = (0, at);
        let outcome = match unsafe_recover(
            &topology,
            &UnsafeRequest {
                tablet: request.tablet.clone(),
                confirm_tablet: request.confirm_tablet.clone(),
                survivor: request.survivor.clone(),
                reason: request.reason.clone(),
            },
            survivor_watermark,
            range,
            at,
        ) {
            Ok(outcome) => outcome,
            Err(e) => return crate::service::refusal(&e),
        };

        if self.plane.registry().holds(&group) {
            if let Err(e) = self.plane.registry().force_single_voter(&group) {
                return crate::service::refusal(&e);
            }
        }
        if let Err(e) = self.plane.apply_all(outcome.commands.clone()) {
            return crate::service::refusal(&e);
        }
        reply(
            "UnsafeRecoverResponse",
            encode_unsafe_recover_response(&UnsafeRecoverResponse {
                tablet: outcome.tablet,
                audit_id: outcome.audit_id,
                degraded_range_start: outcome.mark.range_start,
                degraded_range_end: outcome.mark.range_end,
                survivor_watermark,
                performed_at: at,
            }),
        )
    }

    fn clear_degraded(&self, payload: &[u8]) -> Outcome {
        let request = match decode_clear_degraded_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let topology = self.plane.topology();
        match clear_degraded(
            &topology,
            &request.tablet,
            &request.accepted_by,
            &request.reason,
            tallyowl_obs::time::now_ms(),
        ) {
            Err(e) => crate::service::refusal(&e),
            Ok(command) => self.run(command),
        }
    }

    /// Take a cluster snapshot.
    ///
    /// **The rows travel, not only the marks.** Until L090 this took the state
    /// machine's marks and called the result a snapshot, which meant the one
    /// thing a restore needed was the one thing it did not hold. The tablet's
    /// own store is copied into `<snapshot root>/<snapshot id>/<tablet>`, the
    /// group snapshots so that the marks are current, and the digest covers
    /// both.
    fn snapshot(&self, payload: &[u8]) -> Outcome {
        let request = match decode_cluster_snapshot_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let topology = self.plane.topology();
        let registry = self.plane.registry();
        let at = tallyowl_obs::time::now_ms();
        let snapshot_id = format!("snapshot-{at}");

        let Some(root) = self.snapshot_root.as_ref() else {
            return crate::service::refusal(&TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                "This node was started without a place to keep snapshots, so it cannot take one."
                    .to_string(),
            ));
        };

        let mut parts = Vec::new();
        for tablet in topology.tablets() {
            if request
                .tablet
                .as_deref()
                .is_some_and(|wanted| wanted != tablet.name)
            {
                continue;
            }
            // **What a node can back up is what it holds the data for.** The
            // consensus group is a separate question: a node may hold a
            // tablet's store and not be a voter in its group, and it is the
            // store that a restore needs.
            let Ok(segments) = self.segments_for(&tablet.name) else {
                continue;
            };
            let group = crate::groups::GroupKey::Tablet(tablet.name.clone());
            let mut state = Vec::new();
            if registry.holds(&group) {
                if let Err(e) = registry.snapshot_now(&group) {
                    return crate::service::refusal(&e);
                }
                state = registry
                    .machine(&group)
                    .map(|machine| machine.snapshot())
                    .unwrap_or_default();
            }

            // The rows. This is the part that makes it a backup.
            let into = root.join(&snapshot_id).join(&tablet.name);
            match segments.snapshot_into(&tablet.name, &into, at) {
                Err(e) => return crate::service::refusal(&e),
                Ok(taken) => {
                    // The watermark goes into the digest beside the marks, so a
                    // snapshot cannot be confused with one taken at another
                    // moment from the same state.
                    let mut described = state;
                    described.extend_from_slice(&taken.commit_watermark.to_le_bytes());
                    described.extend_from_slice(&(taken.segments as u64).to_le_bytes());
                    parts.push((tablet.name.clone(), described));
                }
            }
        }

        if parts.is_empty() {
            return crate::service::refusal(&TallyOwlError::new(
                ErrorCode::NotFound,
                "This node holds no tablet that matches, so there was nothing to snapshot."
                    .to_string(),
            ));
        }

        let snapshot = crate::recovery::take_snapshot(snapshot_id, at, parts);
        reply(
            "ClusterSnapshotResponse",
            encode_cluster_snapshot_response(
                &tallyowl_cluster_api::types::ClusterSnapshotResponse {
                    snapshot_id: snapshot.snapshot_id,
                    tablets: snapshot.tablets,
                    taken_at: snapshot.taken_at,
                    total_bytes: snapshot.total_bytes,
                    whole_digest: snapshot.whole_digest.to_vec(),
                },
            ),
        )
    }

    /// Restore from a snapshot. The default answer to a permanent quorum loss.
    ///
    /// `docs/FAILURE_MODES.md` section 6.2 makes restore the **default**, which
    /// makes a false acknowledgement here worse than anywhere else in the
    /// system. So this does the whole thing or none of it:
    ///
    /// 1. every segment in the snapshot is verified before anything is
    ///    published, and a missing or damaged one refuses with the file named;
    /// 2. a node that already holds data for the tablet refuses, and names the
    ///    offline procedure. Restoring on top of live data can lose an
    ///    acknowledged write and there is no way to take that back;
    /// 3. otherwise every segment is adopted through the same path a catching
    ///    up replica uses, and the answer says how much came back.
    fn restore(&self, payload: &[u8]) -> Outcome {
        let request = match decode_restore_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let Some(root) = self.snapshot_root.as_ref() else {
            return crate::service::refusal(&TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                "This node was started without a place to keep snapshots, so it cannot find one."
                    .to_string(),
            ));
        };
        let Some((held, segments)) = self.restoring.as_ref() else {
            return crate::service::refusal(&TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                "This node holds no tablet, so there is nothing to restore onto.".to_string(),
            ));
        };
        let tablet = request.onto_tablet.as_deref().unwrap_or(held.as_str());
        let from = root.join(&request.snapshot_id).join(tablet);
        if !from.is_dir() {
            return crate::service::refusal(&TallyOwlError::new(
                ErrorCode::NotFound,
                format!(
                    "There is no snapshot named `{}` for `{tablet}` on this node. `snapshot-cluster` names one when it takes it.",
                    request.snapshot_id
                ),
            ));
        }

        match segments.restore_from(tablet, &from) {
            Err(e) => crate::service::refusal(&e),
            Ok(report) => {
                let generation = self.plane.topology().generation;
                reply(
                    "ClusterAck",
                    encode_cluster_ack(&ClusterAck {
                        accepted: true,
                        generation,
                        message: Some(format!(
                            "Restored {} stored files and {} rows onto `{tablet}` from `{}`. Everything written after the snapshot is not here; replay the queued deliveries, which stable batch IDs make safe to repeat.",
                            report.segments_copied, report.rows, request.snapshot_id
                        )),
                    }),
                )
            }
        }
    }

    fn segments_for(
        &self,
        tablet: &str,
    ) -> Result<Arc<dyn crate::transfer::TabletSegments>, TallyOwlError> {
        match self.restoring.as_ref() {
            Some((held, segments)) if held == tablet => Ok(Arc::clone(segments)),
            _ => Err(TallyOwlError::new(
                ErrorCode::NotFound,
                format!("This node does not hold `{tablet}`'s stored data."),
            )),
        }
    }

    fn directory(&self, payload: &[u8]) -> Outcome {
        let request = match decode_directory_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let directory = self.plane.directory();
        let wanted = match request.project_id.as_ref().map(|id| sixteen(id)) {
            Some(Err(e)) => return crate::service::refusal(&e),
            Some(Ok(id)) => Some(id),
            None => None,
        };
        let placements = directory
            .placements()
            .filter(|p| wanted.is_none_or(|id| p.project_id == id))
            .map(|p| ProjectPlacement {
                project_id: p.project_id.to_vec(),
                cells: p.cells.clone(),
                moving_to: p.moving_to.clone(),
                policy_generation: p.policy_generation,
            })
            .collect();
        reply(
            "DirectoryResponse",
            encode_directory_response(&DirectoryResponse {
                placements,
                generation: directory.generation,
            }),
        )
    }

    fn assign_project(&self, payload: &[u8]) -> Outcome {
        let request = match decode_assign_project_request(payload) {
            Ok(request) => request,
            Err(e) => return malformed(e),
        };
        let project_id = match sixteen(&request.project_id) {
            Ok(id) => id,
            Err(e) => return crate::service::refusal(&e),
        };
        match self.plane.apply_directory(DirectoryCommand::AssignProject {
            project_id,
            cells: request.cells,
        }) {
            Ok(generation) => self.ack(generation),
            Err(e) => crate::service::refusal(&e),
        }
    }
}

fn sixteen(bytes: &[u8]) -> Result<[u8; 16], TallyOwlError> {
    bytes.try_into().map_err(|_| {
        TallyOwlError::invalid_argument(format!(
            "An ID is 16 bytes and this one is {}.",
            bytes.len()
        ))
    })
}

fn to_wire_member(member: &Member) -> GroupMember {
    GroupMember {
        node: member.node.clone(),
        role: match member.role {
            MemberRole::Voter => WireRole::Voter,
            MemberRole::Learner => WireRole::Learner,
        },
        address: member.address.clone(),
        region: Some(member.region.clone()),
        domain: Some(member.domain.clone()),
    }
}

fn from_wire_member(member: &GroupMember) -> Member {
    Member {
        node: member.node.clone(),
        role: from_wire_role(member.role.clone()),
        address: member.address.clone(),
        region: member.region.clone().unwrap_or_else(|| "home".into()),
        domain: member.domain.clone().unwrap_or_else(|| "default".into()),
    }
}

fn from_wire_role(role: WireRole) -> MemberRole {
    match role {
        WireRole::Voter => MemberRole::Voter,
        WireRole::Learner => MemberRole::Learner,
    }
}

fn to_wire_state(state: TabletState) -> WireTabletState {
    match state {
        TabletState::Active => WireTabletState::Active,
        TabletState::Splitting => WireTabletState::Splitting,
        TabletState::Merging => WireTabletState::Merging,
        TabletState::Moving => WireTabletState::Moving,
        TabletState::Draining => WireTabletState::Draining,
        TabletState::Retired => WireTabletState::Retired,
    }
}

fn to_wire_condition(node: &crate::topology::Node) -> WireCondition {
    WireCondition {
        node: node.name.clone(),
        state: match node.state {
            NodeState::Healthy => WireNodeState::Healthy,
            NodeState::Slow => WireNodeState::Slow,
            NodeState::Unreachable => WireNodeState::Unreachable,
            NodeState::ReadOnly => WireNodeState::ReadOnly,
            NodeState::Draining => WireNodeState::Draining,
        },
        cause: node.cause.as_deref().and_then(wire_cause),
        append_latency_us: 0,
        group_median_append_latency_us: 0,
        fsync_latency_us: 0,
        group_median_fsync_latency_us: 0,
        queue_depth: 0,
        accepted_bytes_each_second: 0,
        since: node.since,
        action_taken: None,
    }
}

fn wire_cause(cause: &str) -> Option<tallyowl_cluster_api::types::SlowCause> {
    use tallyowl_cluster_api::types::SlowCause as Wire;
    Some(match cause {
        "storage-errors" => Wire::StorageErrors,
        "storage-saturated" => Wire::StorageSaturated,
        "storage-slow" => Wire::StorageSlow,
        "write-volume" => Wire::WriteVolume,
        "compaction-pressure" => Wire::CompactionPressure,
        "memory-pressure" => Wire::MemoryPressure,
        "network-latency" => Wire::NetworkLatency,
        // A cause nobody could establish is reported as `unknown`, not dropped.
        // FAILURE_MODES.md section 6.1.
        _ => Wire::Unknown,
    })
}

/// A topology with nothing in it, for a process that has not enrolled yet.
pub fn empty_topology() -> Topology {
    Topology::new()
}
