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
        ("TallyOwlIngest", "capture") => match variant {
            Some("CaptureResponse") => Ok(payload),
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
        ("TallyOwlIngest", "capture-critical") => match variant {
            Some("CaptureCriticalResponse") => Ok(payload),
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
        ("TallyOwlIngest", "policy-version") => match variant {
            Some("PolicyVersionResponse") => Ok(payload),
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
        ("TallyOwlAlertReceiver", "notify") => match variant {
            Some("AlertNotifyResponse") => Ok(payload),
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

/// Typed client for the TallyOwlIngest service.
pub struct TallyOwlIngestClient<T: Transport> {
    #[allow(dead_code)]
    transport: T,
}

impl<T: Transport> TallyOwlIngestClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Accept telemetry on the application's existing connection. Best effort.
    pub fn capture(&self, req: CaptureRequest) -> Result<CaptureResponse, ClientError> {
        let csil_req = match encode_capture_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlIngest", "capture", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlIngest",
            "capture",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_capture_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Accept telemetry and wait for the collector durability boundary.
    pub fn capture_critical(
        &self,
        req: CaptureCriticalRequest,
    ) -> Result<CaptureCriticalResponse, ClientError> {
        let csil_req = match encode_capture_critical_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlIngest", "capture-critical", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlIngest",
            "capture-critical",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_capture_critical_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Report the collection policy that the client must apply.
    pub fn policy_version(
        &self,
        req: PolicyVersionRequest,
    ) -> Result<PolicyVersionResponse, ClientError> {
        let csil_req = match encode_policy_version_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply =
            self.transport
                .call_reply("TallyOwlIngest", "policy-version", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlIngest",
            "policy-version",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_policy_version_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }
}

/// Typed client for the TallyOwlAlertReceiver service.
pub struct TallyOwlAlertReceiverClient<T: Transport> {
    #[allow(dead_code)]
    transport: T,
}

impl<T: Transport> TallyOwlAlertReceiverClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Take one signed alert notification.
    pub fn notify(&self, req: AlertNotifyRequest) -> Result<AlertNotifyResponse, ClientError> {
        let csil_req = match encode_alert_notify_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlAlertReceiver", "notify", &csil_req)?;
        let csil_resp = resolve_rpc_reply(
            "TallyOwlAlertReceiver",
            "notify",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_alert_notify_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }
}
