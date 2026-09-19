//! Node enrollment: the head's half of `docs/NODE_IDENTITY.md` section 4.
//!
//! # What this decides, and what it refuses to decide
//!
//! Section 4: "The controller intersects the requested scope with the token
//! policy. It does not give a permission that is absent from the token." Every
//! refusal here is one of those intersections failing.
//!
//! Two things this deliberately does not do:
//!
//! - **it does not place a tablet.** A role token can enroll a storage process
//!   and the cell controller owns placement, so an enrollment returns an
//!   identity and never an assignment;
//! - **it does not touch a voter set.** [`NodeRole`] has no name for a voter, so
//!   there is nothing here to guard.
//!
//! # One refusal, whatever the reason
//!
//! An enrollment that fails says one sentence. The difference between "that
//! token does not exist" and "that token cannot enroll that role" is a fact
//! about the installation, and an unauthenticated caller has not earned it. The
//! reason reaches the log and the metric, where an operator needs it.

use std::sync::Arc;

use tallyowl_control_api::types::NodeRole as WireRole;
use tallyowl_control_api::types::{
    CreateRoleTokenRequest, CreateRoleTokenResponse, EnrollNodeRequest, EnrollNodeResponse,
    ListRequest, NodeCapabilities, NodeList, NodeSummary, RenewNodeCertificateRequest,
    RevokeRoleTokenRequest, RoleTokenList, RoleTokenPolicy as WirePolicy, RoleTokenSummary,
};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_obs::time::now_ms;
use tallyowl_store::certificates::{
    sign_request, IssuedCertificate, Subject, DEFAULT_CERTIFICATE_LIFETIME_MS,
};
use tallyowl_store::control::{Role, SignedIn, OPERATOR_ISSUER};
use tallyowl_store::identity::{
    EnrollmentRefusal, NodeRecord, NodeRole, ResolvedToken, RoleTokenPolicy, ENROLLMENT_REFUSAL,
};
use tallyowl_store::SegmentedStore;

/// How long an expired node record stays before it is removed.
///
/// Section 7: the controller removes an expired pod identity after its lease and
/// certificate safety periods. An hour past expiry is long enough that an
/// operator can still see what a crashed pod was, and short enough that a
/// churning deployment does not accumulate records for ever.
pub const NODE_RECORD_SAFETY_MS: i64 = 60 * 60_000;

pub struct EnrollmentService {
    pub store: Arc<SegmentedStore>,
    pub metrics: Arc<Registry>,
}

