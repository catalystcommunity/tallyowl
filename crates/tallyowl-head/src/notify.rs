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
    /// The whole delivery: the connection, the request, and the response.
    pub timeout: Duration,
    /// Which addresses a webhook may reach. See [`Egress`].
    pub egress: Egress,
}

/// Which addresses a webhook may reach.
///
/// **A webhook address comes from a project administrator, and the request
/// leaves from inside the installation's network.** Without a rule, a rule
/// target of `http://169.254.169.254/...` or `http://127.0.0.1:5111/...` made
/// the head send requests to a cloud metadata service or to its own operational
/// port, and the status came back in the delivery record. So an address that
/// is not a public one is refused unless an operator listed its host.
///
/// The check is on the address the connection is made to, after the name is
/// resolved, so a public name that resolves to a private address is refused
/// as well.
#[derive(Debug, Clone, Default)]
pub struct Egress {
    allowed_private: Vec<String>,
}

impl Egress {
    /// The hosts an operator listed in `alerts.allowedPrivateTargets`. A host
    /// is written as it appears in the webhook address. `*` permits every
    /// address, which is what an installation with one tenant may want.
    pub fn allowing(hosts: impl IntoIterator<Item = String>) -> Egress {
        Egress {
            allowed_private: hosts
                .into_iter()
                .map(|host| host.trim().to_ascii_lowercase())
                .filter(|host| !host.is_empty())
                .collect(),
        }
    }

    /// Every address is permitted.
    pub fn any() -> Egress {
        Egress::allowing(["*".to_string()])
    }

    pub fn permits(&self, host: &str, address: std::net::IpAddr) -> Result<(), TallyOwlError> {
        if is_public(address) {
            return Ok(());
        }
        let host = host.to_ascii_lowercase();
        if self
            .allowed_private
            .iter()
            .any(|allowed| allowed == "*" || *allowed == host)
        {
            return Ok(());
        }
        Err(TallyOwlError::new(
            tallyowl_obs::ErrorCode::PermissionDenied,
            format!(
                "The webhook host `{host}` is at {address}, which is inside this installation's own network, so nothing was sent. If this receiver is one you run, add `{host}` to `alerts.allowedPrivateTargets`."
            ),
        )
        .retryable(false))
    }
}

/// Whether an address is one on the public internet.
fn is_public(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                // 100.64.0.0/10, the shared address space of a carrier network.
                || (a == 100 && (64..128).contains(&b))
                // 0.0.0.0/8 and 240.0.0.0/4 route nowhere a receiver is.
                || a == 0
                || a >= 240)
        }
        std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public(std::net::IpAddr::V4(v4)),
            None => {
                let first = v6.segments()[0];
                !(v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    // fc00::/7, unique local. fe80::/10, link local.
                    || (first & 0xfe00) == 0xfc00
                    || (first & 0xffc0) == 0xfe80)
            }
        },
    }
}

/// Refuse an address that holds a control character or a space.
///
/// The host and the path of a webhook address are written into the request
/// line and the `Host` header. A carriage return in either let a rule's author
/// write the rest of the request: other headers, or a second request to
/// another service.
pub fn check_address_text(url: &str) -> Result<(), TallyOwlError> {
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(TallyOwlError::invalid_argument(
            "A notification address cannot hold a space, a line break, or another control character. Write one full address, such as `https://example.test/alerts`.",
        ));
    }
    Ok(())
}

impl Channel for Webhook {
    fn name(&self) -> String {
        format!("webhook {}", self.url)
    }

    fn send(&self, notification: &Notification) -> Attempt {
        let body = notification.body();
        let timestamp = tallyowl_obs::time::now_ms();
        let signature = sign(&self.secret, timestamp, &body);
        let deadline = std::time::Instant::now() + self.timeout;
        match post(&self.url, &body, timestamp, &signature, deadline, &self.egress) {
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
            // An address that was refused will be refused again. Everything
            // else is the network, and the network recovers.
            Err(failure) => {
                let retryable = failure.retryable;
                Attempt::failed(failure.message, retryable)
            }
        }
    }
}

/// A native CSIL callback, for a service that already speaks to TallyOwl.
///
/// It carries the same notification and the same signature as a webhook: a
/// keyed hash with the target's secret over the time and the body. The receiver
/// is an application, not an enrolled node, so the connection proves nothing
/// about who called. The signature does. The body travels as the exact bytes
/// that were signed, so a receiver verifies bytes and never an encoding it
/// made itself. See `TallyOwlAlertReceiver` in `csil/tallyowl-ingest.csil`.
pub struct CsilCallback {
    pub address: String,
    pub secret: String,
    pub timeout: Duration,
    pub sender: std::sync::Arc<dyn CallbackSender>,
}

