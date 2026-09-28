//! How the driver reaches the collector. D62.
//!
//! - A `unix:<path>` address or a loopback address is reached in plaintext,
//!   because nothing crosses a network.
//! - Any other address is reached over TLS, and the collector's certificate is
//!   checked against the authorities the operating system trusts.
//! - Plaintext to any other address needs `allow_plaintext`.
//!
//! The project key is still how the application proves itself. TLS is how the
//! collector proves itself, and it keeps the key off the network in the clear.

use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_rpc::{Address, Pipeline};

/// TLS settings that replace the default ones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsSettings {
    /// The authorities to trust, in DER form. Nothing means the authorities of
    /// the operating system.
    pub roots: Option<Vec<Vec<u8>>>,
    /// The name the collector's certificate must carry. Nothing means the host
    /// part of the address.
    pub server_name: Option<String>,
}

/// How the driver reaches the collector. The default is the secure one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transport {
    /// Use TLS with these settings, on every address, loopback included.
    pub tls: Option<TlsSettings>,
    /// Send plaintext to an address that is not loopback. Use it only on a
    /// network that something else protects.
    pub allow_plaintext: bool,
}

impl Transport {
    /// Trust the authorities in this PEM text, and no others. Use it when the
    /// collector's certificate comes from a private authority.
    pub fn with_roots_pem(self, pem: &[u8]) -> Result<Transport, TallyOwlError> {
        let mut reader = pem;
        let mut roots = Vec::new();
        for certificate in rustls_pemfile::certs(&mut reader) {
            let certificate = certificate.map_err(|e| {
                TallyOwlError::invalid_argument(format!(
                    "The authority certificate could not be read as PEM: {e}"
                ))
            })?;
            roots.push(certificate.to_vec());
        }
        if roots.is_empty() {
            return Err(TallyOwlError::invalid_argument(
                "The text holds no certificate. Give the PEM text of the authority that signed the collector's certificate, which starts with `-----BEGIN CERTIFICATE-----`.",
            ));
        }
        Ok(self.with_roots_der(roots))
    }

    /// Trust these authorities, in DER form, and no others.
    pub fn with_roots_der(mut self, roots: Vec<Vec<u8>>) -> Transport {
        self.tls.get_or_insert_with(TlsSettings::default).roots = Some(roots);
        self
    }

    /// Check the collector's certificate against this name rather than the
    /// host part of the address.
    pub fn with_server_name(mut self, name: impl Into<String>) -> Transport {
        self.tls
            .get_or_insert_with(TlsSettings::default)
            .server_name = Some(name.into());
        self
    }

    /// Send plaintext to a network address.
    pub fn allowing_plaintext(mut self) -> Transport {
        self.allow_plaintext = true;
        self
    }
}

/// What one address and one transport setting mean. Deciding opens nothing, so
/// the whole decision is testable without a network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DialPlan {
    Plaintext,
    Tls {
        server_name: String,
        roots: Option<Vec<Vec<u8>>>,
    },
}

pub(crate) fn plan_dial(address: &str, transport: &Transport) -> Result<DialPlan, TallyOwlError> {
    let parsed = Address::parse(address)?;
    let settings = match &transport.tls {
        Some(settings) => settings.clone(),
        None if parsed.plaintext_permitted() || transport.allow_plaintext => {
            return Ok(DialPlan::Plaintext)
        }
        None => TlsSettings::default(),
    };
    let server_name = match (settings.server_name, parsed.host()) {
        (Some(name), _) => name,
        (None, Some(host)) => host.to_string(),
        (None, None) => {
            return Err(TallyOwlError::invalid_argument(format!(
                "TLS to {parsed} needs a server name, because a socket file has no host name. Set it with `Transport::with_server_name`."
            )))
        }
    };
    Ok(DialPlan::Tls {
        server_name,
        roots: settings.roots,
    })
}

/// The pipeline for one plan.
pub(crate) fn pipeline(
    address: &str,
    max_frame_bytes: usize,
    window: usize,
    transport: &Transport,
) -> Result<Pipeline, TallyOwlError> {
    match plan_dial(address, transport)? {
        DialPlan::Plaintext => Ok(Pipeline::new(address.to_string(), max_frame_bytes, window)),
        DialPlan::Tls { server_name, roots } => Pipeline::server_auth(
            address.to_string(),
            max_frame_bytes,
            window,
            &server_name,
            roots,
        ),
    }
}

