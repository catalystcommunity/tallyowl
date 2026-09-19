//! The head's two service surfaces.
//!
//! The head answers `commit-batch` on the `TallyOwlCollector` contract, because
//! that operation is the collector-to-head hop. It answers `run-query` on the
//! `TallyOwlControl` contract. One listener carries both, because a CSIL-RPC
//! request names its own service.
//!
//! An application never speaks to the head. It reaches a collector, and the
//! collector reaches this.

use std::sync::Arc;

use tallyowl_collector_api::codec::{
    decode_commit_batch_request, decode_resolve_key_request, encode_commit_batch_response,
    encode_resolve_key_response, encode_service_error as encode_collector_error,
};
use tallyowl_control_api::codec::{
    decode_begin_login_request, decode_complete_login_request, decode_create_role_token_request,
    decode_enroll_node_request, decode_list_request, decode_query_request,
    decode_renew_node_certificate_request, decode_revoke_role_token_request, encode_api_key_list,
    encode_begin_login_response, encode_complete_login_response, encode_create_role_token_response,
    encode_empty, encode_enroll_node_response, encode_node_list, encode_project_list,
    encode_query_response, encode_role_token_list, encode_service_error as encode_control_error,
    encode_workspace_list,
};
use tallyowl_obs::error::TallyOwlError;
use tallyowl_obs::log::Logger;
use tallyowl_rpc::{
    error_outcome, malformed, reply, unknown_operation, Dispatcher, Outcome, Request,
};

use crate::control::ControlService;
use crate::ingest::Ingest;
use crate::query::{check_consistency, QueryService};

pub const COLLECTOR_SERVICE: &str = "TallyOwlCollector";
pub const CONTROL_SERVICE: &str = "TallyOwlControl";

pub struct HeadService {
    pub ingest: Arc<Ingest>,
    pub query: Arc<QueryService>,
    pub control: Arc<ControlService>,
    pub enrollment: Arc<crate::enrollment::EnrollmentService>,
    pub sign_in: Arc<crate::linkkeys::SignIn>,
    pub logger: Arc<Logger>,
    /// Phase 8. Collection policy, and the saved analyses a dashboard is made
    /// of.
    pub policy: Arc<crate::policy::PolicyService>,
    pub saved: Arc<crate::saved::SavedService>,
    /// Phase 9. The attribution weights and windows of each project.
    pub attribution: Arc<crate::attribution::AttributionService>,
    /// Phase 10. Alert rules, their state, and the workflows that run them.
    ///
    /// Both are optional because a head that was started without a durable
    /// queue can still answer every other operation. An alert operation then
    /// refuses by name rather than the whole head refusing to start: an
    /// installation with no Corndogs is a broken installation for ingest as
    /// well, and that is where it is reported.
    pub alerts: Option<Arc<crate::alerts::AlertService>>,
    pub workflows: Option<Arc<crate::workflows::Workflows>>,
}

