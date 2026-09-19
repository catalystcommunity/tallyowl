//! Generated service traits from CSIL specification

use super::types::*;

/// TallyOwlControl service trait
pub trait TallyOwlControl {
    type Context;
    /// Run one analytic query.
    fn run_query(
        &self,
        ctx: &Self::Context,
        input: QueryRequest,
    ) -> Result<QueryResponse, ServiceError>;
    /// List the workspaces that the session can read.
    fn list_workspaces(
        &self,
        ctx: &Self::Context,
        input: ListRequest,
    ) -> Result<WorkspaceList, ServiceError>;
    /// List the projects that the session can read.
    fn list_projects(
        &self,
        ctx: &Self::Context,
        input: ListRequest,
    ) -> Result<ProjectList, ServiceError>;
    /// List the active keys for a project.
    fn list_api_keys(
        &self,
        ctx: &Self::Context,
        input: ListRequest,
    ) -> Result<ApiKeyList, ServiceError>;
    /// Create or replace an alert rule.
    fn put_alert_rule(
        &self,
        ctx: &Self::Context,
        input: AlertRule,
    ) -> Result<AlertRule, ServiceError>;
    /// List alert rules.
    fn list_alert_rules(
        &self,
        ctx: &Self::Context,
        input: AlertListRequest,
    ) -> Result<AlertRuleList, ServiceError>;
    /// List current alert instances and their state.
    fn list_alert_instances(
        &self,
        ctx: &Self::Context,
        input: AlertListRequest,
    ) -> Result<AlertInstanceList, ServiceError>;
    /// Start an erasure. The tombstone applies immediately and stays active
    /// for late arrivals until its horizon ends. See D28.
    fn request_deletion(
        &self,
        ctx: &Self::Context,
        input: DeletionRequest,
    ) -> Result<DeletionResponse, ServiceError>;
    /// Fetch the compiled collection policy for a source.
    fn get_policy(
        &self,
        ctx: &Self::Context,
        input: PolicyRequest,
    ) -> Result<CompiledPolicy, ServiceError>;
    /// Start a sign-in. Returns the URL to send the browser to.
    fn begin_login(
        &self,
        ctx: &Self::Context,
        input: BeginLoginRequest,
    ) -> Result<BeginLoginResponse, ServiceError>;
    /// Finish a sign-in with what the callback carried, and get a session.
    fn complete_login(
        &self,
        ctx: &Self::Context,
        input: CompleteLoginRequest,
    ) -> Result<CompleteLoginResponse, ServiceError>;
    /// Create a reusable role token. The token text is returned once and is
    /// never recoverable afterwards.
    fn create_role_token(
        &self,
        ctx: &Self::Context,
        input: CreateRoleTokenRequest,
    ) -> Result<CreateRoleTokenResponse, ServiceError>;
    /// List role tokens and what each one has enrolled.
    fn list_role_tokens(
        &self,
        ctx: &Self::Context,
        input: ListRequest,
    ) -> Result<RoleTokenList, ServiceError>;
    /// Stop a token enrolling anything more.
    fn revoke_role_token(
        &self,
        ctx: &Self::Context,
        input: RevokeRoleTokenRequest,
    ) -> Result<Empty, ServiceError>;
    /// Enroll a node. The node generates its private key and sends only a
    /// certificate request; the control plane signs and never sees the key.
    fn enroll_node(
        &self,
        ctx: &Self::Context,
        input: EnrollNodeRequest,
    ) -> Result<EnrollNodeResponse, ServiceError>;
    /// Renew a node certificate against the node's current identity.
    fn renew_node_certificate(
        &self,
        ctx: &Self::Context,
        input: RenewNodeCertificateRequest,
    ) -> Result<EnrollNodeResponse, ServiceError>;
    /// List enrolled nodes.
    fn list_nodes(&self, ctx: &Self::Context, input: ListRequest)
        -> Result<NodeList, ServiceError>;
    /// Set one level of collection policy. It compiles before it is stored,
    /// because invalid policy never replaces valid policy.
    fn put_policy(
        &self,
        ctx: &Self::Context,
        input: PolicyDocument,
    ) -> Result<CompiledPolicy, ServiceError>;
    /// Save an analysis, or replace it.
    fn put_analysis(
        &self,
        ctx: &Self::Context,
        input: SavedAnalysis,
    ) -> Result<SavedAnalysis, ServiceError>;
    /// List the saved analyses of a project.
    fn list_analyses(
        &self,
        ctx: &Self::Context,
        input: SavedRequest,
    ) -> Result<SavedAnalysisList, ServiceError>;
    /// Remove a saved analysis. Refused while a dashboard shows it.
    fn delete_analysis(
        &self,
        ctx: &Self::Context,
        input: SavedRequest,
    ) -> Result<Empty, ServiceError>;
    /// Save a dashboard, or replace it.
    fn put_dashboard(
        &self,
        ctx: &Self::Context,
        input: SavedDashboard,
    ) -> Result<SavedDashboard, ServiceError>;
    /// List the dashboards of a project.
    fn list_dashboards(
        &self,
        ctx: &Self::Context,
        input: SavedRequest,
    ) -> Result<SavedDashboardList, ServiceError>;
    /// Set the attribution weights and windows of a project. A change
    /// recomputes every later result and rewrites no stored fact. See D40.
    fn put_attribution_settings(
        &self,
        ctx: &Self::Context,
        input: AttributionSettings,
    ) -> Result<AttributionSettings, ServiceError>;
    /// Read the attribution settings a project answers under.
    fn get_attribution_settings(
        &self,
        ctx: &Self::Context,
        input: SavedRequest,
    ) -> Result<AttributionSettings, ServiceError>;
    /// Remove a dashboard. The analyses it showed stay, because an analysis is
    /// a question somebody saved and a dashboard is only one arrangement of
    /// several. Removing an analysis is `delete-analysis`, and it is refused
    /// while a dashboard still shows it.
    fn delete_dashboard(
        &self,
        ctx: &Self::Context,
        input: SavedRequest,
    ) -> Result<Empty, ServiceError>;
    /// Remove an alert rule and its instance.
    fn delete_alert_rule(
        &self,
        ctx: &Self::Context,
        input: DeleteAlertRuleRequest,
    ) -> Result<Empty, ServiceError>;
    /// Suppress notification for a period. The evaluation still runs and still
    /// records state, so an operator can see what happened while it was quiet.
    fn silence_alert(
        &self,
        ctx: &Self::Context,
        input: SilenceRequest,
    ) -> Result<AlertInstance, ServiceError>;
    /// Put a firing rule back to `ok` by hand and send the resolution.
    fn resolve_alert(
        &self,
        ctx: &Self::Context,
        input: ResolveRequest,
    ) -> Result<AlertInstance, ServiceError>;
    /// What every workflow is doing: its lag, its failures, and what is in
    /// quarantine waiting for a person.
    fn list_workflows(
        &self,
        ctx: &Self::Context,
        input: ListRequest,
    ) -> Result<WorkflowList, ServiceError>;
    /// Start one workflow now rather than at its schedule.
    fn run_workflow(
        &self,
        ctx: &Self::Context,
        input: RunWorkflowRequest,
    ) -> Result<RunWorkflowResponse, ServiceError>;
    /// Every notification attempt, delivered or not.
    fn list_notifications(
        &self,
        ctx: &Self::Context,
        input: AlertListRequest,
    ) -> Result<NotificationDeliveryList, ServiceError>;
}