/// Whether a failure is a TLS handshake that the collector will refuse the same
/// way next time. Nothing was sent, so no batch pays an attempt for it.
pub(crate) fn is_handshake_refusal(error: &TallyOwlError) -> bool {
    error.code == ErrorCode::FailedPrecondition && !error.retryable
}

/// The same refusal, with what a developer changes in this driver.
pub(crate) fn explain_handshake(error: TallyOwlError) -> TallyOwlError {
    if !is_handshake_refusal(&error) {
        return error;
    }
    let hint = if error.message.contains("no trusted authority") {
        " In this driver, give the authority with `Transport::with_roots_pem`."
    } else if error.message.contains("not for the name") {
        " In this driver, set the name with `Transport::with_server_name`."
    } else if error.message.contains("did not answer with TLS") {
        " If something else protects this network, `Transport::allowing_plaintext` sends plaintext."
    } else {
        ""
    };
    let message = format!("{}{hint}", error.message);
    TallyOwlError { message, ..error }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tls(server_name: &str) -> DialPlan {
        DialPlan::Tls {
            server_name: server_name.to_string(),
            roots: None,
        }
    }

    #[test]
    fn the_dial_plan_follows_d62() {
        let default = Transport::default();
        let cases: Vec<(&str, &str, Transport, Result<DialPlan, ()>)> = vec![
            (
                "a unix socket is plaintext",
                "unix:/run/collector.sock",
                default.clone(),
                Ok(DialPlan::Plaintext),
            ),
            (
                "loopback is plaintext",
                "127.0.0.1:5100",
                default.clone(),
                Ok(DialPlan::Plaintext),
            ),
            (
                "IPv6 loopback is plaintext",
                "[::1]:5100",
                default.clone(),
                Ok(DialPlan::Plaintext),
            ),
            (
                "localhost is plaintext",
                "localhost:5100",
                default.clone(),
                Ok(DialPlan::Plaintext),
            ),
            (
                "a network address is TLS by default",
                "collector.internal:5100",
                default.clone(),
                Ok(tls("collector.internal")),
            ),
            (
                "a private address is still a network address",
                "10.0.0.7:5100",
                default.clone(),
                Ok(tls("10.0.0.7")),
            ),
            (
                "plaintext to a network address needs the setting",
                "collector.internal:5100",
                default.clone().allowing_plaintext(),
                Ok(DialPlan::Plaintext),
            ),
            (
                "a TLS setting applies even on loopback",
                "127.0.0.1:5100",
                default.clone().with_server_name("collector"),
                Ok(tls("collector")),
            ),
            (
                "TLS on a socket file needs a name",
                "unix:/run/collector.sock",
                default.clone().with_roots_der(vec![vec![1]]),
                Err(()),
            ),
            (
                "an address with no port is refused",
                "collector",
                default.clone(),
                Err(()),
            ),
            (
                "`unix:` with no path is refused",
                "unix:",
                default.clone(),
                Err(()),
            ),
        ];
        for (name, address, transport, want) in cases {
            let got = plan_dial(address, &transport).map_err(|_| ());
            assert_eq!(got, want, "{name}");
        }
        assert_eq!(
            plan_dial("127.0.0.1:5100", &default.with_roots_der(vec![vec![7]])),
            Ok(DialPlan::Tls {
                server_name: "127.0.0.1".to_string(),
                roots: Some(vec![vec![7]]),
            }),
            "the roots reach the plan"
        );
    }

    #[test]
    fn pem_text_with_no_certificate_is_refused_in_words() {
        let error = Transport::default()
            .with_roots_pem(b"not a certificate")
            .unwrap_err();
        assert!(
            error.message.contains("BEGIN CERTIFICATE"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_handshake_refusal_says_what_to_change_in_this_driver() {
        let refusal = TallyOwlError::new(
            ErrorCode::FailedPrecondition,
            "collector:5100 showed a certificate that no trusted authority signed.",
        )
        .retryable(false);
        assert!(explain_handshake(refusal)
            .message
            .contains("with_roots_pem"));
        let unavailable = TallyOwlError::unavailable("gone");
        assert_eq!(explain_handshake(unavailable.clone()), unavailable);
    }
}
