//! Role tokens and enrolled nodes.
//!
//! `docs/NODE_IDENTITY.md` sections 2, 3, 4, 7, and 8. This is the third
//! credential type: a source key authorizes telemetry for a project, a role
//! token authorizes enrollment, and a node certificate identifies one enrolled
//! process. **One is never a replacement for another**, and nothing here can
//! produce a source key or resolve one.
//!
//! # What a role token is
//!
//! One line of text, `towr_<token-id>_<secret>`, with 32 random bytes of
//! secret. It takes the same shape as a source key on purpose, and it takes a
//! different prefix on purpose too: a token pasted where a key belongs is
//! refused with a message that says which one it is, rather than failing as an
//! unknown key.
//!
//! **TallyOwl stores a token ID and a keyed digest and never the value.**
//! Section 2 requires it. A lost token is reissued and never recovered.
//!
//! # What a token cannot do
//!
//! Section 3 states two limits, and both are enforced here rather than left to
//! a caller:
//!
//! - a token can enroll a storage process and cannot assign a tablet to it,
//!   because the cell controller owns placement;
//! - a token cannot create a controller voter, cannot create a global directory
//!   voter, and cannot change a tablet voter set.
//!
//! The second is enforced by [`NodeRole`] having no name for a voter. A policy
//! cannot ask for what the type cannot express, so no check can be forgotten.
//!
//! # Intersection, never widening
//!
//! Section 4: "The controller intersects the requested scope with the token
//! policy. It does not give a permission that is absent from the token." An
//! enrollment therefore returns an effective role and location that can be
//! narrower than the request and is never wider.
//!
//! # Overlapping active tokens
//!
//! An installation holds as many active tokens as an operator wants, for the
//! same reason a source holds many keys: a rotation issues the new token, both
//! work, the deployment moves, and the old one is revoked.

use crate::catalog::{Catalog, CatalogError};
use crate::cbor::{self, MapBuilder, Value};
use crate::control::{AuthFailure, IssuedRoleToken};
use crate::row::hex;

const ROLE_TOKEN_PREFIX: &str = "auth/role-token/";
const NODE_PREFIX: &str = "control/node/";

/// The text that starts every role token. It differs from the source-key prefix
/// so that a credential used in the wrong place is refused with a message that
/// says which kind it is.
pub const ROLE_TOKEN_PREFIX_TEXT: &str = "towr_";

/// The roles a token may permit. `docs/NODE_IDENTITY.md` section 3.
///
/// **There is deliberately no name for a controller voter, a global directory
/// voter, or a tablet voter.** Section 3 says a role token cannot create or
/// change any of them, and a type that cannot express the request is a stronger
/// guarantee than a check somebody has to remember to write. An administrator
/// starts a voter change and the applicable controller quorum commits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NodeRole {
    CollectorIntake,
    CollectorForwarder,
    CompatibilityReceiver,
    IngestGateway,
    QueryCoordinator,
    Projector,
    WorkflowWorker,
    ReadReplica,
    ExportReplica,
    StorageProcess,
}

impl NodeRole {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeRole::CollectorIntake => "collector-intake",
            NodeRole::CollectorForwarder => "collector-forwarder",
            NodeRole::CompatibilityReceiver => "compatibility-receiver",
            NodeRole::IngestGateway => "ingest-gateway",
            NodeRole::QueryCoordinator => "query-coordinator",
            NodeRole::Projector => "projector",
            NodeRole::WorkflowWorker => "workflow-worker",
            NodeRole::ReadReplica => "read-replica",
            NodeRole::ExportReplica => "export-replica",
            NodeRole::StorageProcess => "storage-process",
        }
    }

    pub fn parse(text: &str) -> Option<NodeRole> {
        Some(match text {
            "collector-intake" => NodeRole::CollectorIntake,
            "collector-forwarder" => NodeRole::CollectorForwarder,
            "compatibility-receiver" => NodeRole::CompatibilityReceiver,
            "ingest-gateway" => NodeRole::IngestGateway,
            "query-coordinator" => NodeRole::QueryCoordinator,
            "projector" => NodeRole::Projector,
            "workflow-worker" => NodeRole::WorkflowWorker,
            "read-replica" => NodeRole::ReadReplica,
            "export-replica" => NodeRole::ExportReplica,
            "storage-process" => NodeRole::StorageProcess,
            _ => return None,
        })
    }

    /// Every role, for a policy that permits an installation's whole set.
    pub const ALL: [NodeRole; 10] = [
        NodeRole::CollectorIntake,
        NodeRole::CollectorForwarder,
        NodeRole::CompatibilityReceiver,
        NodeRole::IngestGateway,
        NodeRole::QueryCoordinator,
        NodeRole::Projector,
        NodeRole::WorkflowWorker,
        NodeRole::ReadReplica,
        NodeRole::ExportReplica,
        NodeRole::StorageProcess,
    ];
}

