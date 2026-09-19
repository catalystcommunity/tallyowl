//! Sending an alert notification, and the two channels that exist.
//!
//! `docs/ALERTS.md` section 6 and D13. Two channels and no more:
//!
//! - a generic **signed webhook** for an outside system;
//! - a native **CSIL callback** for a service that already holds a connection.
//!
//! A downstream system builds email or a chat service on the webhook. TallyOwl
//! does not build it. That is a deliberate boundary rather than a gap: every
//! chat product has its own shape, its own retry behaviour, and its own
//! credential handling, and a product that carried three of them would be
//! maintaining three integrations that its own users can write in an afternoon.
//!
//! # What a notification carries, and what it never carries
//!
//! The rule, the state, the observed value, the evaluation time, and a link to
//! the query. **A notification never carries telemetry rows.** A webhook goes
//! to an outside system over a network TallyOwl does not own, and a row can
//! hold anything an application put in it.
//!
//! # Why it is signed
//!
//! A receiver has to be able to tell a notification from TallyOwl apart from an
//! HTTP request somebody else sent to the same address. The signature covers
//! the timestamp and the body, so a replay of an old body is visible as an old
//! timestamp rather than accepted as a fresh alert.
//!
//! # A failed delivery never changes the alert state
//!
//! Section 6 states it and it is worth saying why: the state is what the data
//! said, and whether a webhook answered is a fact about the network. A delivery
//! failure that cleared a firing alert would turn an outage in the receiver
//! into an all-clear.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use tallyowl_obs::error::TallyOwlError;

/// How long a delivery attempt waits before it gives up on this attempt.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// The header that carries the signature.
pub const SIGNATURE_HEADER: &str = "tallyowl-signature";
/// The header that carries the moment the body was signed.
pub const TIMESTAMP_HEADER: &str = "tallyowl-timestamp";

/// What one notification says.
///
/// It is built once and sent to every target of one rule, so two receivers of
/// one alert cannot be told two different things.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub rule_id: String,
    pub rule_name: String,
    pub project_id: String,
    pub state: String,
    pub outcome: String,
    pub observed_value: Option<f64>,
    pub evaluated_at: i64,
    /// Where a person goes to see the query this alert runs.
    pub query_link: String,
    pub reason: String,
    /// True when this is a repeat while the rule stays firing rather than a
    /// change. A receiver that pages somebody reads it.
    pub escalation: bool,
}

impl Notification {
    /// The JSON body a webhook receives.
    ///
    /// **JSON rather than CBOR, and this is the one place.** A webhook goes to
    /// something TallyOwl did not write and cannot ask to speak its wire
    /// format. Everything inside TallyOwl stays CSIL.
    pub fn body(&self) -> String {
        let value = match self.observed_value {
            Some(value) => format!("{value}"),
            None => "null".to_string(),
        };
        format!(
            "{{\"rule_id\":{},\"rule\":{},\"project\":{},\"state\":{},\"outcome\":{},\"value\":{value},\"evaluated_at\":{},\"query\":{},\"reason\":{},\"escalation\":{}}}",
            quote(&self.rule_id),
            quote(&self.rule_name),
            quote(&self.project_id),
            quote(&self.state),
            quote(&self.outcome),
            self.evaluated_at,
            quote(&self.query_link),
            quote(&self.reason),
            self.escalation
        )
    }
}