impl EnrollmentService {
    pub fn declare_metrics(metrics: &Registry) {
        metrics.declare(
            "tallyowl_enrollments_total",
            tallyowl_obs::MetricKind::Counter,
            "Node enrollment attempts, by outcome.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_enrollments_total` is not a name the registry accepts: {}", e.0));
    }

    fn count(&self, outcome: &str) {
        self.metrics.increment(
            "tallyowl_enrollments_total",
            &labels(&[("outcome", outcome)]),
        );
    }

    /// The one sentence a caller reads, whatever went wrong.
    fn refuse(&self, refusal: &EnrollmentRefusal) -> TallyOwlError {
        self.count(refusal.as_str());
        let retryable = matches!(
            refusal,
            EnrollmentRefusal::UsesExhausted | EnrollmentRefusal::ActiveNodeLimit
        );
        // A malformed request is the one case where the caller can act on the
        // detail, and it says nothing about the installation.
        if let EnrollmentRefusal::MalformedRequest(detail) = refusal {
            return TallyOwlError::new(ErrorCode::InvalidArgument, detail.clone());
        }
        TallyOwlError::new(ErrorCode::PermissionDenied, ENROLLMENT_REFUSAL).retryable(retryable)
    }

    // -----------------------------------------------------------------------
    // Role tokens
    // -----------------------------------------------------------------------

    /// Create a reusable role token. Only an owner may.
    ///
    /// A token that can enroll an ingest gateway is close to a key to the
    /// installation, so this is the narrowest role that exists rather than the
    /// one that merely administers a workspace.
    pub fn create_role_token(
        &self,
        signed_in: &SignedIn,
        request: CreateRoleTokenRequest,
    ) -> Result<CreateRoleTokenResponse, TallyOwlError> {
        self.require_installation_authority(signed_in)?;
        let policy = read_policy(&request.policy)?;
        if policy.roles.is_empty() {
            return Err(TallyOwlError::new(
                ErrorCode::InvalidArgument,
                "A role token that permits no role can enroll nothing. Name at least one role.",
            ));
        }

        let now = now_ms();
        self.store
            .guard_control_write()
            .map_err(crate::ingest::to_service_error)?;
        let issued = self
            .store
            .catalog()
            .issue_role_token(&request.label, policy.clone(), now)
            .map_err(control_failure)?;

        self.count("created");
        Ok(CreateRoleTokenResponse {
            token_id: issued.token.token_id,
            token: issued.credential,
            policy: write_policy(&policy),
            created_at: now,
        })
    }

    pub fn list_role_tokens(
        &self,
        signed_in: &SignedIn,
        _request: ListRequest,
    ) -> Result<RoleTokenList, TallyOwlError> {
        self.require_installation_authority(signed_in)?;
        let now = now_ms();
        let mut tokens = Vec::new();
        for token in self
            .store
            .catalog()
            .role_tokens()
            .map_err(control_failure)?
        {
            let active = self
                .store
                .catalog()
                .active_nodes_for(&token.token_id, now)
                .unwrap_or(0);
            tokens.push(RoleTokenSummary {
                token_id: token.token_id,
                label: token.label,
                policy: write_policy(&token.policy),
                created_at: token.created_at,
                uses: token.uses,
                active_nodes: active,
                last_used_at: token.last_used_at,
                revoked: token.revoked_at.is_some(),
            });
        }
        Ok(RoleTokenList {
            tokens,
            next_cursor: None,
        })
    }

    pub fn revoke_role_token(
        &self,
        signed_in: &SignedIn,
        request: RevokeRoleTokenRequest,
    ) -> Result<(), TallyOwlError> {
        self.require_installation_authority(signed_in)?;
        self.store
            .guard_control_write()
            .map_err(crate::ingest::to_service_error)?;
        self.store
            .catalog()
            .revoke_role_token(
                &request.token_id,
                request.cascade.unwrap_or(false),
                now_ms(),
            )
            .map_err(control_failure)?;
        self.count("revoked");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Enrollment
    // -----------------------------------------------------------------------

    /// Enroll a node, and give it an identity the control plane chose.
    ///
    /// **The token authenticates this call.** A node has no session and no
    /// certificate yet, so it presents the token and nothing else. Everything
    /// the node asked for is then intersected with what the token permits.
    pub fn enroll(&self, request: EnrollNodeRequest) -> Result<EnrollNodeResponse, TallyOwlError> {
        let now = now_ms();
        let resolved = self
            .store
            .catalog()
            .resolve_role_token(&request.token, now)
            .map_err(|failure| self.refuse(&EnrollmentRefusal::Credential(failure)))?;

        let requested_role = from_wire_role(&request.requested_role);

        let cell = request.cell.as_deref();
        let region = request.region.as_deref();
        self.check_scope(&resolved, requested_role, cell, region, now)?;

        self.store
            .guard_control_write()
            .map_err(crate::ingest::to_service_error)?;
        let authority = self
            .store
            .catalog()
            .certificate_authority()
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;

        // A stateful node asks for its own identity again; a stateless one gets
        // a fresh identity every time. Section 8 against section 7.
        let node_id = match request.node_id.as_deref() {
            Some(existing) => self.reclaim_identity(existing, &resolved, requested_role, now)?,
            None => new_node_id(),
        };

        let lifetime = resolved
            .policy
            .certificate_lifetime_ms
            .unwrap_or(DEFAULT_CERTIFICATE_LIFETIME_MS)
            .clamp(60_000, DEFAULT_CERTIFICATE_LIFETIME_MS);

        let issued = sign_request(
            &authority,
            &request.certificate_request,
            &Subject {
                node_id: node_id.clone(),
                role: requested_role.as_str().to_string(),
                cell: cell.map(str::to_string),
                region: region.map(str::to_string),
            },
            now,
            lifetime,
        )
        .map_err(|e| self.refuse(&EnrollmentRefusal::MalformedRequest(e.to_string())))?;

        self.record(
            &node_id,
            &resolved,
            requested_role,
            cell,
            region,
            &issued,
            request.capabilities.as_ref(),
            now,
        )?;
        self.count("enrolled");

        Ok(response_for(
            node_id,
            requested_role,
            cell,
            region,
            &issued,
            request.capabilities,
        ))
    }

    /// Renew against the node's current identity rather than the role token.
    ///
    /// Section 6: a deployment can drop the token after enrollment. This is what
    /// makes that true.
    pub fn renew(
        &self,
        request: RenewNodeCertificateRequest,
    ) -> Result<EnrollNodeResponse, TallyOwlError> {
        let now = now_ms();
        let held = self
            .store
            .catalog()
            .node(&request.node_id)
            .map_err(control_failure)?
            .ok_or_else(|| {
                self.refuse(&EnrollmentRefusal::Credential(
                    tallyowl_store::control::AuthFailure::Unknown,
                ))
            })?;

        // A revoked or long-expired node re-enrolls with a token. Renewal is for
        // an identity that is still its own.
        if !held.is_active(now) {
            return Err(self.refuse(&EnrollmentRefusal::Credential(
                tallyowl_store::control::AuthFailure::Revoked,
            )));
        }
        // The token that enrolled it must still be good. Section 2: a revoked
        // token prevents new enrollment, and a renewal is new certificate
        // material against a token an operator has withdrawn.
        let token = self
            .store
            .catalog()
            .role_token(&held.token_id)
            .map_err(control_failure)?;
        if !token.is_some_and(|token| token.is_active(now)) {
            return Err(self.refuse(&EnrollmentRefusal::Credential(
                tallyowl_store::control::AuthFailure::Revoked,
            )));
        }

        self.store
            .guard_control_write()
            .map_err(crate::ingest::to_service_error)?;
        let authority = self
            .store
            .catalog()
            .certificate_authority()
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        let issued = sign_request(
            &authority,
            &request.certificate_request,
            &Subject {
                node_id: held.node_id.clone(),
                role: held.role.as_str().to_string(),
                cell: held.cell.clone(),
                region: held.region.clone(),
            },
            now,
            DEFAULT_CERTIFICATE_LIFETIME_MS,
        )
        .map_err(|e| self.refuse(&EnrollmentRefusal::MalformedRequest(e.to_string())))?;

        let mut renewed = held.clone();
        renewed.certificate_serial = issued.serial.clone();
        renewed.expires_at = issued.expires_at;
        self.store
            .catalog()
            .put_node(&renewed)
            .map_err(control_failure)?;
        self.count("renewed");

        Ok(response_for(
            held.node_id,
            held.role,
            held.cell.as_deref(),
            held.region.as_deref(),
            &issued,
            None,
        ))
    }

    pub fn list_nodes(
        &self,
        signed_in: &SignedIn,
        _request: ListRequest,
    ) -> Result<NodeList, TallyOwlError> {
        self.require_installation_authority(signed_in)?;
        let nodes = self
            .store
            .catalog()
            .nodes()
            .map_err(control_failure)?
            .into_iter()
            .map(|node| NodeSummary {
                node_id: node.node_id,
                token_id: node.token_id,
                role: to_wire_role(node.role),
                cell: node.cell,
                region: node.region,
                certificate_serial: node.certificate_serial,
                enrolled_at: node.enrolled_at,
                expires_at: node.expires_at,
                revoked: node.revoked_at.is_some(),
            })
            .collect();
        Ok(NodeList {
            nodes,
            next_cursor: None,
        })
    }

    /// Remove node records whose certificates expired long enough ago.
    pub fn expire_nodes(&self) -> Result<usize, TallyOwlError> {
        self.store
            .catalog()
            .expire_nodes(now_ms(), NODE_RECORD_SAFETY_MS)
            .map_err(control_failure)
    }

    // -----------------------------------------------------------------------

    /// The intersection. Section 4, and every arm of it refuses rather than
    /// narrows, because narrowing silently would give a node a role it did not
    /// ask for and cannot use.
    fn check_scope(
        &self,
        resolved: &ResolvedToken,
        role: NodeRole,
        cell: Option<&str>,
        region: Option<&str>,
        now: i64,
    ) -> Result<(), TallyOwlError> {
        if !resolved.policy.permits_role(role) {
            return Err(self.refuse(&EnrollmentRefusal::RoleNotPermitted(role)));
        }
        if !resolved.policy.permits_cell(cell) || !resolved.policy.permits_region(region) {
            return Err(self.refuse(&EnrollmentRefusal::LocationNotPermitted));
        }

        let token = self
            .store
            .catalog()
            .role_token(&resolved.token_id)
            .map_err(control_failure)?;
        if let Some(token) = token {
            if resolved
                .policy
                .max_uses
                .is_some_and(|max| token.uses >= max)
            {
                return Err(self.refuse(&EnrollmentRefusal::UsesExhausted));
            }
        }

        // Section 7: this is what stops an incorrect autoscaler creating
        // unlimited identities.
        if let Some(max) = resolved.policy.max_active_nodes {
            let active = self
                .store
                .catalog()
                .active_nodes_for(&resolved.token_id, now)
                .map_err(control_failure)?;
            if active >= max {
                return Err(self.refuse(&EnrollmentRefusal::ActiveNodeLimit));
            }
        }
        Ok(())
    }

    /// A stateful node asking for its own node ID again. Section 8.
    ///
    /// The record has to exist, has to belong to this token, and has to be for
    /// the same role. Without those three, a node ID would be a name anybody
    /// holding any token could claim.
    fn reclaim_identity(
        &self,
        node_id: &str,
        resolved: &ResolvedToken,
        role: NodeRole,
        _now: i64,
    ) -> Result<String, TallyOwlError> {
        let Some(held) = self
            .store
            .catalog()
            .node(node_id)
            .map_err(control_failure)?
        else {
            // Nothing holds that name yet, so taking it is not taking anything.
            return Ok(node_id.to_string());
        };
        if held.token_id != resolved.token_id || held.role != role {
            return Err(self.refuse(&EnrollmentRefusal::RoleNotPermitted(role)));
        }
        if held.revoked_at.is_some() {
            return Err(self.refuse(&EnrollmentRefusal::Credential(
                tallyowl_store::control::AuthFailure::Revoked,
            )));
        }
        Ok(node_id.to_string())
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        node_id: &str,
        resolved: &ResolvedToken,
        role: NodeRole,
        cell: Option<&str>,
        region: Option<&str>,
        issued: &IssuedCertificate,
        capabilities: Option<&NodeCapabilities>,
        now: i64,
    ) -> Result<(), TallyOwlError> {
        self.store
            .catalog()
            .put_node(&NodeRecord {
                node_id: node_id.to_string(),
                token_id: resolved.token_id.clone(),
                role,
                cell: cell.map(str::to_string),
                region: region.map(str::to_string),
                certificate_serial: issued.serial.clone(),
                enrolled_at: now,
                expires_at: issued.expires_at,
                revoked_at: None,
                software_version: capabilities
                    .map(|c| c.software_version.clone())
                    .unwrap_or_default(),
            })
            .map_err(control_failure)?;
        self.store
            .catalog()
            .record_token_use(&resolved.token_id, now)
            .map_err(control_failure)
    }

    /// Who may manage role tokens and read the enrolled nodes.
    ///
    /// **A role token belongs to the installation, not to a workspace.** It can
    /// enroll an ingest gateway, which is close to a key to the whole thing, so
    /// the check is the installation's own authority rather than a workspace
    /// role.
    ///
    /// Two identities pass. An operator session, because an installation
    /// enrolls its collectors before it has a workspace for anybody to own, and
    /// requiring a workspace first would make the bootstrap order impossible.
    /// And an owner of any workspace, because that is the highest role a signed
    /// -in person can hold and there is nothing above it yet. L050 records that
    /// the role model is workspace-scoped; an installation-scoped role is the
    /// wider change it names.
    fn require_installation_authority(&self, signed_in: &SignedIn) -> Result<(), TallyOwlError> {
        let allowed = signed_in.issuer == OPERATOR_ISSUER
            || signed_in
                .memberships
                .iter()
                .any(|(_, role)| role.allows(Role::Owner));
        if allowed {
            return Ok(());
        }
        Err(TallyOwlError::new(
            ErrorCode::PermissionDenied,
            "Only somebody who administers this installation can manage role tokens and enrolled nodes.",
        ))
    }
}