/// What a role token permits. `docs/NODE_IDENTITY.md` section 2.
///
/// An empty `cells` or `regions` list means the policy does not restrict that
/// dimension. An absent limit means the installation's default rather than "no
/// limit", which is why each one is an `Option` rather than a sentinel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoleTokenPolicy {
    pub roles: Vec<NodeRole>,
    pub cells: Vec<String>,
    pub regions: Vec<String>,
    pub workspaces: Vec<[u8; 16]>,
    pub projects: Vec<[u8; 16]>,
    pub expires_at: Option<i64>,
    pub max_uses: Option<u64>,
    pub max_active_nodes: Option<u64>,
    /// How long a certificate this token issues may live. Section 6 sets the
    /// first lifetime at 24 hours.
    pub certificate_lifetime_ms: Option<i64>,
    pub enrollments_each_hour: Option<u64>,
    pub audit_labels: Vec<String>,
}

impl RoleTokenPolicy {
    /// A policy that permits one role and nothing else.
    pub fn for_role(role: NodeRole) -> RoleTokenPolicy {
        RoleTokenPolicy {
            roles: vec![role],
            ..RoleTokenPolicy::default()
        }
    }

    /// Whether this policy permits `role`.
    pub fn permits_role(&self, role: NodeRole) -> bool {
        self.roles.contains(&role)
    }

    /// Whether this policy permits `cell`. An empty list does not restrict.
    pub fn permits_cell(&self, cell: Option<&str>) -> bool {
        permits(&self.cells, cell)
    }

    /// Whether this policy permits `region`. An empty list does not restrict.
    pub fn permits_region(&self, region: Option<&str>) -> bool {
        permits(&self.regions, region)
    }
}

/// A location the request named against a list the policy holds.
///
/// An empty list does not restrict. A request that names nothing against a
/// restricting list is refused rather than defaulted: choosing a cell for a
/// caller that did not ask for one would place a node somewhere nobody decided.
fn permits(permitted: &[String], asked: Option<&str>) -> bool {
    if permitted.is_empty() {
        return true;
    }
    asked.is_some_and(|value| permitted.iter().any(|one| one == value))
}

/// One role-token record. The secret is not here and never was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleToken {
    pub token_id: String,
    pub digest: [u8; 32],
    pub label: String,
    pub policy: RoleTokenPolicy,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
    pub last_used_at: Option<i64>,
    /// How many nodes this token has enrolled. `max_uses` bounds it.
    pub uses: u64,
}

impl RoleToken {
    /// Whether this token can enroll anything at `now`.
    pub fn is_active(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.policy.expires_at.is_none_or(|at| now < at)
    }
}

/// One enrolled node. `docs/NODE_IDENTITY.md` sections 7 and 8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRecord {
    pub node_id: String,
    /// The token that enrolled it, so an audit can follow one token's nodes.
    pub token_id: String,
    pub role: NodeRole,
    pub cell: Option<String>,
    pub region: Option<String>,
    pub certificate_serial: String,
    pub enrolled_at: i64,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
    pub software_version: String,
}

impl NodeRecord {
    /// Whether this node's identity is still good at `now`.
    ///
    /// A short certificate life is what limits the effect of delayed revocation
    /// data, so an expired node is inactive without anybody revoking it.
    pub fn is_active(&self, now: i64) -> bool {
        self.revoked_at.is_none() && now < self.expires_at
    }
}

/// Why an enrollment was refused.
///
/// A caller sees one sentence whatever the reason, for the same reason a
/// credential refusal does: the difference between "that token does not exist"
/// and "that token cannot enroll that role" is a fact about the installation
/// that an unauthenticated caller has not earned. The reason reaches the audit
/// record and the metric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrollmentRefusal {
    /// The token did not resolve at all.
    Credential(AuthFailure),
    /// The token does not permit that role.
    RoleNotPermitted(NodeRole),
    /// The token does not permit that cell or region.
    LocationNotPermitted,
    /// The token has enrolled as many nodes as its policy permits.
    UsesExhausted,
    /// The token has as many live nodes as its policy permits. Section 7: this
    /// is what stops an incorrect autoscaler creating unlimited identities.
    ActiveNodeLimit,
    /// The certificate request was not one.
    MalformedRequest(String),
}

