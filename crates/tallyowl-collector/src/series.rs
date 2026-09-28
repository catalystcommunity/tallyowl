//! Metric series cost, at the trust boundary.
//!
//! `docs/DATA_MODEL.md` section 3.4 asks for four things and this module is all
//! four:
//!
//! - configurable active-series and retained-byte budgets;
//! - maximum labels, label bytes, and individual value length;
//! - **exact** accounting and visible cost estimates for high-cardinality
//!   labels;
//! - local and head-side pressure and backpressure counters.
//!
//! # Exact, and never an overflow series
//!
//! The section above that list says TallyOwl supports high-cardinality series
//! and "does not silently put these series in an overflow series". So this
//! ledger holds one entry for each series it admitted, counts them exactly, and
//! refuses a new series **in the open** when a budget is full. A refused point
//! becomes a rejected item with `resource-exhausted` on the receipt, which is
//! the explicit backpressure Phase 6 asks for. Nothing is folded, nothing is
//! rewritten, and nothing is dropped quietly.
//!
//! The alternative shapes were worse. Folding into a bucket makes a value
//! nobody can join to a request. Sampling makes a counter that is wrong by an
//! amount nobody can bound. Dropping quietly makes a chart that reads as an
//! outage.
//!
//! # What the byte budget counts
//!
//! The bytes of a series are the bytes of its **latest** point, and the bytes
//! of a metric are the sum over its active series. That is the live footprint:
//! what one reading of every active series costs downstream. It rises when a
//! series arrives or grows, and it falls when a series expires.
//!
//! An earlier version added every admitted point for as long as the process
//! ran. A steady metric then filled its budget in hours with no growth at all,
//! and every new series of it was refused until a restart, with a message that
//! told the operator to send fewer series.
//!
//! # What the ledger itself may cost
//!
//! The ledger is memory inside the collector, keyed by a name a client chose.
//! So it has three bounds of its own:
//!
//! - one project holds at most `max_metric_names` metric names. A new name past
//!   that is refused in the open, the same way a new series is;
//! - an idle sweep runs on the admit path at a fraction of the idle period. It
//!   removes every idle series and every metric that holds none, so a metric
//!   that stopped gives its place back without having to fill first;
//! - the active totals are kept as running numbers. Reading them costs nothing,
//!   which matters because intake reads them for every batch.
//!
//! # The work budget
//!
//! Merging is quadratic in nothing and linear in the points, but a batch is a
//! caller-controlled size and a collector must not spend unbounded time inside
//! one. `max_merge_points` bounds it: past that count the batch travels
//! unmerged, which costs bytes downstream and never costs correctness.
//!
//! # Merging
//!
//! Two snapshots of one series in one batch merge into one point.
//!
//! - **delta** adds: the values sum, the histogram bucket counts sum, the start
//!   is the earliest and the end is the latest;
//! - **cumulative** supersedes: a cumulative point is a level rather than an
//!   addend, so the point with the later end wins and the earlier one goes.
//!
//! Two histograms with different bounds never merge. Adding bucket counts
//! across different bounds produces a shape that no observation had, and a
//! reader cannot tell it from a real one.

use std::collections::HashMap;
use std::sync::Mutex;

use tallyowl_collector_api::types::{
    MetricPointPayload, MetricPointPayload_temporality as Temporality, TelemetryItem,
};
use tallyowl_compat::label_text;
use tallyowl_obs::error::TallyOwlError;

/// What a project's metrics may cost. Every one of these is a configuration
/// setting; see `metrics.*` in `crates/tallyowl-config/src/schema.rs`.
#[derive(Debug, Clone, Copy)]
pub struct SeriesBudget {
    /// Active series for one metric name in one project.
    pub max_series_for_each_metric: u64,
    /// Retained bytes for one metric name in one project, counted exactly from
    /// the encoded points rather than estimated from a row count.
    pub max_bytes_for_each_metric: u64,
    /// Labels on one series.
    pub max_labels: usize,
    /// Bytes in one label value.
    pub max_label_value_bytes: usize,
    /// Bytes in all the labels of one series together.
    pub max_label_bytes: usize,
    /// How many points in one batch the collector will merge. Past this the
    /// batch travels unmerged.
    pub max_merge_points: usize,
    /// How long a series stays active with nothing arriving for it. A series
    /// that stops is not a series that costs, so the budget gets it back.
    pub idle_expiry_ms: i64,
}

/// Metric names one project may hold, when nothing sets another number. See
/// `metrics.maxMetricNamesForEachProject`.
pub const DEFAULT_MAX_METRIC_NAMES: u64 = 10_000;

impl Default for SeriesBudget {
    fn default() -> SeriesBudget {
        SeriesBudget {
            max_series_for_each_metric: 100_000,
            max_bytes_for_each_metric: 64 * 1024 * 1024,
            max_labels: 32,
            max_label_value_bytes: 1024,
            max_label_bytes: 4096,
            max_merge_points: 10_000,
            idle_expiry_ms: 3_600_000,
        }
    }
}

