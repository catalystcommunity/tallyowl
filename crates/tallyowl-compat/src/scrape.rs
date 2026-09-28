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
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tallyowl_collector_api::types::{
    MetricKind, MetricPointPayload, MetricPointPayload_temporality as Temporality, PropertyOrigin,
};
use tallyowl_obs::error::TallyOwlError;
use tallyowl_wire::{collector as wire, Value};

use crate::exposition::{self, Format};

/// The largest response this scraper holds, unless an operator says otherwise
/// with `compatibility.prometheus.maxBodyBytes`.
pub const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// How long a series may stay away before its reset state is forgotten.
///
/// A target that churns its labels would otherwise grow this state without
/// limit. A series that comes back after this long starts again from its next
/// reading, which costs one interval of `increase` and nothing else.
pub const DEFAULT_IDLE_EXPIRY: Duration = Duration::from_secs(60 * 60);

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
    max_body_bytes: usize,
    idle_expiry_ms: i64,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// The last cumulative value of each series, and the start it is counting
    /// from. Keyed by target, metric name, and series.
    ///
    /// The metric name is part of the key because `series_key` holds the kind
    /// and the labels only. Two counters with no labels on one target are two
    /// series, and one entry for both would read every scrape as a restart.
    seen: HashMap<(String, String, String), Seen>,
    swept_at: i64,
}

