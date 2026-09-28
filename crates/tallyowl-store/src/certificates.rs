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
//! The operator supplies an intermediate authority to each head that signs, as
//! a certificate and a key (D62). A head reads them at start, checks them, and
//! holds them in memory. The catalog never writes a signing key, so a copy of a
//! catalog is not a copy of the installation's authority, and every head of an
//! installation signs with the same one. [`generate_authorities`] makes a root
//! and an intermediate for the home profile and for a test; the root can then
//! stay off every machine that runs TallyOwl.
//!
//! # Short life is the revocation story
//!
//! A certificate lives for `enrollment.certificateLifetimeHours`, 24 by
//! default, and its node renews it at two thirds of that. There is no
//! certificate revocation list and there is not going to be one for a lifetime
//! this short. A revoked node stops renewing, and its identity ends within the
//! remaining third. That is a bounded window an operator can state, rather than
//! a distribution problem that fails quietly.

use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, IsCa, KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::catalog::{Catalog, CatalogError};

/// The default certificate lifetime, and the most a head issues when
/// `enrollment.certificateLifetimeHours` is not set.
pub const DEFAULT_CERTIFICATE_LIFETIME_MS: i64 = 24 * 60 * 60_000;

/// The shortest certificate this head issues. A shorter one would expire
/// between a node's renewal attempts.
pub const SHORTEST_CERTIFICATE_LIFETIME_MS: i64 = 60_000;

/// The name every head's certificate carries beside its node ID.
///
/// A collector dials a head by whatever address the operator gave it, and that
/// address is a Service name, an IP address, or a load balancer. It verifies
/// the name "a head of this installation" instead, which is what it needs to
/// know. Only the installation's authority signs a certificate with this name,
/// and only for the head role, so it proves exactly that.
pub const HEAD_SERVER_NAME: &str = "head.tallyowl.internal";

/// The role a head's own certificate carries.
pub const HEAD_ROLE: &str = "storage-process";

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

/// The authority a head signs node certificates with.
pub struct Authority {
    key_pem: String,
    certificate_pem: String,
    /// The signing certificate, then every intermediate above it that the
    /// operator gave, as DER. An issued chain is the leaf followed by these. A
    /// root is never here: it travels in `installation.authorities`, and a
    /// peer that does not already trust it must not start to because a chain
    /// carried it.
    chain: Vec<Vec<u8>>,
    /// The most this head issues. A role token can only shorten it.
    max_lifetime_ms: i64,
}

impl std::fmt::Debug for Authority {
    // The key is never printed, not even by a test that fails.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authority")
            .field("chain_length", &self.chain.len())
            .field("max_lifetime_ms", &self.max_lifetime_ms)
            .finish_non_exhaustive()
    }
}

impl Authority {
    /// Read and check a signing authority.
    ///
    /// `chain_pem` is the signing certificate followed by its chain, as one PEM
    /// text. `authorities` are the trusted authorities of the installation, as
    /// DER. The signer is refused, with a message that names the setting, when
    /// it is not a certificate authority, is not valid at `now_ms`, does not
    /// match its key, or does not chain to one of `authorities`.
    pub fn from_pem(
        chain_pem: &str,
        key_pem: &str,
        authorities: &[Vec<u8>],
        now_ms: i64,
        max_lifetime_ms: i64,
    ) -> Result<Authority, CertificateError> {
        const SETTING: &str = "installation.signingCertificate";
        let chain = pem_certificates(chain_pem)
            .map_err(|e| refused(SETTING, &format!("it could not be read as PEM: {e}")))?;
        let Some(signer) = chain.first() else {
            return Err(refused(SETTING, "it holds no certificate"));
        };
        let (_, parsed) = X509Certificate::from_der(signer).map_err(|e| {
            refused(
                SETTING,
                &format!("its first certificate could not be read: {e}"),
            )
        })?;

        let is_ca = parsed
            .basic_constraints()
            .ok()
            .flatten()
            .is_some_and(|constraints| constraints.value.ca);
        if !is_ca {
            return Err(refused(
                SETTING,
                "it is not a certificate authority, so it cannot sign a node certificate. Give the intermediate authority's certificate, not a node or server certificate. `tallyowl-head ca create` makes one",
            ));
        }
        let seconds = now_ms / 1000;
        let validity = parsed.validity();
        if seconds < validity.not_before.timestamp() || seconds > validity.not_after.timestamp() {
            return Err(refused(
                SETTING,
                &format!(
                    "it is valid from {} to {}, and it is not valid now. Give a current certificate, or check the clock on this host",
                    validity.not_before, validity.not_after
                ),
            ));
        }
        if !chains_to(&chain, authorities) {
            return Err(refused(
                SETTING,
                "it does not chain to any certificate in `installation.authorities`. Add the root that signed it to `installation.authorities`, or put the missing intermediate after it in the same file",
            ));
        }

        let key = KeyPair::from_pem(key_pem).map_err(|e| {
            refused(
                "installation.signingKey",
                &format!("it could not be read as a PEM private key: {e}"),
            )
        })?;
        if key.public_key_der() != parsed.public_key().raw {
            return Err(refused(
                "installation.signingKey",
                "it is not the key of the certificate in `installation.signingCertificate`. Give the key that belongs to that certificate",
            ));
        }

        // Every certificate after the signer that signs itself is a root.
        let mut on_the_wire = vec![chain[0].clone()];
        on_the_wire.extend(chain[1..].iter().filter(|der| !self_signed(der)).cloned());
        Ok(Authority {
            key_pem: key_pem.to_string(),
            certificate_pem: first_pem_certificate(chain_pem),
            chain: on_the_wire,
            max_lifetime_ms: max_lifetime_ms.max(SHORTEST_CERTIFICATE_LIFETIME_MS),
        })
    }

