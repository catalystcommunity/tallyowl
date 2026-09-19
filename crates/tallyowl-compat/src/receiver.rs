//! The OpenTelemetry push receiver.
//!
//! # It does not listen until an operator says so
//!
//! D12: "The receiver does not open a listening socket by default. An operator
//! enables the listener in configuration. A default TallyOwl installation
//! therefore accepts no OpenTelemetry traffic and exposes no OpenTelemetry
//! port." `compatibility.openTelemetry.enabled` is `false` in every values
//! document, and the collector never calls [`start`] unless it is true.
//!
//! # Logs are refused, and the refusal says so
//!
//! `AGENTS.md`: "OpenTelemetry logs are out of scope." A receiver that answered
//! `404` on `/v1/logs` would look like a misconfigured address, and an operator
//! would spend an afternoon on it. So the path exists, answers `501`, and says
//! in one sentence that TallyOwl does not take log records and what to do
//! instead.
//!
//! # Why OTLP over HTTP and not gRPC
//!
//! An exporter reaches this with `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`,
//! which every OpenTelemetry SDK supports and many default to. gRPC needs an
//! HTTP/2 implementation and its own dependency set inside the always-on
//! collector, for a transport that carries the same bytes. See
//! `docs/IMPLEMENTATION_LOG.md` for what a gRPC listener would cost and where
//! it would go.
//!
//! The body is a protocol buffer. OTLP also defines a JSON encoding, and this
//! build answers `415` for it with the setting to change, rather than reading
//! half of it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tallyowl_collector_api::types::TelemetryItem;

use crate::otlp;
use crate::protobuf::{write_bytes_field, write_varint_field};

/// What the receiver does with the items it normalized.
///
/// The receiver holds no queue and no durability promise of its own. It hands
/// items to collector intake, which is the only place that decides whether a
/// batch reached the durable store. A sink that returns an error makes the
/// receiver answer `503`, so an exporter retries rather than assuming it
/// arrived.
pub trait Sink: Send + Sync {
    fn accept(&self, items: Vec<TelemetryItem>) -> Result<(), String>;
}

/// A running receiver. Dropping the handle asks it to stop.
pub struct Receiver {
    local_address: std::net::SocketAddr,
    stopping: Arc<AtomicBool>,
}

impl Receiver {
    pub fn local_address(&self) -> std::net::SocketAddr {
        self.local_address
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.local_address);
    }
}

/// The largest push this receiver reads.
///
/// An exporter that sends more is told the limit rather than being cut off, and
/// the receiver never allocates from a `Content-Length` the caller chose.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Start listening. The collector calls this only when an operator enabled it.
pub fn start(address: &str, sink: Arc<dyn Sink>) -> std::io::Result<Receiver> {
    let listener = TcpListener::bind(address)?;
    let local_address = listener.local_addr()?;
    let stopping = Arc::new(AtomicBool::new(false));
    let loop_stopping = Arc::clone(&stopping);

    std::thread::Builder::new()
        .name("tallyowl-otlp".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if loop_stopping.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let sink = Arc::clone(&sink);
                std::thread::spawn(move || {
                    let _ = serve_one(stream, sink.as_ref());
                });
            }
        })?;

    Ok(Receiver {
        local_address,
        stopping,
    })
}

/// What one request produced, so a test can drive the routing without a socket.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

/// Route and answer one push.
pub fn handle(
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
    sink: &dyn Sink,
) -> Answer {
    if method != "POST" {
        return text(
            405,
            "This address takes an OpenTelemetry push, which is a POST.\n",
        );
    }
    match path {
        "/v1/metrics" | "/v1/traces" => {}
        "/v1/logs" => {
            // Named, refused, and explained. An unexplained 404 here costs an
            // operator an afternoon.
            return text(
                501,
                "TallyOwl does not take OpenTelemetry log records. It collects events, errors, traces, and metrics. Send your logs to a log system and keep the trace identifier on both sides.\n",
            );
        }
        _ => {
            return text(
                404,
                "This address takes an OpenTelemetry push at /v1/metrics or /v1/traces.\n",
            )
        }
    }

    let content_type = content_type.split(';').next().unwrap_or("").trim();
    if content_type != "application/x-protobuf" {
        return text(
            415,
            "This receiver reads the protocol-buffer encoding. Set OTEL_EXPORTER_OTLP_PROTOCOL to http/protobuf.\n",
        );
    }

    let normalized = if path == "/v1/metrics" {
        otlp::metrics(body)
    } else {
        otlp::traces(body)
    };
    let count = normalized.items.len();

    if let Err(reason) = sink.accept(normalized.items) {
        // Never acknowledge data TallyOwl did not keep. An exporter that reads
        // a 503 retries, and one that read a 200 would not.
        return text(
            503,
            &format!("TallyOwl could not accept this push. {reason}\n"),
        );
    }

    // The OpenTelemetry acknowledgement. An empty message means every point
    // arrived; a partial success names how many did not and why.
    let mut response = Vec::new();
    if normalized.rejected > 0 {
        let mut partial = Vec::new();
        write_varint_field(&mut partial, 1, normalized.rejected);
        if let Some(reason) = &normalized.reason {
            write_bytes_field(&mut partial, 2, reason.as_bytes());
        }
        write_bytes_field(&mut response, 1, &partial);
    }
    let _ = count;
    Answer {
        status: 200,
        content_type: "application/x-protobuf",
        body: response,
    }
}

