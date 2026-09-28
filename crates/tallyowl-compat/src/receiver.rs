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
//!
//! # A body this build cannot read is never answered `200`
//!
//! An exporter reads `200` with an empty body as "every point arrived". So each
//! body that cannot be read gets its own answer, and each answer names the
//! setting to change:
//!
//! | The push | The answer |
//! | --- | --- |
//! | is compressed (`Content-Encoding: gzip`) | `415`. The always-on collector carries no decompressor. Set `compression: none` |
//! | has no `Content-Length`, or is chunked | `411` |
//! | is not an OpenTelemetry request message | `400` |
//!
//! A compressed body is the common one. Many exporters compress by default, and
//! the first byte of a gzip stream is not a valid field, so the request read as
//! an empty message and an operator saw no error and no data.
//!
//! # What one connection may cost
//!
//! The same three bounds as the operational endpoint, from
//! `tallyowl_obs::http`: a deadline on each read and write, a byte bound on the
//! request head, and a cap on the connections served at one time.

use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tallyowl_collector_api::types::TelemetryItem;
use tallyowl_obs::http::{
    bound_connection, read_head, refuse_over_capacity, Gate, HeadError, RequestHead,
};

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
    /// Keep these items, or say why none of them could be kept.
    ///
    /// `Ok` means at least part of the push reached the durable store, and the
    /// [`Kept`] says how much of it did not.
    fn accept(&self, items: Vec<TelemetryItem>) -> Result<Kept, String>;

    /// A push was answered without reading it, for this bounded reason. The
    /// collector counts it, so a fleet of exporters that all send a body this
    /// build cannot read is visible on a chart and not only in their own logs.
    fn unreadable(&self, _reason: &'static str) {}
}

/// What a sink did with one push that it did not refuse outright.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kept {
    /// Items that were offered and not kept: refused by a limit, or in a batch
    /// the durable store did not take.
    pub rejected: u64,
    /// The first reason, in words an exporter's log can carry.
    pub reason: Option<String>,
}

/// How many pushes this receiver serves at one time.
pub const MAX_CONNECTIONS: usize = 256;

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
    start_secured(address, sink, None)
}