    /// The signing certificate, as PEM.
    pub fn certificate_pem(&self) -> &str {
        &self.certificate_pem
    }

    /// The signing certificate, as DER.
    pub fn certificate_der(&self) -> &[u8] {
        &self.chain[0]
    }

    /// The signing certificate and every certificate above it, as DER.
    pub fn chain(&self) -> &[Vec<u8>] {
        &self.chain
    }

    /// The most this head issues.
    pub fn max_lifetime_ms(&self) -> i64 {
        self.max_lifetime_ms
    }

    /// The signing key. Only the process that holds the authority ever reads
    /// this, and it never leaves it.
    pub(crate) fn key_pem(&self) -> &str {
        &self.key_pem
    }
}

fn refused(setting: &str, reason: &str) -> CertificateError {
    CertificateError::Unavailable(format!("The setting `{setting}` cannot be used: {reason}."))
}

/// Every certificate in a PEM text, in order, as DER.
fn pem_certificates(text: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut out = Vec::new();
    for block in x509_parser::pem::Pem::iter_from_buffer(text.as_bytes()) {
        let block = block.map_err(|e| e.to_string())?;
        if block.label == "CERTIFICATE" {
            out.push(block.contents);
        }
    }
    Ok(out)
}

fn self_signed(der: &[u8]) -> bool {
    X509Certificate::from_der(der)
        .map(|(_, certificate)| {
            certificate.subject() == certificate.issuer()
                && certificate.verify_signature(None).is_ok()
        })
        .unwrap_or(false)
}

/// The first certificate block of a PEM text, as written.
fn first_pem_certificate(text: &str) -> String {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    match (text.find(BEGIN), text.find(END)) {
        (Some(begin), Some(end)) if end > begin => format!("{}\n", &text[begin..end + END.len()]),
        _ => String::new(),
    }
}

/// Whether each certificate is signed by the next, and the last is one of the
/// authorities or is signed by one of them.
fn chains_to(chain: &[Vec<u8>], authorities: &[Vec<u8>]) -> bool {
    let parsed: Vec<X509Certificate> = match chain
        .iter()
        .map(|der| X509Certificate::from_der(der).map(|(_, c)| c))
        .collect::<Result<_, _>>()
    {
        Ok(parsed) => parsed,
        Err(_) => return false,
    };
    for pair in parsed.windows(2) {
        if pair[0]
            .verify_signature(Some(pair[1].public_key()))
            .is_err()
        {
            return false;
        }
    }
    let last_der = chain.last().expect("a chain has a first certificate");
    let last = parsed.last().expect("a chain has a first certificate");
    authorities.iter().any(|authority| {
        if authority == last_der {
            return true;
        }
        X509Certificate::from_der(authority)
            .map(|(_, trusted)| last.verify_signature(Some(trusted.public_key())).is_ok())
            .unwrap_or(false)
    })
}

impl Catalog {
    /// Make this process a signer. It is held in memory only.
    pub fn set_signing_authority(&self, authority: Authority) {
        *self.signer.write().expect("signer lock") = Some(Arc::new(authority));
    }

    /// The authority this head signs with.
    ///
    /// A head that has no signing certificate refuses to sign, and says which
    /// settings make it a signer. It never makes an authority of its own: an
    /// authority made here would differ on every head of an installation, and
    /// its key would live in the catalog. D62.
    pub fn certificate_authority(&self) -> Result<Arc<Authority>, CertificateError> {
        self.signer
            .read()
            .expect("signer lock")
            .clone()
            .ok_or_else(|| {
                CertificateError::Unavailable(
                    "This head signs no node certificates, because `installation.signingCertificate` and `installation.signingKey` are not set. Set both to an intermediate authority. `tallyowl-head ca create <directory>` makes one.".to_string(),
                )
            })
    }
}

