//! The outbound Prometheus and OpenMetrics scrape.
//!
//! # It never scrapes by default
//!
//! D12: "The collector scrapes only the targets that an operator configures.
//! There is no default target." `compatibility.prometheus.targets` is empty in
//! every values document, so a collector with no configuration reaches nothing.
//!
//! # Why the HTTP client is written here
//!
//! `AGENTS.md` limits HTTP to a short list and a compatibility scrape is on it.
//! The request this makes is one `GET` with no body, no redirect, and no
//! authentication, against an address inside a trust boundary the operator
//! drew. `tallyowl_obs::http` writes the server half of the same shape for the
//! same reason. Adding a general HTTP client to the always-on collector for
//! twenty lines of request would put a much larger surface on the ingest path.
//!
//! # Counter resets
//!
//! An exposition counter carries no start time, so nothing in the text says
//! whether 5 after 900 is a restart or a defect. `docs/DATA_MODEL.md` needs
//! that answer, and the scraper is the only place that has both readings.
//!
//! So this holds the previous value of each series and moves `start_at` forward
//! when a cumulative value falls. A query then reads the pair exactly as it
//! reads a driver's: **a later `start_at` beside a smaller value is a restart**.
//! Without this the first scrape after a target restarts would look like a
//! counter that went backwards.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::Duration;

use tallyowl_collector_api::types::{MetricKind, MetricPointPayload};
use tallyowl_obs::error::TallyOwlError;

use crate::exposition;

/// What one scrape produced.
#[derive(Debug, Clone, Default)]
pub struct ScrapeResult {
    pub points: Vec<MetricPointPayload>,
    /// Lines the target published that this build could not read. A target's
    /// defect never stops the rest of its metrics from arriving.
    pub faults: Vec<exposition::LineFault>,
    /// Series whose cumulative value fell, which is a restart of the target.
    pub resets: u64,
}

/// A target an operator named, and the state needed to read it correctly.
#[derive(Debug)]
pub struct Scraper {
    targets: Vec<String>,
    timeout: Duration,
    /// The last cumulative value of each series, and the start it is counting
    /// from. Keyed by target and series.
    seen: Mutex<HashMap<(String, String), Seen>>,
}

#[derive(Debug, Clone, Copy)]
struct Seen {
    value: f64,
    start_at: i64,
}

impl Scraper {
    /// Build a scraper over the configured targets.
    ///
    /// An empty list is the ordinary case and it is not an error: it means no
    /// operator asked for a scrape, and the collector then starts no scrape
    /// loop at all.
    pub fn new(targets: Vec<String>, timeout: Duration) -> Scraper {
        Scraper {
            targets,
            timeout,
            seen: Mutex::new(HashMap::new()),
        }
    }

    pub fn targets(&self) -> &[String] {
        &self.targets
    }

    /// Read one target and normalize what it published.
    pub fn scrape_one(&self, target: &str, now_ms: i64) -> Result<ScrapeResult, TallyOwlError> {
        let body = fetch(target, self.timeout)?;
        Ok(self.normalize(target, &body, now_ms))
    }

    /// Turn one target's exposition into native points, with the reset rule
    /// applied. Separated from the fetch so a test can hold the text.
    pub fn normalize(&self, target: &str, body: &str, now_ms: i64) -> ScrapeResult {
        let parsed = exposition::parse(body, now_ms);
        let mut seen = self.seen.lock().expect("scrape lock");
        let mut resets = 0;
        let mut points = parsed.points;

        for point in &mut points {
            // A gauge falls all the time and that is what a gauge is for.
            if point.metric_kind == MetricKind::Gauge {
                continue;
            }
            let key = (target.to_string(), crate::series_key(point));
            let current = current_total(point);
            match seen.get_mut(&key) {
                None => {
                    seen.insert(
                        key,
                        Seen {
                            value: current,
                            start_at: point.start_at,
                        },
                    );
                }
                Some(held) => {
                    if current < held.value {
                        // The target restarted. The new start says so, and a
                        // query reads it the same way it reads a driver's.
                        resets += 1;
                        held.start_at = point.end_at;
                    }
                    held.value = current;
                    point.start_at = held.start_at;
                }
            }
        }

        ScrapeResult {
            points,
            faults: parsed.faults,
            resets,
        }
    }
}

/// The number a reset is judged on. A histogram's total observation count only
/// rises for the same reason a counter does.
fn current_total(point: &MetricPointPayload) -> f64 {
    match (&point.number_value, &point.histogram_value) {
        (Some(value), _) => *value,
        (None, Some(histogram)) => histogram.count as f64,
        _ => 0.0,
    }
}