/// Wire-id ordinals for the TallyOwlControl service (transport compact profiles).
pub mod tally_owl_control_wire_ids {
    pub const SERVICE: u64 = 3;
    pub const OP_RUN_QUERY: u64 = 0;
    pub const OP_LIST_WORKSPACES: u64 = 1;
    pub const OP_LIST_PROJECTS: u64 = 2;
    pub const OP_LIST_API_KEYS: u64 = 3;
    pub const OP_PUT_ALERT_RULE: u64 = 4;
    pub const OP_LIST_ALERT_RULES: u64 = 5;
    pub const OP_LIST_ALERT_INSTANCES: u64 = 6;
    pub const OP_REQUEST_DELETION: u64 = 7;
    pub const OP_GET_POLICY: u64 = 8;
    pub const OP_BEGIN_LOGIN: u64 = 9;
    pub const OP_COMPLETE_LOGIN: u64 = 10;
    pub const OP_CREATE_ROLE_TOKEN: u64 = 11;
    pub const OP_LIST_ROLE_TOKENS: u64 = 12;
    pub const OP_REVOKE_ROLE_TOKEN: u64 = 13;
    pub const OP_ENROLL_NODE: u64 = 14;
    pub const OP_RENEW_NODE_CERTIFICATE: u64 = 15;
    pub const OP_LIST_NODES: u64 = 16;
    pub const OP_PUT_POLICY: u64 = 17;
    pub const OP_PUT_ANALYSIS: u64 = 18;
    pub const OP_LIST_ANALYSES: u64 = 19;
    pub const OP_DELETE_ANALYSIS: u64 = 20;
    pub const OP_PUT_DASHBOARD: u64 = 21;
    pub const OP_LIST_DASHBOARDS: u64 = 22;
    pub const OP_PUT_ATTRIBUTION_SETTINGS: u64 = 23;
    pub const OP_GET_ATTRIBUTION_SETTINGS: u64 = 24;
    pub const OP_DELETE_DASHBOARD: u64 = 25;
    pub const OP_DELETE_ALERT_RULE: u64 = 26;
    pub const OP_SILENCE_ALERT: u64 = 27;
    pub const OP_RESOLVE_ALERT: u64 = 28;
    pub const OP_LIST_WORKFLOWS: u64 = 29;
    pub const OP_RUN_WORKFLOW: u64 = 30;
    pub const OP_LIST_NOTIFICATIONS: u64 = 31;
}
