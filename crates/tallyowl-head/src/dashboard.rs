//! The dashboard's HTTP surface.
//!
//! `docs/DESIGN.md` section 4.2 gives the dashboard hop: "Dashboard browser →
//! head | RPC or Events | Same-origin browser carrier | TallyOwl session from
//! LinkKeys login". This is that carrier, plus the document assets and the
//! sign-in callback route.
//!
//! # What this is not
//!
//! **It is not an ingest API.** `AGENTS.md` forbids one, and this cannot become
//! one: [`carry`] refuses every service except `TallyOwlControl`, so a frame
//! naming `TallyOwlIngest` or `TallyOwlCollector` is rejected before it reaches
//! a decoder. Telemetry reaches a collector over CSIL, never here.
//!
//! `AGENTS.md` permits exactly four browser-only HTTP uses, and this serves
//! three of them: document assets, LinkKeys redirects, and browser carrier
//! establishment. The fourth, the unload flush, belongs to the host
//! application's own route and not to TallyOwl.
//!
//! # The carrier
//!
//! One CSIL-RPC request frame in the body of a `POST`, one response frame back.
//! A browser cannot open a TCP socket, and this is the smallest thing that
//! carries a CSIL envelope without inventing a second protocol: the envelopes,
//! the codecs, and the correlation IDs are the same ones every other hop uses.
//!
//! # The session
//!
//! The session token travels in the `Authorization` header and becomes
//! `RpcRequest.auth`, which is where every other hop puts a credential. The
//! head's own authorization then runs unchanged.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use csilgen_transport::rpc::{RpcRequest, RpcResponse};
use tallyowl_obs::log::Logger;
use tallyowl_rpc::Dispatcher;

use crate::service::CONTROL_SERVICE;

/// The largest request body this surface reads.
///
/// A query tree is small and a sign-in callback is a few kilobytes. Anything
/// larger is a mistake or an attempt, and both are refused before allocation.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// The most bytes the request line and the headers may take together.
///
/// A browser sends a few hundred. A line with no end used to grow a `String`
/// until the head, which also does ingest, ran out of memory.
const MAX_HEAD_BYTES: u64 = 16 * 1024;

/// The most headers one request may carry.
const MAX_HEADERS: usize = 64;

/// What bounds one connection, and how many there may be.
///
/// **This surface faces a browser and shares a process with ingest**, so a
/// client that does nothing must cost nothing for long. Every connection has
/// one deadline for the whole request, not one for each read: a read timeout
/// alone lets a client that sends one byte every few seconds hold a thread for
/// days.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// How long a client has to send its whole request.
    pub request_deadline: Duration,
    /// How long one write of the reply may wait on a client that is not reading.
    pub write_timeout: Duration,
    /// How many connections are served at one time. One more is answered 503.
    pub max_connections: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            request_deadline: Duration::from_secs(10),
            write_timeout: Duration::from_secs(10),
            max_connections: 256,
        }
    }
}

/// A running dashboard surface. Dropping the handle asks it to stop.
pub struct DashboardServer {
    local_address: std::net::SocketAddr,
    stopping: Arc<AtomicBool>,
}

impl DashboardServer {
    pub fn local_address(&self) -> std::net::SocketAddr {
        self.local_address
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.local_address);
    }
}

impl Drop for DashboardServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the dashboard needs to serve itself.
pub struct Settings {
    /// Where the built dashboard bundle is. `./tools.sh build` writes it.
    pub assets: PathBuf,
    /// What the dashboard reports as ingest health.
    pub health: Option<Arc<tallyowl_obs::health::Health>>,
    /// The path the LinkKeys callback arrives at, so the document can complete
    /// a sign-in. It is a path rather than a whole URL, because the whole URL is
    /// `linkkeys.callbackUrl` and the two must not drift.
    pub callback_path: String,
    /// `dashboard.allowPlaintext`. The dashboard serves plaintext and carries
    /// session tokens, so a network address needs a gateway that ends TLS in
    /// front of it, and this setting says that one is there. D62.
    pub allow_plaintext: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            assets: PathBuf::from("packages/dashboard/dist"),
            health: None,
            callback_path: "/sign-in/callback".to_string(),
            allow_plaintext: false,
        }
    }
}

