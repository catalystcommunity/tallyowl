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
        let csil_resp = self
            .transport
            .call("TallyOwlIngest", "capture", &encode_capture_request(&req))
            .await?;
        decode_capture_response(&csil_resp).map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Accept telemetry and wait for the collector durability boundary.
    pub async fn capture_critical(
        &self,
        req: CaptureCriticalRequest,
    ) -> Result<CaptureCriticalResponse, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlIngest",
                "capture-critical",
                &encode_capture_critical_request(&req),
            )
            .await?;
        decode_capture_critical_response(&csil_resp)
            .map_err(|e| ClientError::Transport(e.to_string()))
    }

    /// Report the collection policy that the client must apply.
    pub async fn policy_version(
        &self,
        req: PolicyVersionRequest,
    ) -> Result<PolicyVersionResponse, ClientError> {
        let csil_resp = self
            .transport
            .call(
                "TallyOwlIngest",
                "policy-version",
                &encode_policy_version_request(&req),
            )
            .await?;
        decode_policy_version_response(&csil_resp)
            .map_err(|e| ClientError::Transport(e.to_string()))
    }
}
