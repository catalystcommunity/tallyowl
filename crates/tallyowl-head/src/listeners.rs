//! How the head's ingest listener is secured. D62.
//!
//! Collectors reach `head.listen`. On a loopback or `unix:` address nothing
//! crosses a network, the connection is plaintext, and the peer is `Local`. On
//! any other address a collector proves itself with its enrolled certificate,
//! and so does the head: mutual TLS against the installation's authorities.
//! That is what lets the head check which collector sent a batch.

use std::sync::Arc;

use tallyowl_obs::error::TallyOwlError;
use tallyowl_rpc::material::IdentitySource;
use tallyowl_rpc::trust::TrustSource;
use tallyowl_rpc::{Address, Dispatcher, Server, ServerOptions};

/// This node's identity and the authorities it trusts, when it has a
/// mutual-TLS hop.
pub struct NodeSecurity {
    pub identity: Arc<dyn IdentitySource>,
    pub trust: Arc<dyn TrustSource>,
    /// Called for each connection refused during the TLS handshake.
    pub on_handshake_refused: Option<tallyowl_rpc::HandshakeHook>,
}

/// Connections the ingest listener refused during the TLS handshake.
pub const HANDSHAKES_REFUSED: &str = "tallyowl_tls_handshakes_refused_total";

/// Declare the counter, and return the hook that feeds it under the label
/// `listener="head-ingest"`.
pub fn handshake_counter(
    metrics: &Arc<tallyowl_obs::metrics::Registry>,
) -> tallyowl_rpc::HandshakeHook {
    let _ = metrics.declare(
        HANDSHAKES_REFUSED,
        tallyowl_obs::MetricKind::Counter,
        "Connections refused during the TLS handshake: a plaintext client, or a certificate no trusted authority signed. A steady rise is a misconfigured node or a stranger.",
        &[],
    );
    let metrics = Arc::clone(metrics);
    Arc::new(move || {
        metrics.increment(
            HANDSHAKES_REFUSED,
            &tallyowl_obs::metrics::labels(&[("listener", "head-ingest")]),
        );
    })
}

/// How the ingest listener is exposed, as the startup log says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestExposure {
    Local,
    Mutual,
    Plaintext,
}

impl IngestExposure {
    pub fn describe(self) -> &'static str {
        match self {
            IngestExposure::Local => "plaintext, because the address is loopback or a unix socket",
            IngestExposure::Mutual => {
                "mutual TLS, with this node's certificate and `installation.authorities`"
            }
            IngestExposure::Plaintext => {
                "plaintext on a network address, because `transport.allowPlaintext` is true"
            }
        }
    }
}

/// Decide how `head.listen` is exposed.
pub fn ingest_exposure(
    address: &str,
    has_identity: bool,
    allow_plaintext: bool,
) -> Result<IngestExposure, TallyOwlError> {
    if Address::parse(address)?.plaintext_permitted() {
        return Ok(IngestExposure::Local);
    }
    if has_identity {
        return Ok(IngestExposure::Mutual);
    }
    if allow_plaintext {
        return Ok(IngestExposure::Plaintext);
    }
    Err(TallyOwlError::invalid_argument(format!(
        "`head.listen` is `{address}`, which collectors reach over a network, and this head has no identity to show. Set `installation.authorities` and either `installation.signingCertificate` and `installation.signingKey` or `enrollment.roleToken`, listen on a loopback or `unix:` address, or set `transport.allowPlaintext: true` if something else protects this network."
    )))
}

/// Serve head ingest by the rule.
pub fn serve_ingest(
    address: &str,
    dispatcher: Arc<dyn Dispatcher>,
    max_frame_bytes: usize,
    node: Option<&NodeSecurity>,
    allow_plaintext: bool,
) -> Result<(Server, IngestExposure), TallyOwlError> {
    let exposure = ingest_exposure(address, node.is_some(), allow_plaintext)?;
    let server = match (exposure, node) {
        (IngestExposure::Mutual, Some(node)) => tallyowl_rpc::tls::serve_mutual(
            address,
            dispatcher,
            // A collector that has no certificate yet reaches `enroll-node`
            // here, and an operator's client proves itself with a session
            // token. Each operation that needs a proved node refuses an
            // anonymous peer itself. See D62 and `HeadService::collectors_only`.
            match &node.on_handshake_refused {
                Some(hook) => ServerOptions::new(max_frame_bytes)
                    .allow_anonymous(true)
                    .on_handshake_refused(Arc::clone(hook)),
                None => ServerOptions::new(max_frame_bytes).allow_anonymous(true),
            },
            Arc::clone(&node.identity),
            Arc::clone(&node.trust),
        )?,
        _ => tallyowl_rpc::serve(address, dispatcher, max_frame_bytes).map_err(|e| {
            TallyOwlError::unavailable(format!(
                "Ingest could not listen on `{address}` (`head.listen`). {e}"
            ))
        })?,
    };
    Ok((server, exposure))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_ingest_is_plaintext_and_a_network_one_needs_an_identity() {
        assert_eq!(
            ingest_exposure("127.0.0.1:5110", false, false).unwrap(),
            IngestExposure::Local
        );
        assert_eq!(
            ingest_exposure("unix:/run/tallyowl/head.sock", false, false).unwrap(),
            IngestExposure::Local
        );
        assert_eq!(
            ingest_exposure("0.0.0.0:5110", true, false).unwrap(),
            IngestExposure::Mutual
        );
        assert_eq!(
            ingest_exposure("0.0.0.0:5110", false, true).unwrap(),
            IngestExposure::Plaintext
        );
        let refused = ingest_exposure("0.0.0.0:5110", false, false).unwrap_err();
        assert!(
            refused.message.contains("head.listen"),
            "{}",
            refused.message
        );
        assert!(
            refused.message.contains("installation.authorities"),
            "{}",
            refused.message
        );
    }

    #[test]
    fn a_network_ingest_with_no_identity_does_not_listen() {
        let dispatcher: Arc<dyn Dispatcher> =
            Arc::new(|_: &tallyowl_rpc::Request| tallyowl_rpc::unknown_operation("x", "y"));
        assert!(serve_ingest("0.0.0.0:0", dispatcher, 1024, None, false).is_err());
    }
}
