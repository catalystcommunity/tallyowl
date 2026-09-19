//! Generated service traits from CSIL specification

use super::types::*;

/// TallyOwlIngest service trait
pub trait TallyOwlIngest {
    type Context;
    /// Accept telemetry on the application's existing connection. Best effort.
    fn capture(
        &self,
        ctx: &Self::Context,
        input: CaptureRequest,
    ) -> Result<CaptureResponse, ServiceError>;
    /// Accept telemetry and wait for the collector durability boundary.
    fn capture_critical(
        &self,
        ctx: &Self::Context,
        input: CaptureCriticalRequest,
    ) -> Result<CaptureCriticalResponse, ServiceError>;
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