/// Start listening, over TLS when `tls` is given. D62: the receiver follows the
/// intake rule, so a receiver on a network address serves the same
/// certificates as intake. TLS protects the data on the way. It does not make
/// the receiver authenticate anybody, and a network policy still controls who
/// can reach it.
pub fn start_secured(
    address: &str,
    sink: Arc<dyn Sink>,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> std::io::Result<Receiver> {
    let listener = TcpListener::bind(address)?;
    let local_address = listener.local_addr()?;
    let stopping = Arc::new(AtomicBool::new(false));
    let loop_stopping = Arc::clone(&stopping);
    let gate = Gate::new(MAX_CONNECTIONS);

    std::thread::Builder::new()
        .name("tallyowl-otlp".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if loop_stopping.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                // A thread for each connection has no bound of its own. An
                // exporter that reads a 503 retries, so refusing one more costs
                // nothing that was not already lost to the flood.
                let Some(permit) = gate.enter() else {
                    sink.unreadable("over-capacity");
                    refuse_over_capacity(stream);
                    continue;
                };
                let sink = Arc::clone(&sink);
                let tls = tls.clone();
                std::thread::spawn(move || {
                    let _permit = permit;
                    bound_connection(&stream);
                    match tls {
                        None => {
                            let _ = serve_one(stream, sink.as_ref());
                        }
                        Some(config) => {
                            // A peer that fails the handshake fails its first
                            // read below, and the connection ends with nothing
                            // read.
                            let Ok(session) = rustls::ServerConnection::new(config) else {
                                return;
                            };
                            let secured = rustls::StreamOwned::new(session, stream);
                            let _ = serve_one(secured, sink.as_ref());
                        }
                    }
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

    // A body that is not a request message. Answering 200 here is what made a
    // compressed push look like a success with no data behind it.
    if normalized.malformed && normalized.items.is_empty() {
        sink.unreadable("malformed");
        return text(
            400,
            "This body is not an OpenTelemetry request message, so nothing was kept. Send the uncompressed protocol-buffer encoding: set `compression: none` and OTEL_EXPORTER_OTLP_PROTOCOL to http/protobuf.\n",
        );
    }

    let mut rejected = normalized.rejected;
    let mut reason = normalized.reason;
    if normalized.malformed {
        // Part of the message was read before it broke. That part is kept, and
        // the exporter is told the rest was not.
        sink.unreadable("malformed");
        reason.get_or_insert_with(|| {
            "The request message ended early. What was read before that point was kept.".to_string()
        });
    }

    match sink.accept(normalized.items) {
        Ok(kept) => {
            rejected += kept.rejected;
            if reason.is_none() {
                reason = kept.reason;
            }
        }
        Err(refusal) => {
            // Never acknowledge data TallyOwl did not keep. An exporter that
            // reads a 503 retries, and one that read a 200 would not.
            return text(
                503,
                &format!("TallyOwl could not accept this push. {refusal}\n"),
            );
        }
    }

    // The OpenTelemetry acknowledgement. An empty message means every point
    // arrived; a partial success names how many did not and why.
    let mut response = Vec::new();
    if rejected > 0 || reason.is_some() {
        let mut partial = Vec::new();
        write_varint_field(&mut partial, 1, rejected);
        if let Some(reason) = &reason {
            write_bytes_field(&mut partial, 2, reason.as_bytes());
        }
        write_bytes_field(&mut response, 1, &partial);
    }
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

/// What the head of a push says about its body, before a byte of it is read.
#[derive(Debug, PartialEq)]
enum Body {
    /// Read this many bytes.
    Read(usize),
    /// Answer this, and read nothing.
    Refuse(u16, &'static str, String),
}

fn body_of(head: &RequestHead) -> Body {
    // Only a push carries a body this receiver reads. Every other request is
    // routed on its method and path alone.
    if head.method != "POST" || !matches!(head.path.as_str(), "/v1/metrics" | "/v1/traces") {
        return Body::Read(0);
    }
    if let Some(encoding) = head.header("content-encoding") {
        if !encoding.eq_ignore_ascii_case("identity") {
            return Body::Refuse(
                415,
                "compressed",
                format!(
                    "This push is compressed with `{encoding}`, and this receiver reads an uncompressed body only. Set `compression: none` on the exporter, or OTEL_EXPORTER_OTLP_COMPRESSION to none.\n"
                ),
            );
        }
    }
    let chunked = head
        .header("transfer-encoding")
        .is_some_and(|value| !value.eq_ignore_ascii_case("identity"));
    let declared = head
        .header("content-length")
        .and_then(|value| value.parse::<usize>().ok());
    match declared {
        Some(_) if chunked => Body::Refuse(411, "no-length", no_length()),
        // Never allocate from a length the caller chose. A push over the limit
        // is answered rather than read.
        Some(declared) if declared > MAX_BODY_BYTES => Body::Refuse(
            413,
            "too-large",
            format!(
                "This push is larger than the {} MiB an OpenTelemetry receiver takes. Send smaller batches.\n",
                MAX_BODY_BYTES / (1024 * 1024)
            ),
        ),
        Some(declared) => Body::Read(declared),
        None => Body::Refuse(411, "no-length", no_length()),
    }
}

fn no_length() -> String {
    "This push does not say how long its body is, so none of it was read. Send a Content-Length and do not use chunked transfer encoding.\n".to_string()
}

fn serve_one<S: Read + Write>(stream: S, sink: &dyn Sink) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let head = match read_head(&mut reader) {
        Ok(head) => head,
        Err(HeadError::Closed) => return Ok(()),
        Err(HeadError::Io(e)) => return Err(e),
        Err(HeadError::TooLarge) => {
            sink.unreadable("head-too-large");
            return write_answer(
                reader.get_mut(),
                &text(
                    431,
                    "This request's headers are larger than an OpenTelemetry receiver reads.\n",
                ),
            );
        }
    };

    let answer = match body_of(&head) {
        Body::Refuse(status, reason, message) => {
            sink.unreadable(reason);
            text(status, &message)
        }
        Body::Read(length) => {
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body)?;
            handle(
                &head.method,
                &head.path,
                head.header("content-type").unwrap_or(""),
                &body,
                sink,
            )
        }
    };
    write_answer(reader.get_mut(), &answer)
}

fn write_answer<W: Write>(stream: &mut W, answer: &Answer) -> std::io::Result<()> {
    let reason = match answer.status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
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
        /// What the sink reports it did not keep.
        kept: Kept,
        unreadable: Mutex<Vec<&'static str>>,
    }

    impl Sink for Collected {
        fn accept(&self, items: Vec<TelemetryItem>) -> Result<Kept, String> {
            if self.refuse {
                return Err("the durable store is unreachable".to_string());
            }
            self.items.lock().expect("lock").extend(items);
            Ok(self.kept.clone())
        }

        fn unreadable(&self, reason: &'static str) {
            self.unreadable.lock().expect("lock").push(reason);
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
    fn a_body_that_is_not_a_message_is_refused_and_counted_rather_than_acknowledged() {
        // A 200 with an empty body means "every point arrived". Nothing did.
        let sink = Collected::default();
        let answer = handle(
            "POST",
            "/v1/traces",
            "application/x-protobuf",
            &[0xff; 64],
            &sink,
        );
        assert_eq!(answer.status, 400);
        assert!(String::from_utf8(answer.body)
            .unwrap()
            .contains("compression: none"));
        assert!(sink.items.lock().unwrap().is_empty());
        assert_eq!(*sink.unreadable.lock().unwrap(), vec!["malformed"]);
    }

    #[test]
    fn a_message_that_breaks_part_way_keeps_what_was_read_and_says_the_rest_was_not() {
        let mut push = one_metric_push();
        push.extend_from_slice(&[0xff; 8]);
        let sink = Collected::default();
        let answer = handle(
            "POST",
            "/v1/metrics",
            "application/x-protobuf",
            &push,
            &sink,
        );
        assert_eq!(answer.status, 200);
        assert_eq!(sink.items.lock().unwrap().len(), 1);
        assert!(!answer.body.is_empty(), "the exporter is told");
    }

    #[test]
    fn what_intake_refused_reaches_the_exporter_as_a_partial_success() {
        // A full series budget or a label over its limit is a refusal the
        // producer can act on, and only if the producer hears about it.
        let sink = Collected {
            kept: Kept {
                rejected: 3,
                reason: Some("The metric `requests` holds too many series.".to_string()),
            },
            ..Collected::default()
        };
        let answer = handle(
            "POST",
            "/v1/metrics",
            "application/x-protobuf",
            &one_metric_push(),
            &sink,
        );
        assert_eq!(answer.status, 200);
        let mut reader = Reader::new(&answer.body);
        let partial = reader.next_field().expect("a partial-success field");
        let mut inner = Reader::new(partial.wire.as_bytes());
        assert_eq!(inner.next_field().expect("a count").wire.as_u64(), 3);
        assert!(inner
            .next_field()
            .expect("a reason")
            .wire
            .as_text()
            .contains("too many series"));
    }

    fn head(headers: &[(&str, &str)]) -> RequestHead {
        RequestHead {
            method: "POST".to_string(),
            path: "/v1/metrics".to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        }
    }

    #[test]
    fn a_compressed_push_is_refused_with_the_setting_that_turns_compression_off() {
        let refused = body_of(&head(&[
            ("content-encoding", "gzip"),
            ("content-length", "20"),
        ]));
        let Body::Refuse(status, reason, message) = refused else {
            panic!("a compressed body must not be read");
        };
        assert_eq!((status, reason), (415, "compressed"));
        assert!(message.contains("compression: none"));
        assert_eq!(
            body_of(&head(&[
                ("content-encoding", "identity"),
                ("content-length", "20")
            ])),
            Body::Read(20)
        );
    }

    #[test]
    fn a_push_with_no_length_is_refused_rather_than_read_as_empty() {
        assert!(matches!(body_of(&head(&[])), Body::Refuse(411, _, _)));
        assert!(matches!(
            body_of(&head(&[("transfer-encoding", "chunked")])),
            Body::Refuse(411, _, _)
        ));
        assert!(matches!(
            body_of(&head(&[
                ("transfer-encoding", "chunked"),
                ("content-length", "4")
            ])),
            Body::Refuse(411, _, _)
        ));
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
    fn a_compressed_push_over_a_socket_is_refused_and_counted() {
        let sink = Arc::new(Collected::default());
        let receiver = start("127.0.0.1:0", Arc::clone(&sink) as Arc<dyn Sink>).expect("start");
        let mut stream = TcpStream::connect(receiver.local_address()).expect("connect");
        write!(
            stream,
            "POST /v1/metrics HTTP/1.1\r\nHost: t\r\nContent-Type: application/x-protobuf\r\nContent-Encoding: gzip\r\nContent-Length: 4\r\n\r\n"
        )
        .expect("write");
        let _ = stream.write_all(&[0x1f, 0x8b, 0x08, 0x00]);
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        assert!(response.contains("415"), "{response}");
        assert!(sink.items.lock().unwrap().is_empty());
        assert_eq!(*sink.unreadable.lock().unwrap(), vec!["compressed"]);
        receiver.stop();
    }

    #[test]
    fn a_header_line_with_no_end_is_answered_and_the_receiver_keeps_serving() {
        let sink = Arc::new(Collected::default());
        let receiver = start("127.0.0.1:0", Arc::clone(&sink) as Arc<dyn Sink>).expect("start");
        let mut stream = TcpStream::connect(receiver.local_address()).expect("connect");
        let mut request = b"POST /v1/metrics HTTP/1.1\r\nX: ".to_vec();
        request.extend(vec![b'a'; tallyowl_obs::http::MAX_HEAD_BYTES as usize + 64]);
        let _ = stream.write_all(&request);
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        assert!(response.contains("431"), "{response}");

        let (status, _) = post(
            receiver.local_address(),
            "/v1/metrics",
            "application/x-protobuf",
            &one_metric_push(),
        );
        assert_eq!(status, 200);
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