#[derive(Debug, Clone, Copy)]
struct Seen {
    value: f64,
    start_at: i64,
    seen_at: i64,
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
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            idle_expiry_ms: DEFAULT_IDLE_EXPIRY.as_millis() as i64,
            state: Mutex::new(State::default()),
        }
    }

    /// The largest response body one scrape holds.
    pub fn with_max_body_bytes(mut self, bytes: usize) -> Scraper {
        self.max_body_bytes = bytes.max(1);
        self
    }

    /// How long a series may stay away before its reset state is forgotten.
    pub fn with_idle_expiry(mut self, expiry: Duration) -> Scraper {
        self.idle_expiry_ms = (expiry.as_millis() as i64).max(1);
        self
    }

    pub fn targets(&self) -> &[String] {
        &self.targets
    }

    /// How many series this scraper holds reset state for.
    pub fn held_series(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .seen
            .len()
    }

    /// Read one target and normalize what it published.
    pub fn scrape_one(&self, target: &str, now_ms: i64) -> Result<ScrapeResult, TallyOwlError> {
        let fetched = fetch(target, self.timeout, self.max_body_bytes)?;
        Ok(self.normalize_as(target, &fetched.body, fetched.format, now_ms))
    }

    /// Turn one target's exposition into native points, with the reset rule
    /// applied. Separated from the fetch so a test can hold the text.
    pub fn normalize(&self, target: &str, body: &str, now_ms: i64) -> ScrapeResult {
        self.normalize_as(target, body, Format::Prometheus, now_ms)
    }

    /// As [`Scraper::normalize`], for a body whose format the target declared.
    pub fn normalize_as(
        &self,
        target: &str,
        body: &str,
        format: Format,
        now_ms: i64,
    ) -> ScrapeResult {
        let parsed = exposition::parse_as(body, now_ms, format);
        let instance = instance_of(target);
        // A panic elsewhere in a scrape must not make every later scrape of
        // every target panic on a poisoned lock. The map is valid after any
        // partial update: an entry is either the old reading or the new one.
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut resets = 0;
        let mut points = parsed.points;

        for point in &mut points {
            name_the_producer(point, &instance);

            // A gauge falls all the time and that is what a gauge is for.
            if point.metric_kind == MetricKind::Gauge {
                continue;
            }
            let current = current_total(point);
            // A reading that is not a number says nothing about a restart. It
            // must not replace the held value either: every comparison with it
            // is false, so the next real fall would go unseen.
            if current.is_nan() {
                continue;
            }
            let key = (
                target.to_string(),
                point.metric_name.clone(),
                crate::series_key(point),
            );
            match state.seen.get_mut(&key) {
                None => {
                    state.seen.insert(
                        key,
                        Seen {
                            value: current,
                            start_at: point.start_at,
                            seen_at: now_ms,
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
                    held.seen_at = now_ms;
                    point.start_at = held.start_at;
                }
            }
        }

        // Forget what stopped reporting. The sweep runs a few times in each
        // expiry rather than on every scrape, because it reads every entry.
        if now_ms - state.swept_at >= self.idle_expiry_ms / 4 {
            let oldest = now_ms - self.idle_expiry_ms;
            state.seen.retain(|_, held| held.seen_at >= oldest);
            state.swept_at = now_ms;
        }

        ScrapeResult {
            points,
            faults: parsed.faults,
            resets,
        }
    }
}

/// The `host:port` a target is reached at, which is what tells two producers of
/// one series apart.
pub fn instance_of(target: &str) -> String {
    match split_url(target) {
        Ok(url) => url.host_header,
        // A target that does not parse never reaches a fetch. A caller that
        // normalizes held text still gets a stable value.
        Err(_) => target.to_string(),
    }
}

/// Put the producer into the series identity.
///
/// The identity of a series is its kind and its labels. Two replicas of one
/// application publish the same names and the same labels, so without this
/// they are one series whose value jumps between two totals, and a `rate` over
/// it reads every jump down as a restart. The `instance` label is what keeps
/// them apart. A label of that name which the target published itself is kept
/// under `exported_instance`, because the target's word for itself is still a
/// fact worth keeping.
fn name_the_producer(point: &mut MetricPointPayload, instance: &str) {
    for label in &mut point.labels {
        if label.key == "instance" {
            label.key = "exported_instance".to_string();
        }
    }
    // The origin is `collector`: this value is the address the collector
    // reached, so it is one the collector can vouch for.
    point.labels.push(wire::property(
        "instance",
        Value::Text(instance.to_string()),
        PropertyOrigin::Collector,
    ));
}

/// How one scrape went, as two gauges that travel with it.
///
/// `up` is 1 when the target answered and 0 when it did not, and
/// `scrape_duration_seconds` is how long the attempt took. Both carry the
/// `instance` label, so a chart or an alert reads them for each target. They are
/// produced for a failed scrape too, because a target that is down otherwise
/// leaves nothing in TallyOwl to query.
pub fn target_health(target: &str, up: bool, seconds: f64, now_ms: i64) -> Vec<MetricPointPayload> {
    let instance = instance_of(target);
    let gauge = |name: &str, help: &str, unit: Option<&str>, value: f64| MetricPointPayload {
        metric_name: name.to_string(),
        metric_kind: MetricKind::Gauge,
        unit: unit.map(str::to_string),
        description: Some(help.to_string()),
        monotonic: false,
        temporality: Temporality::Cumulative,
        start_at: now_ms,
        end_at: now_ms,
        labels: vec![wire::property(
            "instance",
            Value::Text(instance.clone()),
            PropertyOrigin::Collector,
        )],
        number_value: Some(value),
        histogram_value: None,
        exemplar_trace_id: None,
    };
    vec![
        gauge(
            "up",
            "1 when the last scrape of this target succeeded, and 0 when it did not.",
            None,
            if up { 1.0 } else { 0.0 },
        ),
        gauge(
            "scrape_duration_seconds",
            "How long the last scrape of this target took.",
            Some("seconds"),
            seconds,
        ),
    ]
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

/// What a target answered, and the format it said the answer is in.
#[derive(Debug, Clone, PartialEq)]
struct Fetched {
    body: String,
    format: Format,
}

/// One `GET`, one response, no redirect.
///
/// A redirect is not followed on purpose. An operator names a target inside a
/// trust boundary, and following a redirect would let that target send the
/// collector somewhere the operator did not name.
///
/// `timeout` bounds the **whole** fetch: the connection, the request, and every
/// byte of the answer. A deadline on each read alone is not a bound, because a
/// target that sends one byte inside each deadline never reaches it.
fn fetch(target: &str, timeout: Duration, max_body_bytes: usize) -> Result<Fetched, TallyOwlError> {
    let deadline = Instant::now() + timeout;
    let url = split_url(target)?;
    let addresses: Vec<SocketAddr> = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| {
            TallyOwlError::unavailable(format!(
                "The scrape target `{target}` could not be resolved. {e}"
            ))
        })?
        .collect();
    if addresses.is_empty() {
        return Err(TallyOwlError::unavailable(format!(
            "The scrape target `{target}` resolved to no address."
        )));
    }

    // A name can resolve to an address nothing listens on and to one that
    // works, and which comes first is not the operator's choice. Try each.
    let mut stream = None;
    let mut last = String::new();
    for address in &addresses {
        let remaining = remaining(deadline, target, timeout)?;
        match TcpStream::connect_timeout(address, remaining) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(e) => last = e.to_string(),
        }
    }
    let Some(mut stream) = stream else {
        return Err(TallyOwlError::unavailable(format!(
            "The scrape target `{target}` did not answer. {last}"
        )));
    };

    stream
        .set_write_timeout(Some(remaining(deadline, target, timeout)?))
        .ok();
    write!(
        stream,
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: application/openmetrics-text;version=1.0.0,text/plain;version=0.0.4\r\nUser-Agent: tallyowl-collector\r\nConnection: close\r\n\r\n",
        url.path, url.host_header
    )
    .map_err(|e| {
        TallyOwlError::unavailable(format!("The scrape of `{target}` could not be sent. {e}"))
    })?;

    // A target that answers and then closes without draining the request
    // resets the connection. The answer already arrived, so a reset after the
    // first byte is the end of the body rather than a failed scrape.
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    // The head and the chunk framing are not the body, so they get room of
    // their own above the body bound.
    let most = max_body_bytes.saturating_add(64 * 1024);
    loop {
        stream
            .set_read_timeout(Some(remaining(deadline, target, timeout)?))
            .ok();
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                raw.extend_from_slice(&chunk[..read]);
                if raw.len() > most {
                    return Err(too_large(target, max_body_bytes));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset && !raw.is_empty() => break,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(out_of_time(target, timeout))
            }
            Err(e) => {
                return Err(TallyOwlError::unavailable(format!(
                    "The scrape of `{target}` ended early. {e}"
                )))
            }
        }
    }

    read_response(target, &raw, max_body_bytes)
}