/// Whether an address to listen on is loopback, so nothing crosses a network.
fn is_loopback(address: &str) -> bool {
    let host = address.rsplit_once(':').map_or(address, |(host, _)| host);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Start the dashboard surface.
pub fn start(
    address: &str,
    dispatcher: Arc<dyn Dispatcher>,
    settings: Settings,
    logger: Arc<Logger>,
) -> std::io::Result<DashboardServer> {
    start_with(address, dispatcher, settings, Limits::default(), logger)
}

/// Start the dashboard surface with its own limits.
pub fn start_with(
    address: &str,
    dispatcher: Arc<dyn Dispatcher>,
    settings: Settings,
    limits: Limits,
    logger: Arc<Logger>,
) -> std::io::Result<DashboardServer> {
    if !settings.allow_plaintext && !is_loopback(address) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "`dashboard.listen` is `{address}`, which is reached over a network, and the dashboard serves plaintext and carries session tokens. Put it behind a gateway that ends TLS and set `dashboard.allowPlaintext: true`, or listen on a loopback address."
            ),
        ));
    }
    let listener = TcpListener::bind(address)?;
    let local_address = listener.local_addr()?;
    let stopping = Arc::new(AtomicBool::new(false));
    let loop_stopping = Arc::clone(&stopping);
    let settings = Arc::new(settings);
    let open = Arc::new(AtomicUsize::new(0));

    std::thread::Builder::new()
        .name("tallyowl-dashboard".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if loop_stopping.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_write_timeout(Some(limits.write_timeout));
                // The count rises here and not in the thread, so the limit is
                // on threads that exist and not on threads that have started.
                if open.fetch_add(1, Ordering::SeqCst) >= limits.max_connections {
                    open.fetch_sub(1, Ordering::SeqCst);
                    let _ = write_response(
                        &mut stream,
                        503,
                        "text/plain; charset=utf-8",
                        b"The dashboard is serving as many connections as it permits. Try again.",
                    );
                    continue;
                }
                let served = Served(Arc::clone(&open));
                let dispatcher = Arc::clone(&dispatcher);
                let settings = Arc::clone(&settings);
                let logger = Arc::clone(&logger);
                let spawned = std::thread::Builder::new()
                    .name("tallyowl-dashboard-request".into())
                    .spawn(move || {
                        let _served = served;
                        serve_one(stream, &dispatcher, &settings, limits, &logger);
                    });
                // A thread the system would not start drops its closure, and
                // the count with it.
                drop(spawned);
            }
        })?;

    Ok(DashboardServer {
        local_address,
        stopping,
    })
}

/// One served connection. Dropping it gives the place back, whether the thread
/// returned or unwound.
struct Served(Arc<AtomicUsize>);

impl Drop for Served {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A socket that stops answering reads at one moment.
///
/// Each read gets the time that is left, so the deadline is for the whole
/// request whatever the client's pace.
struct Deadlined<'a> {
    stream: &'a TcpStream,
    until: Instant,
}

impl Read for Deadlined<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(left))?;
        let mut stream = self.stream;
        stream.read(buffer)
    }
}