/// What the ledger refused and what it holds, for the counters a service
/// exposes and for a test to read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pressure {
    pub series_refused: u64,
    pub bytes_refused: u64,
    pub labels_refused: u64,
    /// New metric names refused because the project already holds its limit.
    pub names_refused: u64,
    /// Metric names the ledger holds, across every project.
    pub active_metrics: u64,
    pub active_series: u64,
    pub active_bytes: u64,
    pub expired_series: u64,
}

#[derive(Debug)]
struct SeriesEntry {
    last_seen_ms: i64,
    bytes: u64,
}

#[derive(Debug, Default)]
struct MetricState {
    series: HashMap<String, SeriesEntry>,
    bytes: u64,
}

/// Exact per-project, per-metric-name accounting.
#[derive(Debug)]
pub struct SeriesLedger {
    budget: SeriesBudget,
    max_metric_names: u64,
    state: Mutex<LedgerState>,
}

#[derive(Debug, Default)]
struct LedgerState {
    /// Keyed by project and metric name. A project cannot spend another
    /// project's budget, which is the multi-tenant half of this.
    metrics: HashMap<(Vec<u8>, String), MetricState>,
    /// How many metric names each project holds. It is what the name limit
    /// reads, so the limit costs one lookup and not a scan.
    names: HashMap<Vec<u8>, u64>,
    /// Running totals. `pressure` holds the refusal counters, and these two
    /// are kept here as the series come and go.
    active_series: u64,
    active_bytes: u64,
    last_sweep_ms: i64,
    pressure: Pressure,
}

impl LedgerState {
    /// Remove every series that has been idle for the whole idle period, and
    /// every metric that then holds none.
    fn sweep(&mut self, horizon: i64) {
        let mut expired = 0u64;
        let mut freed = 0u64;
        let names = &mut self.names;
        self.metrics.retain(|(project, _), metric| {
            metric.series.retain(|_, entry| {
                let keep = entry.last_seen_ms > horizon;
                if !keep {
                    expired += 1;
                    freed = freed.saturating_add(entry.bytes);
                    metric.bytes = metric.bytes.saturating_sub(entry.bytes);
                }
                keep
            });
            let keep = !metric.series.is_empty();
            if !keep {
                if let Some(held) = names.get_mut(project) {
                    *held = held.saturating_sub(1);
                    if *held == 0 {
                        names.remove(project);
                    }
                }
            }
            keep
        });
        self.active_series = self.active_series.saturating_sub(expired);
        self.active_bytes = self.active_bytes.saturating_sub(freed);
        self.pressure.expired_series += expired;
    }
}

impl SeriesLedger {
    pub fn new(budget: SeriesBudget) -> SeriesLedger {
        SeriesLedger {
            budget,
            max_metric_names: DEFAULT_MAX_METRIC_NAMES,
            state: Mutex::new(LedgerState::default()),
        }
    }

    /// Set how many metric names one project may hold.
    pub fn with_max_metric_names(mut self, names: u64) -> SeriesLedger {
        self.max_metric_names = names.max(1);
        self
    }

    /// How often the idle sweep runs. A quarter of the idle period keeps an
    /// idle series for at most a quarter longer than the setting says.
    fn sweep_interval_ms(&self) -> i64 {
        (self.budget.idle_expiry_ms / 4).max(1_000)
    }

    /// Run the idle sweep when one is due. Intake calls this for each batch,
    /// so a ledger that stopped receiving metric points still gives its
    /// series back.
    pub fn sweep_if_due(&self, now_ms: i64) {
        let mut state = self.state.lock().expect("series lock");
        self.sweep_if_due_locked(&mut state, now_ms);
    }

    fn sweep_if_due_locked(&self, state: &mut LedgerState, now_ms: i64) {
        if now_ms - state.last_sweep_ms < self.sweep_interval_ms() {
            return;
        }
        state.last_sweep_ms = now_ms;
        state.sweep(now_ms - self.budget.idle_expiry_ms);
    }

    pub fn budget(&self) -> SeriesBudget {
        self.budget
    }

    pub fn pressure(&self) -> Pressure {
        let state = self.state.lock().expect("series lock");
        let mut pressure = state.pressure;
        pressure.active_series = state.active_series;
        pressure.active_bytes = state.active_bytes;
        pressure.active_metrics = state.metrics.len() as u64;
        pressure
    }