/// How long the fetch has left, or the refusal that says it has none.
fn remaining(
    deadline: Instant,
    target: &str,
    timeout: Duration,
) -> Result<Duration, TallyOwlError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(out_of_time(target, timeout));
    }
    Ok(left)
}

fn out_of_time(target: &str, timeout: Duration) -> TallyOwlError {
    TallyOwlError::unavailable(format!(
        "The scrape of `{target}` did not finish inside {} ms. Raise `compatibility.prometheus.timeout`, or find out why the target answers slowly.",
        timeout.as_millis()
    ))
}

fn too_large(target: &str, max_body_bytes: usize) -> TallyOwlError {
    TallyOwlError::unavailable(format!(
        "The scrape target `{target}` sent more than the {} KiB this collector reads from one target. Raise `compatibility.prometheus.maxBodyBytes`, or publish fewer series from this target.",
        max_body_bytes / 1024
    ))
}

/// Split one raw response into its status, its format, and its body.
///
/// The split and the chunk framing are read as **bytes**. A chunk length counts
/// bytes on the wire, and a body turned into text first has a different length
/// wherever a byte was not valid text, so a length applied to the text lands in
/// the wrong place or inside a character.
fn read_response(
    target: &str,
    raw: &[u8],
    max_body_bytes: usize,
) -> Result<Fetched, TallyOwlError> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| {
            TallyOwlError::unavailable(format!(
                "The scrape target `{target}` sent no response body."
            ))
        })?;
    let head = String::from_utf8_lossy(&raw[..split]).to_lowercase();
    let body = &raw[split + 4..];

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

    let header = |name: &str| -> Option<&str> {
        head.lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(held, _)| held.trim() == name)
            .map(|(_, value)| value.trim())
    };
    let format = match header("content-type") {
        Some(value) if value.starts_with("application/openmetrics-text") => Format::OpenMetrics,
        _ => Format::Prometheus,
    };

    // A chunked body is the ordinary case for a target that streams. Undo the
    // framing before the parser sees it, because a chunk length looks exactly
    // like a metric line with no value.
    let body = if header("transfer-encoding").is_some_and(|value| value.contains("chunked")) {
        dechunk(body).ok_or_else(|| cut_short(target))?
    } else {
        // A body shorter than its declared length was cut, and its last line
        // may be half a number. A half number reads as a real one.
        if let Some(declared) = header("content-length").and_then(|v| v.parse::<usize>().ok()) {
            if body.len() < declared {
                return Err(cut_short(target));
            }
        }
        body.to_vec()
    };
    if body.len() > max_body_bytes {
        return Err(too_large(target, max_body_bytes));
    }

    Ok(Fetched {
        body: String::from_utf8_lossy(&body).into_owned(),
        format,
    })
}