/// A JSON string, with the five characters that need escaping and nothing else.
///
/// A rule name is a person's own words and can hold a quotation mark. A body
/// that broke on one would be a notification nobody received.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The signature a receiver checks.
///
/// It is a keyed BLAKE3 hash over the timestamp and the body, in that order,
/// separated by a full stop. Keyed BLAKE3 is a message authentication code;
/// D44 already keeps the crate in the workspace for a segment content address.
///
/// **The timestamp is inside the signature.** Signing the body alone would let
/// somebody replay a firing notification a week later and have it check out.
pub fn sign(secret: &str, timestamp: i64, body: &str) -> String {
    // A key is exactly 32 bytes, and an operator's secret is text of any
    // length, so the secret is hashed to a key first.
    let key: [u8; 32] = *blake3::hash(secret.as_bytes()).as_bytes();
    let mut hasher = blake3::Hasher::new_keyed(&key);
    hasher.update(timestamp.to_string().as_bytes());
    hasher.update(b".");
    hasher.update(body.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// What one attempt did.
#[derive(Debug, Clone, PartialEq)]
pub struct Attempt {
    pub delivered: bool,
    pub detail: String,
    /// True when trying again could work. A refused address never becomes a
    /// good one, and retrying it for a day is a queue full of work that cannot
    /// succeed.
    pub retryable: bool,
}

impl Attempt {
    pub fn delivered(detail: impl Into<String>) -> Attempt {
        Attempt {
            delivered: true,
            detail: detail.into(),
            retryable: false,
        }
    }

    pub fn failed(detail: impl Into<String>, retryable: bool) -> Attempt {
        Attempt {
            delivered: false,
            detail: detail.into(),
            retryable,
        }
    }
}

/// Somewhere a notification can be sent.
///
/// The trait exists so a test can drive the retry behaviour against a channel
/// that refuses, stalls, or answers late, without a network. It is a stand-in
/// for somebody else's server rather than a mock of TallyOwl's own storage,
/// which `AGENTS.md` forbids for a different reason.
pub trait Channel: Send + Sync {
    fn send(&self, notification: &Notification) -> Attempt;
    /// What an operator reads in the interface.
    fn name(&self) -> String;
}

/// A signed HTTP webhook.
pub struct Webhook {
    pub url: String,
    pub secret: String,
    pub timeout: Duration,
}

impl Channel for Webhook {
    fn name(&self) -> String {
        format!("webhook {}", self.url)
    }

    fn send(&self, notification: &Notification) -> Attempt {
        let body = notification.body();
        let timestamp = tallyowl_obs::time::now_ms();
        let signature = sign(&self.secret, timestamp, &body);
        match post(&self.url, &body, timestamp, &signature, self.timeout) {
            Ok(status) if (200..300).contains(&status) => {
                Attempt::delivered(format!("The receiver answered {status}."))
            }
            // A receiver that says the request was wrong will say it again.
            // Retrying a 4xx for a day fills a queue with work that cannot
            // succeed, and hides the real failures behind it.
            Ok(status) if (400..500).contains(&status) && status != 408 && status != 429 => {
                Attempt::failed(
                    format!(
                        "The receiver at {} refused this notification with {status}, and it will refuse it again. Check the address and the secret.",
                        self.url
                    ),
                    false,
                )
            }
            Ok(status) => Attempt::failed(
                format!("The receiver at {} answered {status}.", self.url),
                true,
            ),
            Err(failure) => Attempt::failed(failure.message, true),
        }
    }
}

/// A native CSIL callback, for a service that already speaks to TallyOwl.
///
/// It carries the same notification. The difference is the transport and the
/// fact that the receiver is inside the installation, so there is no signature:
/// the connection is the authentication, exactly as it is for every other
/// node-to-node call.
pub struct CsilCallback {
    pub address: String,
    pub timeout: Duration,
    pub sender: std::sync::Arc<dyn CallbackSender>,
}

/// What actually delivers a native callback.
///
/// It is behind a trait because the head cannot depend on the shape of whatever
/// service is listening, and because a test needs to drive the retry behaviour
/// without a second process.
pub trait CallbackSender: Send + Sync {
    fn deliver(&self, address: &str, body: &[u8], timeout: Duration) -> Attempt;
}

/// The operation a receiving service answers.
///
/// **A receiver declares this operation and nothing else.** It is one operation
/// on the service TallyOwl already speaks, so a service that already holds a
/// connection adds a handler rather than an HTTP endpoint, a signature check,
/// and a JSON parser.
pub const CALLBACK_SERVICE: &str = "TallyOwlAlertReceiver";
pub const CALLBACK_OPERATION: &str = "notify";

/// Deliver a native callback over CSIL-RPC.
///
/// **There is no signature.** A webhook is signed because it crosses to a system
/// TallyOwl did not issue a certificate to; a native callback runs over the same
/// mutual TLS every other node-to-node hop uses, and there the connection *is*
/// the authentication. Signing it as well would be a second answer to a question
/// already answered, and the second one would be the one somebody trusted after
/// the first was misconfigured.
pub struct RpcCallbacks {
    pub max_frame_bytes: usize,
}

impl CallbackSender for RpcCallbacks {
    fn deliver(&self, address: &str, body: &[u8], _timeout: Duration) -> Attempt {
        let client = tallyowl_rpc::Client::new(address.to_string(), self.max_frame_bytes);
        match client.call(CALLBACK_SERVICE, CALLBACK_OPERATION, body.to_vec()) {
            Ok(_) => Attempt::delivered(format!("`{address}` took the notification.")),
            Err(failure) => Attempt::failed(
                format!(
                    "`{address}` did not take the notification. {}",
                    failure.message
                ),
                // A service that does not have the operation will not grow it
                // by being asked again, and retrying for a day would fill the
                // queue with work that cannot succeed.
                failure.retryable,
            ),
        }
    }
}

impl Channel for CsilCallback {
    fn name(&self) -> String {
        format!("callback {}", self.address)
    }

    fn send(&self, notification: &Notification) -> Attempt {
        self.sender
            .deliver(&self.address, notification.body().as_bytes(), self.timeout)
    }
}

/// How long to wait before attempt `n`, with jitter, capped.
///
/// `docs/ALERTS.md` section 6: "Delivery retries with capped jitter." The cap
/// is what stops a receiver that has been down for an hour from being asked
/// once a second for that hour, and the jitter is what stops a thousand rules
/// asking again at the same instant when it comes back. L012 records the same
/// mistake on the delivery path.
pub fn backoff_ms(attempt: u64, seed: u64) -> i64 {
    const BASE_MS: u64 = 1_000;
    const CAP_MS: u64 = 300_000;
    let step = BASE_MS.saturating_mul(1u64 << attempt.min(12)).min(CAP_MS);
    // Up to a quarter of the step, either way, so two receivers that failed
    // together do not come back together.
    let spread = step / 4;
    let jitter = match spread {
        0 => 0,
        spread => (seed % (spread * 2)) as i64 - spread as i64,
    };
    (step as i64 + jitter).max(1)
}

/// A minimal HTTP POST. The same shape the compatibility scraper uses for a
/// GET, and for the same reason: one outbound HTTP client, no dependency.
fn post(
    url: &str,
    body: &str,
    timestamp: i64,
    signature: &str,
    timeout: Duration,
) -> Result<u16, TallyOwlError> {
    let (secure, host, port, path) = split_url(url)?;
    let address = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| {
            TallyOwlError::unavailable(format!("The address `{url}` could not be resolved. {e}"))
        })?
        .next()
        .ok_or_else(|| {
            TallyOwlError::unavailable(format!("The address `{url}` resolved to no address."))
        })?;

    let mut stream = connect(secure, &host, address, timeout, url)?;

    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{SIGNATURE_HEADER}: {signature}\r\n{TIMESTAMP_HEADER}: {timestamp}\r\nUser-Agent: tallyowl-head\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|e| {
        TallyOwlError::unavailable(format!("The notification to `{url}` could not be sent. {e}"))
    })?;

    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => raw.extend_from_slice(&chunk[..read]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset && !raw.is_empty() => break,
            Err(e) => {
                return Err(TallyOwlError::unavailable(format!(
                    "The notification to `{url}` ended early. {e}"
                )))
            }
        }
        if raw.len() > 64 * 1024 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    text.split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| {
            TallyOwlError::unavailable(format!("The receiver at `{url}` sent no status line."))
        })
}