    /// Admit one metric point, or refuse it and say why.
    ///
    /// `size` is the encoded size of the whole item, so the byte budget counts
    /// what the point actually costs downstream rather than a guess.
    pub fn admit(
        &self,
        project_id: &[u8],
        point: &MetricPointPayload,
        size: u64,
        now_ms: i64,
    ) -> Result<(), TallyOwlError> {
        let mut state = self.state.lock().expect("series lock");

        if point.labels.len() > self.budget.max_labels {
            state.pressure.labels_refused += 1;
            return Err(TallyOwlError::over_limit(
                "Metric series",
                &format!("{} labels", point.labels.len()),
                &format!("{} labels", self.budget.max_labels),
                "Send fewer labels on this metric, or raise `metrics.maxLabels`.",
            ));
        }
        let mut label_bytes = 0usize;
        for label in &point.labels {
            let value = label_text(label);
            if value.len() > self.budget.max_label_value_bytes {
                state.pressure.labels_refused += 1;
                return Err(TallyOwlError::over_limit(
                    "Metric label value",
                    &format!("{} bytes", value.len()),
                    &format!("{} bytes", self.budget.max_label_value_bytes),
                    "Send a shorter label value, or raise `metrics.maxLabelValueBytes`.",
                ));
            }
            label_bytes += label.key.len() + value.len();
        }
        if label_bytes > self.budget.max_label_bytes {
            state.pressure.labels_refused += 1;
            return Err(TallyOwlError::over_limit(
                "Metric series labels",
                &format!("{label_bytes} bytes"),
                &format!("{} bytes", self.budget.max_label_bytes),
                "Send fewer or shorter labels, or raise `metrics.maxLabelBytes`.",
            ));
        }

        self.sweep_if_due_locked(&mut state, now_ms);

        let series_key = series_key(point);
        let budget = self.budget;
        let key = (project_id.to_vec(), point.metric_name.clone());
        let state = &mut *state;

        if !state.metrics.contains_key(&key) {
            // A name this project has not sent before. The name is a string a
            // client chose, so the number of them is bounded like everything
            // else a client chooses.
            let held = state.names.get(project_id).copied().unwrap_or(0);
            if held >= self.max_metric_names {
                state.pressure.names_refused += 1;
                return Err(TallyOwlError::over_limit(
                    "This project",
                    &format!("{} metric names", held + 1),
                    &format!("{} metric names", self.max_metric_names),
                    "Put the value that changes in a label and keep the metric name fixed, or raise `metrics.maxMetricNamesForEachProject`.",
                ));
            }
        }

        if let Some(metric) = state.metrics.get_mut(&key) {
            if let Some(entry) = metric.series.get_mut(&series_key) {
                // The latest point replaces the earlier one in the footprint.
                // An admitted series stays admitted even when the byte budget
                // is now full, because refusing it would lose the middle of a
                // series rather than the start of a new one, and a broken
                // series is harder to read than a missing one.
                entry.last_seen_ms = now_ms;
                metric.bytes = metric
                    .bytes
                    .saturating_sub(entry.bytes)
                    .saturating_add(size);
                state.active_bytes = state
                    .active_bytes
                    .saturating_sub(entry.bytes)
                    .saturating_add(size);
                entry.bytes = size;
                return Ok(());
            }

            // The periodic sweep may not have run yet. A budget that is about
            // to refuse checks this one metric first, so a refusal is never
            // caused by a series that already stopped.
            if metric.series.len() as u64 >= budget.max_series_for_each_metric
                || metric.bytes.saturating_add(size) > budget.max_bytes_for_each_metric
            {
                let horizon = now_ms - budget.idle_expiry_ms;
                let mut expired = 0u64;
                let mut freed = 0u64;
                metric.series.retain(|_, entry| {
                    let keep = entry.last_seen_ms > horizon;
                    if !keep {
                        expired += 1;
                        freed = freed.saturating_add(entry.bytes);
                    }
                    keep
                });
                metric.bytes = metric.bytes.saturating_sub(freed);
                state.active_series = state.active_series.saturating_sub(expired);
                state.active_bytes = state.active_bytes.saturating_sub(freed);
                state.pressure.expired_series += expired;
            }

            if metric.series.len() as u64 >= budget.max_series_for_each_metric {
                let held = metric.series.len();
                state.pressure.series_refused += 1;
                return Err(TallyOwlError::over_limit(
                    &format!("The metric `{}`", point.metric_name),
                    &format!("{} active series", held + 1),
                    &format!("{} active series", budget.max_series_for_each_metric),
                    "Remove a label that takes many values, or raise `metrics.maxSeriesForEachMetric` for this installation.",
                ));
            }
            if metric.bytes.saturating_add(size) > budget.max_bytes_for_each_metric {
                let held = metric.bytes;
                state.pressure.bytes_refused += 1;
                return Err(TallyOwlError::over_limit(
                    &format!("The metric `{}`", point.metric_name),
                    &format!("{} bytes in one reading of its active series", held + size),
                    &format!("{} bytes", budget.max_bytes_for_each_metric),
                    "Send fewer or shorter labels on this metric, or raise `metrics.maxBytesForEachMetric`.",
                ));
            }
        } else if size > budget.max_bytes_for_each_metric {
            state.pressure.bytes_refused += 1;
            return Err(TallyOwlError::over_limit(
                &format!("The metric `{}`", point.metric_name),
                &format!("{size} bytes in one point"),
                &format!("{} bytes", budget.max_bytes_for_each_metric),
                "Send fewer or shorter labels on this metric, or raise `metrics.maxBytesForEachMetric`.",
            ));
        }

        // Nothing refused it. Only now does a new name take a place, so a
        // refused point never leaves an empty entry behind.
        let metric = match state.metrics.entry(key) {
            std::collections::hash_map::Entry::Occupied(held) => held.into_mut(),
            std::collections::hash_map::Entry::Vacant(free) => {
                *state.names.entry(project_id.to_vec()).or_insert(0) += 1;
                free.insert(MetricState::default())
            }
        };
        metric.series.insert(
            series_key,
            SeriesEntry {
                last_seen_ms: now_ms,
                bytes: size,
            },
        );
        metric.bytes = metric.bytes.saturating_add(size);
        state.active_series += 1;
        state.active_bytes = state.active_bytes.saturating_add(size);
        Ok(())
    }
}

