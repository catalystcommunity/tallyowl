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

/// Typed client for the TallyOwlCollector service.
pub struct TallyOwlCollectorAsyncClient<T: AsyncTransport> {
    #[allow(dead_code)]
    transport: T,
}

impl<T: AsyncTransport> TallyOwlCollectorAsyncClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// An app driver submits a batch. The response means durable acceptance.
    pub async fn submit_batch(
        &self,
        req: SubmitBatchRequest,
    ) -> Result<SubmitBatchResponse, ClientError> {
        let csil_req = match encode_submit_batch_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlCollector", "submit-batch", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlCollector",
            "submit-batch",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_submit_batch_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// A forwarder submits a batch to the head for final storage.
    pub async fn commit_batch(
        &self,
        req: CommitBatchRequest,
    ) -> Result<CommitBatchResponse, ClientError> {
        let csil_req = match encode_commit_batch_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlCollector", "commit-batch", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlCollector",
            "commit-batch",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_commit_batch_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Fetch the compiled collection policy.
    pub async fn fetch_policy(
        &self,
        req: FetchPolicyRequest,
    ) -> Result<FetchPolicyResponse, ClientError> {
        let csil_req = match encode_fetch_policy_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlCollector", "fetch-policy", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlCollector",
            "fetch-policy",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_fetch_policy_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Report collector health for readiness and for operators.
    pub async fn health(&self, req: HealthRequest) -> Result<CollectorHealth, ClientError> {
        let csil_req = match encode_health_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlCollector", "health", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlCollector",
            "health",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_collector_health(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Resolve a source credential to its tenancy. The head answers this from
    /// the control catalog. A collector never stores a key.
    pub async fn resolve_key(
        &self,
        req: ResolveKeyRequest,
    ) -> Result<ResolveKeyResponse, ClientError> {
        let csil_req = match encode_resolve_key_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlCollector", "resolve-key", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlCollector",
            "resolve-key",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_resolve_key_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }
}

/// Typed client for the TallyOwlIngest service.
pub struct TallyOwlIngestAsyncClient<T: AsyncTransport> {
    #[allow(dead_code)]
    transport: T,
}

impl<T: AsyncTransport> TallyOwlIngestAsyncClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Accept telemetry on the application's existing connection. Best effort.
    pub async fn capture(&self, req: CaptureRequest) -> Result<CaptureResponse, ClientError> {
        let csil_req = match encode_capture_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlIngest", "capture", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlIngest",
            "capture",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_capture_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Accept telemetry and wait for the collector durability boundary.
    pub async fn capture_critical(
        &self,
        req: CaptureCriticalRequest,
    ) -> Result<CaptureCriticalResponse, ClientError> {
        let csil_req = match encode_capture_critical_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlIngest", "capture-critical", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlIngest",
            "capture-critical",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_capture_critical_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Report the collection policy that the client must apply.
    pub async fn policy_version(
        &self,
        req: PolicyVersionRequest,
    ) -> Result<PolicyVersionResponse, ClientError> {
        let csil_req = match encode_policy_version_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlIngest", "policy-version", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlIngest",
            "policy-version",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_policy_version_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }
}

/// Typed client for the TallyOwlAlertReceiver service.
pub struct TallyOwlAlertReceiverAsyncClient<T: AsyncTransport> {
    #[allow(dead_code)]
    transport: T,
}

impl<T: AsyncTransport> TallyOwlAlertReceiverAsyncClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Take one signed alert notification.
    pub async fn notify(
        &self,
        req: AlertNotifyRequest,
    ) -> Result<AlertNotifyResponse, ClientError> {
        let csil_req = match encode_alert_notify_request(&req) {
            Ok(csil_bytes) => csil_bytes,
            Err(e) => return Err(ClientError::Encode(e.to_string())),
        };
        let csil_reply = self
            .transport
            .call_reply("TallyOwlAlertReceiver", "notify", &csil_req)
            .await?;
        let csil_resp = super::client::resolve_rpc_reply(
            "TallyOwlAlertReceiver",
            "notify",
            csil_reply.variant.as_deref(),
            csil_reply.payload,
        )?;
        decode_alert_notify_response(&csil_resp).map_err(|e| ClientError::Decode(e.to_string()))
    }
}
