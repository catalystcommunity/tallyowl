//! The control plane the head serves.
//!
//! The head owns the control catalog, so the head is the only process that can
//! answer "whose key is this?". A collector asks and holds the answer for a
//! short time; it never stores a key and never becomes the authority for one.
//! See `docs/DELIVERY.md` section 9 and D32.
//!
//! # Every refusal reads the same
//!
//! A credential that does not resolve produces one sentence, whether the key
//! never existed, was revoked, or expired. The reason reaches the metric,
//! because an operator has to tell "somebody is guessing" from "the rotation
//! missed an application", and a caller does not get to learn which keys exist.

use std::sync::Arc;

use tallyowl_collector_api::types::{ResolveKeyRequest, ResolveKeyResponse};
use tallyowl_control_api::types::{
    ApiKeyList, ApiKeySummary, ListRequest, Project as WireProject, ProjectList,
    Workspace as WireWorkspace, WorkspaceList,
};
use tallyowl_obs::error::TallyOwlError;
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_obs::time::now_ms;
use tallyowl_store::control::{Role, SignedIn, REFUSAL};
use tallyowl_store::SegmentedStore;

/// How long a collector may hold a resolution before it asks again.
///
/// This is the whole of revocation latency, so it is short. It is not zero,
/// because a resolution for every batch would put a round trip on the ingest
/// path and make the head's availability the collector's availability.
pub const DEFAULT_KEY_CACHE_TTL_MS: i64 = 30_000;

pub struct ControlService {
    pub store: Arc<SegmentedStore>,
    pub metrics: Arc<Registry>,
    pub key_cache_ttl_ms: i64,
}