/// One `GET`, one response, no redirect.
///
/// A redirect is not followed on purpose. An operator names a target inside a
/// trust boundary, and following a redirect would let that target send the
/// collector somewhere the operator did not name.
fn fetch(target: &str, timeout: Duration) -> Result<String, TallyOwlError> {
    let (host, port, path) = split_url(target)?;
    let address = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| {
            TallyOwlError::unavailable(format!(
                "The scrape target `{target}` could not be resolved. {e}"
            ))
        })?
        .next()
        .ok_or_else(|| {
            TallyOwlError::unavailable(format!(
                "The scrape target `{target}` resolved to no address."
            ))
        })?;

    let mut stream = TcpStream::connect_timeout(&address, timeout).map_err(|e| {
        TallyOwlError::unavailable(format!("The scrape target `{target}` did not answer. {e}"))
    })?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/openmetrics-text;version=1.0.0,text/plain;version=0.0.4\r\nUser-Agent: tallyowl-collector\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| {
        TallyOwlError::unavailable(format!("The scrape of `{target}` could not be sent. {e}"))
    })?;

    // A target that answers and then closes without draining the request
    // resets the connection. The answer already arrived, so a reset after the
    // first byte is the end of the body rather than a failed scrape.
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => raw.extend_from_slice(&chunk[..read]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset && !raw.is_empty() => break,
            Err(e) => {
                return Err(TallyOwlError::unavailable(format!(
                    "The scrape of `{target}` ended early. {e}"
                )))
            }
        }
    }
    let text = String::from_utf8_lossy(&raw).into_owned();

    let (head, body) = text.split_once("\r\n\r\n").ok_or_else(|| {
        TallyOwlError::unavailable(format!(
            "The scrape target `{target}` sent no response body."
        ))
    })?;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if status != 200 {
        return Err(TallyOwlError::unavailable(format!(
            "The scrape target `{target}` answered {status}. A target must answer 200 with its metrics."
        )));
    }
    // A chunked body is the ordinary case for a target that streams. Undo the
    // framing before the parser sees it, because a chunk length looks exactly
    // like a metric line with no value.
    if head.to_lowercase().contains("transfer-encoding: chunked") {
        return Ok(dechunk(body));
    }
    Ok(body.to_string())
}

fn dechunk(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some((size_line, after)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or(""), 16);
        let Ok(size) = size else { break };
        if size == 0 || size > after.len() {
            break;
        }
        out.push_str(&after[..size]);
        rest = after[size..].trim_start_matches("\r\n");
    }
    out
}

