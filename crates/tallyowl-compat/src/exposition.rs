//! The Prometheus and OpenMetrics text exposition, read into native points.
//!
//! # What normalizing means here
//!
//! `AGENTS.md` says to normalize at the collector, and `docs/DATA_MODEL.md`
//! section 3.4 says the receiver normalizes "temporality, types, units, labels,
//! and resource attributes into this metric model immediately". So this parser
//! produces `MetricPointPayload` and nothing else. No exposition text travels
//! past this module, and every later hop is native CSIL.
//!
//! # The four types, and what each becomes
//!
//! | Exposition type | Becomes |
//! | --- | --- |
//! | `counter` | a cumulative, monotonic counter |
//! | `gauge` | a gauge |
//! | `histogram` | a cumulative histogram, bounds from `le` |
//! | `summary` | a `_sum` counter, a `_count` counter, and one gauge for each quantile |
//! | `untyped`, or no `# TYPE` | a gauge |
//!
//! **A summary has no native type and it is not going to get one.** A summary
//! reports quantiles that the target already computed, and a quantile that
//! somebody else computed cannot be merged with another one: two targets each
//! reporting a p99 have no p99 between them. Turning each quantile into its own
//! gauge series keeps the number the target published and refuses to imply that
//! it can be combined. A histogram, which carries buckets, does merge, and this
//! module keeps that difference visible.
//!
//! # Bucket counts are cumulative on both sides
//!
//! An exposition histogram bucket holds every observation at or below its
//! bound, and `HistogramValue.counts` in `csil/tallyowl-ingest.csil` holds the
//! same thing. So the counts copy across without a running total, and the
//! `+Inf` bucket becomes `count` rather than a bound.

use std::collections::BTreeMap;

use tallyowl_collector_api::types::{
    HistogramValue, MetricKind, MetricPointPayload, MetricPointPayload_temporality as Temporality,
    PropertyOrigin,
};
use tallyowl_wire::{collector as wire, Value};

/// One sample, before the histogram and summary parts are gathered.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub value: f64,
    /// The exposition may carry a time for a sample. OpenMetrics writes it in
    /// seconds, and this holds the milliseconds `docs/CONVENTIONS.md` requires.
    pub timestamp_ms: Option<i64>,
    /// An OpenMetrics exemplar's trace ID, as raw bytes.
    pub exemplar_trace_id: Option<Vec<u8>>,
}

/// Why one line could not be read. A malformed line never stops a scrape: a
/// target with one broken metric still has useful ones, and refusing all of
/// them would turn a target's defect into a TallyOwl outage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineFault {
    pub line_number: usize,
    pub reason: String,
}

/// What one exposition document produced.
#[derive(Debug, Clone, Default)]
pub struct Parsed {
    pub points: Vec<MetricPointPayload>,
    pub faults: Vec<LineFault>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Declared {
    Counter,
    Gauge,
    Histogram,
    Summary,
    Untyped,
}

impl Declared {
    fn parse(word: &str) -> Declared {
        match word {
            "counter" => Declared::Counter,
            "gauge" => Declared::Gauge,
            "histogram" => Declared::Histogram,
            "summary" => Declared::Summary,
            _ => Declared::Untyped,
        }
    }
}

#[derive(Debug, Default)]
struct Family {
    declared: Option<Declared>,
    help: Option<String>,
    unit: Option<String>,
    samples: Vec<Sample>,
}

/// Read one exposition document.
///
/// `now_ms` is the scrape time, which becomes `end_at` for a sample that
/// carries no time of its own. A scrape is an observation at the moment it
/// happened, and a target that publishes no timestamp is saying "now".
pub fn parse(text: &str, now_ms: i64) -> Parsed {
    let mut families: BTreeMap<String, Family> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut faults = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            let rest = rest.trim_start();
            // `# EOF` ends an OpenMetrics document. Anything after it is not
            // part of the exposition.
            if rest == "EOF" {
                break;
            }
            let mut words = rest.splitn(3, char::is_whitespace);
            let keyword = words.next().unwrap_or("");
            let name = words.next().unwrap_or("").trim();
            let value = words.next().unwrap_or("").trim();
            if name.is_empty() {
                continue;
            }
            let entry = entry(&mut families, &mut order, name);
            match keyword {
                "TYPE" => entry.declared = Some(Declared::parse(value)),
                "HELP" => entry.help = Some(unescape_help(value)),
                "UNIT" => entry.unit = Some(value.to_string()),
                _ => {}
            }
            continue;
        }