impl EnrollmentRefusal {
    /// The word an audit record and a metric label carry.
    pub fn as_str(&self) -> &'static str {
        match self {
            EnrollmentRefusal::Credential(failure) => failure.as_str(),
            EnrollmentRefusal::RoleNotPermitted(_) => "role-not-permitted",
            EnrollmentRefusal::LocationNotPermitted => "location-not-permitted",
            EnrollmentRefusal::UsesExhausted => "uses-exhausted",
            EnrollmentRefusal::ActiveNodeLimit => "active-node-limit",
            EnrollmentRefusal::MalformedRequest(_) => "malformed-request",
        }
    }
}

/// The one sentence a caller reads, whatever went wrong.
pub const ENROLLMENT_REFUSAL: &str =
    "This role token cannot enroll that node. Ask the person who runs TallyOwl for a token that permits it.";

/// What a token resolved to, and the scope an enrollment may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedToken {
    pub token_id: String,
    pub policy: RoleTokenPolicy,
}

impl Catalog {
    // -----------------------------------------------------------------------
    // Role tokens
    // -----------------------------------------------------------------------

    /// Issue a reusable role token. The returned text exists once.
    pub fn issue_role_token(
        &self,
        label: &str,
        policy: RoleTokenPolicy,
        now: i64,
    ) -> Result<IssuedRoleToken, CatalogError> {
        let secret = crate::control::new_secret();
        let token_id = hex(&crate::control::new_id_bytes());
        let digest = self.credential_digest(&secret)?;
        let token = RoleToken {
            token_id: token_id.clone(),
            digest,
            label: label.to_string(),
            policy,
            created_at: now,
            revoked_at: None,
            last_used_at: None,
            uses: 0,
        };
        self.put_role_token(&token)?;
        Ok(IssuedRoleToken {
            credential: format!(
                "{ROLE_TOKEN_PREFIX_TEXT}{token_id}_{}",
                crate::control::encode_secret(&secret)
            ),
            token,
        })
    }