/// The identity of one series. It lives in `tallyowl_compat` because the
/// scraper watches the same identity for a target restart, and two rules that
/// had to agree would drift the first time somebody changed one of them.
pub use tallyowl_compat::series_key;

/// What one merge produced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// Points that went away because another point of the same series carried
    /// them.
    pub merged: u64,
    /// True when the batch held more points than the work budget, so nothing
    /// was merged.
    pub over_work_budget: bool,
}

/// Merge every compatible metric point in one batch.
///
/// Items that are not metric points travel through untouched and keep their
/// order. A merged point keeps the envelope of the **latest** point of its
/// series, so the surviving event ID belongs to the reading that survived.
pub fn merge(
    items: Vec<TelemetryItem>,
    budget: &SeriesBudget,
) -> (Vec<TelemetryItem>, MergeReport) {
    let points = items.iter().filter(|i| i.metric_point.is_some()).count();
    if points <= 1 {
        return (items, MergeReport::default());
    }
    if points > budget.max_merge_points {
        return (
            items,
            MergeReport {
                merged: 0,
                over_work_budget: true,
            },
        );
    }

    // Position of the first item of each series, so the merged batch keeps the
    // order the caller sent.
    let mut slot_of: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<TelemetryItem> = Vec::with_capacity(items.len());
    let mut merged = 0u64;

    for item in items {
        let Some(point) = item.metric_point.as_ref() else {
            out.push(item);
            continue;
        };
        let key = merge_key(point);
        match slot_of.get(&key) {
            None => {
                slot_of.insert(key, out.len());
                out.push(item);
            }
            Some(&slot) => {
                let held = out[slot]
                    .metric_point
                    .as_ref()
                    .expect("a merge slot holds a metric point");
                let incoming = point;
                if let Some(combined) = combine(held, incoming) {
                    merged += 1;
                    // The later reading owns the merged point, so the event ID
                    // that survives is the one whose end time the point carries.
                    let later = incoming.end_at >= held.end_at;
                    if later {
                        let mut item = item;
                        item.metric_point = Some(combined);
                        out[slot] = item;
                    } else {
                        out[slot].metric_point = Some(combined);
                    }
                } else {
                    // Incompatible: two histograms with different bounds. Both
                    // travel, because a merged shape nobody observed is worse
                    // than two points a reader can see are different.
                    out.push(item);
                }
            }
        }
    }

    (
        out,
        MergeReport {
            merged,
            over_work_budget: false,
        },
    )
}

/// The identity a merge groups on. Unlike `series_key` this includes the
/// temporality, because a delta point and a cumulative point of one series say
/// different things and adding them is meaningless.
fn merge_key(point: &MetricPointPayload) -> String {
    let mut key = series_key(point);
    key.push('\u{3}');
    key.push_str(match point.temporality {
        Temporality::Delta => "delta",
        Temporality::Cumulative => "cumulative",
    });
    key.push_str(&point.metric_name);
    key
}