fn response_for(
    node_id: String,
    role: NodeRole,
    cell: Option<&str>,
    region: Option<&str>,
    issued: &IssuedCertificate,
    capabilities: Option<NodeCapabilities>,
) -> EnrollNodeResponse {
    EnrollNodeResponse {
        node_id,
        certificate_chain: issued.chain.clone(),
        certificate_serial: issued.serial.clone(),
        effective_role: to_wire_role(role),
        cell: cell.map(str::to_string),
        region: region.map(str::to_string),
        issued_at: issued.issued_at,
        expires_at: issued.expires_at,
        renew_after: issued.renew_after,
        permitted_capabilities: capabilities,
    }
}

/// A node ID the control plane chose. It carries no meaning, per D9.
fn new_node_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the system random source");
    format!("node-{}", tallyowl_store::row::hex(&bytes))
}

/// The wire role and the stored role are the same ten names. The contract owns
/// the set, so this is a total mapping in both directions and neither side can
/// carry a role the other does not know.
fn from_wire_role(role: &WireRole) -> NodeRole {
    match role {
        WireRole::CollectorIntake => NodeRole::CollectorIntake,
        WireRole::CollectorForwarder => NodeRole::CollectorForwarder,
        WireRole::CompatibilityReceiver => NodeRole::CompatibilityReceiver,
        WireRole::IngestGateway => NodeRole::IngestGateway,
        WireRole::QueryCoordinator => NodeRole::QueryCoordinator,
        WireRole::Projector => NodeRole::Projector,
        WireRole::WorkflowWorker => NodeRole::WorkflowWorker,
        WireRole::ReadReplica => NodeRole::ReadReplica,
        WireRole::ExportReplica => NodeRole::ExportReplica,
        WireRole::StorageProcess => NodeRole::StorageProcess,
    }
}