fn serve_one(
    mut stream: TcpStream,
    dispatcher: &Arc<dyn Dispatcher>,
    settings: &Settings,
    limits: Limits,
    logger: &Logger,
) {
    let read = read_request(Deadlined {
        stream: &stream,
        until: Instant::now() + limits.request_deadline,
    });
    let request = match read {
        Ok(request) => request,
        Err(refusal) => {
            let _ = write_response(
                &mut stream,
                refusal.status(),
                "text/plain; charset=utf-8",
                refusal.message().as_bytes(),
            );
            return;
        }
    };

    let path = request.path.as_str();
    // The callback route is the document again: the browser lands here with the
    // token in the query string, and the document completes the sign-in over
    // the carrier. The route exists so that `linkkeys.callbackUrl` has a server
    // behind it rather than a 404.
    let is_callback = path == settings.callback_path;

    match (request.method.as_str(), path) {
        ("POST", "/api/rpc") => {
            let (status, body) = carry(dispatcher, &request);
            let _ = write_response(&mut stream, status, "application/cbor", &body);
        }
        // Ingest health, in the shape the dashboard's health panel reads. It is
        // the same `Health` the operational endpoint reports, so the dashboard
        // and `/readyz` can never disagree.
        ("GET", "/api/health") => {
            let body = health_json(settings.health.as_deref());
            let _ = write_response(&mut stream, 200, "application/json; charset=utf-8", &body);
        }
        // The bundle is a tree of ES modules that import each other by relative
        // path, so the whole tree is served rather than one file.
        ("GET", _) if path.starts_with("/assets/") => {
            let Some(file) = asset_path(&settings.assets, path) else {
                let _ = write_response(
                    &mut stream,
                    404,
                    "text/plain; charset=utf-8",
                    b"No such asset.",
                );
                return;
            };
            match std::fs::read(&file) {
                Ok(bytes) => {
                    let _ =
                        write_response(&mut stream, 200, "text/javascript; charset=utf-8", &bytes);
                }
                Err(e) => {
                    logger.warning(
                        "The dashboard bundle is not built, so the dashboard cannot load.",
                        &[
                            ("path", &file.display().to_string()),
                            ("reason", &e.to_string()),
                        ],
                    );
                    let _ = write_response(
                        &mut stream,
                        503,
                        "text/plain; charset=utf-8",
                        b"The dashboard is not built. Run `./tools.sh build`.",
                    );
                }
            }
        }
        ("GET", _) if path == "/" || is_callback => {
            let _ = write_response(
                &mut stream,
                200,
                "text/html; charset=utf-8",
                &document(&settings.callback_path),
            );
        }
        _ => {
            let _ = write_response(
                &mut stream,
                404,
                "text/plain; charset=utf-8",
                b"This address serves the TallyOwl dashboard and nothing else.",
            );
        }
    }
}

/// Carry one CSIL-RPC frame to the control service and back.
///
/// **Only `TallyOwlControl`.** A frame naming any other service is refused here,
/// which is what keeps this from becoming the HTTP ingest API `AGENTS.md`
/// forbids. The refusal is a transport status, so a caller can tell it from an
/// application error exactly as on every other hop.
pub fn carry(dispatcher: &Arc<dyn Dispatcher>, request: &HttpRequest) -> (u16, Vec<u8>) {
    let Ok(mut decoded) = RpcRequest::decode(&request.body) else {
        return (
            400,
            encode_or_empty(&RpcResponse::transport_error(
                csilgen_transport::Status::MalformedEnvelope,
                "The request was not a CSIL-RPC frame.".to_string(),
            )),
        );
    };

    if decoded.service != CONTROL_SERVICE {
        return (
            403,
            encode_or_empty(
                &RpcResponse::transport_error(
                    csilgen_transport::Status::Forbidden,
                    format!(
                        "This address carries `{CONTROL_SERVICE}` only. `{}` is not reachable over \
                         HTTP; telemetry reaches a collector over CSIL.",
                        decoded.service
                    ),
                )
                .with_id(decoded.id),
            ),
        );
    }

    // The session travels in the header and becomes the frame's credential,
    // which is where every other hop puts one. A frame that carried its own
    // would let a document choose an identity the browser did not present.
    decoded.auth = request.authorization.clone();

    let id = decoded.id;
    let reply = match dispatcher.dispatch(&decoded) {
        tallyowl_rpc::Outcome::Reply { variant, payload } => {
            RpcResponse::ok(variant, payload).with_id(id)
        }
        tallyowl_rpc::Outcome::Transport(status, message) => {
            RpcResponse::transport_error(status, message).with_id(id)
        }
    };
    (200, encode_or_empty(&reply))
}

