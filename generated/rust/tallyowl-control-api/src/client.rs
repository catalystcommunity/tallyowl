//! Generated transport-agnostic service clients from CSIL specification

use super::codec::*;
use super::types::*;

/// Error from a generated client call: a structured error the service returned,
/// or a transport-level failure. The caller-supplied `Transport` decides how an
/// error response maps onto `Service`. `Encode` means the request has no valid
/// wire form, so the client did not call the transport.
#[derive(Debug, Clone)]
pub enum ClientError {
    Service { code: i64, message: String },
    Transport(String),
    Encode(String),
    ApplicationServiceError(Box<ServiceError>),
    Decode(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Service { code, message } => write!(f, "service error {code}: {message}"),
            Self::Encode(msg) => write!(f, "invalid request: {msg}"),
            Self::ApplicationServiceError(e) => write!(f, "application error: {e:?}"),
            Self::Decode(msg) => write!(f, "invalid reply: {msg}"),
            ClientError::Transport(msg) => write!(f, "transport error: {msg}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// A status-zero CSIL-RPC reply: `variant` names the declared output arm that
/// `payload` encodes. A transport returns it from `call_reply`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireReply {
    pub variant: Option<String>,
    pub payload: Vec<u8>,
}

/// The caller-supplied byte carrier: it performs the call named by `(service, op)`
/// with the already-encoded request bytes and returns the response bytes, or an
/// error. The generated client owns (de)serialization via the codec; the carrier
/// only moves bytes, so it can be HTTP, a queue, or an in-process loop.
pub trait Transport {
    fn call(&self, service: &str, op: &str, req: &[u8]) -> Result<Vec<u8>, ClientError>;

    /// Perform a call whose operation declares several reply arms and return
    /// the status-zero reply `variant` with its payload, so the client can
    /// return the success arm or the declared error arm.
    fn call_reply(&self, service: &str, op: &str, req: &[u8]) -> Result<WireReply, ClientError>;
}
impl From<CsilCborError> for ClientError {
    fn from(error: CsilCborError) -> Self {
        Self::Decode(error.0)
    }
}

/// Resolve a status-zero RPC reply into its success payload or its declared error
/// arm. Generated clients call it with the reply from `Transport::call_reply`.
pub fn resolve_rpc_reply(
    service: &str,
    op: &str,
    variant: Option<&str>,
    payload: Vec<u8>,
) -> Result<Vec<u8>, ClientError> {
    match (service, op) {
        ("TallyOwlControl", "run-query") => match variant {
            Some("QueryResponse") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "list-workspaces") => match variant {
            Some("WorkspaceList") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "list-projects") => match variant {
            Some("ProjectList") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "list-api-keys") => match variant {
            Some("ApiKeyList") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "put-alert-rule") => match variant {
            Some("AlertRule") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "list-alert-rules") => match variant {
            Some("AlertRuleList") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "list-alert-instances") => match variant {
            Some("AlertInstanceList") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "request-deletion") => match variant {
            Some("DeletionResponse") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "get-policy") => match variant {
            Some("CompiledPolicy") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "begin-login") => match variant {
            Some("BeginLoginResponse") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "complete-login") => match variant {
            Some("CompleteLoginResponse") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "create-role-token") => match variant {
            Some("CreateRoleTokenResponse") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "list-role-tokens") => match variant {
            Some("RoleTokenList") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "revoke-role-token") => match variant {
            Some("Empty") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "enroll-node") => match variant {
            Some("EnrollNodeResponse") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "renew-node-certificate") => match variant {
            Some("EnrollNodeResponse") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "list-nodes") => match variant {
            Some("NodeList") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "put-policy") => match variant {
            Some("CompiledPolicy") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "put-analysis") => match variant {
            Some("SavedAnalysis") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "list-analyses") => match variant {
            Some("SavedAnalysisList") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "delete-analysis") => match variant {
            Some("Empty") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "put-dashboard") => match variant {
            Some("SavedDashboard") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "list-dashboards") => match variant {
            Some("SavedDashboardList") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "put-attribution-settings") => match variant {
            Some("AttributionSettings") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "get-attribution-settings") => match variant {
            Some("AttributionSettings") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "delete-dashboard") => match variant {
            Some("Empty") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "delete-alert-rule") => match variant {
            Some("Empty") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "silence-alert") => match variant {
            Some("AlertInstance") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "resolve-alert") => match variant {
            Some("AlertInstance") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "list-workflows") => match variant {
            Some("WorkflowList") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "run-workflow") => match variant {
            Some("RunWorkflowResponse") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "list-notifications") => match variant {
            Some("NotificationDeliveryList") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "get-user-preferences") => match variant {
            Some("UserPreferences") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        ("TallyOwlControl", "put-user-preferences") => match variant {
            Some("UserPreferences") => Ok(payload),
            Some("ServiceError") => {
                let value = decode_service_error(&payload)?;
                let value = Box::new(value);
                let error = ClientError::ApplicationServiceError(value);
                Err(error)
            }
            _ => Err(ClientError::Decode(format!(
                "invalid reply variant {variant:?}"
            ))),
        },
        _ => Ok(payload),
    }
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
        let csil_req = match encode_query_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "run-query", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "run-query",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_query_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List the workspaces that the session can read.
    pub fn list_workspaces(&self, req: ListRequest) -> Result<WorkspaceList, ClientError> {
        let csil_req = match encode_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "list-workspaces", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "list-workspaces",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_workspace_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List the projects that the session can read.
    pub fn list_projects(&self, req: ListRequest) -> Result<ProjectList, ClientError> {
        let csil_req = match encode_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "list-projects", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "list-projects",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_project_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List the active keys for a project.
    pub fn list_api_keys(&self, req: ListRequest) -> Result<ApiKeyList, ClientError> {
        let csil_req = match encode_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "list-api-keys", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "list-api-keys",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_api_key_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Create or replace an alert rule.
    pub fn put_alert_rule(&self, req: AlertRule) -> Result<AlertRule, ClientError> {
        let csil_req = match encode_alert_rule(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "put-alert-rule", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "put-alert-rule",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_alert_rule(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List alert rules.
    pub fn list_alert_rules(&self, req: AlertListRequest) -> Result<AlertRuleList, ClientError> {
        let csil_req = match encode_alert_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "list-alert-rules", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "list-alert-rules",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_alert_rule_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List current alert instances and their state.
    pub fn list_alert_instances(
        &self,
        req: AlertListRequest,
    ) -> Result<AlertInstanceList, ClientError> {
        let csil_req = match encode_alert_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "list-alert-instances", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "list-alert-instances",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_alert_instance_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Start an erasure. The tombstone applies immediately and stays active
    /// for late arrivals until its horizon ends. See D28.
    pub fn request_deletion(&self, req: DeletionRequest) -> Result<DeletionResponse, ClientError> {
        let csil_req = match encode_deletion_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "request-deletion", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "request-deletion",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_deletion_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Fetch the compiled collection policy for a source.
    pub fn get_policy(&self, req: PolicyRequest) -> Result<CompiledPolicy, ClientError> {
        let csil_req = match encode_policy_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "get-policy", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "get-policy",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_compiled_policy(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Start a sign-in. Returns the URL to send the browser to.
    pub fn begin_login(&self, req: BeginLoginRequest) -> Result<BeginLoginResponse, ClientError> {
        let csil_req = match encode_begin_login_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "begin-login", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "begin-login",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_begin_login_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Finish a sign-in with what the callback carried, and get a session.
    pub fn complete_login(
        &self,
        req: CompleteLoginRequest,
    ) -> Result<CompleteLoginResponse, ClientError> {
        let csil_req = match encode_complete_login_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "complete-login", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "complete-login",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_complete_login_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Create a reusable role token. The token text is returned once and is
    /// never recoverable afterwards.
    pub fn create_role_token(
        &self,
        req: CreateRoleTokenRequest,
    ) -> Result<CreateRoleTokenResponse, ClientError> {
        let csil_req = match encode_create_role_token_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "create-role-token", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "create-role-token",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_create_role_token_response(&csil_resp)
            .map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List role tokens and what each one has enrolled.
    pub fn list_role_tokens(&self, req: ListRequest) -> Result<RoleTokenList, ClientError> {
        let csil_req = match encode_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "list-role-tokens", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "list-role-tokens",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_role_token_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Stop a token enrolling anything more.
    pub fn revoke_role_token(&self, req: RevokeRoleTokenRequest) -> Result<Empty, ClientError> {
        let csil_req = match encode_revoke_role_token_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "revoke-role-token", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "revoke-role-token",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_empty(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Enroll a node. The node generates its private key and sends only a
    /// certificate request; the control plane signs and never sees the key.
    pub fn enroll_node(&self, req: EnrollNodeRequest) -> Result<EnrollNodeResponse, ClientError> {
        let csil_req = match encode_enroll_node_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "enroll-node", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "enroll-node",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_enroll_node_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Renew a node certificate against the node's current identity.
    pub fn renew_node_certificate(
        &self,
        req: RenewNodeCertificateRequest,
    ) -> Result<EnrollNodeResponse, ClientError> {
        let csil_req = match encode_renew_node_certificate_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "renew-node-certificate", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "renew-node-certificate",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_enroll_node_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List enrolled nodes.
    pub fn list_nodes(&self, req: ListRequest) -> Result<NodeList, ClientError> {
        let csil_req = match encode_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "list-nodes", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "list-nodes",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_node_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Set one level of collection policy. It compiles before it is stored,
    /// because invalid policy never replaces valid policy.
    pub fn put_policy(&self, req: PolicyDocument) -> Result<CompiledPolicy, ClientError> {
        let csil_req = match encode_policy_document(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "put-policy", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "put-policy",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_compiled_policy(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Save an analysis, or replace it.
    pub fn put_analysis(&self, req: SavedAnalysis) -> Result<SavedAnalysis, ClientError> {
        let csil_req = match encode_saved_analysis(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "put-analysis", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "put-analysis",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_saved_analysis(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List the saved analyses of a project.
    pub fn list_analyses(&self, req: SavedRequest) -> Result<SavedAnalysisList, ClientError> {
        let csil_req = match encode_saved_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "list-analyses", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "list-analyses",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_saved_analysis_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Remove a saved analysis. Refused while a dashboard shows it.
    pub fn delete_analysis(&self, req: SavedRequest) -> Result<Empty, ClientError> {
        let csil_req = match encode_saved_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "delete-analysis", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "delete-analysis",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_empty(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Save a dashboard, or replace it.
    pub fn put_dashboard(&self, req: SavedDashboard) -> Result<SavedDashboard, ClientError> {
        let csil_req = match encode_saved_dashboard(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "put-dashboard", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "put-dashboard",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_saved_dashboard(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// List the dashboards of a project.
    pub fn list_dashboards(&self, req: SavedRequest) -> Result<SavedDashboardList, ClientError> {
        let csil_req = match encode_saved_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "list-dashboards", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "list-dashboards",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_saved_dashboard_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Set the attribution weights and windows of a project. A change
    /// recomputes every later result and rewrites no stored fact. See D40.
    pub fn put_attribution_settings(
        &self,
        req: AttributionSettings,
    ) -> Result<AttributionSettings, ClientError> {
        let csil_req = match encode_attribution_settings(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "put-attribution-settings", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "put-attribution-settings",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_attribution_settings(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Read the attribution settings a project answers under.
    pub fn get_attribution_settings(
        &self,
        req: SavedRequest,
    ) -> Result<AttributionSettings, ClientError> {
        let csil_req = match encode_saved_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "get-attribution-settings", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
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
    pub fn delete_dashboard(&self, req: SavedRequest) -> Result<Empty, ClientError> {
        let csil_req = match encode_saved_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "delete-dashboard", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "delete-dashboard",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_empty(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Remove an alert rule and its instance.
    pub fn delete_alert_rule(&self, req: DeleteAlertRuleRequest) -> Result<Empty, ClientError> {
        let csil_req = match encode_delete_alert_rule_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "delete-alert-rule", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "delete-alert-rule",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_empty(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Suppress notification for a period. The evaluation still runs and still
    /// records state, so an operator can see what happened while it was quiet.
    pub fn silence_alert(&self, req: SilenceRequest) -> Result<AlertInstance, ClientError> {
        let csil_req = match encode_silence_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "silence-alert", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "silence-alert",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_alert_instance(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Put a firing rule back to `ok` by hand and send the resolution.
    pub fn resolve_alert(&self, req: ResolveRequest) -> Result<AlertInstance, ClientError> {
        let csil_req = match encode_resolve_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "resolve-alert", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "resolve-alert",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_alert_instance(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// What every workflow is doing: its lag, its failures, and what is in
    /// quarantine waiting for a person.
    pub fn list_workflows(&self, req: ListRequest) -> Result<WorkflowList, ClientError> {
        let csil_req = match encode_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "list-workflows", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "list-workflows",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_workflow_list(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Start one workflow now rather than at its schedule.
    pub fn run_workflow(
        &self,
        req: RunWorkflowRequest,
    ) -> Result<RunWorkflowResponse, ClientError> {
        let csil_req = match encode_run_workflow_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlControl", "run-workflow", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "run-workflow",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_run_workflow_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Every notification attempt, delivered or not.
    pub fn list_notifications(
        &self,
        req: AlertListRequest,
    ) -> Result<NotificationDeliveryList, ClientError> {
        let csil_req = match encode_alert_list_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "list-notifications", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
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
    pub fn get_user_preferences(&self, req: Empty) -> Result<UserPreferences, ClientError> {
        let csil_req = match encode_empty(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "get-user-preferences", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "get-user-preferences",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_user_preferences(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Replace the preferences of the signed-in person. Each person can do
    /// this, with or without a role in a workspace.
    pub fn put_user_preferences(
        &self,
        req: UserPreferences,
    ) -> Result<UserPreferences, ClientError> {
        let csil_req = match encode_user_preferences(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlControl", "put-user-preferences", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlControl",
            "put-user-preferences",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_user_preferences(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }
}