impl Dispatcher for HeadService {
    fn dispatch(&self, request: &Request) -> Outcome {
        match (request.service.as_str(), request.op.as_str()) {
            (COLLECTOR_SERVICE, "commit-batch") => self.commit_batch(request),
            (COLLECTOR_SERVICE, "resolve-key") => self.resolve_key(request),
            (CONTROL_SERVICE, "run-query") => self.run_query(request),
            (CONTROL_SERVICE, "list-workspaces") => self.list_workspaces(request),
            (CONTROL_SERVICE, "list-projects") => self.list_projects(request),
            (CONTROL_SERVICE, "list-api-keys") => self.list_api_keys(request),
            (CONTROL_SERVICE, "begin-login") => self.begin_login(request),
            (CONTROL_SERVICE, "complete-login") => self.complete_login(request),
            (CONTROL_SERVICE, "create-role-token") => self.create_role_token(request),
            (CONTROL_SERVICE, "list-role-tokens") => self.list_role_tokens(request),
            (CONTROL_SERVICE, "revoke-role-token") => self.revoke_role_token(request),
            (CONTROL_SERVICE, "enroll-node") => self.enroll_node(request),
            (CONTROL_SERVICE, "renew-node-certificate") => self.renew_node_certificate(request),
            (CONTROL_SERVICE, "list-nodes") => self.list_nodes(request),
            (CONTROL_SERVICE, "request-deletion") => self.request_deletion(request),
            (CONTROL_SERVICE, "get-policy") => self.get_policy(request),
            (CONTROL_SERVICE, "put-policy") => self.put_policy(request),
            (CONTROL_SERVICE, "put-analysis") => self.put_analysis(request),
            (CONTROL_SERVICE, "list-analyses") => self.list_analyses(request),
            (CONTROL_SERVICE, "delete-analysis") => self.delete_analysis(request),
            (CONTROL_SERVICE, "put-dashboard") => self.put_dashboard(request),
            (CONTROL_SERVICE, "list-dashboards") => self.list_dashboards(request),
            (CONTROL_SERVICE, "delete-dashboard") => self.delete_dashboard(request),
            (CONTROL_SERVICE, "put-attribution-settings") => self.put_attribution_settings(request),
            (CONTROL_SERVICE, "get-attribution-settings") => self.get_attribution_settings(request),
            (CONTROL_SERVICE, "put-alert-rule") => self.put_alert_rule(request),
            (CONTROL_SERVICE, "list-alert-rules") => self.list_alert_rules(request),
            (CONTROL_SERVICE, "list-alert-instances") => self.list_alert_instances(request),
            (CONTROL_SERVICE, "delete-alert-rule") => self.delete_alert_rule(request),
            (CONTROL_SERVICE, "silence-alert") => self.silence_alert(request),
            (CONTROL_SERVICE, "resolve-alert") => self.resolve_alert(request),
            (CONTROL_SERVICE, "list-workflows") => self.list_workflows(request),
            (CONTROL_SERVICE, "run-workflow") => self.run_workflow(request),
            (CONTROL_SERVICE, "list-notifications") => self.list_notifications(request),
            (COLLECTOR_SERVICE, "fetch-policy") => self.fetch_policy(request),
            (service, op) => unknown_operation(service, op),
        }
    }
}

impl HeadService {
    fn commit_batch(&self, request: &Request) -> Outcome {
        let decoded = match decode_commit_batch_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        match self.ingest.commit(decoded) {
            Ok(receipt) => {
                self.logger.info(
                    "Committed a batch.",
                    &[
                        ("accepted", &receipt.accepted.to_string()),
                        ("watermark", &receipt.commit_watermark.to_string()),
                        (
                            "deduplicated",
                            &receipt.deduplicated.unwrap_or(false).to_string(),
                        ),
                    ],
                );
                reply(
                    "CommitBatchResponse",
                    encode_commit_batch_response(&receipt),
                )
            }
            Err(e) => {
                self.log_failure("A batch did not commit.", &e);
                error_outcome(encode_collector_error(&crate::wire::to_collector_error(&e)))
            }
        }
    }

    /// Resolve a source credential for a collector.
    ///
    /// The log line says the outcome and never the credential. A credential in
    /// a log is a credential in a backup, a ticket, and a screen share.
    fn resolve_key(&self, request: &Request) -> Outcome {
        let decoded = match decode_resolve_key_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        match self.control.resolve_key(decoded) {
            Ok(response) => {
                self.logger.info(
                    "Resolved a source credential.",
                    &[("key_id", &response.key_id)],
                );
                reply("ResolveKeyResponse", encode_resolve_key_response(&response))
            }
            Err(e) => {
                self.logger
                    .warning("Refused a source credential.", &[("code", e.code.as_str())]);
                error_outcome(encode_collector_error(&crate::wire::to_collector_error(&e)))
            }
        }
    }

