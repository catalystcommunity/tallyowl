//! Mutual TLS under the CSIL stream carrier.
//!
//! `docs/DESIGN.md` section 4.2 puts TLS on every server-to-server hop, and
//! `docs/NODE_IDENTITY.md` section 4 step 9 says an enrolled node makes a mutual
//! TLS connection with the certificate it was issued.
//!
//! # Why this needed no contract change
//!
//! `StreamCarrier` is generic over any `Read + Write`, and a rustls stream is
//! one. TLS therefore goes *under* the carrier: the framing, the envelopes, and
//! every generated codec are unchanged and do not know it is there. The alpha
//! report predicted this and it held.
//!
//! # What identity means here
//!
//! Both ends verify against the installation's own certificate authority, and
//! **the server requires a client certificate**. That is what makes it mutual:
//! a peer without an enrolled identity cannot complete the handshake, so an
//! unauthorized process never reaches a decoder.
//!
//! A node's certificate carries its node ID as the common name and its role as
//! an organizational unit, so a service can read the peer's identity from the
//! connection rather than looking it up.
//!
//! # A secured connection serves as many requests at a time as a plain one
//!
//! It did not always. Until L058 was fixed, this module used
//! `rustls::StreamOwned`, which owns both directions, so a TLS connection served
//! one request at a time and a pipelining client gained only the network
//! latency. [`crate::duplex::TlsDuplex`] replaced it: the rustls connection sits
//! behind a mutex that nobody holds across a blocking socket call, and two
//! handles read and write at the same time. The server loop and the client are
//! now the same code for both carriers.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};

use crate::address::{Address, Socket};
use crate::duplex::{rustls_error, TlsDuplex};
use crate::material::{CertificateSet, IdentitySource, SetResolver, StaticIdentity};
use crate::trust::{StaticTrust, TrustSource};
use crate::{Client, Dispatcher, Server, ServerOptions, DEFAULT_MAX_IN_FLIGHT};

/// What one process presents, and what it trusts.
///
/// The chain and the key come from enrollment; the authority comes from the
/// same place and is what both ends verify against.
#[derive(Clone)]
pub struct Identity {
    /// The leaf first, then the authority. This is `EnrollNodeResponse.certificate_chain`.
    pub chain: Vec<Vec<u8>>,
    /// This process's own private key, in DER form. It was generated here and
    /// has never left.
    pub private_key: Vec<u8>,
    /// The installation authority, in DER form.
    pub authority: Vec<u8>,
    /// The name a client verifies the server against. A node certificate's
    /// common name is its node ID, so this is the node ID of the peer.
    pub expected_server_name: String,
}

fn tls_failure(what: &str, detail: impl std::fmt::Display) -> TallyOwlError {
    TallyOwlError::new(
        ErrorCode::FailedPrecondition,
        format!("The secure connection could not be prepared. {what}: {detail}"),
    )
}

fn roots(authorities: &[Vec<u8>]) -> Result<RootCertStore, TallyOwlError> {
    if authorities.is_empty() {
        return Err(tls_failure(
            "no authority is trusted",
            "set `installation.authorities`",
        ));
    }
    let mut store = RootCertStore::empty();
    for authority in authorities {
        store
            .add(CertificateDer::from(authority.clone()))
            .map_err(|e| tls_failure("a trusted authority was not usable", e))?;
    }
    Ok(store)
}

fn chain_of(identity: &Identity) -> Vec<CertificateDer<'static>> {
    identity
        .chain
        .iter()
        .map(|der| CertificateDer::from(der.clone()))
        .collect()
}

fn key_of(identity: &Identity) -> Result<PrivateKeyDer<'static>, TallyOwlError> {
    PrivateKeyDer::try_from(identity.private_key.clone())
        .map_err(|e| tls_failure("this process's own key was not usable", e))
}

