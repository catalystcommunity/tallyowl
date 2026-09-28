//! The operational endpoint: health and metrics, and nothing else.
//!
//! This is not an ingest surface. `AGENTS.md` forbids a generic HTTP ingest API
//! and permits a service to expose its own Prometheus and OpenMetrics endpoint,
//! which is what this is. It answers three paths and refuses every other, so it
//! cannot grow into one by accident.
//!
//! | Path | Answers |
//! | --- | --- |
//! | `/livez` | Is this process working, or should something restart it? |
//! | `/readyz` | Can this process do its job right now? |
//! | `/metrics` | The Prometheus and OpenMetrics text exposition |
//!
//! The server runs on threads rather than an async runtime, matching the CSIL
//! transport it sits beside.
//!
//! # What one connection may cost
//!
//! A thread serves each connection, so a connection that never finishes would
//! hold a thread for ever. Three bounds stop that, and the compatibility
//! receiver uses the same three through [`read_head`] and [`Gate`]:
//!
//! - every read and write has a deadline of [`IO_TIMEOUT`];
//! - a request line and its headers fit inside [`MAX_HEAD_BYTES`], so a line
//!   with no end cannot grow a buffer without limit;
//! - at most [`MAX_CONNECTIONS`] are served at one time, and one more is
//!   answered `503` rather than given a thread.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::health::Health;
use crate::log::Logger;
use crate::metrics::Registry;

/// How long one read or one write may take before the connection is dropped.
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// The most bytes a request line and its headers may hold together.
pub const MAX_HEAD_BYTES: u64 = 16 * 1024;

/// The most header lines one request may carry.
pub const MAX_HEADERS: usize = 100;

/// How many connections this endpoint serves at one time. A probe, a scrape,
/// and a person with a browser are a handful; this leaves room for many of
/// each and still bounds the threads a flood can take.
pub const MAX_CONNECTIONS: usize = 256;

/// A request line and its headers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    /// The path with any query string removed.
    pub path: String,
    /// Each header, with its name in lower case and its value trimmed.
    pub headers: Vec<(String, String)>,
}

impl RequestHead {
    /// The value of one header, by its lower-case name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(held, _)| held == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Why a request head could not be read.
#[derive(Debug)]
pub enum HeadError {
    /// The peer closed before it sent a request line.
    Closed,
    /// The head is larger than [`MAX_HEAD_BYTES`] or holds more than
    /// [`MAX_HEADERS`] lines.
    TooLarge,
    Io(std::io::Error),
}

/// Read a request line and its headers, inside the byte and line bounds.
///
/// The bound is on the whole head rather than on one line, so a caller cannot
/// be made to hold more than [`MAX_HEAD_BYTES`] whatever shape the request has.
/// The reader is left at the first byte of the body.
pub fn read_head<R: BufRead>(reader: &mut R) -> Result<RequestHead, HeadError> {
    let mut limited = reader.take(MAX_HEAD_BYTES);
    let mut request_line = String::new();
    if read_bounded_line(&mut limited, &mut request_line)? == 0 {
        return Err(HeadError::Closed);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    // A query string is ignored; the path alone selects the answer.
    let path = parts
        .next()
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("")
        .to_string();

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if read_bounded_line(&mut limited, &mut line)? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(HeadError::TooLarge);
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_lowercase(), value.trim().to_string()));
        }
    }
    Ok(RequestHead {
        method,
        path,
        headers,
    })
}

/// One line from a bounded reader. A line that the bound cut short is a head
/// that is too large, not a short line.
fn read_bounded_line<R: BufRead>(
    limited: &mut std::io::Take<R>,
    line: &mut String,
) -> Result<usize, HeadError> {
    let mut raw = Vec::new();
    let read = limited.read_until(b'\n', &mut raw).map_err(HeadError::Io)?;
    if read > 0 && !raw.ends_with(b"\n") && limited.limit() == 0 {
        return Err(HeadError::TooLarge);
    }
    // A header is ASCII in practice. A byte that is not text is replaced, so a
    // hostile header cannot fail the read; it only fails to match anything.
    line.push_str(&String::from_utf8_lossy(&raw));
    Ok(read)
}

/// Counts the connections a listener is serving, so it can refuse one more.
///
/// A listener that gives every connection a thread has no bound of its own.
/// This is the bound: [`Gate::enter`] answers `None` at the cap, and the permit
/// it returns gives the place back when the connection ends, including when the
/// thread that held it panics.
#[derive(Debug)]
pub struct Gate {
    open: AtomicUsize,
    cap: usize,
}

impl Gate {
    pub fn new(cap: usize) -> Arc<Gate> {
        Arc::new(Gate {
            open: AtomicUsize::new(0),
            cap,
        })
    }

    pub fn enter(self: &Arc<Gate>) -> Option<Permit> {
        let mut held = self.open.load(Ordering::Relaxed);
        loop {
            if held >= self.cap {
                return None;
            }
            match self.open.compare_exchange_weak(
                held,
                held + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(Permit {
                        gate: Arc::clone(self),
                    })
                }
                Err(now) => held = now,
            }
        }
    }

    pub fn open(&self) -> usize {
        self.open.load(Ordering::Relaxed)
    }
}