    /// A control list, with the authorization every one of them needs.
    ///
    /// A read that skipped the check because it "only lists names" is how one
    /// tenant learns another tenant exists, and D8 makes the workspace the
    /// isolation boundary.
    fn control_list<T>(
        &self,
        request: &Request,
        answer: impl FnOnce(
            &tallyowl_store::control::SignedIn,
            tallyowl_control_api::types::ListRequest,
        ) -> Result<T, TallyOwlError>,
        encode: impl FnOnce(&T) -> Vec<u8>,
        variant: &str,
    ) -> Outcome {
        let decoded = match decode_list_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = self
            .control
            .signed_in(request.auth.as_deref())
            .and_then(|who| answer(&who, decoded));
        match outcome {
            Ok(value) => reply(variant, encode(&value)),
            Err(e) => {
                self.log_failure("A control request was refused.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Role tokens and node enrollment. NODE_IDENTITY.md sections 2 to 6.
    // -----------------------------------------------------------------------

    fn create_role_token(&self, request: &Request) -> Outcome {
        let decoded = match decode_create_role_token_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let signed_in = match self.control.signed_in(request.auth.as_deref()) {
            Ok(signed_in) => signed_in,
            Err(e) => return self.control_error("A role token was not created.", &e),
        };
        match self.enrollment.create_role_token(&signed_in, decoded) {
            Ok(response) => {
                // The token ID reaches the log and the token never does. A
                // token in a log is a token in a backup and a screen share.
                self.logger
                    .info("Created a role token.", &[("token_id", &response.token_id)]);
                reply(
                    "CreateRoleTokenResponse",
                    encode_create_role_token_response(&response),
                )
            }
            Err(e) => self.control_error("A role token was not created.", &e),
        }
    }

    fn list_role_tokens(&self, request: &Request) -> Outcome {
        let decoded = match decode_list_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let signed_in = match self.control.signed_in(request.auth.as_deref()) {
            Ok(signed_in) => signed_in,
            Err(e) => return self.control_error("Role tokens were not listed.", &e),
        };
        match self.enrollment.list_role_tokens(&signed_in, decoded) {
            Ok(list) => reply("RoleTokenList", encode_role_token_list(&list)),
            Err(e) => self.control_error("Role tokens were not listed.", &e),
        }
    }

    fn revoke_role_token(&self, request: &Request) -> Outcome {
        let decoded = match decode_revoke_role_token_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let signed_in = match self.control.signed_in(request.auth.as_deref()) {
            Ok(signed_in) => signed_in,
            Err(e) => return self.control_error("A role token was not revoked.", &e),
        };
        let token_id = decoded.token_id.clone();
        match self.enrollment.revoke_role_token(&signed_in, decoded) {
            Ok(()) => {
                self.logger
                    .info("Revoked a role token.", &[("token_id", &token_id)]);
                reply(
                    "Empty",
                    encode_empty(&tallyowl_control_api::types::Empty {}),
                )
            }
            Err(e) => self.control_error("A role token was not revoked.", &e),
        }
    }

    /// Enroll a node.
    ///
    /// This is the one control operation that takes no session. A node has no
    /// identity yet, so the role token it presents is what authenticates it.
    fn enroll_node(&self, request: &Request) -> Outcome {
        let decoded = match decode_enroll_node_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        match self.enrollment.enroll(decoded) {
            Ok(response) => {
                self.logger.info(
                    "Enrolled a node.",
                    &[
                        ("node_id", &response.node_id),
                        ("serial", &response.certificate_serial),
                        ("expires_at", &response.expires_at.to_string()),
                    ],
                );
                reply("EnrollNodeResponse", encode_enroll_node_response(&response))
            }
            Err(e) => self.control_error("A node was not enrolled.", &e),
        }
    }

    fn renew_node_certificate(&self, request: &Request) -> Outcome {
        let decoded = match decode_renew_node_certificate_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        match self.enrollment.renew(decoded) {
            Ok(response) => reply("EnrollNodeResponse", encode_enroll_node_response(&response)),
            Err(e) => self.control_error("A certificate was not renewed.", &e),
        }
    }

    fn list_nodes(&self, request: &Request) -> Outcome {
        let decoded = match decode_list_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let signed_in = match self.control.signed_in(request.auth.as_deref()) {
            Ok(signed_in) => signed_in,
            Err(e) => return self.control_error("Nodes were not listed.", &e),
        };
        match self.enrollment.list_nodes(&signed_in, decoded) {
            Ok(list) => reply("NodeList", encode_node_list(&list)),
            Err(e) => self.control_error("Nodes were not listed.", &e),
        }
    }

    /// One place that logs a control failure and encodes it.
    fn control_error(&self, what: &str, error: &TallyOwlError) -> Outcome {
        self.log_failure(what, error);
        error_outcome(encode_control_error(&crate::wire::to_control_error(error)))
    }

    /// Start a sign-in.
    ///
    /// This is the one control operation that does not check authorization,
    /// because it is how somebody becomes authorized. Everything it can do is
    /// bounded by the configuration: a domain the installation does not trust
    /// and a callback it did not configure are both refused.
    fn begin_login(&self, request: &Request) -> Outcome {
        let decoded = match decode_begin_login_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        match self.sign_in.begin(decoded) {
            Ok(response) => {
                self.logger
                    .info("Started a sign-in.", &[("login_id", &response.login_id)]);
                reply("BeginLoginResponse", encode_begin_login_response(&response))
            }
            Err(e) => {
                self.log_failure("A sign-in did not start.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    /// Finish a sign-in with what the callback carried.
    fn complete_login(&self, request: &Request) -> Outcome {
        let decoded = match decode_complete_login_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        match self.sign_in.complete(decoded) {
            Ok(response) => {
                // The subject reaches the log and the token never does. A
                // session token in a log is a session token in a backup, a
                // ticket, and a screen share.
                self.logger.info(
                    "Somebody signed in.",
                    &[
                        ("subject", &response.subject),
                        ("workspaces", &response.memberships.len().to_string()),
                    ],
                );
                reply(
                    "CompleteLoginResponse",
                    encode_complete_login_response(&response),
                )
            }
            Err(e) => {
                self.log_failure("A sign-in did not finish.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn list_workspaces(&self, request: &Request) -> Outcome {
        self.control_list(
            request,
            |who, list| self.control.list_workspaces(who, list),
            encode_workspace_list,
            "WorkspaceList",
        )
    }

    fn list_projects(&self, request: &Request) -> Outcome {
        self.control_list(
            request,
            |who, list| self.control.list_projects(who, list),
            encode_project_list,
            "ProjectList",
        )
    }

    fn list_api_keys(&self, request: &Request) -> Outcome {
        self.control_list(
            request,
            |who, list| self.control.list_api_keys(who, list),
            encode_api_key_list,
            "ApiKeyList",
        )
    }

    fn run_query(&self, request: &Request) -> Outcome {
        let decoded = match decode_query_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        // A query names a project, and reading that project's telemetry needs a
        // role in the workspace that holds it. Without this check a caller who
        // guessed a project ID would read another tenant's data.
        let outcome = self
            .authorize_query(request, &decoded)
            .and_then(|()| check_consistency(&decoded))
            .and_then(|()| self.query.run(decoded));
        match outcome {
            Ok(response) => reply("QueryResponse", encode_query_response(&response)),
            Err(e) => {
                self.log_failure("A query did not run.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    /// Check that whoever sent this query may read the project it names.
    fn authorize_query(
        &self,
        request: &Request,
        decoded: &tallyowl_control_api::types::QueryRequest,
    ) -> Result<(), TallyOwlError> {
        let who = self.control.signed_in(request.auth.as_deref())?;
        for project_id in crate::query::projects_named(decoded)? {
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Viewer)?;
        }
        Ok(())
    }

    /// An error a retry can fix is a warning, because the system is expected to
    /// recover from it. A permanent one needs a person.
    fn log_failure(&self, message: &str, error: &TallyOwlError) {
        let fields = [("code", error.code.as_str())];
        if error.retryable {
            self.logger.warning(message, &fields);
        } else {
            self.logger.error(message, &fields);
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 8: erasure, collection policy, saved analyses, and dashboards.
//
// Each of these was declared in the contract and answered by nothing. A
// declared operation nobody answers is worse than one nobody declared, because
// a caller reads the contract.
// ---------------------------------------------------------------------------

impl HeadService {
    /// Remove one person, or a named set of events, or a time range.
    ///
    /// `docs/PLAN.md` Phase 8 asks for "per-user erasure across detailed data,
    /// derived state, local segments, cold objects, and caches". One standing
    /// predicate for each of a person's identifiers covers all five, because
    /// every read applies the predicate and compaction is what reclaims the
    /// bytes. [`crate::erasure`] holds the reasoning.
    ///
    /// **An erasure needs the administrator role**, not the viewer role a query
    /// needs. Removing somebody is not a read.
    fn request_deletion(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_deletion_request, encode_deletion_response};
        use tallyowl_control_api::types::DeletionResponse;

        let decoded = match decode_deletion_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| -> Result<DeletionResponse, TallyOwlError> {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.target.project_id)?;
            // An erasure is the owner's act. `tallyowl_store::control::Role`
            // says so where it is defined: an owner is "everything an admin does,
            // and requests an erasure".
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Owner)?;

            let target = match (
                decoded.target.end_user_id.as_deref(),
                decoded.target.event_ids.as_deref(),
                decoded.target.range.as_ref(),
            ) {
                (Some(who), _, _) => crate::erasure::Target::EndUser(who.to_string()),
                (None, Some(ids), _) if !ids.is_empty() => crate::erasure::Target::Events(
                    ids.iter()
                        .filter_map(|id| <[u8; 16]>::try_from(id.as_slice()).ok())
                        .collect(),
                ),
                (None, _, Some(range)) => {
                    crate::erasure::Target::Range(range.range_start, range.range_end)
                }
                _ => {
                    return Err(TallyOwlError::invalid_argument(
                        "This erasure names nothing to remove. Name the person, the events, or the time range.",
                    ))
                }
            };

            let erasure = crate::erasure::Request {
                project_id,
                target,
                reason: decoded.reason.clone(),
                horizon_ms: crate::erasure::DEFAULT_HORIZON_MS,
                requested_at: tallyowl_obs::time::now_ms(),
            };
            let identity = self.query.identity_of(project_id)?;
            let report = crate::erasure::run(self.query.store.as_ref(), &erasure, &identity)?;
            Ok(DeletionResponse {
                request_id: report.request_id,
                tombstone_generation: report.tombstone_generation,
                accepted_at: report.accepted_at,
                predicates: Some(report.predicates as u64),
                identifiers: (!report.identifiers.is_empty()).then_some(report.identifiers),
            })
        })();

        match outcome {
            Ok(response) => reply("DeletionResponse", encode_deletion_response(&response)),
            Err(e) => {
                self.log_failure("An erasure did not run.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn get_policy(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_policy_request, encode_compiled_policy};
        let decoded = match decode_policy_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            if let Some(project_id) = decoded.project_id.as_deref() {
                self.control.allow_project(
                    &who,
                    to_project(project_id)?,
                    tallyowl_store::control::Role::Viewer,
                )?;
            }
            Ok::<_, TallyOwlError>(self.policy.compiled(&decoded))
        })();
        match outcome {
            Ok(compiled) => reply("CompiledPolicy", encode_compiled_policy(&compiled)),
            Err(e) => error_outcome(encode_control_error(&crate::wire::to_control_error(&e))),
        }
    }

    fn put_policy(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_policy_document, encode_compiled_policy};
        let decoded = match decode_policy_document(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            // Setting policy is an administrator's act, and a project-scoped
            // one still needs the role in that project.
            if let Some(project_id) = decoded.scope_id.as_deref().and_then(project_from_text) {
                self.control.allow_project(
                    &who,
                    project_id,
                    tallyowl_store::control::Role::Admin,
                )?;
            }
            self.policy.put(&decoded)
        })();
        match outcome {
            Ok(compiled) => reply("CompiledPolicy", encode_compiled_policy(&compiled)),
            Err(e) => {
                self.log_failure("A policy was not stored.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn put_analysis(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_saved_analysis, encode_saved_analysis};
        let decoded = match decode_saved_analysis(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Admin)?;
            self.saved.put_analysis(&decoded, &who.subject)
        })();
        match outcome {
            Ok(saved) => reply("SavedAnalysis", encode_saved_analysis(&saved)),
            Err(e) => {
                self.log_failure("An analysis was not saved.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn list_analyses(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_saved_request, encode_saved_analysis_list};
        let decoded = match decode_saved_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Viewer)?;
            Ok::<_, TallyOwlError>(self.saved.list_analyses(project_id))
        })();
        match outcome {
            Ok(list) => reply("SavedAnalysisList", encode_saved_analysis_list(&list)),
            Err(e) => error_outcome(encode_control_error(&crate::wire::to_control_error(&e))),
        }
    }

    fn delete_analysis(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_saved_request, encode_empty};
        use tallyowl_control_api::types::Empty;
        let decoded = match decode_saved_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Admin)?;
            let id = decoded
                .id
                .as_deref()
                .ok_or_else(|| TallyOwlError::invalid_argument("Name the analysis to remove."))?;
            self.saved.remove_analysis(project_id, id)
        })();
        match outcome {
            Ok(()) => reply("Empty", encode_empty(&Empty {})),
            Err(e) => {
                self.log_failure("An analysis was not removed.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    /// Remove a dashboard. The analyses it showed stay.
    ///
    /// It exists so that the starter dashboard can be got rid of. A capability
    /// a person cannot undo is a capability they have to live with, and
    /// `crate::starter` promises that a delete sticks.
    fn delete_dashboard(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_saved_request, encode_empty};
        use tallyowl_control_api::types::Empty;
        let decoded = match decode_saved_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Admin)?;
            let id = decoded
                .id
                .as_deref()
                .ok_or_else(|| TallyOwlError::invalid_argument("Name the dashboard to remove."))?;
            self.saved.remove_dashboard(project_id, id)
        })();
        match outcome {
            Ok(()) => reply("Empty", encode_empty(&Empty {})),
            Err(e) => {
                self.log_failure("A dashboard was not removed.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn put_dashboard(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_saved_dashboard, encode_saved_dashboard};
        let decoded = match decode_saved_dashboard(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Admin)?;
            self.saved.put_dashboard(&decoded, &who.subject)
        })();
        match outcome {
            Ok(saved) => reply("SavedDashboard", encode_saved_dashboard(&saved)),
            Err(e) => {
                self.log_failure("A dashboard was not saved.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn list_dashboards(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_saved_request, encode_saved_dashboard_list};
        let decoded = match decode_saved_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Viewer)?;
            Ok::<_, TallyOwlError>(self.saved.list_dashboards(project_id))
        })();
        match outcome {
            Ok(list) => reply("SavedDashboardList", encode_saved_dashboard_list(&list)),
            Err(e) => error_outcome(encode_control_error(&crate::wire::to_control_error(&e))),
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 9: policy distribution, and attribution configuration.
// ---------------------------------------------------------------------------

impl HeadService {
    // -----------------------------------------------------------------------
    // Alerts and workflows. Phase 10.
    // -----------------------------------------------------------------------

    // The alert service, or a refusal that says why there is none.
    fn alerts(&self) -> Result<&Arc<crate::alerts::AlertService>, TallyOwlError> {
        self.alerts.as_ref().ok_or_else(|| {
            TallyOwlError::new(
                tallyowl_obs::ErrorCode::FailedPrecondition,
                "This head was started without alerting, so it holds no rules. Alerting needs the durable queue that every other workflow uses."
                    .to_string(),
            )
        })
    }

    fn workflows(&self) -> Result<&Arc<crate::workflows::Workflows>, TallyOwlError> {
        self.workflows.as_ref().ok_or_else(|| {
            TallyOwlError::new(
                tallyowl_obs::ErrorCode::FailedPrecondition,
                "This head was started without the durable queue, so it runs no workflows."
                    .to_string(),
            )
        })
    }

    fn put_alert_rule(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_alert_rule, encode_alert_rule};
        let decoded = match decode_alert_rule(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Admin)?;
            self.alerts()?.put_rule(&decoded, &who.subject)
        })();
        match outcome {
            Ok(rule) => reply("AlertRule", encode_alert_rule(&rule)),
            Err(e) => {
                self.log_failure("An alert rule was not saved.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn list_alert_rules(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_alert_list_request, encode_alert_rule_list};
        let decoded = match decode_alert_list_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Viewer)?;
            self.alerts()?.rules(project_id)
        })();
        match outcome {
            Ok(rules) => reply(
                "AlertRuleList",
                encode_alert_rule_list(&tallyowl_control_api::types::AlertRuleList {
                    rules,
                    next_cursor: None,
                }),
            ),
            Err(e) => {
                self.log_failure("Alert rules were not listed.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn list_alert_instances(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_alert_list_request, encode_alert_instance_list};
        let decoded = match decode_alert_list_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Viewer)?;
            self.alerts()?.instances(project_id)
        })();
        match outcome {
            Ok(instances) => reply(
                "AlertInstanceList",
                encode_alert_instance_list(&tallyowl_control_api::types::AlertInstanceList {
                    instances,
                    next_cursor: None,
                }),
            ),
            Err(e) => {
                self.log_failure("Alert instances were not listed.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn delete_alert_rule(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::decode_delete_alert_rule_request;
        let decoded = match decode_delete_alert_rule_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Admin)?;
            self.alerts()?.remove_rule(project_id, &decoded.rule_id)
        })();
        match outcome {
            Ok(()) => reply(
                "Empty",
                encode_empty(&tallyowl_control_api::types::Empty {}),
            ),
            Err(e) => {
                self.log_failure("An alert rule was not removed.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn silence_alert(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_silence_request, encode_alert_instance};
        let decoded = match decode_silence_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Admin)?;
            self.alerts()?.silence(
                project_id,
                &decoded.rule_id,
                decoded.until,
                decoded.reason.as_deref().unwrap_or_default(),
            )
        })();
        match outcome {
            Ok(instance) => reply("AlertInstance", encode_alert_instance(&instance)),
            Err(e) => {
                self.log_failure("An alert was not silenced.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn resolve_alert(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_resolve_request, encode_alert_instance};
        let decoded = match decode_resolve_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Admin)?;
            self.alerts()?.resolve(
                project_id,
                &decoded.rule_id,
                decoded.reason.as_deref().unwrap_or_default(),
            )
        })();
        match outcome {
            Ok(instance) => reply("AlertInstance", encode_alert_instance(&instance)),
            Err(e) => {
                self.log_failure("An alert was not resolved.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn list_workflows(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::encode_workflow_list;
        let outcome = (|| {
            // Workflow state is about the installation rather than about one
            // project, so it needs a signed-in operator and nothing narrower.
            let _who = self.control.signed_in(request.auth.as_deref())?;
            Ok::<_, TallyOwlError>(self.workflows()?.status())
        })();
        match outcome {
            Ok(status) => reply(
                "WorkflowList",
                encode_workflow_list(&tallyowl_control_api::types::WorkflowList {
                    workflows: status.iter().map(crate::wire::to_workflow_status).collect(),
                }),
            ),
            Err(e) => {
                self.log_failure("Workflows were not listed.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn run_workflow(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{
            decode_run_workflow_request, encode_run_workflow_response,
        };
        let decoded = match decode_run_workflow_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Admin)?;
            let kind = crate::wire::from_workflow_kind(&decoded.kind);
            // Alert evaluation and notification are scheduled work. Running one
            // by hand would need a rule to run, and `put-alert-rule` is how a
            // person asks for that.
            if matches!(
                kind,
                crate::workflows::Kind::AlertEvaluation | crate::workflows::Kind::Notification
            ) {
                return Err(TallyOwlError::invalid_argument(
                    "Alert evaluation and notification run on their own schedule. Change the rule's interval to make it run sooner.",
                ));
            }
            let mut work = crate::workflows::Work::new(kind, project_id);
            if let Some(range) = &decoded.range {
                work.range_start = range.range_start;
                work.range_end = range.range_end;
            }
            work.destination = decoded.destination.clone().unwrap_or_default();
            self.workflows()?
                .submit(&work)
                .map(|task_id| (task_id, kind))
        })();
        match outcome {
            Ok((task_id, kind)) => reply(
                "RunWorkflowResponse",
                encode_run_workflow_response(&tallyowl_control_api::types::RunWorkflowResponse {
                    task_id,
                    kind: crate::wire::to_workflow_kind(kind),
                    accepted: true,
                    reason: None,
                }),
            ),
            Err(e) => {
                self.log_failure("A workflow was not started.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn list_notifications(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{
            decode_alert_list_request, encode_notification_delivery_list,
        };
        let decoded = match decode_alert_list_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Viewer)?;
            let alerts = self.alerts()?;
            alerts
                .store
                .catalog()
                .notifications()
                .map_err(|e| TallyOwlError::internal(e.to_string()))
        })();
        match outcome {
            Ok(held) => reply(
                "NotificationDeliveryList",
                encode_notification_delivery_list(
                    &tallyowl_control_api::types::NotificationDeliveryList {
                        deliveries: held.iter().rev().map(crate::wire::to_delivery).collect(),
                        next_cursor: None,
                    },
                ),
            ),
            Err(e) => {
                self.log_failure("Notification attempts were not listed.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    /// Answer a collector's policy fetch. `docs/POLICY.md` section 7.
    ///
    /// **This is the other end of the collection policy.** The head applies the
    /// policy at commit, so nothing wrong is stored either way; a blocked event
    /// that reaches the head has already cost a batch, a queue write, and a
    /// delivery, and a kill switch that only takes effect after transport is not
    /// a kill switch.
    ///
    /// A collector passes the version it holds and gets nothing back when that
    /// version is current, so the ordinary fetch is a few bytes each way.
    ///
    /// **It takes no session.** A collector authenticates as a node, not as a
    /// person, and it names a source. The head refuses a source it does not
    /// know rather than answering with the installation defaults, because a
    /// snapshot for an unknown source is a snapshot nothing asked for.
    fn fetch_policy(&self, request: &Request) -> Outcome {
        use tallyowl_collector_api::codec::{
            decode_fetch_policy_request, encode_fetch_policy_response,
        };
        use tallyowl_collector_api::types::FetchPolicyResponse;

        let decoded = match decode_fetch_policy_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| -> Result<FetchPolicyResponse, TallyOwlError> {
            let source_id: [u8; 16] = decoded.source_id.as_slice().try_into().map_err(|_| {
                TallyOwlError::invalid_argument(format!(
                    "A source identifier is 16 bytes and this one is {}.",
                    decoded.source_id.len()
                ))
            })?;
            let source = self.control.source(source_id)?.ok_or_else(|| {
                TallyOwlError::not_found(
                    "This installation has no source with that identifier, so there is no policy to send.",
                )
            })?;

            let version = self.policy.version();
            // **Nothing has ever been set.** The generation rises on every
            // write and starts at zero, so a zero here means no operator has
            // configured a policy at all. That is not a snapshot: it is the
            // absence of one, and sending the defaults with version zero would
            // be sending a snapshot a collector cannot tell from an older one.
            // The honest answer is the same one collector intake gives — there
            // is nothing here to apply — and a collector that has none collects
            // everything, which is what an unconfigured installation means.
            //
            // The running loop found this. A clean installation logged a
            // refused policy every fetch interval, for ever, because the head
            // was sending version zero and the collector was right to refuse
            // it. See L124.
            if version == 0 || decoded.known_version == Some(version) {
                // Nothing changed. The head returns nothing rather than the
                // same snapshot again, which is what makes a short fetch
                // interval affordable.
                return Ok(FetchPolicyResponse {
                    policy: None,
                    unchanged: true,
                });
            }
            Ok(FetchPolicyResponse {
                policy: Some(self.policy.snapshot_for(&source)),
                unchanged: false,
            })
        })();

        match outcome {
            Ok(response) => reply(
                "FetchPolicyResponse",
                encode_fetch_policy_response(&response),
            ),
            Err(e) => {
                self.log_failure("A collector did not get its collection policy.", &e);
                error_outcome(encode_collector_error(&crate::wire::to_collector_error(&e)))
            }
        }
    }

    /// Set a project's attribution weights and windows. D40.
    ///
    /// Setting them is an administrator's act, because a weight change moves
    /// every campaign report the project produces.
    fn put_attribution_settings(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{
            decode_attribution_settings, encode_attribution_settings,
        };
        let decoded = match decode_attribution_settings(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Admin)?;
            let stored = self.attribution.put(
                project_id,
                crate::attribution::from_wire(&decoded),
                &who.subject,
            )?;
            Ok::<_, TallyOwlError>(crate::attribution::to_wire(project_id, &stored))
        })();
        match outcome {
            Ok(settings) => {
                self.logger.info(
                    "Changed a project's attribution settings. Every later result recomputes and no stored fact moved.",
                    &[(
                        "settings_version",
                        &settings.settings_version.unwrap_or(0).to_string(),
                    )],
                );
                reply(
                    "AttributionSettings",
                    encode_attribution_settings(&settings),
                )
            }
            Err(e) => {
                self.log_failure("Attribution settings were not stored.", &e);
                error_outcome(encode_control_error(&crate::wire::to_control_error(&e)))
            }
        }
    }

    fn get_attribution_settings(&self, request: &Request) -> Outcome {
        use tallyowl_control_api::codec::{decode_saved_request, encode_attribution_settings};
        let decoded = match decode_saved_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };
        let outcome = (|| {
            let who = self.control.signed_in(request.auth.as_deref())?;
            let project_id = to_project(&decoded.project_id)?;
            self.control
                .allow_project(&who, project_id, tallyowl_store::control::Role::Viewer)?;
            Ok::<_, TallyOwlError>(crate::attribution::to_wire(
                project_id,
                &self.attribution.settings(project_id),
            ))
        })();
        match outcome {
            Ok(settings) => reply(
                "AttributionSettings",
                encode_attribution_settings(&settings),
            ),
            Err(e) => error_outcome(encode_control_error(&crate::wire::to_control_error(&e))),
        }
    }
}

fn to_project(bytes: &[u8]) -> Result<[u8; 16], TallyOwlError> {
    bytes.try_into().map_err(|_| {
        TallyOwlError::invalid_argument(format!(
            "A project identifier is 16 bytes and this one is {}.",
            bytes.len()
        ))
    })
}

/// A scope identifier that is a project, when it is one.
///
/// A policy at the workspace, environment, or source level names something that
/// is not a project, so there is no project role to check. The installation
/// level names nothing at all.
fn project_from_text(text: &str) -> Option<[u8; 16]> {
    tallyowl_store::row::from_hex(text)?.try_into().ok()
}