impl CsilCallback {
    /// The request a receiver gets, signed at `signed_at`.
    pub fn request(&self, notification: &Notification, signed_at: i64) -> Vec<u8> {
        use tallyowl_collector_api::codec::encode_alert_notify_request;
        use tallyowl_collector_api::types::AlertNotifyRequest;
        let body = notification.body();
        encode_alert_notify_request(&AlertNotifyRequest {
            signed_at,
            signature: sign(&self.secret, signed_at, &body),
            body: body.into_bytes(),
        })
    }
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
/// connection adds a handler rather than an HTTP endpoint. The app drivers
/// verify the signature for it: `VerifyAlertCallback` in Go and
/// `verify_alert_callback` in Rust.
pub const CALLBACK_SERVICE: &str = "TallyOwlAlertReceiver";
pub const CALLBACK_OPERATION: &str = "notify";

/// Deliver a native callback over CSIL-RPC.
///
/// The transport follows D62: plaintext to a loopback or `unix:` receiver, and
/// TLS to any other, with the receiver's certificate checked against the
/// operating system's trusted authorities. TLS proves the receiver to the head.
/// The signature in the request proves the head to the receiver.
pub struct RpcCallbacks {
    pub max_frame_bytes: usize,
}

impl CallbackSender for RpcCallbacks {
    fn deliver(&self, address: &str, body: &[u8], timeout: Duration) -> Attempt {
        // A receiver that accepts the connection and never answers held the
        // only notification worker for ever, because the client had a connect
        // timeout and no other. Every read and every write is bounded now.
        let client = match callback_client(address, self.max_frame_bytes) {
            Ok(client) => client
                .with_connect_timeout(timeout)
                .with_io_timeout(timeout),
            // An address that does not parse, or a host with no usable trust
            // store, does not become deliverable by being asked again.
            Err(failure) => {
                return Attempt::failed(
                    format!(
                        "`{address}` could not be reached securely. {}",
                        failure.message
                    ),
                    false,
                )
            }
        };
        match client.call(CALLBACK_SERVICE, CALLBACK_OPERATION, body.to_vec()) {
            // A receiver that refuses answers a typed error, and the call
            // itself succeeded. Reading only the call result counted a refused
            // notification as delivered.
            Ok(response) if response.variant.as_deref() == Some(tallyowl_rpc::SERVICE_ERROR_VARIANT) => {
                match tallyowl_collector_api::codec::decode_service_error(&response.payload) {
                    Ok(refusal) => Attempt::failed(
                        format!("`{address}` refused the notification. {}", refusal.message),
                        refusal.retryable,
                    ),
                    Err(e) => Attempt::failed(
                        format!("`{address}` refused the notification with an answer that could not be read: {e}"),
                        false,
                    ),
                }
            }
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

/// The client for one callback receiver, by the D62 rule.
fn callback_client(
    address: &str,
    max_frame_bytes: usize,
) -> Result<tallyowl_rpc::Client, tallyowl_obs::error::TallyOwlError> {
    let parsed = tallyowl_rpc::Address::parse(address)?;
    if parsed.plaintext_permitted() {
        return Ok(tallyowl_rpc::Client::new(
            address.to_string(),
            max_frame_bytes,
        ));
    }
    let host = parsed.host().unwrap_or_default().to_string();
    tallyowl_rpc::Client::server_auth(address.to_string(), max_frame_bytes, &host, None)
}

impl Channel for CsilCallback {
    fn name(&self) -> String {
        format!("callback {}", self.address)
    }

    fn send(&self, notification: &Notification) -> Attempt {
        // Sending an unsigned callback would teach a receiver to accept one.
        if self.secret.is_empty() {
            return Attempt::failed(
                format!(
                    "The callback to `{}` has no secret, so it was not sent. Give the target a `secret_ref` that names a secret this head can read.",
                    self.address
                ),
                false,
            );
        }
        // Each attempt signs again, so a retry carries a fresh time and is not
        // refused as a replay.
        let request = self.request(notification, tallyowl_obs::time::now_ms());
        self.sender.deliver(&self.address, &request, self.timeout)
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
    deadline: std::time::Instant,
    egress: &Egress,
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
    // The address the connection is made to, and not the name. A name that an
    // outsider controls can resolve to anything.
    egress.permits(&host, address.ip())?;

    // **One deadline for the whole delivery.** The timeout used to apply to
    // each read, so a receiver that sent one byte every nine seconds held the
    // worker for days. What is left of the deadline is set again before every
    // read and write.
    let out_of_time = || {
        TallyOwlError::unavailable(format!(
            "The receiver at `{url}` did not finish answering in the time a delivery is allowed."
        ))
    };
    let left = |deadline: std::time::Instant| {
        deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|left| !left.is_zero())
    };

    let (mut stream, socket) = connect(
        secure,
        &host,
        address,
        left(deadline).ok_or_else(out_of_time)?,
        url,
    )?;

    let remaining = left(deadline).ok_or_else(out_of_time)?;
    socket.set_write_timeout(Some(remaining)).ok();
    socket.set_read_timeout(Some(remaining)).ok();
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
        let remaining = left(deadline).ok_or_else(out_of_time)?;
        socket.set_read_timeout(Some(remaining)).ok();
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => raw.extend_from_slice(&chunk[..read]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset && !raw.is_empty() => break,
            // The read waited for what was left of the deadline and got nothing.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(out_of_time())
            }
            Err(e) => {
                return Err(TallyOwlError::unavailable(format!(
                    "The notification to `{url}` ended early. {e}"
                )))
            }
        }
        // The status line is all this reads, and it is at the start.
        if raw.len() > 64 * 1024 || raw.windows(2).any(|pair| pair == b"\r\n") {
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
    check_address_text(url)?;
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
) -> Result<(Box<dyn ReadWrite>, TcpStream), TallyOwlError> {
    let stream = TcpStream::connect_timeout(&address, timeout).map_err(|e| {
        TallyOwlError::unavailable(format!("The receiver at `{url}` did not answer. {e}"))
    })?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    // A second handle on the same socket, so the caller can lower the timeouts
    // as its deadline approaches. A TLS stream owns the first one.
    let socket = stream.try_clone().map_err(|e| {
        TallyOwlError::unavailable(format!(
            "The connection to `{url}` could not be prepared. {e}"
        ))
    })?;
    if !secure {
        return Ok((Box::new(stream), socket));
    }
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| {
        TallyOwlError::invalid_argument(format!("`{host}` is not a name a certificate can name."))
    })?;
    let connection = rustls::ClientConnection::new(public_roots(), name).map_err(|e| {
        TallyOwlError::unavailable(format!(
            "The TLS connection to `{url}` could not be started. {e}"
        ))
    })?;
    Ok((
        Box::new(rustls::StreamOwned::new(connection, stream)),
        socket,
    ))
}

/// What `connect` returns. A webhook sends one request and reads one response
/// on one connection, so the half-duplex `StreamOwned` that L058 replaced on
/// the RPC path is exactly right here.
trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

#[cfg(test)]
mod tests {

    #[test]
    fn the_head_signs_the_shared_callback_vector_byte_for_byte() {
        // golden/alert-callback.json. The Go and Rust app drivers verify the
        // same file, so a change to the body, the signature, or the request
        // encoding fails here and in both drivers.
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../golden/alert-callback.json"),
        )
        .expect("the shared vector");
        let hex_of = |name: &str| {
            let start = text.find(&format!("\"{name}\": \"")).expect(name) + name.len() + 5;
            text[start..]
                .split('"')
                .next()
                .expect("a value")
                .to_string()
        };
        let notification = Notification {
            rule_id: "error-rate".into(),
            rule_name: "Errors \"high\"".into(),
            project_id: "0f0e0d0c0b0a09080706050403020100".into(),
            state: "firing".into(),
            outcome: "value".into(),
            observed_value: Some(12.5),
            evaluated_at: 1_790_000_000_000,
            query_link: "https://tallyowl.example/q/1".into(),
            reason: "above 10".into(),
            escalation: false,
        };
        let callback = CsilCallback {
            address: "127.0.0.1:1".into(),
            secret: "correct horse battery staple".into(),
            timeout: Duration::from_secs(1),
            sender: std::sync::Arc::new(RpcCallbacks { max_frame_bytes: 1 }),
        };
        assert_eq!(
            sign(
                "correct horse battery staple",
                1_790_000_000_123,
                &notification.body()
            ),
            hex_of("signature")
        );
        let request: String = callback
            .request(&notification, 1_790_000_000_123)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(request, hex_of("request"));
    }

    #[test]
    fn a_callback_with_no_secret_is_never_sent() {
        struct Counting(std::sync::atomic::AtomicUsize);
        impl CallbackSender for Counting {
            fn deliver(&self, _: &str, _: &[u8], _: Duration) -> Attempt {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Attempt::delivered("sent")
            }
        }
        let sender = std::sync::Arc::new(Counting(std::sync::atomic::AtomicUsize::new(0)));
        let attempt = CsilCallback {
            address: "127.0.0.1:5300".into(),
            secret: String::new(),
            timeout: Duration::from_secs(1),
            sender: sender.clone(),
        }
        .send(&Notification {
            rule_id: "r".into(),
            rule_name: "r".into(),
            project_id: "p".into(),
            state: "firing".into(),
            outcome: "value".into(),
            observed_value: None,
            evaluated_at: 0,
            query_link: String::new(),
            reason: String::new(),
            escalation: false,
        });
        assert!(!attempt.delivered && !attempt.retryable, "{attempt:?}");
        assert!(attempt.detail.contains("secret_ref"), "{}", attempt.detail);
        assert_eq!(sender.0.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn a_callback_address_that_cannot_be_read_fails_once_and_is_not_retried() {
        let attempt = RpcCallbacks {
            max_frame_bytes: 1024 * 1024,
        }
        .deliver("receiver-with-no-port", b"{}", Duration::from_millis(50));
        assert!(
            !attempt.retryable,
            "a bad address never becomes deliverable"
        );
    }

    #[test]
    fn a_callback_client_builds_for_a_loopback_a_network_and_a_unix_address() {
        // Built, not dialled: building a TLS client reads the trust store and
        // opens nothing, so this needs no network.
        assert!(callback_client("127.0.0.1:5000", 1024).is_ok());
        assert!(callback_client("receiver.example:5000", 1024).is_ok());
        assert!(callback_client("unix:/run/receiver.sock", 1024).is_ok());
    }
    use super::*;

    fn ip(text: &str) -> std::net::IpAddr {
        text.parse().expect("an address")
    }

    #[test]
    fn a_webhook_to_an_address_inside_the_installation_is_refused_unless_its_host_is_listed() {
        let strict = Egress::default();
        for inside in [
            "127.0.0.1",
            "169.254.169.254",
            "10.1.2.3",
            "172.16.0.9",
            "192.168.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "::",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            let refused = strict
                .permits("receiver.example", ip(inside))
                .expect_err(inside);
            assert!(!refused.retryable, "{inside} would be retried for a day");
            assert!(
                refused.message.contains("alerts.allowedPrivateTargets"),
                "{}",
                refused.message
            );
        }
        for outside in ["93.184.216.34", "2606:2800:220:1:248:1893:25c8:1946"] {
            strict
                .permits("receiver.example", ip(outside))
                .expect(outside);
        }

        let listed = Egress::allowing(["Alerts.Internal ".to_string(), String::new()]);
        listed
            .permits("alerts.internal", ip("10.1.2.3"))
            .expect("a listed host");
        assert!(listed.permits("other.internal", ip("10.1.2.3")).is_err());
        Egress::any()
            .permits("anything", ip("127.0.0.1"))
            .expect("every address");
    }

    #[test]
    fn an_address_that_would_write_its_own_request_lines_is_refused() {
        for address in [
            "http://127.0.0.1:5111/x HTTP/1.1\r\nHost: internal\r\n\r\n",
            "http://example.test/a\nb",
            "http://exa mple.test/",
            "http://example.test/\u{0}",
        ] {
            assert!(check_address_text(address).is_err(), "{address:?}");
            assert!(split_url(address).is_err(), "{address:?}");
        }
        split_url("https://example.test/alerts?team=a").expect("an ordinary address");
    }

    #[test]
    fn a_receiver_that_answers_one_byte_at_a_time_does_not_hold_the_worker_past_the_deadline() {
        // The timeout applied to each read. One byte every twenty milliseconds
        // never reached it, so one receiver held the only worker for as long as
        // it cared to keep sending.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().unwrap();
        let receiver = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            // Never a line break, so never a status line.
            while stream.write_all(b"x").is_ok() {
                std::thread::sleep(Duration::from_millis(20));
            }
        });

        let webhook = Webhook {
            url: format!("http://{address}/alerts"),
            secret: "shhh".into(),
            timeout: Duration::from_millis(150),
            egress: Egress::any(),
        };
        let attempt = webhook.send(&a_notification());
        assert!(!attempt.delivered);
        assert!(attempt.retryable);
        assert!(
            attempt.detail.contains("did not finish"),
            "{}",
            attempt.detail
        );
        // The receiver's write fails once the head has closed the connection.
        receiver.join().expect("the receiver ends");
    }

    #[test]
    fn a_webhook_to_a_private_address_sends_nothing() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).expect("nonblocking");
        let webhook = Webhook {
            url: format!("http://{address}/alerts"),
            secret: String::new(),
            timeout: Duration::from_millis(150),
            egress: Egress::default(),
        };
        let attempt = webhook.send(&a_notification());
        assert!(!attempt.delivered);
        assert!(!attempt.retryable, "a refused address would be retried");
        assert!(listener.accept().is_err(), "a connection was made");
    }

    #[test]
    fn a_callback_receiver_that_never_answers_gives_the_worker_back() {
        // The client had a connect timeout and no other, so this call never
        // returned and every tenant's notifications stopped behind it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().unwrap().to_string();
        let held = std::thread::spawn(move || listener.accept().map(|(stream, _)| stream));

        let attempt = RpcCallbacks {
            max_frame_bytes: 1024 * 1024,
        }
        .deliver(&address, b"{}", Duration::from_millis(100));
        assert!(!attempt.delivered);
        drop(held.join());
    }

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
