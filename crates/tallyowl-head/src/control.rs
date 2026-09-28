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
    ApiKeyList, ApiKeySummary, ListRequest, PolicyRequest, PolicyScope, Project as WireProject,
    ProjectList, Workspace as WireWorkspace, WorkspaceList,
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
// Pages
// ---------------------------------------------------------------------------

/// The most items one list reply carries, and what a request that names no
/// limit gets.
///
/// Every list used to ignore its request and answer with the whole table. Past
/// the frame limit the reply failed, and there was no way to ask for less.
pub const MAX_PAGE: usize = 1000;

/// One page of a list, and the cursor that continues it.
///
/// `key` orders the list and must be different for every item. The cursor is
/// the key of the last item of the page, so a page continues correctly when
/// items were added or removed in between: nothing is skipped and nothing is
/// repeated, because the next page is "everything after this key" and not
/// "everything after this many".
pub fn page<T>(
    mut items: Vec<T>,
    request: &ListRequest,
    key: impl Fn(&T) -> Vec<u8>,
) -> (Vec<T>, Option<Vec<u8>>) {
    items.sort_by_key(|item| key(item));
    if let Some(cursor) = request.cursor.as_deref() {
        items.retain(|item| key(item).as_slice() > cursor);
    }
    let limit = request
        .limit
        .map(|limit| (limit as usize).clamp(1, MAX_PAGE))
        .unwrap_or(MAX_PAGE);
    if items.len() <= limit {
        return (items, None);
    }
    items.truncate(limit);
    let next = items.last().map(&key);
    (items, next)
}

// ---------------------------------------------------------------------------
// The records that age out
// ---------------------------------------------------------------------------

/// What one reaping pass removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reaped {
    pub sessions: usize,
    pub pending_logins: usize,
    pub nodes: usize,
}

impl Reaped {
    pub fn anything(&self) -> bool {
        self.sessions + self.pending_logins + self.nodes > 0
    }
}

