//! Counter, gauge, and histogram instruments for an application.
//!
//! # What this is, and what it is not
//!
//! `tallyowl_obs::metrics` holds the instruments TallyOwl's own services expose.
//! It refuses a name without the `tallyowl_` prefix and refuses a label that
//! identifies an end user, because those rules protect TallyOwl's operational
//! metrics.
//!
//! This module is the other side: the instruments an **application** declares.
//! An application names its own metrics, so no prefix rule applies here. And
//! `docs/DATA_MODEL.md` section 3.4 permits a request, trace, session, or end
//! user ID as a label, so no label is forbidden either. What it says instead is
//! that TallyOwl must not silently put a high-cardinality series in an overflow
//! series, and that is what the budget below does: it keeps every series it
//! admitted correct, and it refuses a new one in the open rather than folding
//! it into a bucket nobody asked for.
//!
//! # Aggregation happens here
//!
//! An application calls `add` a million times and the meter sends one point for
//! each series in each period. The driver batches; the meter aggregates. A
//! process that sent one point for each call would put its own request rate on
//! the ingest path.
//!
//! # Restart and reset
//!
//! A cumulative counter reports the value since `start_at`. `start_at` is the
//! moment the meter created the series, so a restarted process reports a fresh
//! `start_at` beside a value that begins again at zero. **That pair is what
//! makes a reset visible**: a reader that sees a lower value with the same
//! `start_at` is looking at a defect, and a reader that sees a lower value with
//! a later `start_at` is looking at a restart. `rate` and `increase` in the
//! query executor read it that way.
//!
//! A delta meter reports what happened inside the period and resets its
//! accumulator, so `start_at` is the end of the previous period.

use std::collections::BTreeMap;
use std::sync::Mutex;

use tallyowl_collector_api::types::{
    HistogramValue, MetricKind, MetricPointPayload,
    MetricPointPayload_temporality as WireTemporality, PropertyOrigin, TelemetryKind,
};
use tallyowl_obs::error::TallyOwlError;
use tallyowl_obs::time::now_ms;
use tallyowl_wire::{collector as wire, collector_items_bridge as items, Value};

use crate::{envelope, Capture};

/// Whether a point reports the total since the meter started, or what happened
/// inside one period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Temporality {
    /// The total since `start_at`. A restart is visible as a new `start_at`.
    Cumulative,
    /// What happened since the previous snapshot. The accumulator resets.
    Delta,
}

impl Temporality {
    fn to_wire(self) -> WireTemporality {
        match self {
            Temporality::Cumulative => WireTemporality::Cumulative,
            Temporality::Delta => WireTemporality::Delta,
        }
    }
}

/// What a meter refuses, and when.
///
/// These are the application-side half of the budgets in `docs/DATA_MODEL.md`
/// section 3.4. The collector enforces the same shape at the trust boundary,
/// because an application that does not use a maintained driver still reaches
/// it. This side exists so that an application learns at the call site, where
/// the label that caused it is still in scope.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// Active series for one metric name. A new series past this is refused.
    pub max_series_for_each_metric: usize,
    /// Labels on one series.
    pub max_labels: usize,
    /// Bytes in one label value.
    pub max_label_value_bytes: usize,
}

impl Default for Budget {
    fn default() -> Budget {
        Budget {
            // 2,000 series for one name is enough for a per-route or per-status
            // breakdown of a large application, and small enough that a label
            // holding a request ID reaches it in seconds rather than filling
            // memory quietly.
            max_series_for_each_metric: 2_000,
            max_labels: 16,
            max_label_value_bytes: 256,
        }
    }
}

/// Labels of one series, sorted, so that one series has one key.
pub type Labels = BTreeMap<String, String>;

