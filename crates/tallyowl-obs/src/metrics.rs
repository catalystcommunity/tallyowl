//! Metrics, from `docs/CONVENTIONS.md` section 6.
//!
//! Metric names are the one place where technical language wins, because a
//! metric feeds a dashboard query rather than a person reading prose. The rules
//! are mechanical, so this registry enforces them instead of a reviewer:
//!
//! - lower case, with underscores;
//! - prefixed with `tallyowl_`;
//! - suffixed with the unit;
//! - labelled with workspace and project where per-project cost matters.
//!
//! A metric never carries an end-user ID as a label. That turns a metric series
//! into personal data and multiplies cardinality without limit, so the registry
//! refuses the label rather than trusting a call site.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

impl MetricKind {
    fn as_str(&self) -> &'static str {
        match self {
            MetricKind::Counter => "counter",
            MetricKind::Gauge => "gauge",
            MetricKind::Histogram => "histogram",
        }
    }
}

/// The unit suffixes a metric name may end with. A name that ends with none of
/// them does not say what it counts.
const UNIT_SUFFIXES: &[&str] = &[
    "_total", "_seconds", "_bytes", "_ratio", "_count", "_info", "_ms",
];

/// Label names that never appear on a metric, because they carry an end-user
/// identity.
const FORBIDDEN_LABELS: &[&str] = &["end_user_id", "anonymous_id", "user_id", "session_id"];

/// Why a metric name or label was refused. A refusal is a defect at the call
/// site, so the reason names the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricRule(pub String);

impl std::fmt::Display for MetricRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Check a metric name against every naming rule. Returns the rule it broke.
pub fn check_name(name: &str) -> Result<(), MetricRule> {
    if !name.starts_with("tallyowl_") {
        return Err(MetricRule(format!(
            "The metric name `{name}` does not start with `tallyowl_`."
        )));
    }
    if name != name.to_lowercase() {
        return Err(MetricRule(format!(
            "The metric name `{name}` is not lower case."
        )));
    }
    if name.contains('-') || name.contains(' ') {
        return Err(MetricRule(format!(
            "The metric name `{name}` uses a separator other than an underscore."
        )));
    }
    if !UNIT_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return Err(MetricRule(format!(
            "The metric name `{name}` does not end with a unit suffix. Use one of {}.",
            UNIT_SUFFIXES.join(", ")
        )));
    }
    Ok(())
}

pub fn check_label(label: &str) -> Result<(), MetricRule> {
    if FORBIDDEN_LABELS.contains(&label) {
        return Err(MetricRule(format!(
            "The label `{label}` identifies an end user. A metric never carries one."
        )));
    }
    Ok(())
}

/// The labels of one series, kept sorted so that one series has one key.
pub type Labels = BTreeMap<String, String>;