/// One place inside a [`Gate`]. Dropping it gives the place back.
#[derive(Debug)]
pub struct Permit {
    gate: Arc<Gate>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.gate.open.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Give a connection its read and write deadlines.
pub fn bound_connection(stream: &TcpStream) {
    // A socket that refuses a deadline is one the operating system already
    // closed, and the first read reports that.
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
}

/// Tell a connection over the cap why it was not served, and close it.
pub fn refuse_over_capacity(mut stream: TcpStream) {
    bound_connection(&stream);
    let body = "This address is serving as many connections as it takes at one time. Try again.\n";
    let _ = write!(
        stream,
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

/// A running operational endpoint. Dropping the handle asks it to stop.
pub struct OperationalEndpoint {
    local_address: std::net::SocketAddr,
    stopping: Arc<AtomicBool>,
}

impl OperationalEndpoint {
    pub fn local_address(&self) -> std::net::SocketAddr {
        self.local_address
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        // Wake the accept loop so it observes the flag.
        let _ = TcpStream::connect(self.local_address);
    }
}

/// Start the endpoint on `address`. Binding to port 0 gives an operating-system
/// port, which is what a test wants.
pub fn start(
    address: &str,
    health: Arc<Health>,
    metrics: Arc<Registry>,
    logger: Arc<Logger>,
) -> std::io::Result<OperationalEndpoint> {
    let listener = TcpListener::bind(address)?;
    let local_address = listener.local_addr()?;
    let stopping = Arc::new(AtomicBool::new(false));
    let loop_stopping = Arc::clone(&stopping);
    let gate = Gate::new(MAX_CONNECTIONS);

    std::thread::Builder::new()
        .name("tallyowl-operational".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if loop_stopping.load(Ordering::Relaxed) {
                    break;
                }
                match stream {
                    Ok(stream) => {
                        let Some(permit) = gate.enter() else {
                            refuse_over_capacity(stream);
                            continue;
                        };
                        let health = Arc::clone(&health);
                        let metrics = Arc::clone(&metrics);
                        let logger = Arc::clone(&logger);
                        std::thread::spawn(move || {
                            let _permit = permit;
                            if let Err(e) = serve_one(stream, &health, &metrics) {
                                logger.debug(
                                    "An operational request ended early.",
                                    &[("reason", &e.to_string())],
                                );
                            }
                        });
                    }
                    Err(e) => {
                        logger.warning(
                            "The operational endpoint could not accept a connection.",
                            &[("reason", &e.to_string())],
                        );
                    }
                }
            }
        })?;

    Ok(OperationalEndpoint {
        local_address,
        stopping,
    })
}

fn serve_one(mut stream: TcpStream, health: &Health, metrics: &Registry) -> std::io::Result<()> {
    bound_connection(&stream);
    let mut reader = BufReader::new(stream.try_clone()?);
    // Drain the headers. This endpoint reads no body and no header value, so a
    // request cannot smuggle anything past it.
    let head = match read_head(&mut reader) {
        Ok(head) => head,
        Err(HeadError::Closed) => return Ok(()),
        Err(HeadError::Io(e)) => return Err(e),
        Err(HeadError::TooLarge) => {
            let body = "This request's headers are larger than this address reads.\n";
            write!(
                stream,
                "HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )?;
            return stream.flush();
        }
    };

    let (status, content_type, body) = match (head.method.as_str(), head.path.as_str()) {
        ("GET", "/livez") => {
            let report = health.report();
            let status = if report.live { 200 } else { 503 };
            (status, "application/json", report.to_json())
        }
        ("GET", "/readyz") => {
            let report = health.report();
            let status = if report.ready { 200 } else { 503 };
            (status, "application/json", report.to_json())
        }
        ("GET", "/metrics") => (
            200,
            "text/plain; version=0.0.4; charset=utf-8",
            metrics.render_text(),
        ),
        ("GET", _) => (
            404,
            "text/plain; charset=utf-8",
            "This address serves health and metrics only.\n".to_string(),
        ),
        _ => (
            405,
            "text/plain; charset=utf-8",
            "This address answers a GET request only.\n".to_string(),
        ),
    };

    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Service Unavailable",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Severity;
    use crate::metrics::{labels, MetricKind};
    use std::io::Read;

    fn get(address: std::net::SocketAddr, path: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(address).expect("connect");
        write!(stream, "GET {path} HTTP/1.1\r\nHost: t\r\n\r\n").expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        let status: u16 = response
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("status");
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
        (status, body)
    }

    fn endpoint() -> (OperationalEndpoint, Arc<Health>, Arc<Registry>) {
        let health = Health::new();
        let metrics = Registry::new();
        let logger = Arc::new(Logger::new("test", "0.0.0", Severity::Error));
        let e = start(
            "127.0.0.1:0",
            Arc::clone(&health),
            Arc::clone(&metrics),
            logger,
        )
        .expect("start");
        (e, health, metrics)
    }

    #[test]
    fn readiness_answers_503_until_every_check_passes() {
        let (endpoint, health, _) = endpoint();
        health.declare("durable-store", "Cannot reach the durable store.");

        let (status, body) = get(endpoint.local_address(), "/readyz");
        assert_eq!(status, 503);
        assert!(body.contains("Cannot reach the durable store."));

        health.pass("durable-store");
        let (status, _) = get(endpoint.local_address(), "/readyz");
        assert_eq!(status, 200);
        endpoint.stop();
    }

    #[test]
    fn liveness_is_separate_from_readiness() {
        let (endpoint, health, _) = endpoint();
        health.declare("durable-store", "Cannot reach the durable store.");
        assert_eq!(get(endpoint.local_address(), "/livez").0, 200);
        assert_eq!(get(endpoint.local_address(), "/readyz").0, 503);

        health.stop_living();
        assert_eq!(get(endpoint.local_address(), "/livez").0, 503);
        endpoint.stop();
    }

    #[test]
    fn metrics_render_at_the_expected_path() {
        let (endpoint, _, metrics) = endpoint();
        metrics
            .declare("tallyowl_events_total", MetricKind::Counter, "Events.", &[])
            .unwrap();
        metrics.increment("tallyowl_events_total", &labels(&[]));
        let (status, body) = get(endpoint.local_address(), "/metrics");
        assert_eq!(status, 200);
        assert!(body.contains("tallyowl_events_total 1"));
        endpoint.stop();
    }

    #[test]
    fn no_other_path_answers() {
        let (endpoint, _, _) = endpoint();
        assert_eq!(get(endpoint.local_address(), "/").0, 404);
        assert_eq!(get(endpoint.local_address(), "/v1/events").0, 404);
        endpoint.stop();
    }

    #[test]
    fn a_header_line_with_no_end_is_refused_rather_than_buffered_without_limit() {
        // One byte past the bound and still no newline. The read stops at the
        // bound, so what a caller can make this hold is fixed.
        let endless = vec![b'a'; MAX_HEAD_BYTES as usize + 1];
        let mut reader = std::io::BufReader::new(&endless[..]);
        assert!(matches!(read_head(&mut reader), Err(HeadError::TooLarge)));

        let mut request = b"GET /metrics HTTP/1.1\r\nX: ".to_vec();
        request.extend(vec![b'a'; MAX_HEAD_BYTES as usize]);
        let mut reader = std::io::BufReader::new(&request[..]);
        assert!(matches!(read_head(&mut reader), Err(HeadError::TooLarge)));
    }

    #[test]
    fn more_header_lines_than_the_bound_are_refused() {
        let mut request = b"GET /metrics HTTP/1.1\r\n".to_vec();
        for index in 0..=MAX_HEADERS {
            request.extend(format!("x-{index}: v\r\n").into_bytes());
        }
        request.extend(b"\r\n");
        let mut reader = std::io::BufReader::new(&request[..]);
        assert!(matches!(read_head(&mut reader), Err(HeadError::TooLarge)));
    }

    #[test]
    fn a_head_is_read_and_the_reader_is_left_at_the_body() {
        let request =
            b"POST /v1/metrics?x=1 HTTP/1.1\r\nContent-Type: A/b\r\nContent-Length: 4\r\n\r\nbody";
        let mut reader = std::io::BufReader::new(&request[..]);
        let head = read_head(&mut reader).expect("a head");
        assert_eq!(head.method, "POST");
        assert_eq!(head.path, "/v1/metrics");
        assert_eq!(head.header("content-type"), Some("A/b"));
        let mut rest = String::new();
        reader.read_to_string(&mut rest).expect("the body");
        assert_eq!(rest, "body");
    }

    #[test]
    fn a_gate_refuses_one_more_than_its_cap_and_a_dropped_permit_gives_the_place_back() {
        let gate = Gate::new(2);
        let first = gate.enter().expect("room");
        let _second = gate.enter().expect("room");
        assert!(gate.enter().is_none(), "the cap is two");
        drop(first);
        assert!(
            gate.enter().is_some(),
            "a finished connection frees a place"
        );
    }

    #[test]
    fn an_oversized_head_over_a_socket_is_answered_and_the_endpoint_keeps_serving() {
        let (endpoint, _, _) = endpoint();
        let mut stream = TcpStream::connect(endpoint.local_address()).expect("connect");
        let mut request = b"GET /metrics HTTP/1.1\r\nX: ".to_vec();
        request.extend(vec![b'a'; MAX_HEAD_BYTES as usize + 64]);
        // The endpoint may answer and close before the whole line is written.
        let _ = stream.write_all(&request);
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        assert!(response.contains("431"), "{response}");
        assert_eq!(get(endpoint.local_address(), "/livez").0, 200);
        endpoint.stop();
    }

    #[test]
    fn a_query_string_does_not_change_the_route() {
        let (endpoint, health, _) = endpoint();
        health.declare("x", "not yet");
        health.pass("x");
        assert_eq!(get(endpoint.local_address(), "/readyz?verbose=1").0, 200);
        endpoint.stop();
    }
}
