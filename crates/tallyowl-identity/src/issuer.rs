//! Who signs this process's certificate.
//!
//! [`Issuer`] is the seam between the renewal logic and the network, so a test
//! drives enrollment, renewal, refusal, and outage with no socket.

use std::sync::Arc;

use tallyowl_control_api::codec::{
    decode_enroll_node_response, decode_service_error, encode_enroll_node_request,
    encode_renew_node_certificate_request,
};
use tallyowl_control_api::types::{
    EnrollNodeRequest, EnrollNodeResponse, NodeRole, RenewNodeCertificateRequest,
};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_rpc::material::IdentitySource;
use tallyowl_rpc::trust::TrustSource;
use tallyowl_rpc::{Client, SERVICE_ERROR_VARIANT};
use tallyowl_store::certificates::{sign_request, Authority, Subject, HEAD_ROLE, HEAD_SERVER_NAME};

use crate::Clock;

/// The control service that answers both operations.
const CONTROL_SERVICE: &str = "TallyOwlControl";

/// A certificate request is at most 64 KiB and the answer is a short chain.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Something that turns a certificate request into a certificate.
pub trait Issuer: Send + Sync {
    /// Enroll with a role token. The caller has no certificate yet.
    fn enroll(&self, request: EnrollNodeRequest) -> Result<EnrollNodeResponse, TallyOwlError>;
    /// Renew, showing `current` as the identity it renews.
    fn renew(
        &self,
        request: RenewNodeCertificateRequest,
        current: Arc<dyn IdentitySource>,
    ) -> Result<EnrollNodeResponse, TallyOwlError>;
}

/// A head reached over the network, verified as [`HEAD_SERVER_NAME`] against
/// the installation's authorities.
pub struct RemoteIssuer {
    address: String,
    trust: Arc<dyn TrustSource>,
}

impl RemoteIssuer {
    pub fn new(address: &str, trust: Arc<dyn TrustSource>) -> Result<RemoteIssuer, TallyOwlError> {
        if address.is_empty() {
            return Err(TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                "`head.endpoint` is empty, so this process has no head to enroll with. Set it to the head's address.",
            ));
        }
        Ok(RemoteIssuer {
            address: address.to_string(),
            trust,
        })
    }

    fn call(
        &self,
        client: Client,
        op: &str,
        payload: Vec<u8>,
    ) -> Result<EnrollNodeResponse, TallyOwlError> {
        let response = client.call(CONTROL_SERVICE, op, payload)?;
        if response.variant.as_deref() == Some(SERVICE_ERROR_VARIANT) {
            let error = decode_service_error(&response.payload).map_err(|e| {
                TallyOwlError::internal(format!(
                    "The head refused `{op}` in a form we could not read: {e}"
                ))
            })?;
            let code = from_wire_code(&error.code);
            return Err(TallyOwlError::new(code, error.message).retryable(error.retryable));
        }
        decode_enroll_node_response(&response.payload).map_err(|e| {
            TallyOwlError::internal(format!(
                "The head answered `{op}` in a form we could not read: {e}"
            ))
        })
    }
}

impl Issuer for RemoteIssuer {
    fn enroll(&self, request: EnrollNodeRequest) -> Result<EnrollNodeResponse, TallyOwlError> {
        // No certificate to show yet: the head proves itself, and the role
        // token in the request proves this process.
        let client = Client::server_auth(
            self.address.as_str(),
            MAX_FRAME_BYTES,
            HEAD_SERVER_NAME,
            Some(self.trust.authorities()),
        )?;
        self.call(client, "enroll-node", encode_enroll_node_request(&request))
    }

    fn renew(
        &self,
        request: RenewNodeCertificateRequest,
        current: Arc<dyn IdentitySource>,
    ) -> Result<EnrollNodeResponse, TallyOwlError> {
        let client = Client::mutual(
            self.address.as_str(),
            MAX_FRAME_BYTES,
            HEAD_SERVER_NAME,
            current,
            Arc::clone(&self.trust),
        );
        self.call(
            client,
            "renew-node-certificate",
            encode_renew_node_certificate_request(&request),
        )
    }
}

/// A head that signs its own certificate with the installation's authority.
pub struct LocalIssuer {
    authority: Arc<Authority>,
    node: String,
    clock: Arc<dyn Clock>,
}

impl LocalIssuer {
    pub fn new(authority: Arc<Authority>, node: String, clock: Arc<dyn Clock>) -> LocalIssuer {
        LocalIssuer {
            authority,
            node,
            clock,
        }
    }

    fn issue(
        &self,
        certificate_request: &[u8],
        cell: Option<String>,
        region: Option<String>,
    ) -> Result<EnrollNodeResponse, TallyOwlError> {
        let now = self.clock.now_ms();
        let issued = sign_request(
            &self.authority,
            certificate_request,
            &Subject {
                node_id: self.node.clone(),
                role: HEAD_ROLE.to_string(),
                cell: cell.clone(),
                region: region.clone(),
            },
            now,
            self.authority.max_lifetime_ms(),
        )
        .map_err(|e| TallyOwlError::new(ErrorCode::FailedPrecondition, e.to_string()))?;
        Ok(EnrollNodeResponse {
            node_id: self.node.clone(),
            certificate_chain: issued.chain,
            certificate_serial: issued.serial,
            effective_role: NodeRole::StorageProcess,
            cell,
            region,
            issued_at: issued.issued_at,
            expires_at: issued.expires_at,
            renew_after: issued.renew_after,
            permitted_capabilities: None,
        })
    }
}

impl Issuer for LocalIssuer {
    fn enroll(&self, request: EnrollNodeRequest) -> Result<EnrollNodeResponse, TallyOwlError> {
        self.issue(&request.certificate_request, request.cell, request.region)
    }

    fn renew(
        &self,
        request: RenewNodeCertificateRequest,
        _current: Arc<dyn IdentitySource>,
    ) -> Result<EnrollNodeResponse, TallyOwlError> {
        self.issue(&request.certificate_request, None, None)
    }
}

/// The wire error code and the process's own are the same twelve names.
fn from_wire_code(code: &tallyowl_control_api::types::ErrorCode) -> ErrorCode {
    use tallyowl_control_api::types::ErrorCode as Wire;
    match code {
        Wire::InvalidArgument => ErrorCode::InvalidArgument,
        Wire::Unauthenticated => ErrorCode::Unauthenticated,
        Wire::PermissionDenied => ErrorCode::PermissionDenied,
        Wire::NotFound => ErrorCode::NotFound,
        Wire::AlreadyExists => ErrorCode::AlreadyExists,
        Wire::ResourceExhausted => ErrorCode::ResourceExhausted,
        Wire::FailedPrecondition => ErrorCode::FailedPrecondition,
        Wire::Unavailable => ErrorCode::Unavailable,
        Wire::SchemaUnsupported => ErrorCode::SchemaUnsupported,
        Wire::BudgetExceeded => ErrorCode::BudgetExceeded,
        Wire::IncompleteResult => ErrorCode::IncompleteResult,
        Wire::Internal => ErrorCode::Internal,
    }
}
