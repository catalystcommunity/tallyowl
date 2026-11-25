//! Generated service traits from CSIL specification

use super::types::*;

/// TallyOwlCollector service trait
pub trait TallyOwlCollector {
    type Context;
    #[allow(clippy::result_large_err)]
    /// An app driver submits a batch. The response means durable acceptance.
    fn submit_batch(
        &self,
        ctx: &Self::Context,
        input: SubmitBatchRequest,
    ) -> Result<SubmitBatchResponse, ServiceError>;
    #[allow(clippy::result_large_err)]
    /// A forwarder submits a batch to the head for final storage.
    fn commit_batch(
        &self,
        ctx: &Self::Context,
        input: CommitBatchRequest,
    ) -> Result<CommitBatchResponse, ServiceError>;
    #[allow(clippy::result_large_err)]
    /// Fetch the compiled collection policy.
    fn fetch_policy(
        &self,
        ctx: &Self::Context,
        input: FetchPolicyRequest,
    ) -> Result<FetchPolicyResponse, ServiceError>;
    #[allow(clippy::result_large_err)]
    /// Report collector health for readiness and for operators.
    fn health(
        &self,
        ctx: &Self::Context,
        input: HealthRequest,
    ) -> Result<CollectorHealth, ServiceError>;
    #[allow(clippy::result_large_err)]
    /// Resolve a source credential to its tenancy. The head answers this from
    /// the control catalog. A collector never stores a key.
    fn resolve_key(
        &self,
        ctx: &Self::Context,
        input: ResolveKeyRequest,
    ) -> Result<ResolveKeyResponse, ServiceError>;
}

/// Wire-id ordinals for the TallyOwlCollector service (transport compact profiles).
pub mod tally_owl_collector_wire_ids {
    pub const SERVICE: u64 = 2;
    pub const OP_SUBMIT_BATCH: u64 = 0;
    pub const OP_COMMIT_BATCH: u64 = 1;
    pub const OP_FETCH_POLICY: u64 = 2;
    pub const OP_HEALTH: u64 = 3;
    pub const OP_RESOLVE_KEY: u64 = 4;
}

/// TallyOwlIngest service trait
pub trait TallyOwlIngest {
    type Context;
    #[allow(clippy::result_large_err)]
    /// Accept telemetry on the application's existing connection. Best effort.
    fn capture(
        &self,
        ctx: &Self::Context,
        input: CaptureRequest,
    ) -> Result<CaptureResponse, ServiceError>;
    #[allow(clippy::result_large_err)]
    /// Accept telemetry and wait for the collector durability boundary.
    fn capture_critical(
        &self,
        ctx: &Self::Context,
        input: CaptureCriticalRequest,
    ) -> Result<CaptureCriticalResponse, ServiceError>;
    #[allow(clippy::result_large_err)]
    /// Report the collection policy that the client must apply.
    fn policy_version(
        &self,
        ctx: &Self::Context,
        input: PolicyVersionRequest,
    ) -> Result<PolicyVersionResponse, ServiceError>;
}

/// Wire-id ordinals for the TallyOwlIngest service (transport compact profiles).
pub mod tally_owl_ingest_wire_ids {
    pub const SERVICE: u64 = 1;
    pub const OP_CAPTURE: u64 = 0;
    pub const OP_CAPTURE_CRITICAL: u64 = 1;
    pub const OP_POLICY_VERSION: u64 = 2;
}

/// TallyOwlAlertReceiver service trait
pub trait TallyOwlAlertReceiver {
    type Context;
    #[allow(clippy::result_large_err)]
    /// Take one signed alert notification.
    fn notify(
        &self,
        ctx: &Self::Context,
        input: AlertNotifyRequest,
    ) -> Result<AlertNotifyResponse, ServiceError>;
}

/// Wire-id ordinals for the TallyOwlAlertReceiver service (transport compact profiles).
pub mod tally_owl_alert_receiver_wire_ids {
    pub const SERVICE: u64 = 6;
    pub const OP_NOTIFY: u64 = 0;
}