fn text(status: u16, body: &str) -> Answer {
    Answer {
        status,
        content_type: "text/plain; charset=utf-8",
        body: body.as_bytes().to_vec(),
    }
}

fn serve_one(mut stream: TcpStream, sink: &dyn Sink) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts
        .next()
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("")
        .to_string();

    let mut content_type = String::new();
    let mut content_length = 0usize;
    let mut over_limit = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let lower = line.to_lowercase();
        if let Some(value) = lower.strip_prefix("content-type:") {
            content_type = value.trim().to_string();
        }
        if let Some(value) = lower.strip_prefix("content-length:") {
            let declared: usize = value.trim().parse().unwrap_or(0);
            // Never allocate from a length the caller chose. A push over the
            // limit is answered rather than read.
            over_limit = declared > MAX_BODY_BYTES;
            content_length = declared.min(MAX_BODY_BYTES);
        }
    }

    let answer = if over_limit {
        text(
            413,
            &format!(
                "This push is larger than the {} MiB an OpenTelemetry receiver takes. Send smaller batches.\n",
                MAX_BODY_BYTES / (1024 * 1024)
            ),
        )
    } else {
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        handle(&method, &path, &content_type, &body, sink)
    };

    let reason = match answer.status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        501 => "Not Implemented",
        _ => "Service Unavailable",
    };
    write!(
        stream,
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        answer.status,
        answer.content_type,
        answer.body.len()
    )?;
    stream.write_all(&answer.body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protobuf::{write_varint, Reader};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Collected {
        items: Mutex<Vec<TelemetryItem>>,
        refuse: bool,
    }

    impl Sink for Collected {
        fn accept(&self, items: Vec<TelemetryItem>) -> Result<(), String> {
            if self.refuse {
                return Err("the durable store is unreachable".to_string());
            }
            self.items.lock().expect("lock").extend(items);
            Ok(())
        }
    }

    /// One `ExportMetricsServiceRequest` holding one monotonic sum.
    fn one_metric_push() -> Vec<u8> {
        let mut point = Vec::new();
        write_varint(&mut point, (3 << 3) | 1);
        point.extend_from_slice(&1_000_000_000u64.to_le_bytes());
        write_varint(&mut point, (4 << 3) | 1);
        point.extend_from_slice(&3.0f64.to_bits().to_le_bytes());

        let mut sum = Vec::new();
        write_bytes_field(&mut sum, 1, &point);
        write_varint_field(&mut sum, 2, 2);
        write_varint_field(&mut sum, 3, 1);

        let mut metric = Vec::new();
        write_bytes_field(&mut metric, 1, b"requests");
        write_bytes_field(&mut metric, 7, &sum);

        let mut scope = Vec::new();
        write_bytes_field(&mut scope, 2, &metric);
        let mut resource_metrics = Vec::new();
        write_bytes_field(&mut resource_metrics, 2, &scope);
        let mut out = Vec::new();
        write_bytes_field(&mut out, 1, &resource_metrics);
        out
    }

    #[test]
    fn a_metric_push_is_accepted_and_reaches_the_sink() {
        let sink = Collected::default();
        let answer = handle(
            "POST",
            "/v1/metrics",
            "application/x-protobuf",
            &one_metric_push(),
            &sink,
        );
        assert_eq!(answer.status, 200);
        // An empty body is the OpenTelemetry way of saying every point arrived.
        assert!(answer.body.is_empty());
        assert_eq!(sink.items.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_log_push_is_refused_and_the_refusal_explains_itself() {
        let sink = Collected::default();
        let answer = handle("POST", "/v1/logs", "application/x-protobuf", b"", &sink);
        assert_eq!(answer.status, 501);
        let message = String::from_utf8(answer.body).unwrap();
        assert!(message.contains("does not take OpenTelemetry log records"));
        // It says what to do instead, rather than only what it will not do.
        assert!(message.contains("trace identifier"));
        assert!(sink.items.lock().unwrap().is_empty());
    }

    #[test]
    fn an_unknown_path_says_which_two_paths_exist() {
        let sink = Collected::default();
        let answer = handle("POST", "/v1/anything", "application/x-protobuf", b"", &sink);
        assert_eq!(answer.status, 404);
        let message = String::from_utf8(answer.body).unwrap();
        assert!(message.contains("/v1/metrics") && message.contains("/v1/traces"));
    }

    #[test]
    fn a_get_is_refused_because_a_push_is_a_post() {
        let sink = Collected::default();
        assert_eq!(
            handle("GET", "/v1/metrics", "application/x-protobuf", b"", &sink).status,
            405
        );
    }

    #[test]
    fn a_json_body_names_the_setting_that_makes_an_exporter_send_protobuf() {
        let sink = Collected::default();
        let answer = handle("POST", "/v1/metrics", "application/json", b"{}", &sink);
        assert_eq!(answer.status, 415);
        assert!(String::from_utf8(answer.body)
            .unwrap()
            .contains("OTEL_EXPORTER_OTLP_PROTOCOL"));
    }

    #[test]
    fn a_content_type_with_a_charset_still_reads_as_protobuf() {
        let sink = Collected::default();
        let answer = handle(
            "POST",
            "/v1/metrics",
            "application/x-protobuf; charset=utf-8",
            &one_metric_push(),
            &sink,
        );
        assert_eq!(answer.status, 200);
    }

    #[test]
    fn a_sink_that_cannot_keep_the_data_makes_the_answer_a_retry_rather_than_a_receipt() {
        // Never acknowledge data TallyOwl did not keep.
        let sink = Collected {
            refuse: true,
            ..Collected::default()
        };
        let answer = handle(
            "POST",
            "/v1/metrics",
            "application/x-protobuf",
            &one_metric_push(),
            &sink,
        );
        assert_eq!(answer.status, 503);
    }

    #[test]
    fn a_point_this_build_cannot_store_arrives_as_a_partial_success() {
        // An exponential histogram. The exporter learns the count rather than
        // assuming everything it sent arrived.
        let mut metric = Vec::new();
        write_bytes_field(&mut metric, 1, b"latency");
        write_bytes_field(&mut metric, 10, b"anything");
        let mut scope = Vec::new();
        write_bytes_field(&mut scope, 2, &metric);
        let mut resource_metrics = Vec::new();
        write_bytes_field(&mut resource_metrics, 2, &scope);
        let mut push = Vec::new();
        write_bytes_field(&mut push, 1, &resource_metrics);

        let sink = Collected::default();
        let answer = handle(
            "POST",
            "/v1/metrics",
            "application/x-protobuf",
            &push,
            &sink,
        );
        assert_eq!(answer.status, 200);
        assert!(!answer.body.is_empty(), "a partial success is reported");

        let mut reader = Reader::new(&answer.body);
        let partial = reader.next_field().expect("a partial-success field");
        let mut inner = Reader::new(partial.wire.as_bytes());
        let rejected = inner.next_field().expect("a rejected count");
        assert_eq!(rejected.wire.as_u64(), 1);
        let reason = inner.next_field().expect("a reason");
        assert!(reason.wire.as_text().contains("explicit"));
    }

    #[test]
    fn a_malformed_body_is_answered_rather_than_crashing_the_receiver() {
        let sink = Collected::default();
        let answer = handle(
            "POST",
            "/v1/traces",
            "application/x-protobuf",
            &[0xff; 64],
            &sink,
        );
        assert_eq!(answer.status, 200);
        assert!(sink.items.lock().unwrap().is_empty());
    }

    // ---- over a real socket ------------------------------------------------

    fn post(
        address: std::net::SocketAddr,
        path: &str,
        content_type: &str,
        body: &[u8],
    ) -> (u16, Vec<u8>) {
        let mut stream = TcpStream::connect(address).expect("connect");
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: t\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .expect("write");
        stream.write_all(body).expect("write body");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("read");
        let text = String::from_utf8_lossy(&response).into_owned();
        let status: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("status");
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.as_bytes().to_vec())
            .unwrap_or_default();
        (status, body)
    }

    #[test]
    fn a_push_over_a_socket_reaches_the_sink() {
        let sink = Arc::new(Collected::default());
        let receiver = start("127.0.0.1:0", Arc::clone(&sink) as Arc<dyn Sink>).expect("start");
        let (status, _) = post(
            receiver.local_address(),
            "/v1/metrics",
            "application/x-protobuf",
            &one_metric_push(),
        );
        assert_eq!(status, 200);
        assert_eq!(sink.items.lock().unwrap().len(), 1);
        receiver.stop();
    }

    #[test]
    fn a_push_larger_than_the_limit_is_answered_rather_than_read() {
        let sink = Arc::new(Collected::default());
        let receiver = start("127.0.0.1:0", Arc::clone(&sink) as Arc<dyn Sink>).expect("start");
        let mut stream = TcpStream::connect(receiver.local_address()).expect("connect");
        write!(
            stream,
            "POST /v1/metrics HTTP/1.1\r\nHost: t\r\nContent-Type: application/x-protobuf\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        )
        .expect("write");
        // The body is never sent. The receiver answers from the header, so it
        // never allocates from a length a caller chose.
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        assert!(response.contains("413"), "{response}");
        receiver.stop();
    }

    #[test]
    fn a_log_push_over_a_socket_is_refused_too() {
        let sink = Arc::new(Collected::default());
        let receiver = start("127.0.0.1:0", Arc::clone(&sink) as Arc<dyn Sink>).expect("start");
        let (status, body) = post(
            receiver.local_address(),
            "/v1/logs",
            "application/x-protobuf",
            b"",
        );
        assert_eq!(status, 501);
        assert!(String::from_utf8_lossy(&body).contains("log records"));
        receiver.stop();
    }
}