/// Build a label set from pairs.
pub fn labels(pairs: &[(&str, &str)]) -> Labels {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

#[derive(Debug, Clone)]
struct Family {
    kind: MetricKind,
    unit: Option<String>,
    description: Option<String>,
    bounds: Vec<f64>,
    series: BTreeMap<Labels, SeriesState>,
}

#[derive(Debug, Clone)]
struct SeriesState {
    /// When this series began counting. A cumulative point carries it, and a
    /// restart is visible because a new process produces a new one.
    start_at: i64,
    /// The last time this series changed, which becomes `end_at`.
    updated_at: i64,
    value: f64,
    histogram: Option<HistogramState>,
    /// The trace of one recent observation, so a chart can jump from a point to
    /// a trace. The most recent one wins: a reader following an exemplar wants
    /// a trace that still exists, and the newest is the most likely to.
    exemplar_trace_id: Option<[u8; 16]>,
    /// Whether anything was recorded since the last snapshot. A delta meter
    /// sends nothing for a series that did not move, because a period of zeros
    /// for ten thousand idle series is the same cost as ten thousand events.
    dirty: bool,
}

#[derive(Debug, Clone)]
struct HistogramState {
    bounds: Vec<f64>,
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

/// How much a meter refused, and why. An application reads this to find out
/// that a label it added is costing it series.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pressure {
    pub series_refused: u64,
    pub labels_refused: u64,
    pub value_truncated: u64,
}

/// Every instrument one application declares.
pub struct Meter {
    temporality: Temporality,
    budget: Budget,
    service_name: Option<String>,
    release: Option<String>,
    state: Mutex<MeterState>,
}

struct MeterState {
    families: BTreeMap<String, Family>,
    pressure: Pressure,
    /// The end of the previous snapshot, which is the start of this period for
    /// a delta meter.
    period_started_at: i64,
}

impl Meter {
    /// A cumulative meter. Cumulative is the default because it survives a lost
    /// batch: a missing delta is a hole in a total that nothing can rebuild,
    /// and a missing cumulative point costs one sample of resolution.
    pub fn new() -> Meter {
        Meter::with_temporality(Temporality::Cumulative)
    }

    pub fn with_temporality(temporality: Temporality) -> Meter {
        Meter {
            temporality,
            budget: Budget::default(),
            service_name: None,
            release: None,
            state: Mutex::new(MeterState {
                families: BTreeMap::new(),
                pressure: Pressure::default(),
                period_started_at: now_ms(),
            }),
        }
    }

    pub fn with_budget(mut self, budget: Budget) -> Meter {
        self.budget = budget;
        self
    }

    /// Name the service every point carries. A metric without a service is hard
    /// to read on a dashboard that holds more than one.
    pub fn with_service(mut self, service_name: &str) -> Meter {
        self.service_name = Some(service_name.to_string());
        self
    }

    pub fn with_release(mut self, release: &str) -> Meter {
        self.release = Some(release.to_string());
        self
    }

    pub fn temporality(&self) -> Temporality {
        self.temporality
    }

    pub fn pressure(&self) -> Pressure {
        self.state.lock().expect("meter lock").pressure
    }

    /// Declare a counter. A counter only rises.
    pub fn counter(&self, name: &str, unit: Option<&str>, description: Option<&str>) {
        self.declare(name, MetricKind::Counter, unit, description, &[]);
    }

    /// Declare a gauge. A gauge is an observation at a moment, and it can fall.
    pub fn gauge(&self, name: &str, unit: Option<&str>, description: Option<&str>) {
        self.declare(name, MetricKind::Gauge, unit, description, &[]);
    }

    /// Declare a histogram over explicit upper bounds.
    ///
    /// The bounds travel with every point, so two producers with different
    /// bounds stay mergeable: the query side merges what it can and says so
    /// when it cannot. See `histogram_merge` in the query executor.
    pub fn histogram(
        &self,
        name: &str,
        bounds: &[f64],
        unit: Option<&str>,
        description: Option<&str>,
    ) {
        self.declare(name, MetricKind::Histogram, unit, description, bounds);
    }

    fn declare(
        &self,
        name: &str,
        kind: MetricKind,
        unit: Option<&str>,
        description: Option<&str>,
        bounds: &[f64],
    ) {
        let mut sorted = bounds.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut state = self.state.lock().expect("meter lock");
        state.families.entry(name.to_string()).or_insert(Family {
            kind,
            unit: unit.map(|u| u.to_string()),
            description: description.map(|d| d.to_string()),
            bounds: sorted,
            series: BTreeMap::new(),
        });
    }

    /// Add to a counter.
    ///
    /// A negative delta is refused rather than applied, because a counter that
    /// falls makes every `rate` over it wrong and the reader cannot tell that
    /// from a restart.
    pub fn add(&self, name: &str, labels: &Labels, delta: f64) -> Result<(), TallyOwlError> {
        if delta < 0.0 {
            return Err(TallyOwlError::invalid_argument(format!(
                "The counter `{name}` was given a negative amount. A counter only rises. Use a gauge for a value that falls."
            )));
        }
        self.record(name, labels, MetricKind::Counter, |series, at| {
            series.value += delta;
            series.updated_at = at;
        })
    }

    pub fn increment(&self, name: &str, labels: &Labels) -> Result<(), TallyOwlError> {
        self.add(name, labels, 1.0)
    }

    /// Set a gauge to what it is now.
    pub fn set(&self, name: &str, labels: &Labels, value: f64) -> Result<(), TallyOwlError> {
        self.record(name, labels, MetricKind::Gauge, |series, at| {
            series.value = value;
            series.updated_at = at;
            // A gauge reports the moment it was read, so the period it covers
            // has no width. A reader that averaged it over a period the value
            // was not held for would report a number nothing observed.
            series.start_at = at;
        })
    }

    /// Record one observation in a histogram.
    pub fn observe(&self, name: &str, labels: &Labels, value: f64) -> Result<(), TallyOwlError> {
        self.observe_in_trace(name, labels, value, None)
    }

    /// Record one observation and the trace it came from.
    ///
    /// This is the exemplar. A chart of a latency histogram becomes a way into
    /// one slow request rather than a shape a reader has to go and look for.
    pub fn observe_in_trace(
        &self,
        name: &str,
        labels: &Labels,
        value: f64,
        trace_id: Option<[u8; 16]>,
    ) -> Result<(), TallyOwlError> {
        self.record(name, labels, MetricKind::Histogram, |series, at| {
            series.updated_at = at;
            if let Some(histogram) = series.histogram.as_mut() {
                histogram.sum += value;
                histogram.count += 1;
                for (index, bound) in histogram.bounds.iter().enumerate() {
                    if value <= *bound {
                        histogram.counts[index] += 1;
                    }
                }
            }
            if trace_id.is_some() {
                series.exemplar_trace_id = trace_id;
            }
        })
    }

    fn record(
        &self,
        name: &str,
        labels: &Labels,
        expected: MetricKind,
        apply: impl FnOnce(&mut SeriesState, i64),
    ) -> Result<(), TallyOwlError> {
        let at = now_ms();
        let mut state = self.state.lock().expect("meter lock");

        if labels.len() > self.budget.max_labels {
            state.pressure.labels_refused += 1;
            return Err(TallyOwlError::over_limit(
                "Metric series",
                &format!("{} labels", labels.len()),
                &format!("{} labels", self.budget.max_labels),
                "Send fewer labels, or raise the label budget on this meter.",
            ));
        }
        for value in labels.values() {
            if value.len() > self.budget.max_label_value_bytes {
                state.pressure.value_truncated += 1;
                return Err(TallyOwlError::over_limit(
                    "Metric label value",
                    &format!("{} bytes", value.len()),
                    &format!("{} bytes", self.budget.max_label_value_bytes),
                    "Send a shorter label value, or raise the label-value budget on this meter.",
                ));
            }
        }

        let budget = self.budget.max_series_for_each_metric;
        let Some(family) = state.families.get(name) else {
            return Err(TallyOwlError::failed_precondition(format!(
                "The metric `{name}` was used before it was declared. Declare it with `counter`, `gauge`, or `histogram` first."
            )));
        };
        if family.kind != expected {
            return Err(TallyOwlError::invalid_argument(format!(
                "The metric `{name}` is a {}, and this call records a {}.",
                kind_name(&family.kind),
                kind_name(&expected)
            )));
        }

        let known = family.series.contains_key(labels);
        let admitted = family.series.len();
        if !known && admitted >= budget {
            // Not an overflow series, and not a silent drop. The application is
            // told, at the call site, that this label set costs more series than
            // the budget permits. Every series already admitted keeps counting
            // correctly.
            state.pressure.series_refused += 1;
            return Err(TallyOwlError::over_limit(
                &format!("The metric `{name}`"),
                &format!("{} series", admitted + 1),
                &format!("{budget} series"),
                "Remove a label that takes many values, or raise the series budget on this meter.",
            ));
        }

        let family = state
            .families
            .get_mut(name)
            .expect("the family was found a moment ago");
        let bounds = family.bounds.clone();
        let kind = family.kind.clone();
        let series = family.series.entry(labels.clone()).or_insert(SeriesState {
            start_at: at,
            updated_at: at,
            value: 0.0,
            histogram: (kind == MetricKind::Histogram).then(|| HistogramState {
                counts: vec![0; bounds.len()],
                bounds,
                sum: 0.0,
                count: 0,
            }),
            exemplar_trace_id: None,
            dirty: false,
        });
        apply(series, at);
        series.dirty = true;
        Ok(())
    }

    /// The value a counter or a gauge holds now.
    ///
    /// A host that copies from another source of truth reads this to work out
    /// what to add, rather than keeping a second copy of the number that could
    /// disagree with this one.
    pub fn value(&self, name: &str, labels: &Labels) -> Option<f64> {
        let state = self.state.lock().expect("meter lock");
        Some(state.families.get(name)?.series.get(labels)?.value)
    }

    /// The observation count and the sum a histogram holds now.
    pub fn histogram_totals(&self, name: &str, labels: &Labels) -> Option<(u64, f64)> {
        let state = self.state.lock().expect("meter lock");
        let histogram = state
            .families
            .get(name)?
            .series
            .get(labels)?
            .histogram
            .as_ref()?;
        Some((histogram.count, histogram.sum))
    }

    /// Add a whole bucket shape at once, rather than one observation at a time.
    ///
    /// This exists for a host that already holds a histogram and wants to copy
    /// it: a service publishing its own instruments, or a compatibility edge
    /// carrying one across. Replaying each observation would cost one call for
    /// every sample ever taken, and it would put each sample in the middle of
    /// its bucket rather than where it was.
    ///
    /// `counts` is cumulative, matching what the wire carries: `counts[i]` is
    /// every observation at or below the bound at `i`.
    pub fn observe_bucket_counts(
        &self,
        name: &str,
        labels: &Labels,
        counts: &[u64],
        count: u64,
        sum: f64,
    ) {
        let at = now_ms();
        let mut state = self.state.lock().expect("meter lock");
        let Some(family) = state.families.get(name) else {
            return;
        };
        if family.kind != MetricKind::Histogram {
            return;
        }
        let bounds = family.bounds.clone();
        let family = state.families.get_mut(name).expect("found a moment ago");
        let series = family.series.entry(labels.clone()).or_insert(SeriesState {
            start_at: at,
            updated_at: at,
            value: 0.0,
            histogram: Some(HistogramState {
                counts: vec![0; bounds.len()],
                bounds,
                sum: 0.0,
                count: 0,
            }),
            exemplar_trace_id: None,
            dirty: false,
        });
        if let Some(histogram) = series.histogram.as_mut() {
            for (index, added) in counts.iter().enumerate() {
                if index < histogram.counts.len() {
                    histogram.counts[index] = histogram.counts[index].saturating_add(*added);
                }
            }
            histogram.count = histogram.count.saturating_add(count);
            histogram.sum += sum;
        }
        series.updated_at = at;
        series.dirty = true;
    }

    /// Take one snapshot of every series that moved, as captures the driver can
    /// send.
    ///
    /// A delta meter resets its accumulators here. A cumulative meter does not,
    /// so a lost batch costs one sample rather than a permanent hole.
    pub fn snapshot(&self) -> Vec<Capture> {
        let end_at = now_ms();
        let mut state = self.state.lock().expect("meter lock");
        let period_start = state.period_started_at;
        state.period_started_at = end_at;
        let temporality = self.temporality;
        let service_name = self.service_name.clone();
        let release = self.release.clone();

        let mut out = Vec::new();
        for (name, family) in state.families.iter_mut() {
            for (series_labels, series) in family.series.iter_mut() {
                if !series.dirty && temporality == Temporality::Delta {
                    continue;
                }
                // A gauge has no period either way: it reports the moment it was
                // read, so both ends of it are that moment. Anything else would
                // let a reader average a value over a span it was not held for.
                let gauge = family.kind == MetricKind::Gauge;
                let start_at = match temporality {
                    _ if gauge => series.updated_at,
                    Temporality::Cumulative => series.start_at,
                    Temporality::Delta => period_start,
                };
                let end_at = if gauge { series.updated_at } else { end_at };
                let (number_value, histogram_value) = match family.kind {
                    MetricKind::Histogram => {
                        let histogram = series
                            .histogram
                            .as_ref()
                            .expect("a histogram series holds buckets");
                        (
                            None,
                            Some(HistogramValue {
                                count: histogram.count,
                                sum: histogram.sum,
                                bounds: histogram.bounds.clone(),
                                counts: histogram.counts.clone(),
                            }),
                        )
                    }
                    _ => (Some(series.value), None),
                };

                let payload = MetricPointPayload {
                    metric_name: name.clone(),
                    metric_kind: family.kind.clone(),
                    unit: family.unit.clone(),
                    description: family.description.clone(),
                    monotonic: family.kind == MetricKind::Counter,
                    temporality: temporality.to_wire(),
                    start_at,
                    end_at,
                    labels: series_labels
                        .iter()
                        .map(|(k, v)| {
                            wire::property(k, Value::Text(v.clone()), PropertyOrigin::Client)
                        })
                        .collect(),
                    number_value,
                    histogram_value,
                    exemplar_trace_id: series.exemplar_trace_id.map(|id| id.to_vec()),
                };

                let mut item_envelope = envelope(TelemetryKind::MetricPoint);
                item_envelope.occurred_at = end_at;
                item_envelope.service_name = service_name.clone();
                item_envelope.release = release.clone();
                if let Some(trace_id) = series.exemplar_trace_id {
                    item_envelope.trace_id = Some(trace_id.to_vec());
                }
                out.push(Capture::from_item(items::metric_point(
                    item_envelope,
                    payload,
                )));

                series.dirty = false;
                if temporality == Temporality::Delta {
                    series.value = 0.0;
                    series.start_at = end_at;
                    if let Some(histogram) = series.histogram.as_mut() {
                        histogram.sum = 0.0;
                        histogram.count = 0;
                        histogram.counts.iter_mut().for_each(|c| *c = 0);
                    }
                }
            }
        }
        out
    }
}

impl Default for Meter {
    fn default() -> Meter {
        Meter::new()
    }
}

fn kind_name(kind: &MetricKind) -> &'static str {
    match kind {
        MetricKind::Counter => "counter",
        MetricKind::Gauge => "gauge",
        MetricKind::Histogram => "histogram",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(capture: &Capture) -> &MetricPointPayload {
        capture
            .item()
            .metric_point
            .as_ref()
            .expect("a metric capture carries a metric point")
    }

    fn find<'a>(captures: &'a [Capture], name: &str) -> &'a MetricPointPayload {
        captures
            .iter()
            .map(point)
            .find(|p| p.metric_name == name)
            .expect("the snapshot holds this metric")
    }

    #[test]
    fn a_counter_aggregates_in_process_and_sends_one_point() {
        let meter = Meter::new();
        meter.counter("orders_placed_total", None, Some("Orders placed."));
        let route = labels(&[("route", "/checkout")]);
        for _ in 0..1_000 {
            meter.increment("orders_placed_total", &route).unwrap();
        }
        let captures = meter.snapshot();
        assert_eq!(captures.len(), 1);
        let point = find(&captures, "orders_placed_total");
        assert_eq!(point.number_value, Some(1_000.0));
        assert!(point.monotonic);
        assert_eq!(point.temporality, WireTemporality::Cumulative);
    }

    #[test]
    fn a_cumulative_counter_keeps_its_total_and_its_start_across_snapshots() {
        let meter = Meter::new();
        meter.counter("requests_total", None, None);
        let none = labels(&[]);
        meter.add("requests_total", &none, 5.0).unwrap();
        let first = meter.snapshot();
        meter.add("requests_total", &none, 3.0).unwrap();
        let second = meter.snapshot();

        let a = find(&first, "requests_total");
        let b = find(&second, "requests_total");
        assert_eq!(a.number_value, Some(5.0));
        assert_eq!(b.number_value, Some(8.0));
        // The same run reports the same start. This is what makes a fall in the
        // value a defect rather than a restart.
        assert_eq!(a.start_at, b.start_at);
    }

    #[test]
    fn a_restart_reports_a_new_start_beside_a_value_that_begins_again() {
        let first = Meter::new();
        first.counter("requests_total", None, None);
        let none = labels(&[]);
        first.add("requests_total", &none, 9.0).unwrap();
        let before = find(&first.snapshot(), "requests_total").clone();

        // A new process. The series is new, so its start is new.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = Meter::new();
        second.counter("requests_total", None, None);
        second.add("requests_total", &none, 2.0).unwrap();
        let after = find(&second.snapshot(), "requests_total").clone();

        assert_eq!(before.number_value, Some(9.0));
        assert_eq!(after.number_value, Some(2.0));
        assert!(
            after.start_at > before.start_at,
            "a restarted counter carries a later start"
        );
    }

    #[test]
    fn a_delta_meter_reports_the_period_and_resets() {
        let meter = Meter::with_temporality(Temporality::Delta);
        meter.counter("requests_total", None, None);
        let none = labels(&[]);
        meter.add("requests_total", &none, 5.0).unwrap();
        assert_eq!(
            find(&meter.snapshot(), "requests_total").number_value,
            Some(5.0)
        );
        meter.add("requests_total", &none, 3.0).unwrap();
        assert_eq!(
            find(&meter.snapshot(), "requests_total").number_value,
            Some(3.0)
        );
    }

    #[test]
    fn a_delta_meter_sends_nothing_for_a_series_that_did_not_move() {
        let meter = Meter::with_temporality(Temporality::Delta);
        meter.counter("requests_total", None, None);
        meter.add("requests_total", &labels(&[]), 1.0).unwrap();
        assert_eq!(meter.snapshot().len(), 1);
        assert!(meter.snapshot().is_empty());
    }

    #[test]
    fn a_gauge_falls_and_a_counter_refuses_to() {
        let meter = Meter::new();
        meter.gauge("queue_depth_count", None, None);
        meter.counter("requests_total", None, None);
        let none = labels(&[]);
        meter.set("queue_depth_count", &none, 12.0).unwrap();
        meter.set("queue_depth_count", &none, 4.0).unwrap();
        assert_eq!(
            find(&meter.snapshot(), "queue_depth_count").number_value,
            Some(4.0)
        );
        let refused = meter.add("requests_total", &none, -1.0).unwrap_err();
        assert!(refused.message.contains("only rises"));
    }

    #[test]
    fn a_histogram_carries_its_bounds_and_its_counts() {
        let meter = Meter::new();
        meter.histogram("request_seconds", &[0.01, 0.1, 1.0], Some("s"), None);
        let none = labels(&[]);
        for value in [0.005, 0.05, 0.5, 5.0] {
            meter.observe("request_seconds", &none, value).unwrap();
        }
        let captures = meter.snapshot();
        let histogram = find(&captures, "request_seconds")
            .histogram_value
            .as_ref()
            .expect("a histogram point carries buckets");
        assert_eq!(histogram.count, 4);
        assert_eq!(histogram.bounds, vec![0.01, 0.1, 1.0]);
        assert_eq!(histogram.counts, vec![1, 2, 3]);
        assert!((histogram.sum - 5.555).abs() < 1e-9);
    }

    #[test]
    fn an_exemplar_carries_the_trace_the_observation_came_from() {
        let meter = Meter::new();
        meter.histogram("request_seconds", &[1.0], None, None);
        let trace = [7u8; 16];
        meter
            .observe_in_trace("request_seconds", &labels(&[]), 2.0, Some(trace))
            .unwrap();
        let captures = meter.snapshot();
        assert_eq!(
            find(&captures, "request_seconds").exemplar_trace_id,
            Some(trace.to_vec())
        );
        // The envelope carries it too, so a stored point joins to a trace
        // without reading the payload.
        assert_eq!(captures[0].item().envelope.trace_id, Some(trace.to_vec()));
    }

    #[test]
    fn a_series_budget_refuses_in_the_open_and_keeps_every_admitted_series_correct() {
        let meter = Meter::new().with_budget(Budget {
            max_series_for_each_metric: 2,
            ..Budget::default()
        });
        meter.counter("requests_total", None, None);
        meter
            .add("requests_total", &labels(&[("route", "/a")]), 3.0)
            .unwrap();
        meter
            .add("requests_total", &labels(&[("route", "/b")]), 4.0)
            .unwrap();

        let refused = meter
            .add("requests_total", &labels(&[("route", "/c")]), 5.0)
            .unwrap_err();
        assert_eq!(
            refused.code,
            tallyowl_obs::error::ErrorCode::ResourceExhausted
        );
        assert_eq!(meter.pressure().series_refused, 1);

        // The two that were admitted still count, and nothing was folded into an
        // overflow series.
        let captures = meter.snapshot();
        assert_eq!(captures.len(), 2);
        let values: Vec<Option<f64>> = captures.iter().map(|c| point(c).number_value).collect();
        assert!(values.contains(&Some(3.0)) && values.contains(&Some(4.0)));
        assert!(!captures
            .iter()
            .any(|c| point(c).labels.iter().any(|l| l.key == "overflow")));
    }

    #[test]
    fn too_many_labels_is_refused_at_the_call_site() {
        let meter = Meter::new().with_budget(Budget {
            max_labels: 2,
            ..Budget::default()
        });
        meter.counter("requests_total", None, None);
        let wide = labels(&[("a", "1"), ("b", "2"), ("c", "3")]);
        let refused = meter.add("requests_total", &wide, 1.0).unwrap_err();
        assert!(refused.message.contains("labels"));
        assert_eq!(meter.pressure().labels_refused, 1);
    }

    #[test]
    fn a_metric_used_before_it_is_declared_is_refused() {
        let meter = Meter::new();
        let refused = meter.add("requests_total", &labels(&[]), 1.0).unwrap_err();
        assert!(refused.message.contains("before it was declared"));
    }

    #[test]
    fn a_counter_call_against_a_gauge_is_refused() {
        let meter = Meter::new();
        meter.gauge("queue_depth_count", None, None);
        let refused = meter
            .add("queue_depth_count", &labels(&[]), 1.0)
            .unwrap_err();
        assert!(refused.message.contains("is a gauge"));
    }

    #[test]
    fn a_high_cardinality_label_is_admitted_rather_than_rewritten() {
        // DATA_MODEL.md section 3.4 permits a request ID as a label. The meter
        // keeps it exactly, because the alternative is a value nobody can join.
        let meter = Meter::new();
        meter.counter("requests_total", None, None);
        for index in 0..500 {
            meter
                .add(
                    "requests_total",
                    &labels(&[("request_id", &format!("r-{index}"))]),
                    1.0,
                )
                .unwrap();
        }
        let captures = meter.snapshot();
        assert_eq!(captures.len(), 500);
        assert!(captures
            .iter()
            .any(|c| point(c).labels.iter().any(|l| l.key == "request_id")));
    }

    #[test]
    fn a_meter_stamps_the_service_and_the_release_on_every_point() {
        let meter = Meter::new().with_service("checkout").with_release("1.4.0");
        meter.counter("requests_total", None, None);
        meter.increment("requests_total", &labels(&[])).unwrap();
        let captures = meter.snapshot();
        let envelope = &captures[0].item().envelope;
        assert_eq!(envelope.service_name.as_deref(), Some("checkout"));
        assert_eq!(envelope.release.as_deref(), Some("1.4.0"));
    }
}
