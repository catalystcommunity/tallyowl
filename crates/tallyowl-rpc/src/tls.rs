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
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};

use crate::duplex::TlsDuplex;
use crate::{Client, Dispatcher, Server, Wire, DEFAULT_MAX_IN_FLIGHT};

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

fn roots(authority: &[u8]) -> Result<RootCertStore, TallyOwlError> {
    let mut store = RootCertStore::empty();
    store
        .add(CertificateDer::from(authority.to_vec()))
        .map_err(|e| tls_failure("the installation authority was not usable", e))?;
    Ok(store)
}

fn chain_of(identity: &Identity) -> Vec<CertificateDer<'static>> {
    identity
        .chain
        .iter()
        .map(|der| CertificateDer::from(der.clone()))
        .collect()
}

/// The server side: present this identity, and require one from every peer.
pub fn server_config(identity: &Identity) -> Result<Arc<ServerConfig>, TallyOwlError> {
    let verifier =
        rustls::server::WebPkiClientVerifier::builder(Arc::new(roots(&identity.authority)?))
            .build()
            .map_err(|e| tls_failure("the client verifier was not usable", e))?;
    let key = PrivateKeyDer::try_from(identity.private_key.clone())
        .map_err(|e| tls_failure("this process's own key was not usable", e))?;

    let config = ServerConfig::builder()
        // Mutual, not optional. A peer with no enrolled identity never reaches
        // a decoder.
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain_of(identity), key)
        .map_err(|e| tls_failure("this process's own certificate was not usable", e))?;
    Ok(Arc::new(config))
}

/// The client side: verify the peer against the authority, and present an
/// identity of our own.
pub fn client_config(identity: &Identity) -> Result<Arc<ClientConfig>, TallyOwlError> {
    let key = PrivateKeyDer::try_from(identity.private_key.clone())
        .map_err(|e| tls_failure("this process's own key was not usable", e))?;
    let config = ClientConfig::builder()
        .with_root_certificates(roots(&identity.authority)?)
        .with_client_auth_cert(chain_of(identity), key)
        .map_err(|e| tls_failure("this process's own certificate was not usable", e))?;
    Ok(Arc::new(config))
}

/// Serve CSIL-RPC over mutual TLS.
///
/// A connection serves as many correlated requests at a time as the plain path
/// does, over the same loop.
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
    let config = server_config(identity)?;
    let listener = TcpListener::bind(address)
        .map_err(|e| tls_failure(&format!("{address} could not be bound"), e))?;
    let local_address = listener
        .local_addr()
        .map_err(|e| tls_failure("the bound address could not be read", e))?;
    let stopping = Arc::new(AtomicBool::new(false));
    let loop_stopping = Arc::clone(&stopping);

    std::thread::Builder::new()
        .name("tallyowl-rpc-tls-accept".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if loop_stopping.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let dispatcher = Arc::clone(&dispatcher);
                let config = Arc::clone(&config);
                let connection_stopping = Arc::clone(&loop_stopping);
                std::thread::spawn(move || {
                    let Ok(connection) = ServerConnection::new(config) else {
                        return;
                    };
                    let Ok(duplex) = TlsDuplex::new(rustls::Connection::Server(connection), stream)
                    else {
                        return;
                    };
                    // Finish the handshake before a frame is read. A peer with
                    // no enrolled identity is refused here and never reaches a
                    // decoder.
                    if duplex.handshake().is_err() {
                        return;
                    }
                    crate::serve_connection(
                        Wire::Secure(duplex.clone()),
                        Wire::Secure(duplex),
                        dispatcher,
                        max_frame_bytes,
                        max_in_flight,
                        connection_stopping,
                    );
                });
            }
        })
        .map_err(|e| tls_failure("the accept thread would not start", e))?;

    Ok(Server::new(local_address, stopping))
}

/// The client half of a secured connection: what to present, and what name to
/// verify the peer against.
///
/// This is what `Client::secure` and `Pipeline::secure` hold. It builds the
/// rustls configuration once, so opening a connection does not rebuild a
/// verifier for each attempt.
#[derive(Clone)]
pub struct Secure {
    config: Arc<ClientConfig>,
    server_name: String,
}

impl Secure {
    pub fn new(identity: Identity) -> Result<Secure, TallyOwlError> {
        Ok(Secure {
            config: client_config(&identity)?,
            server_name: identity.expected_server_name,
        })
    }

    /// Put a TLS session on an open socket and finish the handshake.
    pub(crate) fn connect(&self, stream: TcpStream) -> Result<TlsDuplex, TallyOwlError> {
        let name = ServerName::try_from(self.server_name.clone())
            .map_err(|e| tls_failure("the peer name was not usable", e))?;
        let connection = ClientConnection::new(Arc::clone(&self.config), name)
            .map_err(|e| tls_failure("the handshake could not start", e))?;
        let duplex = TlsDuplex::new(rustls::Connection::Client(connection), stream)
            .map_err(|e| tls_failure("the connection could not be prepared", e))?;
        duplex
            .handshake()
            .map_err(|e| TallyOwlError::unavailable(format!("The peer refused us. {e}")))?;
        Ok(duplex)
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