        match sample(line) {
            Ok(sample) => {
                let family_name = family_of(&sample.name, &families);
                entry(&mut families, &mut order, &family_name)
                    .samples
                    .push(sample);
            }
            Err(reason) => faults.push(LineFault {
                line_number: index + 1,
                reason,
            }),
        }
    }

    let mut points = Vec::new();
    for name in order {
        let Some(family) = families.get(&name) else {
            continue;
        };
        if family.samples.is_empty() {
            continue;
        }
        let declared = family.declared.unwrap_or(Declared::Untyped);
        match declared {
            Declared::Histogram => build_histograms(&name, family, now_ms, &mut points),
            Declared::Summary => build_summary(&name, family, now_ms, &mut points),
            _ => build_scalars(&name, family, declared, now_ms, &mut points),
        }
    }

    Parsed { points, faults }
}

fn entry<'a>(
    families: &'a mut BTreeMap<String, Family>,
    order: &mut Vec<String>,
    name: &str,
) -> &'a mut Family {
    if !families.contains_key(name) {
        order.push(name.to_string());
    }
    families.entry(name.to_string()).or_default()
}

/// Which family a sample name belongs to.
///
/// A histogram publishes `name_bucket`, `name_sum`, and `name_count`, and a
/// summary publishes `name` with a `quantile` label plus `name_sum` and
/// `name_count`. The `# TYPE` line names the family, so a sample matches by
/// stripping a known suffix and checking whether that family was declared.
fn family_of(sample_name: &str, families: &BTreeMap<String, Family>) -> String {
    for suffix in ["_bucket", "_sum", "_count", "_created", "_total"] {
        if let Some(base) = sample_name.strip_suffix(suffix) {
            if let Some(family) = families.get(base) {
                if matches!(
                    family.declared,
                    Some(Declared::Histogram) | Some(Declared::Summary) | Some(Declared::Counter)
                ) {
                    return base.to_string();
                }
            }
        }
    }
    sample_name.to_string()
}

fn build_scalars(
    name: &str,
    family: &Family,
    declared: Declared,
    now_ms: i64,
    out: &mut Vec<MetricPointPayload>,
) {
    let counter = declared == Declared::Counter;
    for sample in &family.samples {
        // OpenMetrics writes a counter's value as `name_total` and its creation
        // time as `name_created`. The creation time is a separate fact and not
        // a value of the series.
        if sample.name.ends_with("_created") {
            continue;
        }
        let end_at = sample.timestamp_ms.unwrap_or(now_ms);
        out.push(MetricPointPayload {
            metric_name: name.to_string(),
            metric_kind: if counter {
                MetricKind::Counter
            } else {
                MetricKind::Gauge
            },
            unit: family.unit.clone(),
            description: family.help.clone(),
            monotonic: counter,
            // A scrape target reports a total since it started. It is
            // cumulative, always: there is no delta in this format.
            temporality: Temporality::Cumulative,
            start_at: end_at,
            end_at,
            labels: labels_of(&sample.labels),
            number_value: Some(sample.value),
            histogram_value: None,
            exemplar_trace_id: sample.exemplar_trace_id.clone(),
        });
    }
}