impl ControlService {
    pub fn declare_metrics(metrics: &Registry) {
        metrics.declare(
            "tallyowl_control_requests_total",
            tallyowl_obs::MetricKind::Counter,
            "Control requests, by outcome.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_control_requests_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_key_resolutions_total",
            tallyowl_obs::MetricKind::Counter,
            "Source credential resolutions, by outcome.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_key_resolutions_total` is not a name the registry accepts: {}", e.0));
    }

    pub fn resolve_key(
        &self,
        request: ResolveKeyRequest,
    ) -> Result<ResolveKeyResponse, TallyOwlError> {
        let now = now_ms();
        match self
            .store
            .catalog()
            .resolve_credential(&request.credential, now)
        {
            Ok(resolved) => {
                self.metrics.increment(
                    "tallyowl_key_resolutions_total",
                    &labels(&[("outcome", "resolved")]),
                );
                Ok(ResolveKeyResponse {
                    key_id: resolved.key_id,
                    workspace_id: resolved.workspace_id.to_vec(),
                    project_id: resolved.project_id.to_vec(),
                    source_id: resolved.source_id.to_vec(),
                    cache_ttl_ms: self.key_cache_ttl_ms,
                    expires_at: resolved.expires_at,
                })
            }
            Err(reason) => {
                self.metrics.increment(
                    "tallyowl_key_resolutions_total",
                    &labels(&[("outcome", reason.as_str())]),
                );
                Err(
                    TallyOwlError::new(tallyowl_obs::ErrorCode::Unauthenticated, REFUSAL)
                        .retryable(false),
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Authorization
//
// D7: LinkKeys owns human authentication, and TallyOwl owns the resulting
// application session and authorization. This is the second half.
//
// **Every control operation checks.** A read that skipped the check because it
// "only lists names" is how one tenant learns another tenant exists, and D8
// makes the workspace the isolation boundary.
// ---------------------------------------------------------------------------

/// The message a caller reads when they are signed in and may not do this.
///
/// It says what is missing rather than what exists. "You need the admin role in
/// this workspace" is actionable; "this workspace has no such project" would
/// tell somebody whether a project they cannot see is there.
fn denied(needed: Role) -> TallyOwlError {
    TallyOwlError::new(
        tallyowl_obs::ErrorCode::PermissionDenied,
        format!(
            "You need the `{}` role in this workspace to do that. Ask somebody who has it.",
            needed.as_str()
        ),
    )
    .retryable(false)
}

fn not_signed_in() -> TallyOwlError {
    TallyOwlError::new(
        tallyowl_obs::ErrorCode::Unauthenticated,
        "You are not signed in. Sign in and try again.",
    )
    .retryable(false)
}

impl ControlService {
    /// Who is making this request.
    ///
    /// The credential travels on the connection, so this is where a person and
    /// an application are told apart. A source key never reaches a control
    /// operation and a session token never reaches ingest: one authenticates a
    /// person and the other authenticates an application, and neither is a
    /// substitute for the other. See NODE_IDENTITY.md section 1.
    pub fn signed_in(&self, credential: Option<&str>) -> Result<SignedIn, TallyOwlError> {
        let credential = credential.map(str::trim).filter(|c| !c.is_empty());
        let Some(credential) = credential else {
            return Err(not_signed_in());
        };
        if !tallyowl_store::control::is_session_token(credential) {
            // A source key presented to a control operation is refused as an
            // authentication failure rather than a permission one. It is not
            // that this key lacks a role; it is that a key is not a person.
            self.metrics.increment(
                "tallyowl_control_requests_total",
                &labels(&[("outcome", "not-a-session")]),
            );
            return Err(not_signed_in());
        }
        self.store
            .catalog()
            .resolve_session(credential, now_ms())
            .map_err(|reason| {
                self.metrics.increment(
                    "tallyowl_control_requests_total",
                    &labels(&[("outcome", reason.as_str())]),
                );
                TallyOwlError::new(tallyowl_obs::ErrorCode::Unauthenticated, REFUSAL)
                    .retryable(false)
            })
    }

    /// Check that this person holds at least `needed` in this workspace.
    pub fn allow(
        &self,
        who: &SignedIn,
        workspace_id: [u8; 16],
        needed: Role,
    ) -> Result<(), TallyOwlError> {
        match who.role_in(workspace_id) {
            Some(held) if held.allows(needed) => Ok(()),
            // A person who is not a member and a person whose role is too low
            // read the same. Telling them apart would say whether a workspace
            // exists, and existence is a fact they have not authenticated for.
            _ => {
                self.metrics.increment(
                    "tallyowl_control_requests_total",
                    &labels(&[("outcome", "denied")]),
                );
                Err(denied(needed))
            }
        }
    }

    /// Check that this person may read this project.
    pub fn allow_project(
        &self,
        who: &SignedIn,
        project_id: [u8; 16],
        needed: Role,
    ) -> Result<(), TallyOwlError> {
        let project = self
            .store
            .catalog()
            .project(project_id)
            .map_err(|e| TallyOwlError::internal(e.to_string()))?
            // A project nobody can see and a project that does not exist read
            // the same, for the reason above.
            .ok_or_else(|| denied(needed))?;
        self.allow(who, project.workspace_id, needed)
    }

    /// One source, by identifier.
    ///
    /// A collector fetching its collection policy names its source, and the
    /// head resolves the workspace and the project from the catalog. Tenancy
    /// never travels from a collector. See D32.
    pub fn source(
        &self,
        source_id: [u8; 16],
    ) -> Result<Option<tallyowl_store::control::Source>, TallyOwlError> {
        self.store
            .catalog()
            .source(source_id)
            .map_err(|e| TallyOwlError::internal(e.to_string()))
    }

    /// The workspaces this session can read.
    pub fn list_workspaces(
        &self,
        who: &SignedIn,
        _request: ListRequest,
    ) -> Result<WorkspaceList, TallyOwlError> {
        let held = self
            .store
            .catalog()
            .workspaces()
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        Ok(WorkspaceList {
            workspaces: held
                .into_iter()
                .filter(|workspace| who.role_in(workspace.workspace_id).is_some())
                .map(|workspace| WireWorkspace {
                    workspace_id: workspace.workspace_id.to_vec(),
                    name: workspace.name,
                })
                .collect(),
            next_cursor: None,
        })
    }

    /// The projects this session can read.
    pub fn list_projects(
        &self,
        who: &SignedIn,
        _request: ListRequest,
    ) -> Result<ProjectList, TallyOwlError> {
        let held = self
            .store
            .catalog()
            .projects()
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        Ok(ProjectList {
            projects: held
                .into_iter()
                .filter(|project| who.role_in(project.workspace_id).is_some())
                .map(|project| WireProject {
                    project_id: project.project_id.to_vec(),
                    workspace_id: project.workspace_id.to_vec(),
                    name: project.name,
                    description: project.description,
                })
                .collect(),
            next_cursor: None,
        })
    }

    /// The keys this session can see.
    ///
    /// A key summary never carries a digest and never carries a key. There is
    /// nothing here anybody could authenticate with.
    pub fn list_api_keys(
        &self,
        who: &SignedIn,
        _request: ListRequest,
    ) -> Result<ApiKeyList, TallyOwlError> {
        let held = self
            .store
            .catalog()
            .api_keys()
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        let mut keys = Vec::new();
        for key in held {
            // Seeing which applications exist is an administrative fact, so it
            // needs the administrative role rather than the reading one.
            if who
                .role_in(key.workspace_id)
                .is_some_and(|role| role.allows(Role::Admin))
            {
                keys.push(ApiKeySummary {
                    key_id: key.key_id,
                    project_id: key.project_id.to_vec(),
                    created_at: key.created_at,
                    expires_at: key.expires_at,
                    last_used_at: key.last_used_at,
                    revoked: key.revoked_at.is_some(),
                });
            }
        }
        Ok(ApiKeyList {
            keys,
            next_cursor: None,
        })
    }
}