pub fn labels(pairs: &[(&str, &str)]) -> Labels {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

#[derive(Debug)]
struct Series {
    /// A counter and a gauge hold one value. The gauge stores a signed value as
    /// its two's complement bit pattern, so both share one atomic.
    value: AtomicU64,
    /// A histogram holds a bucket count for each upper bound, plus a sum.
    buckets: Option<Mutex<HistogramState>>,
}

#[derive(Debug)]
struct HistogramState {
    bounds: Vec<f64>,
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

#[derive(Debug)]
struct Family {
    kind: MetricKind,
    help: String,
    bounds: Vec<f64>,
    series: Mutex<BTreeMap<Labels, Arc<Series>>>,
}

/// One reading of one series, in a shape that does not depend on this module.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub name: String,
    pub kind: MetricKind,
    pub help: String,
    pub labels: Labels,
    /// A counter's total or a gauge's level. Zero for a histogram.
    pub value: f64,
    pub histogram: Option<Histogram>,
}

/// A histogram reading. `counts[i]` holds every observation at or below
/// `bounds[i]`, which is what the exposition format and the native format both
/// mean by a bucket.
#[derive(Debug, Clone, PartialEq)]
pub struct Histogram {
    pub bounds: Vec<f64>,
    pub counts: Vec<u64>,
    pub sum: f64,
    pub count: u64,
}

/// Every instrument one process exposes. A service builds one registry and hands
/// out `Arc` clones.
#[derive(Debug)]
pub struct Registry {
    families: Mutex<BTreeMap<String, Arc<Family>>>,
    /// When this process began counting.
    started_at_ms: i64,
    /// True while a self-observation push is in flight anywhere in this
    /// process.
    ///
    /// This exists **only** so an overlapping push skips its period. It does
    /// not suppress anything: suppression is per thread, in `SUPPRESSED`.
    publishing: AtomicBool,
}

thread_local! {
    /// True while **this thread** is publishing the registry as telemetry.
    ///
    /// **This is the recursion guard D12 asks for**, and it has to be per
    /// thread. Publishing self-metrics is itself work that instruments
    /// measure, so a push that counted its own batch would raise a counter,
    /// which the next push would report, which would raise it again.
    ///
    /// A process-wide flag would stop that and take something else with it: a
    /// collector accepting batches on eight other threads during the push
    /// would have every one of those increments dropped, so the numbers the
    /// push then reports would be lower than the truth **because it was
    /// reporting them**. That is a worse fault than the one the guard exists
    /// to prevent, and it is silent. Per thread, the push suppresses its own
    /// work and nothing else's.
    static SUPPRESSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Default for Registry {
    fn default() -> Registry {
        Registry {
            families: Mutex::new(BTreeMap::new()),
            started_at_ms: crate::time::now_ms(),
            publishing: AtomicBool::new(false),
        }
    }
}

/// Held while this thread publishes its own instruments. Dropping it lets this
/// thread record again.
pub struct PublishGuard<'a> {
    registry: &'a Registry,
}

impl Drop for PublishGuard<'_> {
    fn drop(&mut self) {
        SUPPRESSED.with(|flag| flag.set(false));
        self.registry.publishing.store(false, Ordering::Release);
    }
}