/// The server side of mutual TLS: present `identity`, and require a client
/// certificate that one of `authorities` signed.
fn mutual_server_config(
    identity: &Identity,
    authorities: &[Vec<u8>],
    allow_anonymous: bool,
) -> Result<Arc<ServerConfig>, TallyOwlError> {
    install_crypto_provider();
    let mut builder = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots(authorities)?));
    if allow_anonymous {
        // A client may show nothing. A client that shows a certificate is
        // verified exactly as before.
        builder = builder.allow_unauthenticated();
    }
    let verifier = builder
        .build()
        .map_err(|e| tls_failure("the client verifier was not usable", e))?;
    let config = ServerConfig::builder()
        // Mutual, not optional. A peer with no enrolled identity never reaches
        // a decoder.
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain_of(identity), key_of(identity)?)
        .map_err(|e| tls_failure("this process's own certificate was not usable", e))?;
    Ok(Arc::new(config))
}

/// The client side of mutual TLS: verify the peer against `authorities`, and
/// present `identity`.
fn mutual_client_config(
    identity: &Identity,
    authorities: &[Vec<u8>],
) -> Result<Arc<ClientConfig>, TallyOwlError> {
    install_crypto_provider();
    let config = ClientConfig::builder()
        .with_root_certificates(roots(authorities)?)
        .with_client_auth_cert(chain_of(identity), key_of(identity)?)
        .map_err(|e| tls_failure("this process's own certificate was not usable", e))?;
    Ok(Arc::new(config))
}

/// The server side: present this identity, and require one from every peer
/// that its own authority signed.
pub fn server_config(identity: &Identity) -> Result<Arc<ServerConfig>, TallyOwlError> {
    mutual_server_config(identity, std::slice::from_ref(&identity.authority), false)
}

/// The client side: verify the peer against the authority, and present an
/// identity of our own.
pub fn client_config(identity: &Identity) -> Result<Arc<ClientConfig>, TallyOwlError> {
    mutual_client_config(identity, std::slice::from_ref(&identity.authority))
}

/// The server side of a listener that applications reach: present the current
/// pair of `certificates`, and ask for no client certificate. An application
/// proves itself with its project key, above TLS.
///
/// Public so that a listener that is not CSIL, the OpenTelemetry receiver,
/// serves the same certificates by the same rule.
pub fn server_auth_config(certificates: Arc<CertificateSet>) -> Arc<ServerConfig> {
    install_crypto_provider();
    Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(SetResolver(certificates))),
    )
}

/// Why a secured listener dropped a connection.
pub(crate) enum Refusal {
    /// This node has no identity yet, or its own configuration did not build.
    /// The peer did nothing wrong.
    NoIdentity,
    /// The peer failed the handshake.
    Handshake,
}

/// How a listener secures its connections.
pub(crate) enum Security {
    /// Plaintext. The peer is `Local` on a loopback or unix address, and
    /// `Unverified` anywhere else.
    Plain,
    /// Server-authenticated TLS. The peer is `Anonymous`.
    ServerAuth(Arc<ServerConfig>),
    /// Mutual TLS, with the identity and the authorities read again for each
    /// connection. The peer is `Verified`.
    Mutual {
        identity: Arc<dyn IdentitySource>,
        trust: Arc<dyn TrustSource>,
        allow_anonymous: bool,
    },
}

impl Security {
    /// Put a server session on an accepted socket and finish the handshake.
    /// An error when the connection is to be dropped, and why.
    pub(crate) fn accept(&self, socket: Socket) -> Result<(TlsDuplex, crate::Peer), Refusal> {
        let config = match self {
            Security::Plain => return Err(Refusal::NoIdentity),
            Security::ServerAuth(config) => Arc::clone(config),
            Security::Mutual {
                identity,
                trust,
                allow_anonymous,
            } => {
                let current = identity.current().ok_or(Refusal::NoIdentity)?;
                mutual_server_config(&current, &trust.authorities(), *allow_anonymous)
                    .map_err(|_| Refusal::NoIdentity)?
            }
        };
        let connection = ServerConnection::new(config).map_err(|_| Refusal::NoIdentity)?;
        let duplex = TlsDuplex::new(rustls::Connection::Server(connection), socket)
            .map_err(|_| Refusal::Handshake)?;
        // Finish the handshake before a frame is read. A peer that fails it is
        // refused here and never reaches a decoder.
        duplex.handshake().map_err(|_| Refusal::Handshake)?;
        let peer = match self {
            Security::Mutual {
                allow_anonymous, ..
            } => match duplex
                .peer_certificates()
                .map_err(|_| Refusal::Handshake)?
                .and_then(|c| c.into_iter().next())
            {
                Some(leaf) => crate::Peer::Verified(
                    crate::PeerIdentity::from_certificate(&leaf).map_err(|_| Refusal::Handshake)?,
                ),
                // Only reachable when the verifier let a client show nothing.
                None if *allow_anonymous => crate::Peer::Anonymous,
                None => return Err(Refusal::Handshake),
            },
            _ => crate::Peer::Anonymous,
        };
        Ok((duplex, peer))
    }
}

