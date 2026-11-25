//! Generated transport-agnostic service clients from CSIL specification

#![allow(async_fn_in_trait)]

use super::client::ClientError;
use super::codec::*;
use super::types::*;

/// The caller-supplied byte carrier: it performs the call named by `(service, op)`
/// with the already-encoded request bytes and returns the response bytes, or an
/// error. The generated client owns (de)serialization via the codec; the carrier
/// only moves bytes, so it can be HTTP, a queue, or an in-process loop.
pub trait AsyncTransport {
    async fn call(&self, service: &str, op: &str, req: &[u8]) -> Result<Vec<u8>, ClientError>;

    /// Perform a call whose operation declares several reply arms and return
    /// the status-zero reply `variant` with its payload, so the client can
    /// return the success arm or the declared error arm.
    async fn call_reply(
        &self,
        service: &str,
        op: &str,
        req: &[u8],
    ) -> Result<super::client::WireReply, ClientError>;
}

/// Typed client for the TallyOwlControl service.
pub struct TallyOwlControlAsyncClient<T: AsyncTransport> {
    #[allow(dead_code)]
    transport: T,
}

impl<T: AsyncTransport> TallyOwlControlAsyncClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Run one analytic query.
    pub async fn run_query(&self, req: QueryRequest) -> Result<QueryResponse, ClientError> {
        let csil_req = match encode_query_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "run-query", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "run-query",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_query_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List the workspaces that the session can read.
    pub async fn list_workspaces(&self, req: ListRequest) -> Result<WorkspaceList, ClientError> {
        let csil_req = match encode_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "list-workspaces", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "list-workspaces",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_workspace_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List the projects that the session can read.
    pub async fn list_projects(&self, req: ListRequest) -> Result<ProjectList, ClientError> {
        let csil_req = match encode_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "list-projects", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "list-projects",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_project_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List the active keys for a project.
    pub async fn list_api_keys(&self, req: ListRequest) -> Result<ApiKeyList, ClientError> {
        let csil_req = match encode_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "list-api-keys", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "list-api-keys",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_api_key_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Create or replace an alert rule.
    pub async fn put_alert_rule(&self, req: AlertRule) -> Result<AlertRule, ClientError> {
        let csil_req = match encode_alert_rule(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "put-alert-rule", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "put-alert-rule",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_alert_rule(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List alert rules.
    pub async fn list_alert_rules(
        &self,
        req: AlertListRequest,
    ) -> Result<AlertRuleList, ClientError> {
        let csil_req = match encode_alert_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "list-alert-rules", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "list-alert-rules",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_alert_rule_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List current alert instances and their state.
    pub async fn list_alert_instances(
        &self,
        req: AlertListRequest,
    ) -> Result<AlertInstanceList, ClientError> {
        let csil_req = match encode_alert_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "list-alert-instances", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "list-alert-instances",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_alert_instance_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Start an erasure. The tombstone applies immediately and stays active
    /// for late arrivals until its horizon ends. See D28.
    pub async fn request_deletion(
        &self,
        req: DeletionRequest,
    ) -> Result<DeletionResponse, ClientError> {
        let csil_req = match encode_deletion_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "request-deletion", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "request-deletion",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_deletion_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Fetch the compiled collection policy for a source.
    pub async fn get_policy(&self, req: PolicyRequest) -> Result<CompiledPolicy, ClientError> {
        let csil_req = match encode_policy_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "get-policy", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "get-policy",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_compiled_policy(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Start a sign-in. Returns the URL to send the browser to.
    pub async fn begin_login(
        &self,
        req: BeginLoginRequest,
    ) -> Result<BeginLoginResponse, ClientError> {
        let csil_req = match encode_begin_login_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "begin-login", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "begin-login",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_begin_login_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Finish a sign-in with what the callback carried, and get a session.
    pub async fn complete_login(
        &self,
        req: CompleteLoginRequest,
    ) -> Result<CompleteLoginResponse, ClientError> {
        let csil_req = match encode_complete_login_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "complete-login", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "complete-login",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_complete_login_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Create a reusable role token. The token text is returned once and is
    /// never recoverable afterwards.
    pub async fn create_role_token(
        &self,
        req: CreateRoleTokenRequest,
    ) -> Result<CreateRoleTokenResponse, ClientError> {
        let csil_req = match encode_create_role_token_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "create-role-token", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "create-role-token",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_create_role_token_response(&csil_resp)
            .map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List role tokens and what each one has enrolled.
    pub async fn list_role_tokens(&self, req: ListRequest) -> Result<RoleTokenList, ClientError> {
        let csil_req = match encode_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "list-role-tokens", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "list-role-tokens",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_role_token_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Stop a token enrolling anything more.
    pub async fn revoke_role_token(
        &self,
        req: RevokeRoleTokenRequest,
    ) -> Result<Empty, ClientError> {
        let csil_req = match encode_revoke_role_token_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "revoke-role-token", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "revoke-role-token",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_empty(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Enroll a node. The node generates its private key and sends only a
    /// certificate request; the control plane signs and never sees the key.
    pub async fn enroll_node(
        &self,
        req: EnrollNodeRequest,
    ) -> Result<EnrollNodeResponse, ClientError> {
        let csil_req = match encode_enroll_node_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "enroll-node", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "enroll-node",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_enroll_node_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Renew a node certificate against the node's current identity.
    pub async fn renew_node_certificate(
        &self,
        req: RenewNodeCertificateRequest,
    ) -> Result<EnrollNodeResponse, ClientError> {
        let csil_req = match encode_renew_node_certificate_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "renew-node-certificate", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "renew-node-certificate",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_enroll_node_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List enrolled nodes.
    pub async fn list_nodes(&self, req: ListRequest) -> Result<NodeList, ClientError> {
        let csil_req = match encode_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "list-nodes", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "list-nodes",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_node_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Set one level of collection policy. It compiles before it is stored,
    /// because invalid policy never replaces valid policy.
    pub async fn put_policy(&self, req: PolicyDocument) -> Result<CompiledPolicy, ClientError> {
        let csil_req = match encode_policy_document(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "put-policy", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "put-policy",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_compiled_policy(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Save an analysis, or replace it.
    pub async fn put_analysis(&self, req: SavedAnalysis) -> Result<SavedAnalysis, ClientError> {
        let csil_req = match encode_saved_analysis(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "put-analysis", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "put-analysis",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_saved_analysis(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List the saved analyses of a project.
    pub async fn list_analyses(&self, req: SavedRequest) -> Result<SavedAnalysisList, ClientError> {
        let csil_req = match encode_saved_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "list-analyses", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "list-analyses",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_saved_analysis_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Remove a saved analysis. Refused while a dashboard shows it.
    pub async fn delete_analysis(&self, req: SavedRequest) -> Result<Empty, ClientError> {
        let csil_req = match encode_saved_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "delete-analysis", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "delete-analysis",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_empty(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Save a dashboard, or replace it.
    pub async fn put_dashboard(&self, req: SavedDashboard) -> Result<SavedDashboard, ClientError> {
        let csil_req = match encode_saved_dashboard(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "put-dashboard", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "put-dashboard",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_saved_dashboard(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List the dashboards of a project.
    pub async fn list_dashboards(
        &self,
        req: SavedRequest,
    ) -> Result<SavedDashboardList, ClientError> {
        let csil_req = match encode_saved_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "list-dashboards", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "list-dashboards",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_saved_dashboard_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Set the attribution weights and windows of a project. A change
    /// recomputes every later result and rewrites no stored fact. See D40.
    pub async fn put_attribution_settings(
        &self,
        req: AttributionSettings,
    ) -> Result<AttributionSettings, ClientError> {
        let csil_req = match encode_attribution_settings(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "put-attribution-settings", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "put-attribution-settings",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_attribution_settings(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Read the attribution settings a project answers under.
    pub async fn get_attribution_settings(
        &self,
        req: SavedRequest,
    ) -> Result<AttributionSettings, ClientError> {
        let csil_req = match encode_saved_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "get-attribution-settings", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "get-attribution-settings",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_attribution_settings(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Remove a dashboard. The analyses it showed stay, because an analysis is
    /// a question somebody saved and a dashboard is only one arrangement of
    /// several. Removing an analysis is `delete-analysis`, and it is refused
    /// while a dashboard still shows it.
    pub async fn delete_dashboard(&self, req: SavedRequest) -> Result<Empty, ClientError> {
        let csil_req = match encode_saved_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "delete-dashboard", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "delete-dashboard",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_empty(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Remove an alert rule and its instance.
    pub async fn delete_alert_rule(
        &self,
        req: DeleteAlertRuleRequest,
    ) -> Result<Empty, ClientError> {
        let csil_req = match encode_delete_alert_rule_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "delete-alert-rule", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "delete-alert-rule",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_empty(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Suppress notification for a period. The evaluation still runs and still
    /// records state, so an operator can see what happened while it was quiet.
    pub async fn silence_alert(&self, req: SilenceRequest) -> Result<AlertInstance, ClientError> {
        let csil_req = match encode_silence_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "silence-alert", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "silence-alert",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_alert_instance(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Put a firing rule back to `ok` by hand and send the resolution.
    pub async fn resolve_alert(&self, req: ResolveRequest) -> Result<AlertInstance, ClientError> {
        let csil_req = match encode_resolve_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "resolve-alert", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "resolve-alert",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_alert_instance(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// What every workflow is doing: its lag, its failures, and what is in
    /// quarantine waiting for a person.
    pub async fn list_workflows(&self, req: ListRequest) -> Result<WorkflowList, ClientError> {
        let csil_req = match encode_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "list-workflows", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "list-workflows",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_workflow_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Start one workflow now rather than at its schedule.
    pub async fn run_workflow(
        &self,
        req: RunWorkflowRequest,
    ) -> Result<RunWorkflowResponse, ClientError> {
        let csil_req = match encode_run_workflow_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "run-workflow", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "run-workflow",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_run_workflow_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Every notification attempt, delivered or not.
    pub async fn list_notifications(
        &self,
        req: AlertListRequest,
    ) -> Result<NotificationDeliveryList, ClientError> {
        let csil_req = match encode_alert_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "list-notifications", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "list-notifications",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_notification_delivery_list(&csil_resp)
            .map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Read the preferences of the signed-in person. A person who has never
    /// set them gets the defaults: the time of the browser.
    pub async fn get_user_preferences(&self, req: Empty) -> Result<UserPreferences, ClientError> {
        let csil_req = match encode_empty(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "get-user-preferences", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "get-user-preferences",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_user_preferences(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Replace the preferences of the signed-in person. Each person can do
    /// this, with or without a role in a workspace.
    pub async fn put_user_preferences(
        &self,
        req: UserPreferences,
    ) -> Result<UserPreferences, ClientError> {
        let csil_req = match encode_user_preferences(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "put-user-preferences", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlControl",
            "put-user-preferences",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_user_preferences(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }
}