impl Registry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Take the recursion guard, or report that a push is already in flight.
    ///
    /// A caller that gets `None` skips this period. Two overlapping pushes
    /// would each measure the other, which is the shape the guard exists to
    /// prevent.
    ///
    /// The guard suppresses recording on the calling thread only. See
    /// `SUPPRESSED`.
    pub fn begin_publishing(&self) -> Option<PublishGuard<'_>> {
        self.publishing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| {
                SUPPRESSED.with(|flag| flag.set(true));
                PublishGuard { registry: self }
            })
    }

    fn suppressed(&self) -> bool {
        SUPPRESSED.with(|flag| flag.get())
    }

    /// Declare an instrument. A name or a bucket list that breaks a rule is a
    /// defect at the call site, so this refuses it rather than exposing it.
    pub fn declare(
        &self,
        name: &str,
        kind: MetricKind,
        help: &str,
        bounds: &[f64],
    ) -> Result<(), MetricRule> {
        check_name(name)?;
        if kind == MetricKind::Histogram && bounds.is_empty() {
            return Err(MetricRule(format!(
                "The histogram `{name}` declares no buckets."
            )));
        }
        let mut families = self.families.lock().expect("metric lock");
        families.entry(name.to_string()).or_insert_with(|| {
            Arc::new(Family {
                kind,
                help: help.to_string(),
                bounds: bounds.to_vec(),
                series: Mutex::new(BTreeMap::new()),
            })
        });
        Ok(())
    }

    fn series(&self, name: &str, labels: &Labels) -> Option<(Arc<Family>, Arc<Series>)> {
        // The recursion guard. See `publishing`.
        if self.suppressed() {
            return None;
        }
        for label in labels.keys() {
            if check_label(label).is_err() {
                return None;
            }
        }
        let families = self.families.lock().expect("metric lock");
        let family = Arc::clone(families.get(name)?);
        drop(families);
        let mut series = family.series.lock().expect("metric lock");
        let entry = series
            .entry(labels.clone())
            .or_insert_with(|| {
                Arc::new(Series {
                    value: AtomicU64::new(0),
                    buckets: if family.kind == MetricKind::Histogram {
                        Some(Mutex::new(HistogramState {
                            bounds: family.bounds.clone(),
                            counts: vec![0; family.bounds.len()],
                            sum: 0.0,
                            count: 0,
                        }))
                    } else {
                        None
                    },
                })
            })
            .clone();
        drop(series);
        Some((family, entry))
    }

    /// Add to a counter. An undeclared name or a forbidden label is dropped, and
    /// the naming test in this module is what catches it.
    pub fn add(&self, name: &str, labels: &Labels, delta: u64) {
        if let Some((_, series)) = self.series(name, labels) {
            series.value.fetch_add(delta, Ordering::Relaxed);
        }
    }

    pub fn increment(&self, name: &str, labels: &Labels) {
        self.add(name, labels, 1);
    }

    pub fn set_gauge(&self, name: &str, labels: &Labels, value: i64) {
        if let Some((_, series)) = self.series(name, labels) {
            series.value.store(value as u64, Ordering::Relaxed);
        }
    }

    pub fn observe(&self, name: &str, labels: &Labels, value: f64) {
        if let Some((_, series)) = self.series(name, labels) {
            if let Some(state) = &series.buckets {
                let mut state = state.lock().expect("metric lock");
                state.sum += value;
                state.count += 1;
                for (index, bound) in state.bounds.clone().iter().enumerate() {
                    if value <= *bound {
                        state.counts[index] += 1;
                    }
                }
            }
        }
    }

    pub fn counter_value(&self, name: &str, labels: &Labels) -> u64 {
        self.series(name, labels)
            .map(|(_, s)| s.value.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    pub fn gauge_value(&self, name: &str, labels: &Labels) -> i64 {
        self.series(name, labels)
            .map(|(_, s)| s.value.load(Ordering::Relaxed) as i64)
            .unwrap_or(0)
    }

    /// Every instrument, as portable readings.
    ///
    /// D12 requires a service to expose the same instruments through its
    /// exposition endpoint **and** through the native path. This is the native
    /// half: one reading of each series, which a caller turns into metric
    /// points. Both halves read the same registry, so the two can never say
    /// different things about one instrument.
    pub fn snapshot(&self) -> Vec<Reading> {
        let families = self.families.lock().expect("metric lock");
        let mut out = Vec::new();
        for (name, family) in families.iter() {
            let series = family.series.lock().expect("metric lock");
            for (labels, entry) in series.iter() {
                let reading = match family.kind {
                    MetricKind::Histogram => {
                        let state = entry
                            .buckets
                            .as_ref()
                            .expect("a histogram series holds buckets")
                            .lock()
                            .expect("metric lock");
                        Reading {
                            name: name.clone(),
                            kind: family.kind,
                            help: family.help.clone(),
                            labels: labels.clone(),
                            value: 0.0,
                            histogram: Some(Histogram {
                                bounds: state.bounds.clone(),
                                counts: state.counts.clone(),
                                sum: state.sum,
                                count: state.count,
                            }),
                        }
                    }
                    _ => {
                        let raw = entry.value.load(Ordering::Relaxed);
                        Reading {
                            name: name.clone(),
                            kind: family.kind,
                            help: family.help.clone(),
                            labels: labels.clone(),
                            value: if family.kind == MetricKind::Gauge {
                                raw as i64 as f64
                            } else {
                                raw as f64
                            },
                            histogram: None,
                        }
                    }
                };
                out.push(reading);
            }
        }
        out
    }

    /// When this process started counting.
    ///
    /// A cumulative reading is the total since here, and a restarted process
    /// reports a later one beside a value that begins again. That pair is what
    /// makes a reset visible to `rate` and `increase`.
    pub fn started_at_ms(&self) -> i64 {
        self.started_at_ms
    }

    /// The Prometheus and OpenMetrics text exposition. D12 requires every
    /// service to expose the same instruments through this endpoint and through
    /// the native path, so this is the one renderer.
    pub fn render_text(&self) -> String {
        let families = self.families.lock().expect("metric lock");
        let mut out = String::new();
        for (name, family) in families.iter() {
            out.push_str(&format!("# HELP {name} {}\n", family.help));
            out.push_str(&format!("# TYPE {name} {}\n", family.kind.as_str()));
            let series = family.series.lock().expect("metric lock");
            for (labels, entry) in series.iter() {
                match family.kind {
                    MetricKind::Counter | MetricKind::Gauge => {
                        let raw = entry.value.load(Ordering::Relaxed);
                        let value = if family.kind == MetricKind::Gauge {
                            (raw as i64).to_string()
                        } else {
                            raw.to_string()
                        };
                        out.push_str(&format!("{name}{} {value}\n", render_labels(labels, None)));
                    }
                    MetricKind::Histogram => {
                        let state = entry
                            .buckets
                            .as_ref()
                            .expect("a histogram series holds buckets")
                            .lock()
                            .expect("metric lock");
                        for (index, bound) in state.bounds.iter().enumerate() {
                            out.push_str(&format!(
                                "{name}_bucket{} {}\n",
                                render_labels(labels, Some(("le", &format_float(*bound)))),
                                state.counts[index]
                            ));
                        }
                        out.push_str(&format!(
                            "{name}_bucket{} {}\n",
                            render_labels(labels, Some(("le", "+Inf"))),
                            state.count
                        ));
                        out.push_str(&format!(
                            "{name}_sum{} {}\n",
                            render_labels(labels, None),
                            format_float(state.sum)
                        ));
                        out.push_str(&format!(
                            "{name}_count{} {}\n",
                            render_labels(labels, None),
                            state.count
                        ));
                    }
                }
            }
        }
        out
    }
}

fn format_float(value: f64) -> String {
    if value == f64::INFINITY {
        "+Inf".to_string()
    } else if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn render_labels(labels: &Labels, extra: Option<(&str, &str)>) -> String {
    if labels.is_empty() && extra.is_none() {
        return String::new();
    }
    let mut parts: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
        .collect();
    if let Some((k, v)) = extra {
        parts.push(format!("{k}=\"{}\"", escape(v)));
    }
    format!("{{{}}}", parts.join(","))
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_must_carry_the_prefix_and_a_unit() {
        assert!(check_name("tallyowl_batches_accepted_total").is_ok());
        assert!(check_name("batches_accepted_total").is_err());
        assert!(check_name("tallyowl_batches_accepted").is_err());
        assert!(check_name("tallyowl_Batches_Total").is_err());
        assert!(check_name("tallyowl-batches-total").is_err());
    }

    #[test]
    fn a_label_that_identifies_an_end_user_is_refused() {
        assert!(check_label("project_id").is_ok());
        assert!(check_label("end_user_id").is_err());
        assert!(check_label("session_id").is_err());
    }

    #[test]
    fn a_counter_renders_with_its_labels() {
        let r = Registry::new();
        r.declare(
            "tallyowl_batches_accepted_total",
            MetricKind::Counter,
            "Batches the collector accepted.",
            &[],
        )
        .unwrap();
        let l = labels(&[("project_id", "p-1"), ("workspace_id", "w-1")]);
        r.increment("tallyowl_batches_accepted_total", &l);
        r.add("tallyowl_batches_accepted_total", &l, 4);
        assert_eq!(r.counter_value("tallyowl_batches_accepted_total", &l), 5);
        let text = r.render_text();
        assert!(text.contains("# TYPE tallyowl_batches_accepted_total counter"));
        assert!(text.contains(
            "tallyowl_batches_accepted_total{project_id=\"p-1\",workspace_id=\"w-1\"} 5"
        ));
    }

    #[test]
    fn a_forbidden_label_drops_the_observation_rather_than_exposing_it() {
        let r = Registry::new();
        r.declare("tallyowl_events_total", MetricKind::Counter, "Events.", &[])
            .unwrap();
        let bad = labels(&[("end_user_id", "u-1")]);
        r.increment("tallyowl_events_total", &bad);
        assert!(!r.render_text().contains("end_user_id"));
        assert!(!r.render_text().contains("u-1"));
    }

    #[test]
    fn a_gauge_holds_a_negative_value() {
        let r = Registry::new();
        r.declare(
            "tallyowl_queue_depth_count",
            MetricKind::Gauge,
            "Tasks waiting.",
            &[],
        )
        .unwrap();
        let l = labels(&[]);
        r.set_gauge("tallyowl_queue_depth_count", &l, -3);
        assert_eq!(r.gauge_value("tallyowl_queue_depth_count", &l), -3);
        assert!(r.render_text().contains("tallyowl_queue_depth_count -3"));
    }

    #[test]
    fn a_histogram_counts_each_bucket_and_the_total() {
        let r = Registry::new();
        r.declare(
            "tallyowl_commit_seconds",
            MetricKind::Histogram,
            "How long a commit took.",
            &[0.001, 0.01, 0.1],
        )
        .unwrap();
        let l = labels(&[]);
        for v in [0.0005, 0.005, 0.05, 5.0] {
            r.observe("tallyowl_commit_seconds", &l, v);
        }
        let text = r.render_text();
        assert!(text.contains("tallyowl_commit_seconds_bucket{le=\"0.001\"} 1"));
        assert!(text.contains("tallyowl_commit_seconds_bucket{le=\"0.01\"} 2"));
        assert!(text.contains("tallyowl_commit_seconds_bucket{le=\"0.1\"} 3"));
        assert!(text.contains("tallyowl_commit_seconds_bucket{le=\"+Inf\"} 4"));
        assert!(text.contains("tallyowl_commit_seconds_count 4"));
    }

    #[test]
    fn a_histogram_without_buckets_is_refused() {
        let r = Registry::new();
        assert!(r
            .declare("tallyowl_commit_seconds", MetricKind::Histogram, "x", &[])
            .is_err());
    }

    #[test]
    fn a_push_suppresses_its_own_thread_and_no_other() {
        // The recursion guard must not take unrelated work with it. A collector
        // accepting batches on eight threads during a self-observation push
        // would otherwise have every one of those increments dropped, and the
        // numbers the push then reported would be lower than the truth
        // **because it was reporting them**. Silently.
        let registry = Registry::new();
        registry
            .declare("tallyowl_events_total", MetricKind::Counter, "Events.", &[])
            .unwrap();
        let none = labels(&[]);

        let guard = registry
            .begin_publishing()
            .expect("nothing else is publishing");

        // The publishing thread's own work does not count. That is the guard.
        registry.add("tallyowl_events_total", &none, 100);

        // Another thread's work does.
        let other = Arc::clone(&registry);
        std::thread::spawn(move || {
            other.add("tallyowl_events_total", &labels(&[]), 7);
        })
        .join()
        .expect("the other thread finishes");

        drop(guard);
        assert_eq!(
            registry.counter_value("tallyowl_events_total", &none),
            7,
            "the push suppressed itself and nothing else"
        );
    }

    #[test]
    fn a_snapshot_reads_every_kind_in_the_shape_the_exposition_shows() {
        let registry = Registry::new();
        registry
            .declare("tallyowl_events_total", MetricKind::Counter, "Events.", &[])
            .unwrap();
        registry
            .declare(
                "tallyowl_queue_depth_count",
                MetricKind::Gauge,
                "Depth.",
                &[],
            )
            .unwrap();
        registry
            .declare(
                "tallyowl_commit_seconds",
                MetricKind::Histogram,
                "Commits.",
                &[0.1, 1.0],
            )
            .unwrap();
        let none = labels(&[]);
        registry.add("tallyowl_events_total", &none, 3);
        registry.set_gauge("tallyowl_queue_depth_count", &none, -2);
        registry.observe("tallyowl_commit_seconds", &none, 0.5);

        let readings = registry.snapshot();
        assert_eq!(readings.len(), 3);
        let by_name = |name: &str| {
            readings
                .iter()
                .find(|r| r.name == name)
                .expect("the snapshot holds it")
                .clone()
        };
        assert_eq!(by_name("tallyowl_events_total").value, 3.0);
        // The same negative level the text exposition shows, not its unsigned
        // bit pattern.
        assert_eq!(by_name("tallyowl_queue_depth_count").value, -2.0);
        let histogram = by_name("tallyowl_commit_seconds")
            .histogram
            .expect("a histogram reading");
        assert_eq!(histogram.bounds, vec![0.1, 1.0]);
        assert_eq!(histogram.counts, vec![0, 1]);
        assert_eq!(histogram.count, 1);
    }

    #[test]
    fn a_label_value_with_a_quotation_mark_stays_parseable() {
        let r = Registry::new();
        r.declare("tallyowl_events_total", MetricKind::Counter, "Events.", &[])
            .unwrap();
        r.increment("tallyowl_events_total", &labels(&[("reason", "a \"b\"")]));
        assert!(r.render_text().contains(r#"reason="a \"b\"""#));
    }
}