/// Serve CSIL-RPC over mutual TLS.
///
/// A connection serves as many correlated requests at a time as the plain path
/// does, over the same loop. This is the one-identity form of [`serve_mutual`].
pub fn serve_tls(
    address: &str,
    dispatcher: Arc<dyn Dispatcher>,
    max_frame_bytes: usize,
    identity: &Identity,
) -> Result<Server, TallyOwlError> {
    serve_tls_with_in_flight(
        address,
        dispatcher,
        max_frame_bytes,
        identity,
        DEFAULT_MAX_IN_FLIGHT,
    )
}

/// Serve CSIL-RPC over mutual TLS, and say how many correlated requests one
/// connection may serve at the same time.
pub fn serve_tls_with_in_flight(
    address: &str,
    dispatcher: Arc<dyn Dispatcher>,
    max_frame_bytes: usize,
    identity: &Identity,
    max_in_flight: usize,
) -> Result<Server, TallyOwlError> {
    serve_tls_with(
        address,
        dispatcher,
        identity,
        ServerOptions::new(max_frame_bytes).max_in_flight(max_in_flight),
    )
}

/// Serve CSIL-RPC over mutual TLS with every listener option stated. The peer
/// must hold a certificate that `identity.authority` signed.
pub fn serve_tls_with(
    address: &str,
    dispatcher: Arc<dyn Dispatcher>,
    identity: &Identity,
    options: ServerOptions,
) -> Result<Server, TallyOwlError> {
    // Checked now, so a bad identity fails the start rather than every
    // connection.
    server_config(identity)?;
    serve_mutual(
        address,
        dispatcher,
        options,
        Arc::new(StaticIdentity(identity.clone())),
        Arc::new(StaticTrust(vec![identity.authority.clone()])),
    )
}

/// Serve CSIL-RPC with server-authenticated TLS: the listener shows the
/// current pair of `certificates`, and a client shows none. This is the
/// listener applications reach. See D62.
pub fn serve_server_auth(
    address: &str,
    dispatcher: Arc<dyn Dispatcher>,
    options: ServerOptions,
    certificates: Arc<CertificateSet>,
) -> Result<Server, TallyOwlError> {
    crate::serve_secured(
        address,
        dispatcher,
        options,
        Security::ServerAuth(server_auth_config(certificates)),
    )
}

/// Serve CSIL-RPC over mutual TLS. Each connection presents the current
/// identity and verifies the peer against the current authorities, so a
/// renewal or a new authority needs no restart. See D62.
pub fn serve_mutual(
    address: &str,
    dispatcher: Arc<dyn Dispatcher>,
    options: ServerOptions,
    identity: Arc<dyn IdentitySource>,
    trust: Arc<dyn TrustSource>,
) -> Result<Server, TallyOwlError> {
    let allow_anonymous = options.anonymous_allowed();
    crate::serve_secured(
        address,
        dispatcher,
        options,
        Security::Mutual {
            identity,
            trust,
            allow_anonymous,
        },
    )
}

/// What a client checks and what it shows.
#[derive(Clone)]
enum ClientMode {
    /// Verify the server against these roots, and show nothing.
    ServerAuth(Arc<ClientConfig>),
    /// Verify the server against the current authorities, and show the
    /// current identity. Read again for each connection.
    Mutual {
        identity: Arc<dyn IdentitySource>,
        trust: Arc<dyn TrustSource>,
    },
}

/// The client half of a secured connection: what to check, what to show, and
/// the name the server's certificate must carry.
#[derive(Clone)]
pub struct Secure {
    mode: ClientMode,
    server_name: String,
}

impl Secure {
    /// Mutual TLS with one fixed identity, verified against its own authority.
    pub fn new(identity: Identity) -> Result<Secure, TallyOwlError> {
        // Checked now, so a bad identity fails here rather than on the first
        // call.
        client_config(&identity)?;
        let server_name = identity.expected_server_name.clone();
        let authority = identity.authority.clone();
        Ok(Secure::mutual(
            server_name,
            Arc::new(StaticIdentity(identity)),
            Arc::new(StaticTrust(vec![authority])),
        ))
    }

