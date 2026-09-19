//! The installation certificate authority, and the signing half of enrollment.
//!
//! `docs/NODE_IDENTITY.md` sections 4 and 6.
//!
//! # The rule this module exists to keep
//!
//! **Each enrolled node generates its private key. The control plane signs only
//! the certificate request.** `AGENTS.md` states it and this module cannot
//! break it: [`sign_request`] takes a certificate signing request and gives back
//! a certificate. There is no function here that produces a key for somebody
//! else, and no private key ever crosses the wire in either direction.
//!
//! # The authority
//!
//! One self-signed authority for each installation, generated on first use and
//! held in the catalog beside the credential secret. Its private key is the one
//! secret this process does hold, and it is the reason
//! `docs/FAILURE_MODES.md` section 7 says a catalog rebuild from segment
//! manifests cannot put credentials back.
//!
//! # Short life is the revocation story
//!
//! Section 6: the first certificate lifetime is 24 hours and renewal starts
//! after 8 hours. There is no certificate revocation list and there is not going
//! to be one for a lifetime this short. A revoked node stops renewing, and its
//! identity ends within the remaining lifetime. That is a bounded window an
//! operator can state, rather than a distribution problem that fails quietly.

use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, IsCa, KeyPair, KeyUsagePurpose,
};

use crate::catalog::{Catalog, CatalogError};
use crate::cbor::{self, MapBuilder, Value};

/// Where the authority's own key and certificate live.
const AUTHORITY_KEY: &str = "auth/certificate-authority";

/// The first certificate lifetime. NODE_IDENTITY.md section 6.
pub const DEFAULT_CERTIFICATE_LIFETIME_MS: i64 = 24 * 60 * 60_000;

/// How long before expiry a node starts renewing. Section 6 says renewal starts
/// after 8 hours of a 24-hour life, which is 16 hours before it ends.
///
/// A node that misses one attempt has two thirds of its life left to try again,
/// which is what keeps a short lifetime from becoming an outage.
pub const DEFAULT_RENEWAL_LEAD_MS: i64 = 16 * 60 * 60_000;

/// Why a certificate could not be issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertificateError {
    /// The certificate request did not read as one.
    Malformed(String),
    /// The authority could not be read or written.
    Unavailable(String),
}

impl std::fmt::Display for CertificateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CertificateError::Malformed(m) | CertificateError::Unavailable(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for CertificateError {}

impl From<CatalogError> for CertificateError {
    fn from(error: CatalogError) -> CertificateError {
        CertificateError::Unavailable(error.to_string())
    }
}

/// The identity the control plane assigned, and the one the certificate names.
///
/// This is a type rather than four arguments because the node ID and the role
/// are both short strings and putting them in the wrong order would produce a
/// certificate that looks right and names the wrong thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub node_id: String,
    pub role: String,
    pub cell: Option<String>,
    pub region: Option<String>,
}

/// One issued certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedCertificate {
    /// The node's certificate, then the authority's, in that order. A verifier
    /// reads the chain from the leaf up.
    pub chain: Vec<Vec<u8>>,
    pub serial: String,
    pub issued_at: i64,
    pub expires_at: i64,
    /// When the node should start renewing. Section 6.
    pub renew_after: i64,
}

/// The installation's certificate authority.
pub struct Authority {
    key_pem: String,
    certificate_pem: String,
    /// The stored certificate's own bytes.
    ///
    /// Re-signing the authority to rebuild an `rcgen` object produces a fresh
    /// signature, so the chain must carry the certificate that was stored and
    /// not that rebuild. A verifier that pinned the stored one would otherwise
    /// see a certificate it had never trusted.
    certificate_der: Vec<u8>,
}

impl Authority {
    /// The authority's certificate, for a node that has to verify the control
    /// plane before it sends its token. Step 2 of the enrollment sequence.
    pub fn certificate_pem(&self) -> &str {
        &self.certificate_pem
    }

    /// The authority's certificate in DER form, for a TLS verifier.
    pub fn certificate_der(&self) -> &[u8] {
        &self.certificate_der
    }

    /// The authority's private key. Only the process that holds the catalog
    /// ever reads this, and it never leaves it.
    pub(crate) fn key_pem(&self) -> &str {
        &self.key_pem
    }
}

impl Catalog {
    /// The installation's certificate authority, generated on first use.
    ///
    /// One authority for each installation. It is generated rather than
    /// configured so that a home installation reaches a working mutual TLS
    /// setup with no key ceremony, which is the same reason the credential
    /// secret is generated.
    pub fn certificate_authority(&self) -> Result<Authority, CertificateError> {
        if let Some(bytes) = self.read_authority()? {
            let value = crate::control::decode_value(&bytes)?;
            let key_pem = crate::control::text_field(&value, "key");
            let certificate_pem = crate::control::text_field(&value, "cert");
            let certificate_der = value
                .field("der")
                .and_then(crate::cbor::Value::as_bytes)
                .unwrap_or_default()
                .to_vec();
            if !key_pem.is_empty() && !certificate_pem.is_empty() && !certificate_der.is_empty() {
                return Ok(Authority {
                    key_pem,
                    certificate_pem,
                    certificate_der,
                });
            }
            return Err(CertificateError::Unavailable(
                "The installation's certificate authority could not be read.".to_string(),
            ));
        }

        let authority = new_authority()?;
        self.write_authority(&authority)?;
        Ok(authority)
    }