/// Remove every session, pending sign-in, and node record that has expired.
///
/// **The three reapers existed and nothing called them.** A pending sign-in is
/// written by `begin-login`, which needs no credential, so the table grew with
/// every visit to the sign-in page. NODE_IDENTITY.md section 7 promises that an
/// expired pod identity is removed, and a churning deployment left one record
/// for each pod it ever ran. The maintenance loop calls this once each pass.
///
/// One reaper that fails does not stop the other two. The first failure is what
/// the caller reads.
pub fn reap_expired(store: &SegmentedStore, now: i64) -> Result<Reaped, TallyOwlError> {
    let catalog = store.catalog();
    let sessions = catalog.expire_sessions(now);
    let pending_logins = catalog.expire_pending_logins(now);
    let nodes = catalog.expire_nodes(now, crate::enrollment::NODE_RECORD_SAFETY_MS);
    let failed = [&sessions, &pending_logins, &nodes]
        .into_iter()
        .find_map(|result| result.as_ref().err())
        .map(|e| TallyOwlError::internal(e.to_string()));
    match failed {
        Some(failure) => Err(failure),
        None => Ok(Reaped {
            sessions: sessions.unwrap_or(0),
            pending_logins: pending_logins.unwrap_or(0),
            nodes: nodes.unwrap_or(0),
        }),
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

/// Whether this person holds the installation's own authority.
///
/// Two identities pass. An operator session, because an installation is set up
/// before it has a workspace for anybody to own. And an owner of any workspace,
/// because that is the highest role a signed-in person can hold and there is
/// nothing above it yet. L050 records that the role model is workspace-scoped;
/// an installation-scoped role is the wider change it names.
pub fn holds_installation_authority(who: &SignedIn) -> bool {
    who.issuer == tallyowl_store::control::OPERATOR_ISSUER
        || who
            .memberships
            .iter()
            .any(|(_, role)| role.allows(Role::Owner))
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

    /// Check that this person holds the installation's own authority.
    ///
    /// The rule is [`holds_installation_authority`]. A policy at the
    /// installation or the environment scope applies to every tenant, so it
    /// needs this and not a workspace role.
    pub fn allow_installation(&self, who: &SignedIn) -> Result<(), TallyOwlError> {
        if holds_installation_authority(who) {
            return Ok(());
        }
        self.metrics.increment(
            "tallyowl_control_requests_total",
            &labels(&[("outcome", "denied")]),
        );
        Err(TallyOwlError::new(
            tallyowl_obs::ErrorCode::PermissionDenied,
            "Only somebody who administers this installation can do that. Ask the person who runs TallyOwl.",
        )
        .retryable(false))
    }

    /// Check that this person may act on the policy of one scope.
    ///
    /// **The scope decides the check, not the shape of the ID.** A check that
    /// ran only when the ID looked like a project let any signed-in person set
    /// the installation's kill switch, and refused an owner who set the policy
    /// of their own workspace.
    ///
    /// - The installation and an environment apply to every tenant. They need
    ///   the installation's own authority.
    /// - A workspace needs the role in that workspace.
    /// - A project and a source need the role in the workspace that owns them.
    ///
    /// A scope that does not resolve is refused. It reads the same as a scope
    /// the person may not act on, for the reason [`Self::allow`] gives.
    pub fn allow_policy_scope(
        &self,
        who: &SignedIn,
        scope: &PolicyScope,
        scope_id: Option<&str>,
        needed: Role,
    ) -> Result<(), TallyOwlError> {
        let id = || -> Result<[u8; 16], TallyOwlError> {
            scope_id
                .and_then(tallyowl_store::row::from_hex)
                .and_then(|bytes| <[u8; 16]>::try_from(bytes).ok())
                .ok_or_else(|| denied(needed))
        };
        match scope {
            PolicyScope::Installation | PolicyScope::Environment => self.allow_installation(who),
            PolicyScope::Workspace => self.allow(who, id()?, needed),
            PolicyScope::Project => self.allow_project(who, id()?, needed),
            PolicyScope::Source => {
                let source = self.source(id()?)?.ok_or_else(|| denied(needed))?;
                self.allow(who, source.workspace_id, needed)
            }
        }
    }

    /// Check that this person may read the policy this request compiles.
    ///
    /// The narrowest level the request names decides the check, because that
    /// level is what the answer describes. A request that names no workspace,
    /// project, or source reads the installation's policy, and that belongs to
    /// the installation.
    pub fn allow_policy_read(
        &self,
        who: &SignedIn,
        request: &PolicyRequest,
    ) -> Result<(), TallyOwlError> {
        let needed = Role::Viewer;
        let id = |bytes: &[u8]| <[u8; 16]>::try_from(bytes).map_err(|_| denied(needed));
        if let Some(source_id) = request.source_id.as_deref() {
            let source = self.source(id(source_id)?)?.ok_or_else(|| denied(needed))?;
            return self.allow(who, source.workspace_id, needed);
        }
        if let Some(project_id) = request.project_id.as_deref() {
            return self.allow_project(who, id(project_id)?, needed);
        }
        if let Some(workspace_id) = request.workspace_id.as_deref() {
            return self.allow(who, id(workspace_id)?, needed);
        }
        self.allow_installation(who)
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
        request: ListRequest,
    ) -> Result<WorkspaceList, TallyOwlError> {
        let held = self
            .store
            .catalog()
            .workspaces()
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        let (workspaces, next_cursor) = page(
            held.into_iter()
                .filter(|workspace| who.role_in(workspace.workspace_id).is_some())
                .map(|workspace| WireWorkspace {
                    workspace_id: workspace.workspace_id.to_vec(),
                    name: workspace.name,
                })
                .collect(),
            &request,
            |workspace| workspace.workspace_id.clone(),
        );
        Ok(WorkspaceList {
            workspaces,
            next_cursor,
        })
    }

    /// The projects this session can read.
    pub fn list_projects(
        &self,
        who: &SignedIn,
        request: ListRequest,
    ) -> Result<ProjectList, TallyOwlError> {
        let held = self
            .store
            .catalog()
            .projects()
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        let (projects, next_cursor) = page(
            held.into_iter()
                .filter(|project| who.role_in(project.workspace_id).is_some())
                .map(|project| WireProject {
                    project_id: project.project_id.to_vec(),
                    workspace_id: project.workspace_id.to_vec(),
                    name: project.name,
                    description: project.description,
                })
                .collect(),
            &request,
            |project| project.project_id.clone(),
        );
        Ok(ProjectList {
            projects,
            next_cursor,
        })
    }

    /// The keys this session can see.
    ///
    /// A key summary never carries a digest and never carries a key. There is
    /// nothing here anybody could authenticate with.
    pub fn list_api_keys(
        &self,
        who: &SignedIn,
        request: ListRequest,
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
        let (keys, next_cursor) = page(keys, &request, |key| key.key_id.clone().into_bytes());
        Ok(ApiKeyList { keys, next_cursor })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tallyowl_store::control::Member;

    fn service(name: &str) -> ControlService {
        let base = std::env::var("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target"));
        let place = base
            .join("head-control-tests")
            .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
        let _ = std::fs::remove_dir_all(&place);
        let store = SegmentedStore::open_with(
            &place,
            tallyowl_store::Sealing {
                max_open_rows: 100,
                max_open_ms: i64::MAX,
                verify_on_read: true,
                reserve_bytes: 0,
            },
            tallyowl_store::wal::GroupCommit::default(),
        )
        .expect("the store opens");
        ControlService {
            store: Arc::new(store),
            metrics: Registry::new(),
            key_cache_ttl_ms: DEFAULT_KEY_CACHE_TTL_MS,
        }
    }

    /// One workspace with one project and one source, and a person who holds
    /// `role` in it. A `None` role is a person who signed in and was given
    /// nothing, which is what a first LinkKeys sign-in produces (L054).
    fn person(
        control: &ControlService,
        subject: &str,
        role: Option<Role>,
    ) -> (SignedIn, tallyowl_store::control::Source) {
        let catalog = control.store.catalog();
        catalog.provision("shop", "web", 1).expect("provisions");
        let source = catalog.sources().expect("sources").remove(0);
        if let Some(role) = role {
            catalog
                .put_member(&Member {
                    subject: subject.into(),
                    workspace_id: source.workspace_id,
                    role,
                    display_name: subject.into(),
                    added_at: 1,
                })
                .expect("a member");
        }
        let token = catalog
            .issue_session(subject, "id.example", now_ms(), 60_000)
            .expect("a session")
            .token;
        (control.signed_in(Some(&token)).expect("signs in"), source)
    }

    #[test]
    fn a_person_with_no_role_cannot_set_the_installation_policy() {
        let control = service("no-role");
        let (who, _) = person(&control, "nobody", None);
        for scope in [PolicyScope::Installation, PolicyScope::Environment] {
            let refused = control
                .allow_policy_scope(&who, &scope, Some("production"), Role::Admin)
                .expect_err("a kill switch for every tenant needs the installation's authority");
            assert_eq!(refused.code, tallyowl_obs::ErrorCode::PermissionDenied);
        }
        control
            .allow_policy_scope(&who, &PolicyScope::Installation, None, Role::Admin)
            .expect_err("an absent scope ID is still the installation");
    }

    #[test]
    fn a_workspace_admin_sets_the_policy_of_that_workspace_and_a_viewer_does_not() {
        let control = service("workspace");
        let (admin, source) = person(&control, "admin", Some(Role::Admin));
        let (viewer, _) = person(&control, "viewer", Some(Role::Viewer));
        let workspace = tallyowl_store::row::hex(&source.workspace_id);
        let project = tallyowl_store::row::hex(&source.project_id);
        let source_text = tallyowl_store::row::hex(&source.source_id);

        for (scope, id) in [
            (PolicyScope::Workspace, &workspace),
            (PolicyScope::Project, &project),
            (PolicyScope::Source, &source_text),
        ] {
            control
                .allow_policy_scope(&admin, &scope, Some(id), Role::Admin)
                .expect("an admin of the workspace sets policy inside it");
            control
                .allow_policy_scope(&viewer, &scope, Some(id), Role::Admin)
                .expect_err("a viewer reads and does not set");
        }
        // An admin of one workspace is not the installation.
        control
            .allow_policy_scope(&admin, &PolicyScope::Installation, None, Role::Admin)
            .expect_err("an admin is not an owner");
    }

    #[test]
    fn a_scope_that_does_not_resolve_is_refused() {
        let control = service("unresolved");
        let (admin, _) = person(&control, "admin", Some(Role::Admin));
        let unknown = tallyowl_store::row::hex(&[0x5a; 16]);
        for scope in [
            PolicyScope::Workspace,
            PolicyScope::Project,
            PolicyScope::Source,
        ] {
            control
                .allow_policy_scope(&admin, &scope, Some(&unknown), Role::Admin)
                .expect_err("nothing owns this ID");
            control
                .allow_policy_scope(&admin, &scope, Some("production"), Role::Admin)
                .expect_err("this is not an ID");
            control
                .allow_policy_scope(&admin, &scope, None, Role::Admin)
                .expect_err("a narrower scope names something");
        }
    }

    #[test]
    fn reading_policy_checks_the_narrowest_level_the_request_names() {
        let control = service("read");
        let (viewer, source) = person(&control, "viewer", Some(Role::Viewer));
        let (nobody, _) = person(&control, "nobody", None);
        let request = |workspace: bool, project: bool| PolicyRequest {
            workspace_id: workspace.then(|| source.workspace_id.to_vec()),
            project_id: project.then(|| source.project_id.to_vec()),
            environment: None,
            source_id: None,
        };
        control
            .allow_policy_read(&viewer, &request(true, true))
            .expect("a viewer reads their project's policy");
        control
            .allow_policy_read(&viewer, &request(true, false))
            .expect("a viewer reads their workspace's policy");
        control
            .allow_policy_read(&viewer, &request(false, false))
            .expect_err("the installation's policy is not a viewer's to read");
        for named in [
            request(true, true),
            request(true, false),
            request(false, false),
        ] {
            control
                .allow_policy_read(&nobody, &named)
                .expect_err("a person with no role reads nothing");
        }
    }

    #[test]
    fn a_list_comes_back_in_pages_and_a_cursor_continues_it() {
        let ask = |cursor: Option<Vec<u8>>, limit: Option<u64>| ListRequest { cursor, limit };
        let key = |item: &u8| vec![*item];
        let items = || vec![5u8, 1, 4, 2, 3];

        let (first, next) = page(items(), &ask(None, Some(2)), key);
        assert_eq!(first, vec![1, 2]);
        assert_eq!(next, Some(vec![2]));

        // An item that arrives between two pages is neither skipped nor shown
        // twice, because the cursor is a key and not a count.
        let mut grown = items();
        grown.push(0);
        let (second, next) = page(grown, &ask(next, Some(2)), key);
        assert_eq!(second, vec![3, 4]);
        let (last, next) = page(items(), &ask(next, Some(2)), key);
        assert_eq!(last, vec![5]);
        assert_eq!(next, None, "the last page still offered a cursor");

        // No limit is the largest page and not the whole table, and a limit of
        // nothing still makes progress.
        let many: Vec<u32> = (0..MAX_PAGE as u32 + 5).collect();
        let (capped, next) = page(many, &ask(None, None), |item| item.to_be_bytes().to_vec());
        assert_eq!(capped.len(), MAX_PAGE);
        assert!(next.is_some());
        let (one, _) = page(items(), &ask(None, Some(0)), key);
        assert_eq!(one, vec![1]);
    }

    #[test]
    fn the_records_that_age_out_are_removed_and_the_live_ones_stay() {
        let control = service("reap");
        let catalog = control.store.catalog();
        let now = 1_000_000;
        catalog
            .issue_session("old", "id.example", now - 10, 5)
            .expect("issued");
        let live = catalog
            .issue_session("live", "id.example", now, 60_000)
            .expect("issued");
        catalog
            .put_pending_login("state-old", b"p", now - 1)
            .expect("stored");

        let reaped = reap_expired(&control.store, now).expect("reaps");
        assert_eq!(reaped.sessions, 1);
        assert_eq!(reaped.pending_logins, 1);
        assert!(reaped.anything());
        assert!(catalog
            .session(&live.record.session_id)
            .expect("reads")
            .is_some());
        assert!(!reap_expired(&control.store, now).expect("reaps").anything());
    }
}