/// A root authority and an intermediate it signed, as PEM.
pub struct GeneratedAuthorities {
    pub root_certificate_pem: String,
    pub root_key_pem: String,
    /// The intermediate's certificate followed by the root's. This is the text
    /// `installation.signingCertificate` names.
    pub intermediate_chain_pem: String,
    pub intermediate_key_pem: String,
}

impl std::fmt::Debug for GeneratedAuthorities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GeneratedAuthorities { .. }")
    }
}

/// Make a root authority and an intermediate for it.
///
/// The root lives ten years and the intermediate five. Rotating an authority is
/// an operator event rather than a routine one; the node certificates are what
/// stay short.
pub fn generate_authorities(now_ms: i64) -> Result<GeneratedAuthorities, CertificateError> {
    const YEAR_MS: i64 = 365 * 24 * 60 * 60_000;
    let authority_params = |name: &str, until: i64, path_length: Option<u8>| {
        let mut params = CertificateParams::default();
        let mut distinguished = DistinguishedName::new();
        distinguished.push(DnType::CommonName, name);
        params.distinguished_name = distinguished;
        params.is_ca = IsCa::Ca(match path_length {
            Some(length) => BasicConstraints::Constrained(length),
            None => BasicConstraints::Unconstrained,
        });
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params.not_before = time_from_ms(now_ms - 60_000)?;
        params.not_after = time_from_ms(now_ms + until)?;
        Ok::<_, CertificateError>(params)
    };
    let new_key = || {
        KeyPair::generate().map_err(|e| {
            CertificateError::Unavailable(format!("A key could not be generated: {e}"))
        })
    };

    let root_key = new_key()?;
    let root = authority_params("TallyOwl root authority", 10 * YEAR_MS, None)?
        .self_signed(&root_key)
        .map_err(|e| CertificateError::Unavailable(format!("The root could not be made: {e}")))?;

    let intermediate_key = new_key()?;
    let mut intermediate_params =
        authority_params("TallyOwl intermediate authority", 5 * YEAR_MS, Some(0))?;
    intermediate_params.use_authority_key_identifier_extension = true;
    let intermediate = intermediate_params
        .signed_by(&intermediate_key, &root, &root_key)
        .map_err(|e| {
            CertificateError::Unavailable(format!("The intermediate could not be made: {e}"))
        })?;

    Ok(GeneratedAuthorities {
        root_certificate_pem: root.pem(),
        root_key_pem: root_key.serialize_pem(),
        intermediate_chain_pem: format!("{}{}", intermediate.pem(), root.pem()),
        intermediate_key_pem: intermediate_key.serialize_pem(),
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
///
/// The lifetime is `lifetime_ms`, never more than the authority's maximum and
/// never less than [`SHORTEST_CERTIFICATE_LIFETIME_MS`]. The node renews it at
/// two thirds of its life (D62), so it outlasts a head outage of one third.
///
/// The certificate names the node ID as a DNS subject alternative name, because
/// a TLS client checks that name and never the common name. A head's
/// certificate also names [`HEAD_SERVER_NAME`].
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
    let mut names = vec![SanType::DnsName(node_id.as_str().try_into().map_err(
        |e| CertificateError::Malformed(format!("The node ID `{node_id}` is not a DNS name: {e}")),
    )?)];
    if role == HEAD_ROLE {
        names.push(SanType::DnsName(
            HEAD_SERVER_NAME
                .try_into()
                .expect("the head server name is a DNS name"),
        ));
    }
    params.subject_alt_names = names;

    // The serial is chosen here, so the value the catalog records is the value
    // a TLS peer reads out of the certificate. The top bit is clear and the
    // first byte is not zero, so the DER integer is exactly these eight bytes.
    let mut serial = [0u8; 8];
    getrandom::fill(&mut serial).map_err(|e| {
        CertificateError::Unavailable(format!("The system random source failed: {e}"))
    })?;
    serial[0] = (serial[0] & 0x7f) | 0x40;
    params.serial_number = Some(SerialNumber::from(serial.to_vec()));

    let lifetime_ms = lifetime_ms.clamp(
        SHORTEST_CERTIFICATE_LIFETIME_MS,
        authority.max_lifetime_ms(),
    );
    let expires_at = now + lifetime_ms;
    params.not_before = time_from_ms(now)?;
    params.not_after = time_from_ms(expires_at)?;

    let certificate = params
        .signed_by(&request.public_key, &issuer, &authority_key)
        .map_err(|e| {
            CertificateError::Unavailable(format!("The certificate could not be signed: {e}"))
        })?;

    let mut chain = vec![certificate.der().to_vec()];
    chain.extend(authority.chain().iter().cloned());
    Ok(IssuedCertificate {
        chain,
        serial: crate::row::hex(&serial),
        issued_at: now,
        expires_at,
        renew_after: now + lifetime_ms * 2 / 3,
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
