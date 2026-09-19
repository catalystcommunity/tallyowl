//! Generated transport-agnostic service clients from CSIL specification

use super::codec::*;
use super::types::*;

/// Error from a generated client call: a structured error the service returned,
/// or a transport-level failure. The caller-supplied `Transport` decides how an
/// error response maps onto `Service`.
#[derive(Debug, Clone)]
pub enum ClientError {
    Service { code: i64, message: String },
    Transport(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Service { code, message } => write!(f, "service error {code}: {message}"),
            ClientError::Transport(msg) => write!(f, "transport error: {msg}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// The caller-supplied byte carrier: it performs the call named by `(service, op)`
/// with the already-encoded request bytes and returns the response bytes, or an
/// error. The generated client owns (de)serialization via the codec; the carrier
/// only moves bytes, so it can be HTTP, a queue, or an in-process loop.
pub trait Transport {
    fn call(&self, service: &str, op: &str, req: &[u8]) -> Result<Vec<u8>, ClientError>;
}

/// Typed client for the TallyOwlControl service.
pub struct TallyOwlControlClient<T: Transport> {
    #[allow(dead_code)]
    transport: T,
}

impl<T: Transport> TallyOwlControlClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Run one analytic query.
    pub fn run_query(&self, req: QueryRequest) -> Result<QueryResponse, ClientError> {
        let csil_resp =
            self.transport
                .call("TallyOwlControl", "run-query", &encode_query_request(&req))?;
        decode_query_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// List the workspaces that the session can read.
    pub fn list_workspaces(&self, req: ListRequest) -> Result<WorkspaceList, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "list-workspaces",
            &encode_list_request(&req),
        )?;
        decode_workspace_list(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// List the projects that the session can read.
    pub fn list_projects(&self, req: ListRequest) -> Result<ProjectList, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "list-projects",
            &encode_list_request(&req),
        )?;
        decode_project_list(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// List the active keys for a project.
    pub fn list_api_keys(&self, req: ListRequest) -> Result<ApiKeyList, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "list-api-keys",
            &encode_list_request(&req),
        )?;
        decode_api_key_list(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Create or replace an alert rule.
    pub fn put_alert_rule(&self, req: AlertRule) -> Result<AlertRule, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "put-alert-rule",
            &encode_alert_rule(&req),
        )?;
        decode_alert_rule(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// List alert rules.
    pub fn list_alert_rules(&self, req: AlertListRequest) -> Result<AlertRuleList, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "list-alert-rules",
            &encode_alert_list_request(&req),
        )?;
        decode_alert_rule_list(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// List current alert instances and their state.
    pub fn list_alert_instances(
        &self,
        req: AlertListRequest,
    ) -> Result<AlertInstanceList, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "list-alert-instances",
            &encode_alert_list_request(&req),
        )?;
        decode_alert_instance_list(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Start an erasure. The tombstone applies immediately and stays active
    /// for late arrivals until its horizon ends. See D28.
    pub fn request_deletion(&self, req: DeletionRequest) -> Result<DeletionResponse, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "request-deletion",
            &encode_deletion_request(&req),
        )?;
        decode_deletion_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Fetch the compiled collection policy for a source.
    pub fn get_policy(&self, req: PolicyRequest) -> Result<CompiledPolicy, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "get-policy",
            &encode_policy_request(&req),
        )?;
        decode_compiled_policy(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Start a sign-in. Returns the URL to send the browser to.
    pub fn begin_login(&self, req: BeginLoginRequest) -> Result<BeginLoginResponse, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "begin-login",
            &encode_begin_login_request(&req),
        )?;
        decode_begin_login_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Finish a sign-in with what the callback carried, and get a session.
    pub fn complete_login(
        &self,
        req: CompleteLoginRequest,
    ) -> Result<CompleteLoginResponse, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "complete-login",
            &encode_complete_login_request(&req),
        )?;
        decode_complete_login_response(&csil_resp)
            .map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Create a reusable role token. The token text is returned once and is
    /// never recoverable afterwards.
    pub fn create_role_token(
        &self,
        req: CreateRoleTokenRequest,
    ) -> Result<CreateRoleTokenResponse, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "create-role-token",
            &encode_create_role_token_request(&req),
        )?;
        decode_create_role_token_response(&csil_resp)
            .map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// List role tokens and what each one has enrolled.
    pub fn list_role_tokens(&self, req: ListRequest) -> Result<RoleTokenList, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "list-role-tokens",
            &encode_list_request(&req),
        )?;
        decode_role_token_list(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Stop a token enrolling anything more.
    pub fn revoke_role_token(&self, req: RevokeRoleTokenRequest) -> Result<Empty, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "revoke-role-token",
            &encode_revoke_role_token_request(&req),
        )?;
        decode_empty(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Enroll a node. The node generates its private key and sends only a
    /// certificate request; the control plane signs and never sees the key.
    pub fn enroll_node(&self, req: EnrollNodeRequest) -> Result<EnrollNodeResponse, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "enroll-node",
            &encode_enroll_node_request(&req),
        )?;
        decode_enroll_node_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Renew a node certificate against the node's current identity.
    pub fn renew_node_certificate(
        &self,
        req: RenewNodeCertificateRequest,
    ) -> Result<EnrollNodeResponse, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "renew-node-certificate",
            &encode_renew_node_certificate_request(&req),
        )?;
        decode_enroll_node_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// List enrolled nodes.
    pub fn list_nodes(&self, req: ListRequest) -> Result<NodeList, ClientError> {
        let csil_resp =
            self.transport
                .call("TallyOwlControl", "list-nodes", &encode_list_request(&req))?;
        decode_node_list(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Set one level of collection policy. It compiles before it is stored,
    /// because invalid policy never replaces valid policy.
    pub fn put_policy(&self, req: PolicyDocument) -> Result<CompiledPolicy, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "put-policy",
            &encode_policy_document(&req),
        )?;
        decode_compiled_policy(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Save an analysis, or replace it.
    pub fn put_analysis(&self, req: SavedAnalysis) -> Result<SavedAnalysis, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "put-analysis",
            &encode_saved_analysis(&req),
        )?;
        decode_saved_analysis(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// List the saved analyses of a project.
    pub fn list_analyses(&self, req: SavedRequest) -> Result<SavedAnalysisList, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "list-analyses",
            &encode_saved_request(&req),
        )?;
        decode_saved_analysis_list(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Remove a saved analysis. Refused while a dashboard shows it.
    pub fn delete_analysis(&self, req: SavedRequest) -> Result<Empty, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "delete-analysis",
            &encode_saved_request(&req),
        )?;
        decode_empty(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Save a dashboard, or replace it.
    pub fn put_dashboard(&self, req: SavedDashboard) -> Result<SavedDashboard, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "put-dashboard",
            &encode_saved_dashboard(&req),
        )?;
        decode_saved_dashboard(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// List the dashboards of a project.
    pub fn list_dashboards(&self, req: SavedRequest) -> Result<SavedDashboardList, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "list-dashboards",
            &encode_saved_request(&req),
        )?;
        decode_saved_dashboard_list(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Set the attribution weights and windows of a project. A change
    /// recomputes every later result and rewrites no stored fact. See D40.
    pub fn put_attribution_settings(
        &self,
        req: AttributionSettings,
    ) -> Result<AttributionSettings, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "put-attribution-settings",
            &encode_attribution_settings(&req),
        )?;
        decode_attribution_settings(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Read the attribution settings a project answers under.
    pub fn get_attribution_settings(
        &self,
        req: SavedRequest,
    ) -> Result<AttributionSettings, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "get-attribution-settings",
            &encode_saved_request(&req),
        )?;
        decode_attribution_settings(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Remove a dashboard. The analyses it showed stay, because an analysis is
    /// a question somebody saved and a dashboard is only one arrangement of
    /// several. Removing an analysis is `delete-analysis`, and it is refused
    /// while a dashboard still shows it.
    pub fn delete_dashboard(&self, req: SavedRequest) -> Result<Empty, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "delete-dashboard",
            &encode_saved_request(&req),
        )?;
        decode_empty(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Remove an alert rule and its instance.
    pub fn delete_alert_rule(&self, req: DeleteAlertRuleRequest) -> Result<Empty, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "delete-alert-rule",
            &encode_delete_alert_rule_request(&req),
        )?;
        decode_empty(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Suppress notification for a period. The evaluation still runs and still
    /// records state, so an operator can see what happened while it was quiet.
    pub fn silence_alert(&self, req: SilenceRequest) -> Result<AlertInstance, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "silence-alert",
            &encode_silence_request(&req),
        )?;
        decode_alert_instance(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Put a firing rule back to `ok` by hand and send the resolution.
    pub fn resolve_alert(&self, req: ResolveRequest) -> Result<AlertInstance, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "resolve-alert",
            &encode_resolve_request(&req),
        )?;
        decode_alert_instance(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// What every workflow is doing: its lag, its failures, and what is in
    /// quarantine waiting for a person.
    pub fn list_workflows(&self, req: ListRequest) -> Result<WorkflowList, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "list-workflows",
            &encode_list_request(&req),
        )?;
        decode_workflow_list(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Start one workflow now rather than at its schedule.
    pub fn run_workflow(
        &self,
        req: RunWorkflowRequest,
    ) -> Result<RunWorkflowResponse, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "run-workflow",
            &encode_run_workflow_request(&req),
        )?;
        decode_run_workflow_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Every notification attempt, delivered or not.
    pub fn list_notifications(
        &self,
        req: AlertListRequest,
    ) -> Result<NotificationDeliveryList, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlControl",
            "list-notifications",
            &encode_alert_list_request(&req),
        )?;
        decode_notification_delivery_list(&csil_resp)
            .map_err(|e| ClientError::Transport(e.to_string()))
    }
}