fn encode_or_empty(response: &RpcResponse) -> Vec<u8> {
    response.encode().unwrap_or_default()
}

/// One parsed request. Only what this surface uses.
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub body: Vec<u8>,
}

/// Why a request was not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unread {
    /// It was not an HTTP request, or the client went away.
    Malformed,
    /// The request line and headers passed [`MAX_HEAD_BYTES`] or
    /// [`MAX_HEADERS`].
    HeadTooLarge,
    /// The body is larger than [`MAX_BODY_BYTES`].
    BodyTooLarge,
    /// The whole request did not arrive inside the deadline.
    TooSlow,
}

impl Unread {
    fn status(self) -> u16 {
        match self {
            Unread::Malformed => 400,
            Unread::HeadTooLarge => 431,
            Unread::BodyTooLarge => 413,
            Unread::TooSlow => 408,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Unread::Malformed => "Bad request.",
            Unread::HeadTooLarge => "The request headers are larger than this address reads.",
            Unread::BodyTooLarge => "The request body is larger than this address reads.",
            Unread::TooSlow => "The request did not arrive in time.",
        }
    }
}

fn unread(error: std::io::Error) -> Unread {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => Unread::TooSlow,
        _ => Unread::Malformed,
    }
}

/// Read one line of the head, and take it out of what the head has left.
///
/// The read is through `take`, so a line with no end stops at the budget and
/// never grows past it.
fn read_head_line(reader: &mut impl BufRead, left: &mut u64) -> Result<String, Unread> {
    let mut line = String::new();
    let taken = reader
        .by_ref()
        .take(*left)
        .read_line(&mut line)
        .map_err(unread)? as u64;
    *left -= taken.min(*left);
    if !line.ends_with('\n') && taken > 0 && *left == 0 {
        return Err(Unread::HeadTooLarge);
    }
    Ok(line)
}