fn build_histograms(name: &str, family: &Family, now_ms: i64, out: &mut Vec<MetricPointPayload>) {
    // Group by the labels without `le`, which is what identifies one histogram
    // series across its buckets.
    let mut series: BTreeMap<Vec<(String, String)>, HistogramParts> = BTreeMap::new();
    let mut order: Vec<Vec<(String, String)>> = Vec::new();

    for sample in &family.samples {
        let mut labels = sample.labels.clone();
        let le = labels.remove("le");
        let key: Vec<(String, String)> = labels.into_iter().collect();
        if !series.contains_key(&key) {
            order.push(key.clone());
        }
        let parts = series.entry(key).or_default();
        parts.timestamp_ms = parts.timestamp_ms.or(sample.timestamp_ms);
        if parts.exemplar_trace_id.is_none() {
            parts
                .exemplar_trace_id
                .clone_from(&sample.exemplar_trace_id);
        }
        if sample.name.ends_with("_bucket") {
            if let Some(le) = le {
                parts.buckets.push((parse_bound(&le), sample.value));
            }
        } else if sample.name.ends_with("_sum") {
            parts.sum = Some(sample.value);
        } else if sample.name.ends_with("_count") {
            parts.count = Some(sample.value);
        }
    }

    for key in order {
        let Some(parts) = series.get(&key) else {
            continue;
        };
        let mut buckets = parts.buckets.clone();
        buckets.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        // `+Inf` is the total rather than a bound. Everything below it is a
        // bound, and the counts are already cumulative on both sides.
        let total = buckets
            .iter()
            .find(|(bound, _)| bound.is_infinite())
            .map(|(_, count)| *count)
            .or(parts.count)
            .unwrap_or(0.0);
        let finite: Vec<&(f64, f64)> = buckets.iter().filter(|(b, _)| b.is_finite()).collect();
        let end_at = parts.timestamp_ms.unwrap_or(now_ms);
        let labels: BTreeMap<String, String> = key.iter().cloned().collect();
        out.push(MetricPointPayload {
            metric_name: name.to_string(),
            metric_kind: MetricKind::Histogram,
            unit: family.unit.clone(),
            description: family.help.clone(),
            monotonic: false,
            temporality: Temporality::Cumulative,
            start_at: end_at,
            end_at,
            labels: labels_of(&labels),
            number_value: None,
            histogram_value: Some(HistogramValue {
                count: total as u64,
                sum: parts.sum.unwrap_or(0.0),
                bounds: finite.iter().map(|(b, _)| *b).collect(),
                counts: finite.iter().map(|(_, c)| *c as u64).collect(),
            }),
            exemplar_trace_id: parts.exemplar_trace_id.clone(),
        });
    }
}

#[derive(Debug, Default, Clone)]
struct HistogramParts {
    buckets: Vec<(f64, f64)>,
    sum: Option<f64>,
    count: Option<f64>,
    timestamp_ms: Option<i64>,
    exemplar_trace_id: Option<Vec<u8>>,
}

fn build_summary(name: &str, family: &Family, now_ms: i64, out: &mut Vec<MetricPointPayload>) {
    for sample in &family.samples {
        let end_at = sample.timestamp_ms.unwrap_or(now_ms);
        let (suffix, kind, monotonic) = if sample.name.ends_with("_sum") {
            ("_sum", MetricKind::Counter, true)
        } else if sample.name.ends_with("_count") {
            ("_count", MetricKind::Counter, true)
        } else if sample.name.ends_with("_created") {
            continue;
        } else {
            // A quantile the target computed. It keeps its `quantile` label and
            // becomes a gauge, because a quantile somebody else computed cannot
            // be merged with another one.
            ("", MetricKind::Gauge, false)
        };
        out.push(MetricPointPayload {
            metric_name: format!("{name}{suffix}"),
            metric_kind: kind,
            unit: family.unit.clone(),
            description: family.help.clone(),
            monotonic,
            temporality: Temporality::Cumulative,
            start_at: end_at,
            end_at,
            labels: labels_of(&sample.labels),
            number_value: Some(sample.value),
            histogram_value: None,
            exemplar_trace_id: sample.exemplar_trace_id.clone(),
        });
    }
}

fn labels_of(labels: &BTreeMap<String, String>) -> Vec<tallyowl_collector_api::types::Property> {
    labels
        .iter()
        .map(|(key, value)| {
            // The origin is `client`: a scrape target is outside TallyOwl, so
            // its labels are not values the collector can vouch for. An
            // operator filtering on origin gets a true answer.
            wire::property(key, Value::Text(value.clone()), PropertyOrigin::Client)
        })
        .collect()
}

fn parse_bound(text: &str) -> f64 {
    match text {
        "+Inf" | "Inf" | "inf" | "+inf" => f64::INFINITY,
        other => other.parse().unwrap_or(f64::INFINITY),
    }
}

