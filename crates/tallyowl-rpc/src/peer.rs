//! Who is on the other end of a connection, as the transport proved it.
//!
//! A service decides what a caller may do from this, and never from a name the
//! caller wrote in a request. The transport is the only thing that can prove an
//! identity: a certificate chain that it verified against a trusted authority,
//! or a listener that the operator bound where only this host can reach it.

use tallyowl_obs::error::TallyOwlError;

/// Who is on the other end of a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peer {
    /// A plaintext connection on a loopback address or a unix socket. The
    /// operator trusted the host when they bound the listener there.
    Local,
    /// Server-authenticated TLS: the client showed no certificate.
    Anonymous,
    /// Mutual TLS: the client showed a certificate that chains to a trusted
    /// authority.
    Verified(PeerIdentity),
    /// Plaintext on a non-loopback address, which a service permits only under
    /// `transport.allowPlaintext`. Nothing is proved.
    Unverified,
}

/// What a verified leaf certificate says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    /// The leaf's common name. The head assigns it at enrollment, so it is the
    /// node ID and not a name the peer chose.
    pub node_id: String,
    /// The leaf's organizational unit, which carries the node's role.
    pub role: Option<String>,
    /// The certificate serial, in lowercase hexadecimal with no separators.
    pub certificate_serial: String,
    /// When the certificate stops being valid, in milliseconds since 1970.
    pub expires_at_ms: i64,
}

impl PeerIdentity {
    /// Read a verified leaf certificate. Call this only after the handshake
    /// verified the chain: parsing proves nothing by itself.
    pub fn from_certificate(certificate: &[u8]) -> Result<PeerIdentity, TallyOwlError> {
        use x509_parser::prelude::*;
        let unreadable = |what: &str| {
            TallyOwlError::internal(format!(
                "The peer's certificate was verified, and its {what} could not be read."
            ))
        };
        let (_, parsed) =
            X509Certificate::from_der(certificate).map_err(|_| unreadable("contents"))?;
        let node_id = crate::tls::peer_node_id(certificate).ok_or_else(|| unreadable("name"))?;
        let role = parsed
            .subject()
            .iter_organizational_unit()
            .next()
            .and_then(|unit| unit.as_str().ok())
            .map(str::to_string);
        let certificate_serial = parsed
            .raw_serial()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let expires_at_ms = parsed.validity().not_after.timestamp().saturating_mul(1000);
        Ok(PeerIdentity {
            node_id,
            role,
            certificate_serial,
            expires_at_ms,
        })
    }
}