    /// Server-authenticated TLS. `roots: None` means the roots of the
    /// operating system, which include a corporate authority an administrator
    /// installed there.
    pub fn server_auth(
        server_name: impl Into<String>,
        roots: Option<Vec<Vec<u8>>>,
    ) -> Result<Secure, TallyOwlError> {
        install_crypto_provider();
        let store = match roots {
            Some(authorities) => self::roots(&authorities)?,
            None => system_roots()?,
        };
        let config = ClientConfig::builder()
            .with_root_certificates(store)
            .with_no_client_auth();
        Ok(Secure {
            mode: ClientMode::ServerAuth(Arc::new(config)),
            server_name: server_name.into(),
        })
    }

    /// Mutual TLS with an identity and authorities that can change.
    pub fn mutual(
        server_name: impl Into<String>,
        identity: Arc<dyn IdentitySource>,
        trust: Arc<dyn TrustSource>,
    ) -> Secure {
        Secure {
            mode: ClientMode::Mutual { identity, trust },
            server_name: server_name.into(),
        }
    }

    /// The name the server's certificate must carry.
    pub(crate) fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Put a TLS session on an open socket and finish the handshake.
    pub(crate) fn connect(
        &self,
        address: &Address,
        socket: Socket,
    ) -> Result<TlsDuplex, TallyOwlError> {
        let config = match &self.mode {
            ClientMode::ServerAuth(config) => Arc::clone(config),
            ClientMode::Mutual { identity, trust } => {
                let current = identity.current().ok_or_else(|| {
                    TallyOwlError::unavailable(format!(
                        "This process has no enrolled identity yet, so it cannot connect to {address}. It connects once enrollment finishes."
                    ))
                })?;
                mutual_client_config(&current, &trust.authorities())?
            }
        };
        let name = ServerName::try_from(self.server_name.clone()).map_err(|e| {
            tls_failure(
                &format!("the server name `{}` was not usable", self.server_name),
                e,
            )
        })?;
        let connection = ClientConnection::new(config, name)
            .map_err(|e| tls_failure("the handshake could not start", e))?;
        let duplex = TlsDuplex::new(rustls::Connection::Client(connection), socket)
            .map_err(|e| tls_failure("the connection could not be prepared", e))?;
        duplex
            .handshake()
            .map_err(|e| handshake_failure(address, &self.server_name, &e))?;
        Ok(duplex)
    }
}

/// The roots of the operating system.
fn system_roots() -> Result<RootCertStore, TallyOwlError> {
    let found = rustls_native_certs::load_native_certs();
    let mut store = RootCertStore::empty();
    let (_added, _ignored) = store.add_parsable_certificates(found.certs);
    if store.is_empty() {
        let reason = found
            .errors
            .first()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "the operating system holds none".to_string());
        return Err(tls_failure(
            "no trusted authority was found on this host",
            format!("{reason}. Give this client the authority's certificate"),
        ));
    }
    Ok(store)
}