/// Read one sample line.
///
/// The shape is `name{labels} value [timestamp] [# {labels} value [timestamp]]`,
/// where everything after the `#` is an OpenMetrics exemplar. A label value may
/// hold a brace and a `#`, so the brace group is found by scanning rather than
/// by searching for the last `}`.
fn sample(line: &str) -> Result<Sample, String> {
    let bytes: Vec<char> = line.chars().collect();
    let mut index = 0;
    while index < bytes.len() && !bytes[index].is_whitespace() && bytes[index] != '{' {
        index += 1;
    }
    let name: String = bytes[..index].iter().collect();
    if name.is_empty() {
        return Err("a sample line has no metric name".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
    {
        return Err(format!("`{name}` is not a metric name"));
    }

    let mut labels = BTreeMap::new();
    if index < bytes.len() && bytes[index] == '{' {
        let close = closing_brace(&bytes, index)
            .ok_or_else(|| format!("the label list of `{name}` is not closed"))?;
        labels = parse_labels(&bytes[index + 1..close].iter().collect::<String>())?;
        index = close + 1;
    }

    let rest: String = bytes[index..].iter().collect();
    let (value_part, exemplar_part) = match rest.split_once('#') {
        Some((value, exemplar)) => (value, Some(exemplar)),
        None => (rest.as_str(), None),
    };

    let mut fields = value_part.split_whitespace();
    let value_text = fields
        .next()
        .ok_or_else(|| format!("`{name}` has no value"))?;
    let value = parse_number(value_text)
        .ok_or_else(|| format!("`{name}` has the value `{value_text}`, which is not a number"))?;
    let timestamp_ms = read_timestamp(fields.next());

    // An exemplar's own labels never become the sample's labels. Only its trace
    // is kept, because that is the link a reader follows from a chart.
    let exemplar_trace_id = exemplar_part.and_then(|part| {
        let chars: Vec<char> = part.chars().collect();
        let open = chars.iter().position(|c| *c == '{')?;
        let close = closing_brace(&chars, open)?;
        let inside: String = chars[open + 1..close].iter().collect();
        parse_labels(&inside)
            .ok()?
            .get("trace_id")
            .and_then(|id| hex_bytes(id))
    });

    Ok(Sample {
        name,
        labels,
        value,
        timestamp_ms,
        exemplar_trace_id,
    })
}

/// Prometheus writes a timestamp in milliseconds and OpenMetrics writes seconds
/// with a fraction. A value with a decimal point is therefore seconds.
fn read_timestamp(text: Option<&str>) -> Option<i64> {
    match text? {
        text if text.contains('.') => text.parse::<f64>().ok().map(|s| (s * 1000.0) as i64),
        text => text.parse::<i64>().ok(),
    }
}

/// The `}` that closes the group opened at `open`, skipping any inside a quoted
/// label value.
fn closing_brace(chars: &[char], open: usize) -> Option<usize> {
    let mut index = open + 1;
    let mut in_quotes = false;
    while index < chars.len() {
        match chars[index] {
            '\\' if in_quotes => index += 1,
            '"' => in_quotes = !in_quotes,
            '}' if !in_quotes => return Some(index),
            _ => {}
        }
        index += 1;
    }
    None
}

/// Read a label list, honouring the escapes the format defines.
fn parse_labels(inside: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let mut chars = inside.chars().peekable();
    loop {
        // The name, up to `=`.
        let mut name = String::new();
        while let Some(&c) = chars.peek() {
            chars.next();
            if c == '=' {
                break;
            }
            if !c.is_whitespace() && c != ',' {
                name.push(c);
            }
        }
        if name.is_empty() {
            break;
        }
        // The value, in quotation marks.
        match chars.next() {
            Some('"') => {}
            _ => return Err(format!("the label `{name}` has no quoted value")),
        }
        let mut value = String::new();
        loop {
            match chars.next() {
                None => return Err(format!("the label `{name}` is not closed")),
                Some('"') => break,
                Some('\\') => match chars.next() {
                    Some('n') => value.push('\n'),
                    Some('"') => value.push('"'),
                    Some('\\') => value.push('\\'),
                    Some(other) => {
                        value.push('\\');
                        value.push(other);
                    }
                    None => return Err(format!("the label `{name}` ends in an escape")),
                },
                Some(other) => value.push(other),
            }
        }
        out.insert(name, value);
        // Skip the separator.
        while let Some(&c) = chars.peek() {
            if c == ',' || c.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }
        if chars.peek().is_none() {
            break;
        }
    }
    Ok(out)
}

fn parse_number(text: &str) -> Option<f64> {
    match text {
        "+Inf" | "Inf" | "inf" | "+inf" => Some(f64::INFINITY),
        "-Inf" | "-inf" => Some(f64::NEG_INFINITY),
        "NaN" | "nan" => Some(f64::NAN),
        other => other.parse().ok(),
    }
}

fn unescape_help(text: &str) -> String {
    text.replace("\\n", "\n").replace("\\\\", "\\")
}

fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || text.is_empty() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label_of(point: &MetricPointPayload, key: &str) -> Option<String> {
        point.labels.iter().find(|l| l.key == key).map(|l| {
            wire::read(&l.value)
                .map(|v| v.to_display())
                .unwrap_or_default()
        })
    }

    #[test]
    fn a_counter_becomes_a_cumulative_monotonic_counter() {
        let text = "\
# HELP http_requests_total Requests served.
# TYPE http_requests_total counter
http_requests_total{method=\"get\",code=\"200\"} 1027
";
        let parsed = parse(text, 5_000);
        assert!(parsed.faults.is_empty(), "{:?}", parsed.faults);
        assert_eq!(parsed.points.len(), 1);
        let point = &parsed.points[0];
        assert_eq!(point.metric_name, "http_requests_total");
        assert_eq!(point.metric_kind, MetricKind::Counter);
        assert!(point.monotonic);
        assert_eq!(point.temporality, Temporality::Cumulative);
        assert_eq!(point.number_value, Some(1027.0));
        assert_eq!(point.description.as_deref(), Some("Requests served."));
        assert_eq!(label_of(point, "method").as_deref(), Some("get"));
        assert_eq!(point.end_at, 5_000);
    }

    #[test]
    fn a_gauge_stays_a_gauge_and_a_negative_value_survives() {
        let text = "# TYPE queue_depth gauge\nqueue_depth -12\n";
        let point = &parse(text, 1).points[0];
        assert_eq!(point.metric_kind, MetricKind::Gauge);
        assert!(!point.monotonic);
        assert_eq!(point.number_value, Some(-12.0));
    }

    #[test]
    fn a_metric_with_no_type_line_becomes_a_gauge() {
        // Guessing a counter would make every `rate` over it wrong, and the
        // exposition gives nothing to guess from.
        let point = &parse("something_odd 5\n", 1).points[0];
        assert_eq!(point.metric_kind, MetricKind::Gauge);
    }

    #[test]
    fn a_histogram_gathers_its_buckets_its_sum_and_its_count() {
        let text = "\
# TYPE request_duration_seconds histogram
request_duration_seconds_bucket{le=\"0.1\"} 2
request_duration_seconds_bucket{le=\"0.5\"} 5
request_duration_seconds_bucket{le=\"+Inf\"} 7
request_duration_seconds_sum 3.25
request_duration_seconds_count 7
";
        let parsed = parse(text, 9);
        assert_eq!(parsed.points.len(), 1, "{:?}", parsed.points);
        let histogram = parsed.points[0].histogram_value.as_ref().unwrap();
        assert_eq!(histogram.bounds, vec![0.1, 0.5]);
        assert_eq!(histogram.counts, vec![2, 5]);
        assert_eq!(histogram.count, 7);
        assert_eq!(histogram.sum, 3.25);
    }

    #[test]
    fn one_histogram_with_two_label_sets_becomes_two_series() {
        let text = "\
# TYPE latency_seconds histogram
latency_seconds_bucket{route=\"/a\",le=\"1\"} 1
latency_seconds_bucket{route=\"/a\",le=\"+Inf\"} 2
latency_seconds_bucket{route=\"/b\",le=\"1\"} 3
latency_seconds_bucket{route=\"/b\",le=\"+Inf\"} 4
";
        let parsed = parse(text, 1);
        assert_eq!(parsed.points.len(), 2);
        assert_eq!(label_of(&parsed.points[0], "route").as_deref(), Some("/a"));
        // `le` never survives as a label. It described the bucket, and the
        // bucket is now a bound.
        assert!(label_of(&parsed.points[0], "le").is_none());
    }

    #[test]
    fn a_summary_becomes_a_sum_a_count_and_one_gauge_for_each_quantile() {
        let text = "\
# TYPE rpc_duration_seconds summary
rpc_duration_seconds{quantile=\"0.5\"} 0.012
rpc_duration_seconds{quantile=\"0.99\"} 0.42
rpc_duration_seconds_sum 17.0
rpc_duration_seconds_count 2693
";
        let parsed = parse(text, 1);
        assert_eq!(parsed.points.len(), 4);
        let quantiles: Vec<&MetricPointPayload> = parsed
            .points
            .iter()
            .filter(|p| p.metric_kind == MetricKind::Gauge)
            .collect();
        assert_eq!(quantiles.len(), 2, "each quantile keeps its own series");
        assert_eq!(label_of(quantiles[0], "quantile").as_deref(), Some("0.5"));
        assert!(parsed
            .points
            .iter()
            .any(|p| p.metric_name == "rpc_duration_seconds_sum" && p.monotonic));
        assert!(parsed
            .points
            .iter()
            .any(|p| p.metric_name == "rpc_duration_seconds_count"));
    }

    #[test]
    fn an_openmetrics_exemplar_carries_its_trace() {
        let text = "\
# TYPE latency_seconds histogram
latency_seconds_bucket{le=\"1\"} 1 # {trace_id=\"0102030405060708090a0b0c0d0e0f10\"} 0.7 1609459200.0
latency_seconds_bucket{le=\"+Inf\"} 1
";
        let parsed = parse(text, 1);
        let trace = parsed.points[0].exemplar_trace_id.as_ref().unwrap();
        assert_eq!(trace.len(), 16);
        assert_eq!(trace[0], 1);
        assert_eq!(trace[15], 16);
    }

    #[test]
    fn a_sample_timestamp_beats_the_scrape_time() {
        let text = "# TYPE x_total counter\nx_total 5 1609459200000\n";
        assert_eq!(parse(text, 999).points[0].end_at, 1_609_459_200_000);
    }

    #[test]
    fn an_openmetrics_second_timestamp_becomes_milliseconds() {
        let text = "# TYPE x_total counter\nx_total 5 1609459200.0\n";
        assert_eq!(parse(text, 999).points[0].end_at, 1_609_459_200_000);
    }

    #[test]
    fn an_openmetrics_total_suffix_belongs_to_its_family() {
        let text = "\
# TYPE http_requests counter
# HELP http_requests Requests.
http_requests_total{code=\"200\"} 3
http_requests_created 1609459200.0
# EOF
";
        let parsed = parse(text, 1);
        // The created time is a separate fact and not a value of the series.
        assert_eq!(parsed.points.len(), 1);
        assert_eq!(parsed.points[0].metric_name, "http_requests");
        assert_eq!(parsed.points[0].number_value, Some(3.0));
    }

    #[test]
    fn everything_after_the_end_marker_is_ignored() {
        let text = "# TYPE a_total counter\na_total 1\n# EOF\nb_total 2\n";
        let parsed = parse(text, 1);
        assert_eq!(parsed.points.len(), 1);
    }

    #[test]
    fn a_broken_line_is_reported_and_the_rest_of_the_target_still_arrives() {
        // A target's defect must not become a TallyOwl outage.
        let text = "\
# TYPE good_total counter
good_total 1
bad_line_without_a_value
# TYPE also_good gauge
also_good 2
";
        let parsed = parse(text, 1);
        assert_eq!(parsed.points.len(), 2);
        assert_eq!(parsed.faults.len(), 1);
        assert_eq!(parsed.faults[0].line_number, 3);
    }

    #[test]
    fn a_label_value_with_a_comma_a_quotation_mark_and_a_newline_survives() {
        let text = "# TYPE x gauge\nx{note=\"a,b \\\"c\\\" \\nd\"} 1\n";
        let parsed = parse(text, 1);
        assert!(parsed.faults.is_empty(), "{:?}", parsed.faults);
        assert_eq!(
            label_of(&parsed.points[0], "note").as_deref(),
            Some("a,b \"c\" \nd")
        );
    }

    #[test]
    fn an_infinite_value_arrives_rather_than_failing_the_line() {
        let parsed = parse("# TYPE x gauge\nx +Inf\n", 1);
        assert!(parsed.points[0].number_value.unwrap().is_infinite());
    }

    #[test]
    fn a_unit_line_reaches_the_point() {
        let text = "# TYPE x_seconds gauge\n# UNIT x_seconds seconds\nx_seconds 1\n";
        assert_eq!(parse(text, 1).points[0].unit.as_deref(), Some("seconds"));
    }

    #[test]
    fn an_empty_document_produces_nothing_rather_than_failing() {
        let parsed = parse("", 1);
        assert!(parsed.points.is_empty());
        assert!(parsed.faults.is_empty());
    }

    #[test]
    fn a_histogram_with_no_infinite_bucket_still_reports_a_count() {
        let text = "\
# TYPE h histogram
h_bucket{le=\"1\"} 4
h_count 9
h_sum 2.0
";
        let histogram = parse(text, 1).points[0].histogram_value.clone().unwrap();
        assert_eq!(histogram.count, 9);
        assert_eq!(histogram.bounds, vec![1.0]);
    }
}
