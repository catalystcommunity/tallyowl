//! The connection to this installation's LinkKeys RP server.
//!
//! D7 makes a domain-backed relying party the default. In that mode the
//! relying party in the protocol is a LinkKeys server with its RP function on,
//! usually the IDP of the installation's own home domain. It holds the domain
//! keys, signs the outgoing request, and decrypts the token that comes back.
//! TallyOwl holds only an API key that lets it call four operations of the `Rp`
//! service, and never a private key.
//!
//! The connection is CSIL-RPC over TLS on the LinkKeys TCP port. A LinkKeys
//! certificate is self-signed, so the trust anchor is the set of configured
//! fingerprints of the server's Ed25519 key, not a certificate authority and
//! not the host name. `linkkeys-rpc-client` builds that pinned TLS
//! configuration; its own `send_request` refuses private addresses, which is
//! right for a server's outbound calls and wrong here, because the RP server is
//! usually reached by its in-cluster Service address.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use csilgen_transport::carrier::StreamCarrier;
use csilgen_transport::rpc::RpcClient;

/// The largest reply the head accepts from the RP server. The LinkKeys server
/// and its own client use the same cap.
const MAX_FRAME: usize = 1024 * 1024;

/// One deadline for each stage of a call. A sign-in waits on this, and a
/// person is looking at the page.
const TIMEOUT: Duration = Duration::from_secs(10);

/// A call to one operation of the `Rp` service.
///
/// The sign-in code speaks to this rather than to a socket, so that its tests
/// can answer as an RP server would without a listener.
pub trait RpCaller: Send + Sync {
    /// Send an encoded request to `Rp/<op>` and return the encoded reply. The
    /// error is a sentence for the log. It never contains the API key.
    fn call(&self, op: &str, payload: Vec<u8>) -> Result<Vec<u8>, String>;
}

/// The RP server, over pinned TLS.
pub struct TcpRpCaller {
    address: String,
    fingerprints: Vec<String>,
    api_key: String,
}

impl TcpRpCaller {
    pub fn new(address: String, fingerprints: Vec<String>, api_key: String) -> Self {
        Self {
            address,
            fingerprints: fingerprints
                .into_iter()
                .map(|fingerprint| fingerprint.trim().to_ascii_lowercase())
                .filter(|fingerprint| !fingerprint.is_empty())
                .collect(),
            api_key,
        }
    }

    fn connect(&self) -> Result<TcpStream, String> {
        let addresses = self.address.to_socket_addrs().map_err(|e| {
            format!(
                "The RP server address {} did not resolve: {e}",
                self.address
            )
        })?;
        let mut last = format!("The RP server address {} resolved to nothing", self.address);
        for address in addresses {
            match TcpStream::connect_timeout(&address, TIMEOUT) {
                Ok(stream) => {
                    stream
                        .set_read_timeout(Some(TIMEOUT))
                        .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
                        .map_err(|e| format!("The RP server connection failed: {e}"))?;
                    return Ok(stream);
                }
                Err(e) => last = format!("The RP server at {address} did not answer: {e}"),
            }
        }
        Err(last)
    }
}

impl RpCaller for TcpRpCaller {
    fn call(&self, op: &str, payload: Vec<u8>) -> Result<Vec<u8>, String> {
        if self.fingerprints.is_empty() {
            return Err(
                "No fingerprint is configured for the RP server, so nothing can be \
                        trusted. Set `linkkeys.rp.fingerprints`."
                    .into(),
            );
        }
        let config = linkkeys_rpc_client::tls::client_config(self.fingerprints.clone(), None)
            .map_err(|e| format!("The TLS configuration for the RP server failed: {e}"))?;
        let host = linkkeys_rpc_client::extract_hostname(&self.address).to_string();
        let name = rustls::pki_types::ServerName::try_from(host)
            .map_err(|e| format!("The RP server address is not a valid TLS name: {e}"))?;
        let connection = rustls::ClientConnection::new(Arc::clone(&config), name)
            .map_err(|e| format!("The TLS connection to the RP server failed: {e}"))?;
        let stream = rustls::StreamOwned::new(connection, self.connect()?);

        let carrier = StreamCarrier::with_max_frame(Pinned(stream), MAX_FRAME)
            .map_err(|e| format!("The RP server connection failed: {e}"))?;
        let mut client = RpcClient::new(carrier, false);
        let reply = client
            .call("Rp", op, payload, Some(self.api_key.clone()))
            .map_err(|e| format!("Rp/{op} failed: {e}"))?;
        Ok(reply.payload)
    }
}

/// The TLS stream, with a name that says why it is safe: the handshake checks
/// the server key against the pinned fingerprints before a byte of the request
/// is written.
struct Pinned(rustls::StreamOwned<rustls::ClientConnection, TcpStream>);

impl Read for Pinned {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Pinned {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liblinkkeys::generated::types::RpSignRequest;
    use liblinkkeys::generated::{decode_rp_sign_response, encode_rp_sign_request};

    fn sign_request() -> Vec<u8> {
        encode_rp_sign_request(&RpSignRequest {
            callback_url: "https://tallyowl.example/sign-in/callback".into(),
            nonce: "00112233445566778899aabbccddeeff".into(),
            requested_claims: None,
            authentication_requirements: None,
            flow_context: None,
        })
    }

    #[test]
    fn no_fingerprint_means_no_connection() {
        // An empty pin set would trust nothing, so the call stops before it
        // dials. The address is one nothing listens on.
        let caller = TcpRpCaller::new("127.0.0.1:1".into(), vec![" ".into()], "k".into());
        let failure = caller.call("sign-request", sign_request()).unwrap_err();
        assert!(failure.contains("linkkeys.rp.fingerprints"), "{failure}");
    }

    /// A deployed RP server signs a request for this API key. It checks the
    /// key, its `api_access` relation, and the pinned TLS, end to end. Opt in:
    ///
    /// ```sh
    /// LIVE_RP_ADDRESS=127.0.0.1:4987 LIVE_RP_FINGERPRINTS=<fp>,<fp> \
    /// LIVE_RP_API_KEY_FILE=<file> cargo test -p tallyowl-head --lib \
    ///   linkkeys_rp::tests::a_deployed_rp_server_signs -- --ignored
    /// ```
    #[test]
    #[ignore = "needs a deployed LinkKeys RP server; see the comment"]
    fn a_deployed_rp_server_signs_a_request_for_this_key() {
        let variable = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let key = std::fs::read_to_string(variable("LIVE_RP_API_KEY_FILE")).expect("the key file");
        let caller = TcpRpCaller::new(
            variable("LIVE_RP_ADDRESS"),
            variable("LIVE_RP_FINGERPRINTS")
                .split(',')
                .map(str::to_string)
                .collect(),
            key.trim().to_string(),
        );
        let reply = caller
            .call("sign-request", sign_request())
            .unwrap_or_else(|e| panic!("{e}"));
        let signed = decode_rp_sign_response(&reply).expect("a sign response");
        assert!(!signed.signed_request.is_empty());
    }
}