/// Combine two points of one series, or report that they cannot combine.
fn combine(held: &MetricPointPayload, incoming: &MetricPointPayload) -> Option<MetricPointPayload> {
    let later = if incoming.end_at >= held.end_at {
        incoming
    } else {
        held
    };
    let mut out = later.clone();

    match incoming.temporality {
        // A delta says "this much happened between these two times". Two of
        // them add, and the merged point covers both periods.
        Temporality::Delta => {
            out.start_at = held.start_at.min(incoming.start_at);
            out.end_at = held.end_at.max(incoming.end_at);
            match (&held.histogram_value, &incoming.histogram_value) {
                (Some(a), Some(b)) => {
                    // The same bounds and the same number of buckets. Intake
                    // refuses a histogram whose two lists disagree, but this
                    // function is also reached from the compatibility edge, so
                    // it does not index on a promise made somewhere else.
                    if a.bounds != b.bounds || a.counts.len() != b.counts.len() {
                        return None;
                    }
                    // A sum that does not fit is not a sum. Both points travel
                    // and the head, which holds wider numbers, adds them.
                    let counts = a
                        .counts
                        .iter()
                        .zip(&b.counts)
                        .map(|(a, b)| a.checked_add(*b))
                        .collect::<Option<Vec<u64>>>()?;
                    out.histogram_value = Some(tallyowl_collector_api::types::HistogramValue {
                        count: a.count.checked_add(b.count)?,
                        sum: a.sum + b.sum,
                        bounds: a.bounds.clone(),
                        counts,
                    });
                }
                (None, None) => {
                    out.number_value = Some(
                        held.number_value.unwrap_or(0.0) + incoming.number_value.unwrap_or(0.0),
                    )
                }
                // One carries a histogram and the other a number. That is one
                // name used two ways, and merging it would hide the mistake.
                _ => return None,
            }
        }
        // A cumulative point is a level rather than an addend, so the later
        // reading supersedes the earlier one and carries the earlier start.
        Temporality::Cumulative => {
            if let (Some(a), Some(b)) = (&held.histogram_value, &incoming.histogram_value) {
                if a.bounds != b.bounds {
                    return None;
                }
            } else if held.histogram_value.is_some() != incoming.histogram_value.is_some() {
                return None;
            }
            out.start_at = held.start_at.min(incoming.start_at);
            out.end_at = held.end_at.max(incoming.end_at);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tallyowl_collector_api::types::{HistogramValue, MetricKind, Property, PropertyOrigin};
    use tallyowl_wire::{collector as wire, Value};

    fn labelled(pairs: &[(&str, &str)]) -> Vec<Property> {
        pairs
            .iter()
            .map(|(k, v)| wire::property(k, Value::Text((*v).to_string()), PropertyOrigin::Client))
            .collect()
    }

    fn counter(name: &str, labels: &[(&str, &str)], value: f64) -> MetricPointPayload {
        MetricPointPayload {
            metric_name: name.to_string(),
            metric_kind: MetricKind::Counter,
            unit: None,
            description: None,
            monotonic: true,
            temporality: Temporality::Delta,
            start_at: 1_000,
            end_at: 2_000,
            labels: labelled(labels),
            number_value: Some(value),
            histogram_value: None,
            exemplar_trace_id: None,
        }
    }

    fn histogram(bounds: &[f64], counts: &[u64], sum: f64) -> MetricPointPayload {
        MetricPointPayload {
            metric_name: "request_seconds".to_string(),
            metric_kind: MetricKind::Histogram,
            unit: None,
            description: None,
            monotonic: false,
            temporality: Temporality::Delta,
            start_at: 1_000,
            end_at: 2_000,
            labels: Vec::new(),
            number_value: None,
            histogram_value: Some(HistogramValue {
                count: counts.iter().sum(),
                sum,
                bounds: bounds.to_vec(),
                counts: counts.to_vec(),
            }),
            exemplar_trace_id: None,
        }
    }

    fn item(point: MetricPointPayload) -> TelemetryItem {
        use tallyowl_collector_api::types::{Envelope, TelemetryKind};
        tallyowl_wire::collector_items_bridge::metric_point(
            Envelope {
                event_id: vec![0u8; 16],
                kind: TelemetryKind::MetricPoint,
                schema_version: 1,
                occurred_at: point.end_at,
                observed_at: None,
                received_at: None,
                workspace_id: None,
                project_id: None,
                source_id: None,
                sequence: None,
                release: None,
                service_name: None,
                request_id: None,
                session_id: None,
                end_user_id: None,
                anonymous_id: None,
                trace_id: None,
                span_id: None,
                consent: None,
                sdk_name: "test".to_string(),
                sdk_version: "0".to_string(),
                properties: Vec::new(),
                measurements: None,
            },
            point,
        )
    }

    fn points(items: &[TelemetryItem]) -> Vec<&MetricPointPayload> {
        items
            .iter()
            .filter_map(|i| i.metric_point.as_ref())
            .collect()
    }

    #[test]
    fn two_deltas_of_one_series_add() {
        let batch = vec![
            item(counter("requests_total", &[("route", "/a")], 3.0)),
            item(counter("requests_total", &[("route", "/a")], 4.0)),
        ];
        let (out, report) = merge(batch, &SeriesBudget::default());
        assert_eq!(report.merged, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(points(&out)[0].number_value, Some(7.0));
    }

    #[test]
    fn two_series_of_one_metric_stay_apart() {
        let batch = vec![
            item(counter("requests_total", &[("route", "/a")], 3.0)),
            item(counter("requests_total", &[("route", "/b")], 4.0)),
        ];
        let (out, report) = merge(batch, &SeriesBudget::default());
        assert_eq!(report.merged, 0);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_merged_delta_covers_both_periods() {
        let mut first = counter("requests_total", &[], 1.0);
        first.start_at = 1_000;
        first.end_at = 2_000;
        let mut second = counter("requests_total", &[], 1.0);
        second.start_at = 2_000;
        second.end_at = 3_000;
        let (out, _) = merge(vec![item(first), item(second)], &SeriesBudget::default());
        let merged = points(&out)[0];
        assert_eq!(merged.start_at, 1_000);
        assert_eq!(merged.end_at, 3_000);
    }

    #[test]
    fn a_cumulative_point_supersedes_rather_than_adds() {
        let mut first = counter("requests_total", &[], 5.0);
        first.temporality = Temporality::Cumulative;
        first.end_at = 2_000;
        let mut second = counter("requests_total", &[], 9.0);
        second.temporality = Temporality::Cumulative;
        second.end_at = 3_000;
        let (out, report) = merge(vec![item(first), item(second)], &SeriesBudget::default());
        assert_eq!(report.merged, 1);
        // 9 is the level, not 14.
        assert_eq!(points(&out)[0].number_value, Some(9.0));
        assert_eq!(points(&out)[0].end_at, 3_000);
    }

    #[test]
    fn a_delta_and_a_cumulative_of_one_series_never_merge() {
        let first = counter("requests_total", &[], 5.0);
        let mut second = counter("requests_total", &[], 9.0);
        second.temporality = Temporality::Cumulative;
        let (out, report) = merge(vec![item(first), item(second)], &SeriesBudget::default());
        assert_eq!(report.merged, 0);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn two_histograms_with_the_same_bounds_add_bucket_by_bucket() {
        let a = histogram(&[1.0, 5.0], &[1, 2], 6.0);
        let b = histogram(&[1.0, 5.0], &[3, 4], 10.0);
        let (out, report) = merge(vec![item(a), item(b)], &SeriesBudget::default());
        assert_eq!(report.merged, 1);
        let merged = points(&out)[0].histogram_value.as_ref().unwrap();
        assert_eq!(merged.counts, vec![4, 6]);
        assert_eq!(merged.sum, 16.0);
        assert_eq!(merged.count, 10);
    }

    #[test]
    fn two_histograms_with_different_bounds_both_travel() {
        // Adding bucket counts across different bounds makes a shape nothing
        // observed. Both points survive so a reader can see the difference.
        let a = histogram(&[1.0, 5.0], &[1, 2], 6.0);
        let b = histogram(&[2.0, 8.0], &[3, 4], 10.0);
        let (out, report) = merge(vec![item(a), item(b)], &SeriesBudget::default());
        assert_eq!(report.merged, 0);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_batch_over_the_work_budget_travels_unmerged_and_says_so() {
        let budget = SeriesBudget {
            max_merge_points: 2,
            ..SeriesBudget::default()
        };
        let batch: Vec<TelemetryItem> = (0..3)
            .map(|_| item(counter("requests_total", &[], 1.0)))
            .collect();
        let (out, report) = merge(batch, &budget);
        assert!(report.over_work_budget);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn an_item_that_is_not_a_metric_point_travels_untouched() {
        use tallyowl_collector_api::types::{Envelope, EventPayload, TelemetryKind};
        let event = tallyowl_wire::collector_items_bridge::event(
            Envelope {
                event_id: vec![1u8; 16],
                kind: TelemetryKind::Event,
                schema_version: 1,
                occurred_at: 1,
                observed_at: None,
                received_at: None,
                workspace_id: None,
                project_id: None,
                source_id: None,
                sequence: None,
                release: None,
                service_name: None,
                request_id: None,
                session_id: None,
                end_user_id: None,
                anonymous_id: None,
                trace_id: None,
                span_id: None,
                consent: None,
                sdk_name: "test".to_string(),
                sdk_version: "0".to_string(),
                properties: Vec::new(),
                measurements: None,
            },
            EventPayload {
                name: "signed-up".to_string(),
                route: None,
                page_title: None,
            },
        );
        let batch = vec![
            item(counter("requests_total", &[], 1.0)),
            event,
            item(counter("requests_total", &[], 1.0)),
        ];
        let (out, report) = merge(batch, &SeriesBudget::default());
        assert_eq!(report.merged, 1);
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|i| i.event.is_some()));
    }

    // ---- the ledger --------------------------------------------------------

    fn ledger(budget: SeriesBudget) -> SeriesLedger {
        SeriesLedger::new(budget)
    }

    #[test]
    fn a_series_budget_refuses_a_new_series_and_keeps_the_admitted_ones() {
        let ledger = ledger(SeriesBudget {
            max_series_for_each_metric: 2,
            ..SeriesBudget::default()
        });
        let project = [1u8; 16];
        for route in ["/a", "/b"] {
            ledger
                .admit(
                    &project,
                    &counter("requests_total", &[("route", route)], 1.0),
                    100,
                    10_000,
                )
                .expect("inside the budget");
        }
        let refused = ledger
            .admit(
                &project,
                &counter("requests_total", &[("route", "/c")], 1.0),
                100,
                10_000,
            )
            .expect_err("past the budget");
        assert_eq!(
            refused.code,
            tallyowl_obs::error::ErrorCode::ResourceExhausted
        );
        assert_eq!(ledger.pressure().series_refused, 1);
        assert_eq!(ledger.pressure().active_series, 2);

        // A series already admitted keeps arriving. The budget bounds new
        // series, never the middle of one that is already counting.
        ledger
            .admit(
                &project,
                &counter("requests_total", &[("route", "/a")], 2.0),
                100,
                10_001,
            )
            .expect("an admitted series keeps its place");
    }

    #[test]
    fn one_project_cannot_spend_another_projects_budget() {
        let ledger = ledger(SeriesBudget {
            max_series_for_each_metric: 1,
            ..SeriesBudget::default()
        });
        let point = counter("requests_total", &[("route", "/a")], 1.0);
        ledger.admit(&[1u8; 16], &point, 100, 10_000).unwrap();
        let other = counter("requests_total", &[("route", "/b")], 1.0);
        ledger
            .admit(&[2u8; 16], &other, 100, 10_000)
            .expect("a second project has its own budget");
    }

    #[test]
    fn two_metric_names_have_their_own_budgets() {
        let ledger = ledger(SeriesBudget {
            max_series_for_each_metric: 1,
            ..SeriesBudget::default()
        });
        let project = [1u8; 16];
        ledger
            .admit(&project, &counter("a_total", &[], 1.0), 100, 10_000)
            .unwrap();
        ledger
            .admit(&project, &counter("b_total", &[], 1.0), 100, 10_000)
            .expect("a second name has its own budget");
    }

    #[test]
    fn a_byte_budget_refuses_a_new_series_once_the_bytes_are_spent() {
        let ledger = ledger(SeriesBudget {
            max_bytes_for_each_metric: 250,
            ..SeriesBudget::default()
        });
        let project = [1u8; 16];
        ledger
            .admit(
                &project,
                &counter("requests_total", &[("route", "/a")], 1.0),
                200,
                10_000,
            )
            .unwrap();
        let refused = ledger
            .admit(
                &project,
                &counter("requests_total", &[("route", "/b")], 1.0),
                200,
                10_000,
            )
            .expect_err("past the byte budget");
        assert!(refused.message.contains("bytes"));
        assert_eq!(ledger.pressure().bytes_refused, 1);
    }

    #[test]
    fn an_idle_series_gives_its_place_back() {
        let ledger = ledger(SeriesBudget {
            max_series_for_each_metric: 1,
            idle_expiry_ms: 1_000,
            ..SeriesBudget::default()
        });
        let project = [1u8; 16];
        ledger
            .admit(
                &project,
                &counter("requests_total", &[("route", "/a")], 1.0),
                10,
                10_000,
            )
            .unwrap();
        // An hour later the first series has stopped, so the second gets in.
        ledger
            .admit(
                &project,
                &counter("requests_total", &[("route", "/b")], 1.0),
                10,
                3_600_000,
            )
            .expect("an idle series is not an active series");
        assert_eq!(ledger.pressure().expired_series, 1);
    }

    #[test]
    fn too_many_labels_is_refused_and_counted() {
        let ledger = ledger(SeriesBudget {
            max_labels: 2,
            ..SeriesBudget::default()
        });
        let wide = counter("requests_total", &[("a", "1"), ("b", "2"), ("c", "3")], 1.0);
        let refused = ledger
            .admit(&[1u8; 16], &wide, 10, 10_000)
            .expect_err("wider than the budget");
        assert!(refused.message.contains("labels"));
        assert_eq!(ledger.pressure().labels_refused, 1);
    }

    #[test]
    fn a_label_value_longer_than_the_budget_is_refused() {
        let ledger = ledger(SeriesBudget {
            max_label_value_bytes: 4,
            ..SeriesBudget::default()
        });
        let long = counter("requests_total", &[("route", "/a/very/long/route")], 1.0);
        assert!(ledger.admit(&[1u8; 16], &long, 10, 10_000).is_err());
    }

    #[test]
    fn a_high_cardinality_metric_inside_the_budget_is_admitted_exactly() {
        // Nothing is folded and nothing is sampled. The count is exact, which
        // is what makes the cost visible to an operator.
        let ledger = ledger(SeriesBudget::default());
        let project = [1u8; 16];
        for index in 0..5_000 {
            ledger
                .admit(
                    &project,
                    &counter(
                        "requests_total",
                        &[("request_id", &format!("r-{index}"))],
                        1.0,
                    ),
                    64,
                    10_000,
                )
                .expect("inside the default budget");
        }
        assert_eq!(ledger.pressure().active_series, 5_000);
        assert_eq!(ledger.pressure().active_bytes, 5_000 * 64);
        assert_eq!(ledger.pressure().series_refused, 0);
    }

    #[test]
    fn a_series_is_the_same_series_whatever_order_its_labels_arrive_in() {
        let a = counter("requests_total", &[("x", "1"), ("y", "2")], 1.0);
        let b = counter("requests_total", &[("y", "2"), ("x", "1")], 1.0);
        assert_eq!(series_key(&a), series_key(&b));
    }

    #[test]
    fn one_name_used_as_two_kinds_counts_as_two_series() {
        let mut gauge = counter("requests_total", &[], 1.0);
        gauge.metric_kind = MetricKind::Gauge;
        let counter = counter("requests_total", &[], 1.0);
        assert_ne!(series_key(&gauge), series_key(&counter));
    }

    #[test]
    fn two_delta_histograms_with_the_same_bounds_and_a_different_bucket_count_both_travel() {
        // The second point has one bucket more than its bounds allow. Adding
        // it bucket by bucket once read past the end of the first, which
        // stopped the worker that carried the batch.
        let items = vec![
            item(histogram(&[0.1, 0.5], &[1, 2], 0.4)),
            item(histogram(&[0.1, 0.5], &[1, 2, 3, 4], 0.9)),
        ];
        let (out, report) = merge(items, &SeriesBudget::default());
        assert_eq!(out.len(), 2, "a shape nobody observed is never produced");
        assert_eq!(report.merged, 0);

        // The shorter one second is the same mistake from the other side.
        let items = vec![
            item(histogram(&[0.1, 0.5], &[1, 2, 3], 0.4)),
            item(histogram(&[0.1, 0.5], &[1], 0.9)),
        ];
        let (out, _) = merge(items, &SeriesBudget::default());
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_bucket_count_that_does_not_fit_is_not_added() {
        let items = vec![
            item(histogram(&[0.1], &[u64::MAX], 0.4)),
            item(histogram(&[0.1], &[1], 0.9)),
        ];
        let (out, report) = merge(items, &SeriesBudget::default());
        assert_eq!(out.len(), 2, "a sum that wrapped would read as a reset");
        assert_eq!(report.merged, 0);
    }

    #[test]
    fn a_steady_metric_never_fills_its_byte_budget() {
        // One series, reporting for ever. The budget counts one reading of
        // each active series, so a metric that does not grow does not fill.
        let ledger = ledger(SeriesBudget {
            max_bytes_for_each_metric: 250,
            ..SeriesBudget::default()
        });
        let project = [1u8; 16];
        let steady = counter("requests_total", &[("route", "/a")], 1.0);
        for reading in 0..1_000 {
            ledger
                .admit(&project, &steady, 100, 10_000 + reading * 15_000)
                .unwrap();
        }
        assert_eq!(ledger.pressure().active_bytes, 100);

        // A second series still fits after all of those readings, which is the
        // new pod after a deploy.
        ledger
            .admit(
                &project,
                &counter("requests_total", &[("route", "/b")], 1.0),
                100,
                20_000_000,
            )
            .expect("a steady metric left room for a new series");
        assert_eq!(ledger.pressure().bytes_refused, 0);
    }

    #[test]
    fn a_series_that_grows_is_counted_at_its_new_size() {
        let ledger = ledger(SeriesBudget::default());
        let project = [1u8; 16];
        let point = counter("requests_total", &[], 1.0);
        ledger.admit(&project, &point, 100, 10_000).unwrap();
        ledger.admit(&project, &point, 140, 11_000).unwrap();
        assert_eq!(ledger.pressure().active_bytes, 140);
        ledger.admit(&project, &point, 90, 12_000).unwrap();
        assert_eq!(ledger.pressure().active_bytes, 90);
        assert_eq!(ledger.pressure().active_series, 1);
    }

    #[test]
    fn a_project_cannot_hold_more_metric_names_than_its_limit() {
        let ledger = ledger(SeriesBudget::default()).with_max_metric_names(2);
        let project = [1u8; 16];
        ledger
            .admit(&project, &counter("a_total", &[], 1.0), 10, 10_000)
            .unwrap();
        ledger
            .admit(&project, &counter("b_total", &[], 1.0), 10, 10_000)
            .unwrap();
        let refused = ledger
            .admit(&project, &counter("c_total", &[], 1.0), 10, 10_000)
            .expect_err("a third name is past the limit");
        assert!(refused
            .message
            .contains("metrics.maxMetricNamesForEachProject"));
        assert_eq!(ledger.pressure().names_refused, 1);
        assert_eq!(
            ledger.pressure().active_metrics,
            2,
            "a refused name takes no place"
        );

        // A name this project already holds is not a new name.
        ledger
            .admit(
                &project,
                &counter("a_total", &[("x", "1")], 1.0),
                10,
                10_000,
            )
            .unwrap();
        // Another project has its own limit.
        ledger
            .admit(&[2u8; 16], &counter("c_total", &[], 1.0), 10, 10_000)
            .unwrap();
    }

    #[test]
    fn the_idle_sweep_removes_stopped_series_and_empty_metrics_with_no_budget_full() {
        // Nothing here is near a budget. The earlier ledger only looked for
        // idle series when a budget was full, so these stayed until a restart.
        let ledger = ledger(SeriesBudget {
            idle_expiry_ms: 4_000,
            ..SeriesBudget::default()
        })
        .with_max_metric_names(2);
        let project = [1u8; 16];
        ledger
            .admit(&project, &counter("a_total", &[], 1.0), 10, 10_000)
            .unwrap();
        ledger
            .admit(&project, &counter("b_total", &[], 1.0), 30, 10_000)
            .unwrap();
        assert_eq!(ledger.pressure().active_bytes, 40);

        // `a_total` keeps reporting and `b_total` stops.
        ledger
            .admit(&project, &counter("a_total", &[], 1.0), 10, 13_000)
            .unwrap();
        ledger.sweep_if_due(15_000);
        let pressure = ledger.pressure();
        assert_eq!(pressure.active_series, 1);
        assert_eq!(pressure.active_bytes, 10);
        assert_eq!(pressure.active_metrics, 1, "an empty metric is removed");
        assert_eq!(pressure.expired_series, 1);

        // The name `b_total` held is free again.
        ledger
            .admit(&project, &counter("c_total", &[], 1.0), 10, 15_000)
            .expect("the stopped metric gave its name back");
    }

    #[test]
    fn the_idle_sweep_does_not_run_more_often_than_its_interval() {
        let ledger = ledger(SeriesBudget {
            idle_expiry_ms: 4_000,
            ..SeriesBudget::default()
        });
        let project = [1u8; 16];
        ledger
            .admit(&project, &counter("a_total", &[], 1.0), 10, 10_000)
            .unwrap();
        // The sweep ran at 10,000. The series is idle at 14,000 and the next
        // sweep is not due before 11,000, so these two calls show both sides.
        ledger.sweep_if_due(10_500);
        assert_eq!(ledger.pressure().active_series, 1);
        ledger.sweep_if_due(14_001);
        assert_eq!(ledger.pressure().active_series, 0);
    }
}