/// What a failed handshake means, in words a person can act on.
///
/// A certificate the client cannot accept, or a refusal from the server, fails
/// the same way on every attempt, so it is not retryable. A connection that
/// dropped during the handshake may work on the next attempt.
pub(crate) fn handshake_failure(
    address: &Address,
    server_name: &str,
    error: &std::io::Error,
) -> TallyOwlError {
    use rustls::CertificateError as Cert;
    let permanent = |message: String| {
        TallyOwlError::new(ErrorCode::FailedPrecondition, message).retryable(false)
    };
    let Some(tls) = rustls_error(error) else {
        // A peer that restarts closes the connection the same way, so this one
        // stays retryable. A plaintext listener does it on every attempt, and
        // the message says what to check.
        return TallyOwlError::unavailable(format!(
            "{address} closed the connection during the TLS handshake. If it serves plaintext only, it is not the TLS listener this client expects: ask its operator to give it certificates. {error}"
        ));
    };
    match tls {
        rustls::Error::InvalidCertificate(reason) => match reason {
            Cert::UnknownIssuer | Cert::BadSignature => permanent(format!(
                "{address} showed a certificate that no trusted authority signed. If it uses a private authority, give this client that authority's certificate."
            )),
            Cert::NotValidForName | Cert::NotValidForNameContext { .. } => permanent(format!(
                "{address} showed a certificate that is not for the name `{server_name}`. Connect with a name that its certificate carries, or set the server name to one."
            )),
            Cert::Expired | Cert::ExpiredContext { .. } => permanent(format!(
                "{address} showed a certificate that has expired. The operator of that service must replace it."
            )),
            Cert::NotValidYet | Cert::NotValidYetContext { .. } => permanent(format!(
                "{address} showed a certificate that is not valid yet. Check the clock on this host and on that one."
            )),
            other => permanent(format!(
                "{address} showed a certificate that this client cannot accept: {other:?}."
            )),
        },
        rustls::Error::AlertReceived(alert) => permanent(format!(
            "{address} refused this connection during the TLS handshake ({alert:?}). It did not accept this client's certificate, or it needs one that this client does not have."
        )),
        rustls::Error::InvalidMessage(_) => permanent(format!(
            "{address} did not answer with TLS. It may serve plaintext only. Ask its operator to give it certificates, or connect to its TLS address."
        )),
        other => permanent(format!("The secure connection to {address} failed: {other}.")),
    }
}

/// A client over mutual TLS.
///
/// This is [`Client`] with an identity. It is kept as its own name because a
/// caller that opens a secured connection is making a different statement from
/// one that opens a plain one, and a reader should see which is which.
pub struct TlsClient {
    inner: Client,
}

impl TlsClient {
    pub fn new(
        address: impl Into<String>,
        max_frame_bytes: usize,
        identity: Identity,
    ) -> Result<TlsClient, TallyOwlError> {
        Ok(TlsClient {
            inner: Client::secure(address, max_frame_bytes, identity)?,
        })
    }

    pub fn with_credential(self, credential: impl Into<String>) -> TlsClient {
        TlsClient {
            inner: self.inner.with_credential(credential),
        }
    }

    pub fn with_connect_timeout(self, timeout: Duration) -> TlsClient {
        TlsClient {
            inner: self.inner.with_connect_timeout(timeout),
        }
    }

    /// How long one socket read or one socket write may wait. See
    /// [`Client::with_io_timeout`].
    pub fn with_io_timeout(self, timeout: Duration) -> TlsClient {
        TlsClient {
            inner: self.inner.with_io_timeout(timeout),
        }
    }

    pub fn address(&self) -> &str {
        self.inner.address()
    }

    /// Invoke `service/op` over the secure connection.
    pub fn call(
        &self,
        service: &str,
        op: &str,
        payload: Vec<u8>,
    ) -> Result<csilgen_transport::rpc::RpcResponse, TallyOwlError> {
        self.inner.call(service, op, payload)
    }

    pub fn disconnect(&self) {
        self.inner.disconnect();
    }
}

/// The node ID a peer certificate names, for a service that wants to know who
/// it is talking to.
///
/// The common name is the node ID the control plane assigned, so this is the
/// identity the head issued and not one the peer chose.
pub fn peer_node_id(certificate: &[u8]) -> Option<String> {
    use x509_parser::prelude::*;
    let (_, parsed) = X509Certificate::from_der(certificate).ok()?;
    let name = parsed
        .subject()
        .iter_common_name()
        .next()?
        .as_str()
        .ok()?
        .to_string();
    Some(name)
}

/// Read a PEM private key into the DER form an [`Identity`] holds.
pub fn private_key_der(pem: &str) -> Result<Vec<u8>, TallyOwlError> {
    let key = rustls_pemfile::private_key(&mut pem.as_bytes())
        .map_err(|e| tls_failure("the private key could not be read", e))?
        .ok_or_else(|| tls_failure("the private key could not be read", "it holds no key"))?;
    Ok(key.secret_der().to_vec())
}

/// Ensure a process-wide crypto provider is installed.
///
/// `rustls` needs one and installs none by default. This is idempotent, so
/// every entry point can call it without coordinating.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A reader and a writer, so a test can drive a handshake without a socket.
pub trait DuplexStream: Read + Write + Send {}
impl<T: Read + Write + Send> DuplexStream for T {}