pub fn read_request(stream: impl Read) -> Result<HttpRequest, Unread> {
    let mut reader = BufReader::new(stream);
    let mut left = MAX_HEAD_BYTES;
    let line = read_head_line(&mut reader, &mut left)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or(Unread::Malformed)?.to_string();
    let target = parts.next().ok_or(Unread::Malformed)?.to_string();
    // The query string belongs to the document, not to the router.
    let path = target
        .split_once('?')
        .map(|(path, _)| path.to_string())
        .unwrap_or(target);

    let mut content_length = 0usize;
    let mut authorization = None;
    let mut headers = 0;
    loop {
        let header = read_head_line(&mut reader, &mut left)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        headers += 1;
        if headers > MAX_HEADERS {
            return Err(Unread::HeadTooLarge);
        }
        if let Some((name, value)) = header.split_once(':') {
            let value = value.trim();
            match name.to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.parse().unwrap_or(0),
                "authorization" => {
                    authorization = Some(
                        value
                            .strip_prefix("Bearer ")
                            .unwrap_or(value)
                            .trim()
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
    }

    // The limit is checked before the allocation, so an oversized body never
    // reaches memory.
    if content_length > MAX_BODY_BYTES {
        return Err(Unread::BodyTooLarge);
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).map_err(unread)?;
    }

    Ok(HttpRequest {
        method,
        path,
        authorization,
        body,
    })
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Content-Security-Policy: default-src 'self'; object-src 'none'; base-uri 'none'\r\n\
         Referrer-Policy: no-referrer\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

/// Resolve one asset path under the bundle directory.
///
/// **A component that is not a plain name is refused.** Without that a request
/// for `/assets/../../etc/passwd` would read whatever the process can read, and
/// this surface faces a browser.
fn asset_path(assets: &std::path::Path, path: &str) -> Option<PathBuf> {
    let relative = path.strip_prefix("/assets/")?;
    if relative.is_empty() {
        return None;
    }
    let mut file = assets.to_path_buf();
    for component in relative.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return None;
        }
        if component.contains('\\') {
            return None;
        }
        file.push(component);
    }
    Some(file)
}

/// Ingest health as JSON.
///
/// A check that fails names itself and says why. "Unhealthy" with no reason
/// costs an operator the whole diagnosis. See CONVENTIONS.md section 3.
fn health_json(health: Option<&tallyowl_obs::health::Health>) -> Vec<u8> {
    let Some(health) = health else {
        return br#"{"ready":false,"checks":[{"name":"health","detail":"This build reports no checks."}]}"#
            .to_vec();
    };
    let report = health.report();
    // Only the checks that need saying. A panel that listed every passing check
    // would bury the one that failed.
    let checks: Vec<String> = report
        .checks
        .iter()
        .filter_map(|check| {
            let detail = match &check.state {
                tallyowl_obs::health::CheckState::Ok => return None,
                tallyowl_obs::health::CheckState::Failed { reason } => reason.clone(),
                tallyowl_obs::health::CheckState::Degraded { cause } => {
                    format!("Working, and unwell: {cause:?}")
                }
            };
            Some(format!(
                r#"{{"name":{},"detail":{}}}"#,
                json_text(&check.name),
                json_text(&detail)
            ))
        })
        .collect();
    format!(
        r#"{{"ready":{},"checks":[{}]}}"#,
        report.ready,
        checks.join(",")
    )
    .into_bytes()
}

/// Quote one string for JSON. The values here are check names and operator
/// messages, so this escapes what those can hold and nothing else.
fn json_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
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

/// The dashboard document.
///
/// It carries no logic and no data. Everything it shows arrives over the
/// carrier, so this file never needs to change when a view does.
///
/// The one value it does carry is where a sign-in returns to. The document used
/// to assume `/sign-in/callback`, and an installation that set
/// `dashboard.callbackPath` to anything else had every sign-in refused with
/// "That is not this installation's sign-in address".
fn document(callback_path: &str) -> Vec<u8> {
    let callback_path = callback_path
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;");
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="tallyowl-callback-path" content="{callback_path}">
<title>TallyOwl</title>
</head>
<body>
<main id="dashboard">Loading the dashboard.</main>
<script type="module" src="/assets/packages/dashboard/src/boot.js"></script>
</body>
</html>
"#
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tallyowl_rpc::{reply, Request};

    fn frame(service: &str, op: &str) -> Vec<u8> {
        RpcRequest::new(service, op, vec![1, 2, 3])
            .with_id(7)
            .encode()
            .expect("a frame")
    }

    fn echo() -> Arc<dyn Dispatcher> {
        Arc::new(|request: &Request| reply("EchoResponse", request.payload.clone()))
            as Arc<dyn Dispatcher>
    }

    fn post(body: Vec<u8>, authorization: Option<&str>) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            path: "/api/rpc".into(),
            authorization: authorization.map(str::to_string),
            body,
        }
    }

    #[test]
    fn a_control_frame_reaches_the_service_and_keeps_its_correlation_id() {
        let (status, body) = carry(&echo(), &post(frame(CONTROL_SERVICE, "run-query"), None));
        assert_eq!(status, 200);
        let response = RpcResponse::decode(&body).expect("a reply");
        assert_eq!(response.id, Some(7));
        assert_eq!(response.payload, vec![1, 2, 3]);
    }

    #[test]
    fn this_surface_refuses_every_service_but_control() {
        // The rule that keeps it from becoming the HTTP ingest API AGENTS.md
        // forbids. Telemetry reaches a collector over CSIL, never here.
        for service in ["TallyOwlIngest", "TallyOwlCollector", "Invented"] {
            let (status, body) = carry(&echo(), &post(frame(service, "submit-batch"), None));
            assert_eq!(status, 403, "{service} was carried");
            let response = RpcResponse::decode(&body).expect("a reply");
            assert_eq!(response.status, csilgen_transport::Status::Forbidden);
            assert_eq!(response.id, Some(7), "the refusal names the call");
        }
    }

    #[test]
    fn the_session_comes_from_the_header_and_not_from_the_frame() {
        // A frame that carried its own credential would let a document choose
        // an identity the browser never presented.
        let seen: Arc<std::sync::Mutex<Vec<Option<String>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let dispatcher = Arc::new(move |request: &Request| {
            recorder.lock().unwrap().push(request.auth.clone());
            reply("EchoResponse", Vec::new())
        }) as Arc<dyn Dispatcher>;

        let mut request = RpcRequest::new(CONTROL_SERVICE, "run-query", vec![]);
        request.auth = Some("tows_forged".into());
        let body = request.encode().expect("a frame");

        carry(&dispatcher, &post(body.clone(), Some("tows_real")));
        carry(&dispatcher, &post(body, None));

        assert_eq!(
            *seen.lock().unwrap(),
            vec![Some("tows_real".to_string()), None],
            "the frame's own credential was used"
        );
    }

    #[test]
    fn an_asset_path_cannot_escape_the_bundle_directory() {
        // This surface faces a browser, so a request that walks out of the
        // bundle directory would read whatever the process can read.
        let assets = std::path::Path::new("/srv/dashboard");
        assert_eq!(
            asset_path(assets, "/assets/packages/dashboard/src/boot.js"),
            Some(PathBuf::from(
                "/srv/dashboard/packages/dashboard/src/boot.js"
            ))
        );
        for attempt in [
            "/assets/../../etc/passwd",
            "/assets/a/../../etc/passwd",
            "/assets/./../secrets",
            "/assets/",
            "/assets//etc/passwd",
        ] {
            assert_eq!(asset_path(assets, attempt), None, "{attempt} escaped");
        }
    }

    #[test]
    fn a_body_that_is_not_a_frame_is_refused_as_a_transport_failure() {
        let (status, body) = carry(&echo(), &post(b"not a frame".to_vec(), None));
        assert_eq!(status, 400);
        let response = RpcResponse::decode(&body).expect("a reply");
        assert_eq!(
            response.status,
            csilgen_transport::Status::MalformedEnvelope
        );
    }

    // ---- The HTTP reader, and what bounds it ------------------------------

    #[test]
    fn a_header_line_with_no_end_stops_at_the_limit() {
        // The reader is endless. A `read_line` with no bound would never come
        // back, and on a socket it grew a `String` until the head ran out of
        // memory. This one reads 16 KiB and refuses.
        let endless = b"GET / HTTP/1.1\r\nX-Fill: ".chain(std::io::repeat(b'a'));
        assert_eq!(read_request(endless).err(), Some(Unread::HeadTooLarge));

        let endless_first_line = std::io::repeat(b'G');
        assert_eq!(
            read_request(endless_first_line).err(),
            Some(Unread::HeadTooLarge)
        );
    }

    #[test]
    fn too_many_headers_and_too_large_a_body_are_refused_before_they_are_held() {
        let mut many = b"GET / HTTP/1.1\r\n".to_vec();
        for index in 0..=MAX_HEADERS {
            many.extend_from_slice(format!("X-{index}: v\r\n").as_bytes());
        }
        many.extend_from_slice(b"\r\n");
        assert_eq!(read_request(&many[..]).err(), Some(Unread::HeadTooLarge));

        let large = format!(
            "POST /api/rpc HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert_eq!(
            read_request(large.as_bytes()).err(),
            Some(Unread::BodyTooLarge)
        );
    }

    #[test]
    fn an_ordinary_request_still_reads() {
        let request = read_request(
            &b"POST /api/rpc?x=1 HTTP/1.1\r\nAuthorization: Bearer tos_a_b\r\nContent-Length: 3\r\n\r\nabc"[..],
        )
        .expect("reads");
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/rpc");
        assert_eq!(request.authorization.as_deref(), Some("tos_a_b"));
        assert_eq!(request.body, b"abc");
    }

    fn surface(limits: Limits) -> DashboardServer {
        start_with(
            "127.0.0.1:0",
            echo(),
            Settings {
                callback_path: "/auth/return".into(),
                ..Settings::default()
            },
            limits,
            Arc::new(Logger::new(
                "tallyowl-head",
                "0.0.0",
                tallyowl_obs::log::Severity::Error,
            )),
        )
        .expect("the dashboard listens")
    }

    fn reply_to(mut client: TcpStream) -> String {
        let mut reply = String::new();
        let _ = client.read_to_string(&mut reply);
        reply
    }

    #[test]
    fn a_client_that_never_finishes_its_request_is_answered_and_let_go() {
        // The deadline is for the whole request. The client here sends part of
        // a request and then only listens, so the test waits on the server's
        // deadline and on nothing else.
        let server = surface(Limits {
            request_deadline: Duration::from_millis(40),
            ..Limits::default()
        });
        let mut client = TcpStream::connect(server.local_address()).expect("connects");
        client
            .write_all(b"GET / HTTP/1.1\r\nX-Slow: ")
            .expect("writes");
        let reply = reply_to(client);
        assert!(reply.starts_with("HTTP/1.1 408 "), "{reply}");
    }

    #[test]
    fn one_connection_past_the_limit_is_told_to_try_again() {
        let server = surface(Limits {
            max_connections: 1,
            ..Limits::default()
        });
        // The first connection sends nothing, so it holds the one place.
        let holding = TcpStream::connect(server.local_address()).expect("connects");
        let refused = TcpStream::connect(server.local_address()).expect("connects");
        let reply = reply_to(refused);
        assert!(reply.starts_with("HTTP/1.1 503 "), "{reply}");

        // The place comes back when the first connection ends.
        let mut holding = holding;
        holding
            .write_all(b"GET /nothing HTTP/1.1\r\n\r\n")
            .expect("writes");
        assert!(reply_to(holding).starts_with("HTTP/1.1 404 "));
    }

    #[test]
    fn the_document_names_the_configured_callback_path() {
        let server = surface(Limits::default());
        let mut client = TcpStream::connect(server.local_address()).expect("connects");
        client
            .write_all(b"GET /auth/return?token=x HTTP/1.1\r\n\r\n")
            .expect("writes");
        let reply = reply_to(client);
        assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
        assert!(
            reply.contains(r#"<meta name="tallyowl-callback-path" content="/auth/return">"#),
            "{reply}"
        );
    }

    #[test]
    fn a_dashboard_on_a_network_address_needs_the_gateway_setting() {
        let logger = || {
            Arc::new(Logger::new(
                "tallyowl-head",
                "0.0.0",
                tallyowl_obs::log::Severity::Error,
            ))
        };
        let refused = start_with(
            "0.0.0.0:0",
            echo(),
            Settings::default(),
            Limits::default(),
            logger(),
        )
        .err()
        .expect("a network address with no gateway is refused");
        assert!(
            refused.to_string().contains("dashboard.allowPlaintext"),
            "{refused}"
        );

        let behind_a_gateway = start_with(
            "0.0.0.0:0",
            echo(),
            Settings {
                allow_plaintext: true,
                ..Settings::default()
            },
            Limits::default(),
            logger(),
        )
        .expect("a gateway in front is the operator's statement");
        behind_a_gateway.stop();
        assert!(is_loopback("127.0.0.1:5120") && is_loopback("[::1]:5120"));
        assert!(!is_loopback("10.0.0.4:5120"));
    }
}