/// Split `http://host:port/path` into its parts.
fn split_url(target: &str) -> Result<(String, u16, String), TallyOwlError> {
    let refused = |reason: &str| {
        TallyOwlError::invalid_argument(format!(
            "The scrape target `{target}` is not usable: {reason}. Write it as `http://host:port/metrics`."
        ))
    };
    let rest = match target.split_once("://") {
        Some(("http", rest)) => rest,
        // A scrape crosses a network an operator controls, and this client does
        // not do TLS. Refusing is better than silently reaching the target in
        // the clear when the operator wrote `https`.
        Some(("https", _)) => {
            return Err(refused(
                "this build scrapes over HTTP only, inside a trust boundary you control",
            ))
        }
        Some((scheme, _)) => return Err(refused(&format!("`{scheme}` is not a scheme it reads"))),
        None => target,
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/metrics".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse()
                .map_err(|_| refused(&format!("`{port}` is not a port")))?,
        ),
        None => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return Err(refused("it names no host"));
    }
    Ok((host, port, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn scraper() -> Scraper {
        Scraper::new(vec![], Duration::from_secs(2))
    }

    #[test]
    fn no_target_is_configured_by_default() {
        // D12: a compatibility edge is an operator's choice, never a port or a
        // request that appears because the binary holds the feature.
        assert!(scraper().targets().is_empty());
    }

    #[test]
    fn a_url_splits_into_its_parts() {
        assert_eq!(
            split_url("http://127.0.0.1:9100/metrics").unwrap(),
            ("127.0.0.1".to_string(), 9100, "/metrics".to_string())
        );
        assert_eq!(
            split_url("http://host.example").unwrap(),
            ("host.example".to_string(), 80, "/metrics".to_string())
        );
    }

    #[test]
    fn an_https_target_is_refused_rather_than_reached_in_the_clear() {
        let refused = split_url("https://host.example/metrics").unwrap_err();
        assert!(refused.message.contains("HTTP only"));
    }

    #[test]
    fn a_target_that_is_not_a_url_names_what_to_write() {
        let refused = split_url("ftp://host/metrics").unwrap_err();
        assert!(refused.message.contains("http://host:port/metrics"));
    }

    #[test]
    fn a_counter_that_falls_moves_its_start_forward() {
        let scraper = scraper();
        let first = scraper.normalize("t", "# TYPE x_total counter\nx_total 900\n", 1_000);
        assert_eq!(first.resets, 0);
        let first_start = first.points[0].start_at;

        // The target restarted and its counter began again.
        let second = scraper.normalize("t", "# TYPE x_total counter\nx_total 5\n", 2_000);
        assert_eq!(second.resets, 1);
        assert!(
            second.points[0].start_at > first_start,
            "a restart carries a later start"
        );
        assert_eq!(second.points[0].number_value, Some(5.0));

        // It keeps counting from the new start.
        let third = scraper.normalize("t", "# TYPE x_total counter\nx_total 9\n", 3_000);
        assert_eq!(third.resets, 0);
        assert_eq!(third.points[0].start_at, second.points[0].start_at);
    }

    #[test]
    fn a_counter_that_rises_keeps_the_start_it_had() {
        let scraper = scraper();
        let first = scraper.normalize("t", "# TYPE x_total counter\nx_total 1\n", 1_000);
        let second = scraper.normalize("t", "# TYPE x_total counter\nx_total 2\n", 2_000);
        assert_eq!(first.points[0].start_at, second.points[0].start_at);
    }

    #[test]
    fn a_gauge_that_falls_is_not_a_reset() {
        let scraper = scraper();
        scraper.normalize("t", "# TYPE q gauge\nq 10\n", 1_000);
        let second = scraper.normalize("t", "# TYPE q gauge\nq 1\n", 2_000);
        assert_eq!(second.resets, 0);
    }

    #[test]
    fn two_targets_keep_their_own_reset_state() {
        let scraper = scraper();
        scraper.normalize("a", "# TYPE x_total counter\nx_total 900\n", 1_000);
        let other = scraper.normalize("b", "# TYPE x_total counter\nx_total 5\n", 2_000);
        assert_eq!(other.resets, 0, "target b was never at 900");
    }

    #[test]
    fn two_series_of_one_metric_keep_their_own_reset_state() {
        let scraper = scraper();
        let text = "# TYPE x_total counter\nx_total{r=\"a\"} 900\nx_total{r=\"b\"} 900\n";
        scraper.normalize("t", text, 1_000);
        let after = scraper.normalize(
            "t",
            "# TYPE x_total counter\nx_total{r=\"a\"} 5\nx_total{r=\"b\"} 901\n",
            2_000,
        );
        assert_eq!(after.resets, 1, "only one of the two fell");
    }

    #[test]
    fn a_histogram_that_falls_is_a_reset_too() {
        let scraper = scraper();
        let before =
            "# TYPE h histogram\nh_bucket{le=\"1\"} 40\nh_bucket{le=\"+Inf\"} 50\nh_count 50\n";
        let after =
            "# TYPE h histogram\nh_bucket{le=\"1\"} 1\nh_bucket{le=\"+Inf\"} 2\nh_count 2\n";
        scraper.normalize("t", before, 1_000);
        assert_eq!(scraper.normalize("t", after, 2_000).resets, 1);
    }

    // ---- the fetch ---------------------------------------------------------

    /// A target that answers one request, so the client half is covered by a
    /// real socket rather than by a stand-in.
    fn one_shot_target(response: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Drain the request before answering. A server that closes with
                // the request unread resets the connection, which is a fault of
                // this stand-in rather than of the client under test.
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while stream.read_exact(&mut byte).is_ok() {
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Write);
            }
        });
        format!("http://{address}/metrics")
    }

    #[test]
    fn a_target_that_answers_is_read_and_normalized() {
        let target = one_shot_target(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n# TYPE x_total counter\nx_total 7\n",
        );
        let result = scraper().scrape_one(&target, 1_000).expect("a live target");
        assert_eq!(result.points.len(), 1);
        assert_eq!(result.points[0].number_value, Some(7.0));
    }

    #[test]
    fn a_target_that_answers_with_an_error_is_a_failed_scrape_and_not_an_empty_one() {
        // An empty scrape would read as "this target has no metrics", which is
        // the same shape as an outage and means the opposite.
        let target = one_shot_target("HTTP/1.1 500 Internal Server Error\r\n\r\nno\n");
        let failed = scraper().scrape_one(&target, 1_000).unwrap_err();
        assert!(failed.message.contains("500"));
    }

    #[test]
    fn a_target_nothing_is_listening_on_reports_unavailable() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        drop(listener);
        let failed = Scraper::new(vec![], Duration::from_millis(200))
            .scrape_one(&format!("http://{address}/metrics"), 1_000)
            .unwrap_err();
        assert_eq!(failed.code, tallyowl_obs::error::ErrorCode::Unavailable);
    }

    #[test]
    fn a_chunked_body_is_unframed_before_the_parser_sees_it() {
        let target = one_shot_target(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n21\r\n# TYPE x_total counter\nx_total 3\n\r\n0\r\n\r\n",
        );
        let result = scraper().scrape_one(&target, 1_000).expect("a live target");
        assert_eq!(result.points.len(), 1, "{:?}", result.faults);
        assert_eq!(result.points[0].number_value, Some(3.0));
    }
}