    pub fn put_role_token(&self, token: &RoleToken) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!("{ROLE_TOKEN_PREFIX}{}", token.token_id),
            cbor::encode(&encode_token(token)),
        )])
    }

    pub fn role_token(&self, token_id: &str) -> Result<Option<RoleToken>, CatalogError> {
        let Some(bytes) = self.read(&format!("{ROLE_TOKEN_PREFIX}{token_id}"))? else {
            return Ok(None);
        };
        Ok(Some(decode_token(
            token_id,
            &crate::control::decode_value(&bytes)?,
        )))
    }

    pub fn role_tokens(&self) -> Result<Vec<RoleToken>, CatalogError> {
        let mut out = Vec::new();
        for (key, bytes) in self.scan(ROLE_TOKEN_PREFIX)? {
            out.push(decode_token(
                key.trim_start_matches(ROLE_TOKEN_PREFIX),
                &crate::control::decode_value(&bytes)?,
            ));
        }
        out.sort_by_key(|token| token.created_at);
        Ok(out)
    }

    /// Revoke a token. Section 2: this stops new enrollment and does not revoke
    /// certificates already issued unless the operator asks for cascade.
    ///
    /// Returns how many node identities the cascade revoked, which is zero
    /// without it.
    pub fn revoke_role_token(
        &self,
        token_id: &str,
        cascade: bool,
        now: i64,
    ) -> Result<usize, CatalogError> {
        let Some(mut token) = self.role_token(token_id)? else {
            return Ok(0);
        };
        if token.revoked_at.is_none() {
            token.revoked_at = Some(now);
            self.put_role_token(&token)?;
        }
        if !cascade {
            return Ok(0);
        }
        let mut revoked = 0;
        for mut node in self.nodes()? {
            if node.token_id == token_id && node.revoked_at.is_none() {
                node.revoked_at = Some(now);
                self.put_node(&node)?;
                revoked += 1;
            }
        }
        Ok(revoked)
    }

    /// Resolve one role token to its policy.
    ///
    /// The digest comparison runs in constant time, for the same reason the
    /// source-key one does.
    pub fn resolve_role_token(
        &self,
        credential: &str,
        now: i64,
    ) -> Result<ResolvedToken, AuthFailure> {
        let (token_id, secret) = split_role_token(credential).ok_or(AuthFailure::Malformed)?;
        let held = self
            .role_token(&token_id)
            .map_err(|_| AuthFailure::Unknown)?
            .ok_or(AuthFailure::Unknown)?;
        let offered = self
            .credential_digest(&secret)
            .map_err(|_| AuthFailure::Unknown)?;
        if !crate::control::digests_match(&offered, &held.digest) {
            return Err(AuthFailure::Unknown);
        }
        if held.revoked_at.is_some() {
            return Err(AuthFailure::Revoked);
        }
        if held.policy.expires_at.is_some_and(|at| now >= at) {
            return Err(AuthFailure::Expired);
        }
        Ok(ResolvedToken {
            token_id: held.token_id,
            policy: held.policy,
        })
    }

    /// Record that a token enrolled one more node.
    pub fn record_token_use(&self, token_id: &str, now: i64) -> Result<(), CatalogError> {
        let Some(mut token) = self.role_token(token_id)? else {
            return Ok(());
        };
        token.uses += 1;
        token.last_used_at = Some(now);
        self.put_role_token(&token)
    }

    // -----------------------------------------------------------------------
    // Enrolled nodes
    // -----------------------------------------------------------------------

    pub fn put_node(&self, node: &NodeRecord) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!("{NODE_PREFIX}{}", node.node_id),
            cbor::encode(&encode_node(node)),
        )])
    }

    pub fn node(&self, node_id: &str) -> Result<Option<NodeRecord>, CatalogError> {
        let Some(bytes) = self.read(&format!("{NODE_PREFIX}{node_id}"))? else {
            return Ok(None);
        };
        Ok(Some(decode_node(
            node_id,
            &crate::control::decode_value(&bytes)?,
        )))
    }

    pub fn nodes(&self) -> Result<Vec<NodeRecord>, CatalogError> {
        let mut out = Vec::new();
        for (key, bytes) in self.scan(NODE_PREFIX)? {
            out.push(decode_node(
                key.trim_start_matches(NODE_PREFIX),
                &crate::control::decode_value(&bytes)?,
            ));
        }
        out.sort_by_key(|node| node.enrolled_at);
        Ok(out)
    }

    /// How many of a token's nodes are still live at `now`.
    pub fn active_nodes_for(&self, token_id: &str, now: i64) -> Result<u64, CatalogError> {
        Ok(self
            .nodes()?
            .iter()
            .filter(|node| node.token_id == token_id && node.is_active(now))
            .count() as u64)
    }

    pub fn revoke_node(&self, node_id: &str, now: i64) -> Result<bool, CatalogError> {
        let Some(mut node) = self.node(node_id)? else {
            return Ok(false);
        };
        if node.revoked_at.is_none() {
            node.revoked_at = Some(now);
            self.put_node(&node)?;
        }
        Ok(true)
    }

    /// Remove nodes whose certificates expired long enough ago that nothing
    /// needs the record.
    ///
    /// Section 7: an expired pod identity needs no manual removal, and the
    /// controller removes it after its lease and certificate safety periods.
    pub fn expire_nodes(&self, now: i64, safety_ms: i64) -> Result<usize, CatalogError> {
        let stale: Vec<String> = self
            .nodes()?
            .into_iter()
            .filter(|node| now - node.expires_at > safety_ms)
            .map(|node| format!("{NODE_PREFIX}{}", node.node_id))
            .collect();
        if stale.is_empty() {
            return Ok(0);
        }
        let count = stale.len();
        self.remove_records(&stale)?;
        Ok(count)
    }
}

/// Split `towr_<token-id>_<secret>` into its two halves.
fn split_role_token(credential: &str) -> Option<(String, Vec<u8>)> {
    let rest = credential.trim().strip_prefix(ROLE_TOKEN_PREFIX_TEXT)?;
    let (token_id, secret) = rest.split_once('_')?;
    if token_id.is_empty() || !token_id.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((token_id.to_string(), crate::control::decode_secret(secret)?))
}

/// Whether this text looks like a role token rather than a source key.
pub fn is_role_token(credential: &str) -> bool {
    credential.trim().starts_with(ROLE_TOKEN_PREFIX_TEXT)
}