/// Split an address into a scheme, a host, a port, and a path.
///
/// **An address that asks for TLS gets TLS.** Quietly downgrading `https` to a
/// plaintext request would send what an alert observed to the network the
/// operator asked to be protected from, and say nothing about having done so.
/// This used to refuse such an address for want of an outbound TLS client; it
/// has one now. See L142.
fn split_url(url: &str) -> Result<(bool, String, u16, String), TallyOwlError> {
    let (secure, rest, default_port) = match url.strip_prefix("https://") {
        Some(rest) => (true, rest, 443u16),
        None => match url.strip_prefix("http://") {
            Some(rest) => (false, rest, 80u16),
            None => {
                return Err(TallyOwlError::invalid_argument(format!(
                    "`{url}` is not an address TallyOwl can send a webhook to. Write a full address, such as `https://example.test/alerts`."
                )))
            }
        },
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse().map_err(|_| {
                TallyOwlError::invalid_argument(format!("`{port}` is not a port number."))
            })?,
        ),
        None => (authority.to_string(), default_port),
    };
    if host.is_empty() {
        return Err(TallyOwlError::invalid_argument(format!(
            "`{url}` names no host."
        )));
    }
    Ok((secure, host, port, path))
}

/// The public trust roots, built once.
///
/// **These are for the one outbound connection TallyOwl makes to something it
/// does not own.** Every other TLS hop in this system verifies against the
/// installation's own authority, and using a public root store there would
/// accept any certificate on the internet as a TallyOwl node.
///
/// The platform store comes first, so an operator whose webhook receiver uses a
/// private certificate authority adds a root the ordinary way rather than
/// rebuilding TallyOwl. The bundled set is the fallback, for a container built
/// from scratch that has no platform store at all.
fn public_roots() -> Arc<rustls::ClientConfig> {
    static ROOTS: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    Arc::clone(ROOTS.get_or_init(|| {
        // rustls needs a crypto provider and installs none by default. This is
        // idempotent, and the RPC layer installs the same one.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut store = rustls::RootCertStore::empty();
        let found = rustls_native_certs::load_native_certs();
        for certificate in found.certs {
            let _ = store.add(certificate);
        }
        if store.is_empty() {
            store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(store)
                // A webhook receiver does not know TallyOwl, so there is no
                // client certificate to present. The signature on the body is
                // what tells the receiver who sent it.
                .with_no_client_auth(),
        )
    }))
}