fn cut_short(target: &str) -> TallyOwlError {
    TallyOwlError::unavailable(format!(
        "The scrape target `{target}` closed before it sent its whole answer. Nothing from this scrape was kept, because the last line may be incomplete."
    ))
}

/// Undo chunked framing, or `None` when the body ends before its last chunk.
fn dechunk(body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(body.len());
    let mut rest = body;
    loop {
        let line_end = rest.windows(2).position(|window| window == b"\r\n")?;
        let size_line = std::str::from_utf8(&rest[..line_end]).ok()?;
        let size =
            usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("").trim(), 16)
                .ok()?;
        let after = &rest[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(after.get(..size)?);
        rest = &after[size..];
        rest = rest.strip_prefix(b"\r\n").unwrap_or(rest);
    }
}

/// The parts of a target address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetUrl {
    /// The host as a resolver takes it. An IPv6 literal has no brackets here.
    pub host: String,
    pub port: u16,
    pub path: String,
    /// The `Host` header, which always names the port and brackets an IPv6
    /// literal. A target behind a name-based router needs the port to match.
    pub host_header: String,
}

/// Split `http://host:port/path` into its parts.
pub fn split_url(target: &str) -> Result<TargetUrl, TallyOwlError> {
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
    // A request line is one line. A target that held a space or a line break
    // would write a second request line or a header of its own.
    if target.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(refused("it holds a space or a control character"));
    }
    let parse_port = |port: &str| -> Result<u16, TallyOwlError> {
        port.parse()
            .map_err(|_| refused(&format!("`{port}` is not a port")))
    };
    let (host, port, bracketed) = if let Some(after) = authority.strip_prefix('[') {
        // An IPv6 literal. Its colons are part of the address, so the port is
        // whatever follows the closing bracket.
        let (host, tail) = after
            .split_once(']')
            .ok_or_else(|| refused("the `[` of its IPv6 address is not closed"))?;
        let port = match tail.strip_prefix(':') {
            Some(port) => parse_port(port)?,
            None if tail.is_empty() => 80,
            None => return Err(refused("something follows the `]` that is not a port")),
        };
        (host.to_string(), port, true)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), parse_port(port)?, false),
            None => (authority.to_string(), 80, false),
        }
    };
    if host.is_empty() {
        return Err(refused("it names no host"));
    }
    let host_header = if bracketed {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    Ok(TargetUrl {
        host,
        port,
        path,
        host_header,
    })
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
        let url = split_url("http://127.0.0.1:9100/metrics").unwrap();
        assert_eq!(
            (url.host.as_str(), url.port, url.path.as_str()),
            ("127.0.0.1", 9100, "/metrics")
        );
        let url = split_url("http://host.example").unwrap();
        assert_eq!(
            (url.host.as_str(), url.port, url.path.as_str()),
            ("host.example", 80, "/metrics")
        );
        // A target behind a name-based router matches on the port too.
        assert_eq!(url.host_header, "host.example:80");
    }

    #[test]
    fn an_ipv6_literal_is_a_target() {
        let url = split_url("http://[::1]:9100/metrics").unwrap();
        assert_eq!((url.host.as_str(), url.port), ("::1", 9100));
        assert_eq!(url.host_header, "[::1]:9100");
        assert_eq!(split_url("http://[fd00::2]").unwrap().port, 80);
        assert!(split_url("http://[::1").is_err());
        assert!(split_url("http://[::1]x/metrics").is_err());
    }

    #[test]
    fn a_target_that_holds_a_line_break_cannot_write_a_header_of_its_own() {
        let refused = split_url("http://host/metrics HTTP/1.1\r\nX-Injected: 1").unwrap_err();
        assert!(refused.message.contains("control character"));
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
    fn two_counters_with_the_same_labels_keep_their_own_reset_state() {
        // Nearly every target publishes several counters with no labels at
        // all. With one entry for both, the smaller one reads as a restart of
        // the larger one on every scrape, and the larger one then takes the
        // start the smaller one moved.
        let scraper = scraper();
        let text = |a: u64, b: u64| {
            format!("# TYPE a_total counter\na_total {a}\n# TYPE b_total counter\nb_total {b}\n")
        };
        let first = scraper.normalize("t", &text(900, 5), 1_000);
        assert_eq!(first.resets, 0, "b was never at 900");
        let second = scraper.normalize("t", &text(901, 6), 2_000);
        assert_eq!(second.resets, 0);
        let third = scraper.normalize("t", &text(902, 7), 3_000);
        assert_eq!(third.resets, 0);
        for (before, after) in first.points.iter().zip(third.points.iter()) {
            assert_eq!(before.start_at, after.start_at, "{}", before.metric_name);
        }
    }

    #[test]
    fn a_reading_that_is_not_a_number_does_not_hide_the_next_restart() {
        let scraper = scraper();
        scraper.normalize("t", "# TYPE x_total counter\nx_total 900\n", 1_000);
        let blank = scraper.normalize("t", "# TYPE x_total counter\nx_total NaN\n", 2_000);
        assert_eq!(blank.resets, 0);
        let after = scraper.normalize("t", "# TYPE x_total counter\nx_total 5\n", 3_000);
        assert_eq!(
            after.resets, 1,
            "900 then 5 is a restart, whatever came between"
        );
    }

    #[test]
    fn a_series_that_stops_reporting_is_forgotten_after_the_idle_expiry() {
        // The clock is the `now_ms` a caller passes, so no time has to pass.
        let scraper = Scraper::new(vec![], Duration::from_secs(2))
            .with_idle_expiry(Duration::from_millis(1_000));
        for pod in 0..50 {
            let text = format!("# TYPE x_total counter\nx_total{{pod=\"p{pod}\"}} 1\n");
            scraper.normalize("t", &text, 1_000 + pod);
        }
        assert_eq!(scraper.held_series(), 50);
        scraper.normalize(
            "t",
            "# TYPE x_total counter\nx_total{pod=\"live\"} 1\n",
            10_000,
        );
        assert_eq!(
            scraper.held_series(),
            1,
            "only the series that still reports"
        );
    }

    #[test]
    fn a_series_that_keeps_reporting_is_never_forgotten() {
        let scraper = Scraper::new(vec![], Duration::from_secs(2))
            .with_idle_expiry(Duration::from_millis(1_000));
        let first = scraper.normalize("t", "# TYPE x_total counter\nx_total 1\n", 0);
        let mut last = first.clone();
        for step in 1..20 {
            last = scraper.normalize("t", "# TYPE x_total counter\nx_total 2\n", step * 600);
        }
        assert_eq!(first.points[0].start_at, last.points[0].start_at);
    }

    #[test]
    fn every_scraped_point_names_the_instance_it_came_from() {
        // Two replicas publish the same names and labels. The instance is what
        // keeps them two series and not one that jumps between two totals.
        let scraper = scraper();
        let text = "# TYPE x_total counter\nx_total{route=\"/a\"} 1\n";
        let a = scraper.normalize("http://10.0.0.1:9100/metrics", text, 1_000);
        let b = scraper.normalize("http://10.0.0.2:9100/metrics", text, 1_000);
        assert_ne!(
            crate::series_key(&a.points[0]),
            crate::series_key(&b.points[0])
        );
        let instance = a.points[0]
            .labels
            .iter()
            .find(|label| label.key == "instance")
            .expect("an instance label");
        assert_eq!(crate::label_text(instance), "10.0.0.1:9100");
        assert_eq!(instance.origin, PropertyOrigin::Collector);
    }

    #[test]
    fn an_instance_label_the_target_published_is_kept_under_another_name() {
        let scraper = scraper();
        let text = "# TYPE x_total counter\nx_total{instance=\"mine\"} 1\n";
        let result = scraper.normalize("http://10.0.0.1:9100/metrics", text, 1_000);
        let labels: Vec<(String, String)> = result.points[0]
            .labels
            .iter()
            .map(|label| (label.key.clone(), crate::label_text(label)))
            .collect();
        assert!(labels.contains(&("exported_instance".to_string(), "mine".to_string())));
        assert!(labels.contains(&("instance".to_string(), "10.0.0.1:9100".to_string())));
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
    fn a_chunk_boundary_inside_a_character_does_not_stop_the_scraper() {
        // `é` is two bytes and the first chunk ends between them. Read as text
        // first, each half becomes a three-byte replacement character, and a
        // byte count from the wire then lands inside one of them.
        let body = "# TYPE x_total counter\nx_total{city=\"Orléans\"} 3\n".as_bytes();
        let cut = body.iter().position(|b| *b == 0xc3).expect("the character") + 1;
        let mut raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for part in [&body[..cut], &body[cut..]] {
            raw.extend(format!("{:x}\r\n", part.len()).into_bytes());
            raw.extend_from_slice(part);
            raw.extend(b"\r\n");
        }
        raw.extend(b"0\r\n\r\n");
        let fetched = read_response("t", &raw, DEFAULT_MAX_BODY_BYTES).expect("a whole body");
        let result = scraper().normalize("t", &fetched.body, 1_000);
        assert_eq!(result.points.len(), 1, "{:?}", result.faults);
        let city = result.points[0]
            .labels
            .iter()
            .find(|label| label.key == "city")
            .expect("the label");
        assert_eq!(crate::label_text(city), "Orléans");
    }

    #[test]
    fn a_hostile_chunk_length_is_a_failed_scrape_and_not_a_panic() {
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\n€".as_bytes();
        assert!(read_response("t", raw, DEFAULT_MAX_BODY_BYTES).is_err());
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\nx";
        assert!(read_response("t", raw, DEFAULT_MAX_BODY_BYTES).is_err());
    }

    #[test]
    fn a_body_cut_short_is_refused_rather_than_read_as_half_a_number() {
        // `x_total 12` of `x_total 123456` is a real number, and the wrong one.
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Length: 40\r\n\r\n# TYPE x_total counter\nx_total 12";
        let refused = read_response("t", raw, DEFAULT_MAX_BODY_BYTES).unwrap_err();
        assert!(
            refused.message.contains("whole answer"),
            "{}",
            refused.message
        );
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nx_tot";
        assert!(read_response("t", raw, DEFAULT_MAX_BODY_BYTES).is_err());
    }

    #[test]
    fn a_body_over_the_limit_is_refused_and_the_refusal_names_the_setting() {
        let mut raw = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        raw.extend(vec![b'#'; 2048]);
        let refused = read_response("t", &raw, 1024).unwrap_err();
        assert!(refused
            .message
            .contains("compatibility.prometheus.maxBodyBytes"));
    }

    #[test]
    fn the_declared_format_travels_with_the_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/openmetrics-text; version=1.0.0\r\n\r\n# EOF\n";
        assert_eq!(
            read_response("t", raw, DEFAULT_MAX_BODY_BYTES)
                .unwrap()
                .format,
            Format::OpenMetrics
        );
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\n\r\nx 1\n";
        assert_eq!(
            read_response("t", raw, DEFAULT_MAX_BODY_BYTES)
                .unwrap()
                .format,
            Format::Prometheus
        );
    }

    /// A target that sends its head and then one byte at a time, each inside
    /// the read deadline, for ever.
    fn dripping_target() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
                while stream.write_all(b"#").is_ok() {
                    let _ = stream.flush();
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        });
        format!("http://{address}/metrics")
    }

    #[test]
    fn a_target_that_never_finishes_is_given_up_on_at_the_deadline() {
        // A real socket deadline is the thing under test. A deadline on each
        // read alone never fires here, because every read gets a byte.
        let failed = Scraper::new(vec![], Duration::from_millis(80))
            .scrape_one(&dripping_target(), 1_000)
            .unwrap_err();
        assert!(
            failed.message.contains("compatibility.prometheus.timeout"),
            "{}",
            failed.message
        );
    }

    #[test]
    fn a_target_that_streams_without_end_is_stopped_at_the_body_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
                let block = vec![b'#'; 64 * 1024];
                while stream.write_all(&block).is_ok() {}
            }
        });
        let failed = Scraper::new(vec![], Duration::from_secs(5))
            .with_max_body_bytes(256 * 1024)
            .scrape_one(&format!("http://{address}/metrics"), 1_000)
            .unwrap_err();
        assert!(failed
            .message
            .contains("compatibility.prometheus.maxBodyBytes"));
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