    fn read_authority(&self) -> Result<Option<Vec<u8>>, CatalogError> {
        self.read_record(AUTHORITY_KEY)
    }

    fn write_authority(&self, authority: &Authority) -> Result<(), CatalogError> {
        self.write_record(
            AUTHORITY_KEY,
            cbor::encode(
                &MapBuilder::new()
                    .put("key", Value::text(&authority.key_pem))
                    .put("cert", Value::text(&authority.certificate_pem))
                    .put("der", Value::Bytes(authority.certificate_der.clone()))
                    .build(),
            ),
        )
    }
}

/// A fresh self-signed authority.
///
/// Ten years, because rotating an installation authority is an operator event
/// rather than a routine one, and a short-lived authority would put every node
/// through a re-enrollment on a schedule nobody asked for. The node
/// certificates it signs are what stay short.
fn new_authority() -> Result<Authority, CertificateError> {
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "TallyOwl installation authority");
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];

    // Ten years. Rotating an installation authority is an operator event rather
    // than a routine one, and a short-lived authority would put every node
    // through re-enrollment on a schedule nobody asked for. The node
    // certificates it signs are what stay short.
    let now = tallyowl_obs::time::now_ms();
    params.not_before = time_from_ms(now)?;
    params.not_after = time_from_ms(now + 10 * 365 * 24 * 60 * 60_000)?;

    let key = KeyPair::generate()
        .map_err(|e| CertificateError::Unavailable(format!("A key could not be generated: {e}")))?;
    let certificate = params.self_signed(&key).map_err(|e| {
        CertificateError::Unavailable(format!("The authority could not be created: {e}"))
    })?;
    Ok(Authority {
        key_pem: key.serialize_pem(),
        certificate_pem: certificate.pem(),
        certificate_der: certificate.der().to_vec(),
    })
}

/// Sign a node's certificate request.
///
/// **The request carries a public key and this returns a certificate.** No
/// private key is read, produced, or returned, which is the whole of the rule
/// in `AGENTS.md`: "Each enrolled node generates its private key. The control
/// plane signs only the certificate request."
///
/// The subject is the node ID the control plane assigned, not one the node
/// asked for, and the role and location are recorded as organizational units so
/// a verifier can read the effective scope from the certificate rather than
/// looking it up.
pub fn sign_request(
    authority: &Authority,
    request_der: &[u8],
    subject: &Subject,
    now: i64,
    lifetime_ms: i64,
) -> Result<IssuedCertificate, CertificateError> {
    let Subject {
        node_id,
        role,
        cell,
        region,
    } = subject;
    // This both parses the request and verifies that it is signed by the key it
    // carries, which is what proves the node holds that private key. It does
    // not authorize anything: the role token does that.
    let request = CertificateSigningRequestParams::from_der(&request_der.to_vec().into())
        .map_err(|e| {
            CertificateError::Malformed(format!(
                "The certificate request could not be read. It must be a PKCS#10 request in DER form, signed by the key it carries. {e}"
            ))
        })?;

    let authority_key = KeyPair::from_pem(authority.key_pem()).map_err(|e| {
        CertificateError::Unavailable(format!("The authority key could not be read: {e}"))
    })?;
    let issuer = CertificateParams::from_ca_cert_pem(authority.certificate_pem())
        .and_then(|params| params.self_signed(&authority_key))
        .map_err(|e| {
            CertificateError::Unavailable(format!("The authority could not be read: {e}"))
        })?;

    // The subject is built here rather than taken from the request. A node that
    // could choose its own subject could name itself another node, and the
    // whole point of enrollment is that the control plane decides the identity.
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, node_id.as_str());
    name.push(DnType::OrganizationalUnitName, role.as_str());
    if let Some(cell) = cell {
        name.push(DnType::LocalityName, cell.as_str());
    }
    if let Some(region) = region {
        name.push(DnType::StateOrProvinceName, region.as_str());
    }
    params.distinguished_name = name;
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.use_authority_key_identifier_extension = true;

    let expires_at = now + lifetime_ms;
    params.not_before = time_from_ms(now)?;
    params.not_after = time_from_ms(expires_at)?;

    let certificate = params
        .signed_by(&request.public_key, &issuer, &authority_key)
        .map_err(|e| {
            CertificateError::Unavailable(format!("The certificate could not be signed: {e}"))
        })?;

    let leaf = certificate.der().to_vec();
    let serial = crate::row::hex(&blake3::hash(&leaf).as_bytes()[0..8]);
    Ok(IssuedCertificate {
        chain: vec![leaf, authority.certificate_der().to_vec()],
        serial,
        issued_at: now,
        expires_at,
        // A certificate shorter than the standard lead time still gets a
        // renewal point inside its own life rather than one in the past.
        renew_after: now + (lifetime_ms - DEFAULT_RENEWAL_LEAD_MS).max(lifetime_ms / 3),
    })
}

/// `rcgen` states validity as an `OffsetDateTime`, and CONVENTIONS.md keeps
/// every other time in this project as milliseconds since the epoch. The
/// boundary is here and nowhere else.
fn time_from_ms(ms: i64) -> Result<time::OffsetDateTime, CertificateError> {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).map_err(|e| {
        CertificateError::Unavailable(format!("The time {ms} is not a valid date: {e}"))
    })
}