fn to_wire_role(role: NodeRole) -> WireRole {
    match role {
        NodeRole::CollectorIntake => WireRole::CollectorIntake,
        NodeRole::CollectorForwarder => WireRole::CollectorForwarder,
        NodeRole::CompatibilityReceiver => WireRole::CompatibilityReceiver,
        NodeRole::IngestGateway => WireRole::IngestGateway,
        NodeRole::QueryCoordinator => WireRole::QueryCoordinator,
        NodeRole::Projector => WireRole::Projector,
        NodeRole::WorkflowWorker => WireRole::WorkflowWorker,
        NodeRole::ReadReplica => WireRole::ReadReplica,
        NodeRole::ExportReplica => WireRole::ExportReplica,
        NodeRole::StorageProcess => WireRole::StorageProcess,
    }
}

fn read_policy(wire: &WirePolicy) -> Result<RoleTokenPolicy, TallyOwlError> {
    Ok(RoleTokenPolicy {
        roles: wire.roles.iter().map(from_wire_role).collect(),
        cells: wire.cells.clone().unwrap_or_default(),
        regions: wire.regions.clone().unwrap_or_default(),
        workspaces: wire
            .workspaces
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|id| <[u8; 16]>::try_from(id.as_slice()).ok())
            .collect(),
        projects: wire
            .projects
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|id| <[u8; 16]>::try_from(id.as_slice()).ok())
            .collect(),
        expires_at: wire.expires_at,
        max_uses: wire.max_uses,
        max_active_nodes: wire.max_active_nodes,
        certificate_lifetime_ms: wire.certificate_lifetime_ms.map(|ms| ms as i64),
        enrollments_each_hour: wire.enrollments_each_hour,
        audit_labels: wire.audit_labels.clone().unwrap_or_default(),
    })
}

fn write_policy(policy: &RoleTokenPolicy) -> WirePolicy {
    WirePolicy {
        roles: policy.roles.iter().copied().map(to_wire_role).collect(),
        cells: some_unless_empty(policy.cells.clone()),
        regions: some_unless_empty(policy.regions.clone()),
        workspaces: some_unless_empty(policy.workspaces.iter().map(|id| id.to_vec()).collect()),
        projects: some_unless_empty(policy.projects.iter().map(|id| id.to_vec()).collect()),
        expires_at: policy.expires_at,
        max_uses: policy.max_uses,
        max_active_nodes: policy.max_active_nodes,
        certificate_lifetime_ms: policy.certificate_lifetime_ms.map(|ms| ms as u64),
        enrollments_each_hour: policy.enrollments_each_hour,
        audit_labels: some_unless_empty(policy.audit_labels.clone()),
    }
}

fn some_unless_empty<T>(values: Vec<T>) -> Option<Vec<T>> {
    (!values.is_empty()).then_some(values)
}

fn control_failure(error: tallyowl_store::catalog::CatalogError) -> TallyOwlError {
    TallyOwlError::internal(error.to_string())
}
