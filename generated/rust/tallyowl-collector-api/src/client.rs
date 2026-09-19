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

/// Typed client for the TallyOwlCollector service.
pub struct TallyOwlCollectorClient<T: Transport> {
    #[allow(dead_code)]
    transport: T,
}

impl<T: Transport> TallyOwlCollectorClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// An app driver submits a batch. The response means durable acceptance.
    pub fn submit_batch(
        &self,
        req: SubmitBatchRequest,
    ) -> Result<SubmitBatchResponse, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlCollector",
            "submit-batch",
            &encode_submit_batch_request(&req),
        )?;
        decode_submit_batch_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// A forwarder submits a batch to the head for final storage.
    pub fn commit_batch(
        &self,
        req: CommitBatchRequest,
    ) -> Result<CommitBatchResponse, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlCollector",
            "commit-batch",
            &encode_commit_batch_request(&req),
        )?;
        decode_commit_batch_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Fetch the compiled collection policy.
    pub fn fetch_policy(
        &self,
        req: FetchPolicyRequest,
    ) -> Result<FetchPolicyResponse, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlCollector",
            "fetch-policy",
            &encode_fetch_policy_request(&req),
        )?;
        decode_fetch_policy_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Report collector health for readiness and for operators.
    pub fn health(&self, req: HealthRequest) -> Result<CollectorHealth, ClientError> {
        let csil_resp =
            self.transport
                .call("TallyOwlCollector", "health", &encode_health_request(&req))?;
        decode_collector_health(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Resolve a source credential to its tenancy. The head answers this from
    /// the control catalog. A collector never stores a key.
    pub fn resolve_key(&self, req: ResolveKeyRequest) -> Result<ResolveKeyResponse, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlCollector",
            "resolve-key",
            &encode_resolve_key_request(&req),
        )?;
        decode_resolve_key_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
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
        let csil_resp =
            self.transport
                .call("TallyOwlIngest", "capture", &encode_capture_request(&req))?;
        decode_capture_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Accept telemetry and wait for the collector durability boundary.
    pub fn capture_critical(
        &self,
        req: CaptureCriticalRequest,
    ) -> Result<CaptureCriticalResponse, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlIngest",
            "capture-critical",
            &encode_capture_critical_request(&req),
        )?;
        decode_capture_critical_response(&csil_resp)
            .map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Report the collection policy that the client must apply.
    pub fn policy_version(
        &self,
        req: PolicyVersionRequest,
    ) -> Result<PolicyVersionResponse, ClientError> {
        let csil_resp = self.transport.call(
            "TallyOwlIngest",
            "policy-version",
            &encode_policy_version_request(&req),
        )?;
        decode_policy_version_response(&csil_resp)
            .map_err(|e| ClientError::Transport(e.to_string()))
    }
}