/// A stream to the receiver, plain or wrapped in TLS.
///
/// It is a `Box<dyn>` rather than two copies of the request-writing code below,
/// because two copies of "how to write an HTTP request" is how one of them
/// grows a header the other does not have.
fn connect(
    secure: bool,
    host: &str,
    address: std::net::SocketAddr,
    timeout: Duration,
    url: &str,
) -> Result<Box<dyn ReadWrite>, TallyOwlError> {
    let stream = TcpStream::connect_timeout(&address, timeout).map_err(|e| {
        TallyOwlError::unavailable(format!("The receiver at `{url}` did not answer. {e}"))
    })?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    if !secure {
        return Ok(Box::new(stream));
    }
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| {
        TallyOwlError::invalid_argument(format!("`{host}` is not a name a certificate can name."))
    })?;
    let connection = rustls::ClientConnection::new(public_roots(), name).map_err(|e| {
        TallyOwlError::unavailable(format!(
            "The TLS connection to `{url}` could not be started. {e}"
        ))
    })?;
    Ok(Box::new(rustls::StreamOwned::new(connection, stream)))
}

/// What `connect` returns. A webhook sends one request and reads one response
/// on one connection, so the half-duplex `StreamOwned` that L058 replaced on
/// the RPC path is exactly right here.
trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_notification() -> Notification {
        Notification {
            rule_id: "checkout-errors".into(),
            rule_name: "Checkout errors are \"high\"".into(),
            project_id: "0102".into(),
            state: "firing".into(),
            outcome: "value".into(),
            observed_value: Some(42.0),
            evaluated_at: 1_785_628_800_000,
            query_link: "/analyses/checkout-errors".into(),
            reason: String::new(),
            escalation: false,
        }
    }

    #[test]
    fn a_body_escapes_the_words_a_person_wrote() {
        // A rule name is a person's own words. A body that broke on a quotation
        // mark would be a notification nobody received, and the receiver would
        // report a parse failure rather than an alert.
        let body = a_notification().body();
        assert!(
            body.contains(r#""rule":"Checkout errors are \"high\"""#),
            "{body}"
        );
        assert!(body.contains(r#""value":42"#));
        assert!(body.contains(r#""state":"firing""#));
    }

    #[test]
    fn a_notification_carries_no_telemetry() {
        // ALERTS.md section 6. The body is built from the rule and the
        // evaluation, and there is no field a row could travel in.
        let body = a_notification().body();
        for forbidden in ["event_id", "properties", "rows", "end_user"] {
            assert!(!body.contains(forbidden), "`{forbidden}` reached a webhook");
        }
    }

    #[test]
    fn an_absent_value_is_null_and_never_zero() {
        // A `no-data` outcome has no value. Sending zero would tell a receiver
        // that the measure was zero, which is a different fact and the one that
        // makes a dead pipeline look healthy.
        let mut notification = a_notification();
        notification.observed_value = None;
        notification.outcome = "no-data".into();
        assert!(notification.body().contains(r#""value":null"#));
    }

    #[test]
    fn a_signature_covers_the_timestamp_as_well_as_the_body() {
        // Signing the body alone would let somebody replay a firing
        // notification a week later and have it check out.
        let body = a_notification().body();
        let first = sign("shhh", 1_000, &body);
        let later = sign("shhh", 2_000, &body);
        assert_ne!(first, later, "the timestamp is not inside the signature");
        assert_eq!(first, sign("shhh", 1_000, &body), "signing is not stable");
        assert_ne!(first, sign("other", 1_000, &body), "the key does nothing");
    }

    #[test]
    fn a_secret_of_any_length_produces_a_signature() {
        for secret in ["", "x", &"long".repeat(200)] {
            assert_eq!(sign(secret, 1, "body").len(), 64);
        }
    }

    #[test]
    fn backoff_rises_and_then_stops_rising() {
        // The cap is what stops a receiver that has been down for an hour from
        // being asked once a second for that hour.
        let steps: Vec<i64> = (0..20).map(|n| backoff_ms(n, 0)).collect();
        assert!(steps[0] < steps[3], "it does not rise");
        assert!(
            steps[19] <= 300_000 + 75_000,
            "it rises for ever: {steps:?}"
        );
        assert!(steps.iter().all(|step| *step >= 1));
    }

    #[test]
    fn two_receivers_that_failed_together_do_not_come_back_together() {
        let one = backoff_ms(5, 7);
        let other = backoff_ms(5, 5_000_003);
        assert_ne!(one, other, "there is no jitter");
    }

    #[test]
    fn an_address_that_asks_for_tls_gets_tls_and_the_right_port() {
        // Quietly downgrading `https` to a plaintext request would send what an
        // alert observed to the network the operator asked to be protected
        // from. It used to be refused for want of a client; it is not now.
        assert_eq!(
            split_url("https://example.test/alerts").unwrap(),
            (true, "example.test".to_string(), 443, "/alerts".to_string())
        );
        assert_eq!(
            split_url("http://example.test/alerts").unwrap(),
            (false, "example.test".to_string(), 80, "/alerts".to_string())
        );
    }

    #[test]
    fn an_address_is_split_into_a_scheme_a_host_a_port_and_a_path() {
        assert_eq!(
            split_url("http://example.test:9000/alerts/one").unwrap(),
            (
                false,
                "example.test".to_string(),
                9000,
                "/alerts/one".to_string()
            )
        );
        assert_eq!(
            split_url("https://example.test:8443").unwrap(),
            (true, "example.test".to_string(), 8443, "/".to_string())
        );
    }

    #[test]
    fn an_address_with_no_scheme_is_refused_rather_than_guessed_at() {
        // Guessing `http` for an address a person wrote without one is the same
        // downgrade by another route.
        let failure = split_url("example.test/alerts").expect_err("refused");
        assert!(
            failure.message.contains("full address"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn the_public_roots_load_something_to_verify_against() {
        // A root store that loaded nothing would make every TLS webhook fail
        // with a certificate error, which reads to an operator as "the receiver
        // is broken". The platform store or the bundled set: one has to answer.
        let _ = public_roots();
        assert!(
            !webpki_roots::TLS_SERVER_ROOTS.is_empty(),
            "the bundled fallback is empty, so a host with no platform store trusts nothing"
        );
    }
}
