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

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::health::Health;
use crate::log::Logger;
use crate::metrics::Registry;

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

    std::thread::Builder::new()
        .name("tallyowl-operational".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if loop_stopping.load(Ordering::Relaxed) {
                    break;
                }
                match stream {
                    Ok(stream) => {
                        let health = Arc::clone(&health);
                        let metrics = Arc::clone(&metrics);
                        let logger = Arc::clone(&logger);
                        std::thread::spawn(move || {
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
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    // Drain the headers. This endpoint reads no body and no header value, so a
    // request cannot smuggle anything past it.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    // A query string is ignored; the path alone selects the answer.
    let path = path.split('?').next().unwrap_or("");

    let (status, content_type, body) = match (method, path) {
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
    fn a_query_string_does_not_change_the_route() {
        let (endpoint, health, _) = endpoint();
        health.declare("x", "not yet");
        health.pass("x");
        assert_eq!(get(endpoint.local_address(), "/readyz?verbose=1").0, 200);
        endpoint.stop();
    }
}