fn encode_token(token: &RoleToken) -> Value {
    let policy = &token.policy;
    MapBuilder::new()
        .put("digest", Value::Bytes(token.digest.to_vec()))
        .put("label", Value::text(&token.label))
        .put("created", Value::integer(token.created_at))
        .put("uses", Value::Unsigned(token.uses))
        .put_some("revoked", token.revoked_at.map(Value::integer))
        .put_some("used", token.last_used_at.map(Value::integer))
        .put(
            "roles",
            Value::Array(
                policy
                    .roles
                    .iter()
                    .map(|role| Value::text(role.as_str()))
                    .collect(),
            ),
        )
        .put("cells", text_list(&policy.cells))
        .put("regions", text_list(&policy.regions))
        .put("workspaces", id_list(&policy.workspaces))
        .put("projects", id_list(&policy.projects))
        .put("labels", text_list(&policy.audit_labels))
        .put_some("expires", policy.expires_at.map(Value::integer))
        .put_some("maxUses", policy.max_uses.map(Value::Unsigned))
        .put_some("maxNodes", policy.max_active_nodes.map(Value::Unsigned))
        .put_some(
            "certLife",
            policy.certificate_lifetime_ms.map(Value::integer),
        )
        .put_some("rate", policy.enrollments_each_hour.map(Value::Unsigned))
        .build()
}

fn decode_token(token_id: &str, value: &Value) -> RoleToken {
    RoleToken {
        token_id: token_id.to_string(),
        digest: value
            .field("digest")
            .and_then(Value::as_bytes)
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .unwrap_or([0; 32]),
        label: crate::control::text_field(value, "label"),
        created_at: integer_field(value, "created"),
        revoked_at: value.field("revoked").and_then(Value::as_integer),
        last_used_at: value.field("used").and_then(Value::as_integer),
        uses: integer_field(value, "uses").max(0) as u64,
        policy: RoleTokenPolicy {
            roles: read_text_list(value, "roles")
                .iter()
                .filter_map(|text| NodeRole::parse(text))
                .collect(),
            cells: read_text_list(value, "cells"),
            regions: read_text_list(value, "regions"),
            workspaces: read_id_list(value, "workspaces"),
            projects: read_id_list(value, "projects"),
            audit_labels: read_text_list(value, "labels"),
            expires_at: value.field("expires").and_then(Value::as_integer),
            max_uses: value
                .field("maxUses")
                .and_then(Value::as_integer)
                .map(|v| v as u64),
            max_active_nodes: value
                .field("maxNodes")
                .and_then(Value::as_integer)
                .map(|v| v as u64),
            certificate_lifetime_ms: value.field("certLife").and_then(Value::as_integer),
            enrollments_each_hour: value
                .field("rate")
                .and_then(Value::as_integer)
                .map(|v| v as u64),
        },
    }
}

fn encode_node(node: &NodeRecord) -> Value {
    MapBuilder::new()
        .put("token", Value::text(&node.token_id))
        .put("role", Value::text(node.role.as_str()))
        .put("serial", Value::text(&node.certificate_serial))
        .put("enrolled", Value::integer(node.enrolled_at))
        .put("expires", Value::integer(node.expires_at))
        .put("version", Value::text(&node.software_version))
        .put_some("cell", node.cell.as_deref().map(Value::text))
        .put_some("region", node.region.as_deref().map(Value::text))
        .put_some("revoked", node.revoked_at.map(Value::integer))
        .build()
}

fn decode_node(node_id: &str, value: &Value) -> NodeRecord {
    NodeRecord {
        node_id: node_id.to_string(),
        token_id: crate::control::text_field(value, "token"),
        role: NodeRole::parse(&crate::control::text_field(value, "role"))
            .unwrap_or(NodeRole::CollectorIntake),
        cell: optional_text(value, "cell"),
        region: optional_text(value, "region"),
        certificate_serial: crate::control::text_field(value, "serial"),
        enrolled_at: integer_field(value, "enrolled"),
        expires_at: integer_field(value, "expires"),
        revoked_at: value.field("revoked").and_then(Value::as_integer),
        software_version: crate::control::text_field(value, "version"),
    }
}

fn integer_field(value: &Value, name: &str) -> i64 {
    value.field(name).and_then(Value::as_integer).unwrap_or(0)
}

fn optional_text(value: &Value, name: &str) -> Option<String> {
    value
        .field(name)
        .and_then(Value::as_text)
        .map(str::to_string)
}

fn text_list(values: &[String]) -> Value {
    Value::Array(values.iter().map(Value::text).collect())
}

fn id_list(values: &[[u8; 16]]) -> Value {
    Value::Array(values.iter().map(|id| Value::Bytes(id.to_vec())).collect())
}

fn read_text_list(value: &Value, name: &str) -> Vec<String> {
    value
        .field(name)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_text)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn read_id_list(value: &Value, name: &str) -> Vec<[u8; 16]> {
    value
        .field(name)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_bytes)
                .filter_map(|b| <[u8; 16]>::try_from(b).ok())
                .collect()
        })
        .unwrap_or_default()
}
