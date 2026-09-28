//! Rollups: golden signals from spans, and metric downsampling.
//!
//! # Golden signals are metric series, not a new shape
//!
//! `docs/PLAN.md` Phase 6 asks for "service-operation golden signals derived
//! from spans where appropriate", and `docs/DATA_MODEL.md` lists
//! "service-operation latency and error rate histograms" under rollups.
//!
//! Both are answered by producing **ordinary metric points**. A golden signal
//! then costs no new storage shape, no new query operator, and no new
//! dashboard path: `rate`, `increase`, `histogram_merge`, and `quantile` all
//! read it, because it is the same kind of row a counter from an application
//! is. A separate signal table would have needed its own version of each.
//!
//! Three series come out of a span:
//!
//! | Series | Kind | Answers |
//! | --- | --- | --- |
//! | `tallyowl_service_operation_requests_total` | counter, delta | rate |
//! | `tallyowl_service_operation_errors_total` | counter, delta | errors |
//! | `tallyowl_service_operation_duration_seconds` | histogram, delta | duration |
//!
//! Saturation is the fourth golden signal and it is **not** here. Saturation is
//! a property of a resource rather than of a request, and a span says nothing
//! about how full a queue or a disk was. Deriving one from spans would be a
//! guess with a name that sounds measured.
//!
//! # Delta, not cumulative
//!
//! A rollup describes a window that has closed. It is a delta, and its
//! `start_at` and `end_at` are the window. A cumulative rollup would need this
//! module to remember every previous window, and a re-run over a repaired
//! segment would then disagree with itself.
//!
//! # Downsampling
//!
//! `docs/DATA_MODEL.md` section 6: "A downsample policy keeps one-minute
//! metrics, then hourly metrics. It removes the raw points." This module
//! produces the hourly points. **Removing the raw points is not done here**,
//! because removal is the deletion workflow with tombstones, and a rollup that
//! deleted its own inputs could not be rebuilt from retained raw data, which
//! `AGENTS.md` requires. See the implementation log.

use std::collections::BTreeMap;

use tallyowl_store::row::{EventRow, PropertyValue};

/// The bucket bounds a duration histogram uses, in seconds.
///
/// They run from a millisecond to thirty seconds, roughly two per decade. A
/// fixed layout is what makes two services' histograms merge: `histogram_merge`
/// refuses two layouts rather than rebucketing, so a rollup that chose bounds
/// per service would produce signals nobody could compare.
pub const DURATION_BOUNDS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// The marker every derived row carries. A ledger counting what an application
/// produced excludes them, and a query for TallyOwl's own view includes them.
pub const DERIVED: &str = "tallyowl-rollup";

pub const REQUESTS: &str = "tallyowl_service_operation_requests_total";
pub const ERRORS: &str = "tallyowl_service_operation_errors_total";
pub const DURATION: &str = "tallyowl_service_operation_duration_seconds";

/// One service operation inside one window.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    bucket: i64,
    service: String,
    operation: String,
}

#[derive(Debug, Default, Clone)]
struct Signals {
    requests: u64,
    errors: u64,
    /// Cumulative bucket counts, matching what the wire and the store mean by a
    /// bucket: `counts[i]` is every observation at or below `bounds[i]`.
    counts: Vec<u64>,
    sum: f64,
}

/// Derive golden-signal metric points from spans.
///
/// `bucket_ms` is the window each rollup covers. `docs/DATA_MODEL.md` names one
/// minute as the first resolution.
///
/// A row that is not a span contributes nothing, so a caller can hand this
/// everything it committed rather than filtering first.
pub fn golden_signals(rows: &[EventRow], bucket_ms: i64, project_id: [u8; 16]) -> Vec<EventRow> {
    if bucket_ms <= 0 {
        return Vec::new();
    }
    let mut windows: BTreeMap<Key, Signals> = BTreeMap::new();

    for row in rows {
        if row.kind != "span" {
            continue;
        }
        let Some(duration_ms) = number(row, "duration_ms") else {
            continue;
        };
        let key = Key {
            bucket: row.occurred_at - row.occurred_at.rem_euclid(bucket_ms),
            // A span with no service still gets a signal. Dropping it would
            // make a chart quietly miss the service nobody named, which is the
            // one most likely to be misconfigured.
            service: row.service_name.clone().unwrap_or_else(|| "unknown".into()),
            operation: text(row, "operation").unwrap_or_else(|| row.name.clone()),
        };
        let signals = windows.entry(key).or_insert_with(|| Signals {
            counts: vec![0; DURATION_BOUNDS.len()],
            ..Signals::default()
        });
        signals.requests += 1;
        if text(row, "status").as_deref() == Some("error") {
            signals.errors += 1;
        }
        let seconds = duration_ms / 1000.0;
        signals.sum += seconds;
        for (index, bound) in DURATION_BOUNDS.iter().enumerate() {
            if seconds <= *bound {
                signals.counts[index] += 1;
            }
        }
    }

    let mut out = Vec::new();
    for (key, signals) in windows {
        let end = key.bucket + bucket_ms;
        out.push(counter_row(
            REQUESTS,
            &key,
            end,
            signals.requests as f64,
            project_id,
        ));
        out.push(counter_row(
            ERRORS,
            &key,
            end,
            signals.errors as f64,
            project_id,
        ));
        out.push(histogram_row(&key, end, &signals, project_id));
    }
    out
}

/// Roll metric points up to a coarser resolution.
///
/// A counter's periods add, and a histogram's bucket counts add when the
/// layouts match. **Two layouts never merge**, for the same reason
/// `histogram_merge` refuses them: a merged shape nothing observed reads exactly
/// like a real one. A series whose layout changed inside the window keeps its
/// points at the finer resolution rather than being rewritten wrongly, and the
/// count of those is returned so a caller can report it.
///
/// A gauge is not rolled up. A gauge is an observation at a moment, and there
/// is no way to say which moment an hour of them stands for without choosing
/// one, so the raw points remain the answer.
pub fn downsample(rows: &[EventRow], bucket_ms: i64, project_id: [u8; 16]) -> Downsampled {
    if bucket_ms <= 0 {
        return Downsampled::default();
    }
    let mut windows: BTreeMap<(i64, String, String), Rolled> = BTreeMap::new();
    let mut skipped_gauges = 0u64;
    let mut skipped_layouts = 0u64;

    for row in rows {
        if row.kind != "metric-point" {
            continue;
        }
        // **A rollup is never rolled up again.** A rolled-up point is a delta
        // point whose time is the end of its window, which is the first instant
        // of the next window. The pass read it back an hour later beside that
        // window's finer points and added it in, so every total grew by the
        // total before it.
        if row.properties.contains_key("rollup_resolution_ms") {
            continue;
        }
        let kind = text(row, "metric_kind").unwrap_or_default();
        if kind == "gauge" {
            skipped_gauges += 1;
            continue;
        }
        // Only a delta rolls up by adding. A cumulative point is a level, and
        // adding two levels is meaningless; the finer points stay the answer.
        if text(row, "temporality").as_deref() != Some("delta") {
            continue;
        }
        let name = text(row, "metric_name").unwrap_or_else(|| row.name.clone());
        let series = text(row, "series_key").unwrap_or_default();
        let end_at = integer(row, "end_at").unwrap_or(row.occurred_at);
        let bucket = end_at - end_at.rem_euclid(bucket_ms);

        let entry = windows
            .entry((bucket, name.clone(), series.clone()))
            .or_insert_with(|| Rolled {
                labels: label_properties(row),
                ..Rolled::default()
            });

        match text(row, "histogram_bounds") {
            None => entry.value += number(row, "value").unwrap_or(0.0),
            Some(layout) => {
                let counts = parse_unsigned(&text(row, "histogram_counts").unwrap_or_default());
                match &entry.layout {
                    None => {
                        entry.layout = Some(layout);
                        entry.counts = counts;
                    }
                    Some(held) if *held == layout => {
                        for (index, value) in counts.iter().enumerate() {
                            if index < entry.counts.len() {
                                entry.counts[index] = entry.counts[index].saturating_add(*value);
                            }
                        }
                    }
                    Some(_) => {
                        // The layout changed inside the window. Not merged and
                        // not rebucketed.
                        entry.mixed_layouts = true;
                        skipped_layouts += 1;
                        continue;
                    }
                }
                entry.count += number(row, "histogram_count").unwrap_or(0.0) as u64;
                entry.sum += number(row, "histogram_sum").unwrap_or(0.0);
            }
        }
        entry.kind = kind;
        entry.unit = text(row, "unit");
        entry.rolled_up += 1;
    }

    let mut out = Vec::new();
    for ((bucket, name, _), rolled) in windows {
        if rolled.mixed_layouts {
            continue;
        }
        out.push(rolled.into_row(&name, bucket, bucket + bucket_ms, project_id));
    }
    Downsampled {
        rows: out,
        skipped_gauges,
        skipped_layouts,
    }
}

/// What one downsample pass produced, and what it left alone.
#[derive(Debug, Default, Clone)]
pub struct Downsampled {
    pub rows: Vec<EventRow>,
    /// Gauges, which have no coarser form.
    pub skipped_gauges: u64,
    /// Histogram points whose bucket layout changed inside the window.
    pub skipped_layouts: u64,
}

/// What the downsample pass runs with.
#[derive(Debug, Clone, Copy)]
pub struct DownsampleSettings {
    /// The width of one rolled-up window.
    pub resolution_ms: i64,
    /// `retention.detailed`. A window whose finer points have expired is not
    /// rolled up, because there is nothing left to read. Zero keeps everything.
    pub detailed_ms: i64,
    /// How long after a window closes the pass waits before it rolls it up, so
    /// a point that arrives a little late is inside its window's rollup.
    pub lateness_grace_ms: i64,
}

/// What one run of the pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DownsampleReport {
    pub windows: u64,
    pub points: u64,
    pub skipped_layouts: u64,
    /// One sentence for each window that did not roll up. The pass stops that
    /// project at that window and tries it again on the next run.
    pub failures: Vec<String>,
}

/// The most windows one run rolls up for one project, so a head that was down
/// for a week catches up over several runs and not inside one.
pub const MAX_WINDOWS_EACH_RUN: u64 = 48;

/// Roll up every window that has closed since the last run, for every project.
///
/// **The pass follows a durable watermark and not the clock.** The earlier pass
/// slept one resolution, worked, and rolled up only the window that had just
/// closed. Its period was the resolution plus the work, so the phase drifted
/// and it skipped a window every so often; a restart across a boundary, or any
/// downtime, skipped one for good. After `retention.detailed` the rollup is the
/// only copy, so each skipped window became a permanent hole in a chart and
/// nothing said so. The watermark is the end of the last window rolled up, the
/// catalog holds it, and a run rolls every closed window after it.
///
/// A project with no watermark starts at the newest closed window. Rolling its
/// whole history on the first run after an upgrade would read every retained
/// row of every project in one pass.
pub fn downsample_due(
    store: &tallyowl_store::SegmentedStore,
    settings: DownsampleSettings,
    now: i64,
) -> DownsampleReport {
    use tallyowl_store::Store;

    let mut report = DownsampleReport::default();
    let resolution = settings.resolution_ms;
    if resolution <= 0 {
        return report;
    }
    // The newest moment a window may end at and still be rolled up now.
    let closed_before = now - settings.lateness_grace_ms.max(0);
    let newest_end = closed_before - closed_before.rem_euclid(resolution);

    let projects = match store.catalog().projects() {
        Ok(projects) => projects,
        Err(e) => {
            report
                .failures
                .push(format!("The projects could not be listed. {e}"));
            return report;
        }
    };
    for project in projects {
        let project_id = project.project_id;
        let mut through = match store.catalog().rolled_up_through(project_id) {
            Ok(Some(through)) => through,
            Ok(None) => newest_end - resolution,
            Err(e) => {
                report.failures.push(format!(
                    "The downsample watermark of a project could not be read. {e}"
                ));
                continue;
            }
        };
        // Nothing older than the detailed retention is worth rolling up,
        // because the finer points it would read have already expired.
        if settings.detailed_ms > 0 {
            let oldest = now - settings.detailed_ms;
            let oldest_start = oldest - oldest.rem_euclid(resolution) + resolution;
            through = through.max(oldest_start);
        }

        let mut rolled = 0;
        while through + resolution <= newest_end && rolled < MAX_WINDOWS_EACH_RUN {
            let (start, end) = (through, through + resolution);
            let scanned = match store.scan(
                project_id,
                start,
                end,
                tallyowl_store::store::TimeBasis::OccurredAt,
            ) {
                Ok(scanned) => scanned,
                Err(e) => {
                    report
                        .failures
                        .push(format!("A window could not be read for its rollup. {e}"));
                    break;
                }
            };
            // A rollup over part of a window is a smaller number that looks
            // like the whole one. The window waits until all of it reads.
            if scanned.incomplete {
                report.failures.push(
                    "Part of a window could not be read, so its rollup waits rather than recording a smaller number."
                        .to_string(),
                );
                break;
            }
            let out = downsample(&scanned.rows, resolution, project_id);
            report.skipped_layouts += out.skipped_layouts;
            if !out.rows.is_empty() {
                let count = out.rows.len() as u64;
                // The batch identifier is derived from the window, so a pass
                // that runs twice over one window deduplicates to one rollup.
                let batch_id = downsample_batch_id(project_id, end);
                if let Err(e) = store.commit([0; 16], batch_id, out.rows) {
                    report
                        .failures
                        .push(format!("A rollup could not be committed. {e}"));
                    break;
                }
                report.points += count;
            }
            // After the commit. A stop between the two rolls the window again,
            // and the derived batch identifier makes that one rollup.
            if let Err(e) = store.catalog().mark_rolled_up_through(project_id, end) {
                report
                    .failures
                    .push(format!("The downsample watermark could not be stored. {e}"));
                break;
            }
            through = end;
            rolled += 1;
            report.windows += 1;
        }
    }
    report
}

/// A batch identifier derived from the project and the window, so a pass that
/// runs twice deduplicates to one rollup rather than doubling it.
pub fn downsample_batch_id(project_id: [u8; 16], window_end: i64) -> [u8; 16] {
    let mut input = Vec::with_capacity(32);
    input.extend_from_slice(b"downsample");
    input.extend_from_slice(&project_id);
    input.extend_from_slice(&window_end.to_le_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&blake3::hash(&input).as_bytes()[..16]);
    out
}

#[derive(Debug, Default, Clone)]
struct Rolled {
    kind: String,
    unit: Option<String>,
    labels: Vec<(String, PropertyValue)>,
    value: f64,
    layout: Option<String>,
    counts: Vec<u64>,
    count: u64,
    sum: f64,
    mixed_layouts: bool,
    rolled_up: u64,
}

impl Rolled {
    fn into_row(self, name: &str, start_at: i64, end_at: i64, project_id: [u8; 16]) -> EventRow {
        let mut row = metric_row(name, end_at, project_id, &self.kind);
        row = row
            .with_property("start_at", PropertyValue::Integer(start_at), "collector")
            .with_property("end_at", PropertyValue::Integer(end_at), "collector")
            // A rollup says what it is, so a chart can show the resolution and
            // the approximation. DATA_MODEL.md section 6 requires that.
            .with_property(
                "rollup_resolution_ms",
                PropertyValue::Integer(end_at - start_at),
                "collector",
            )
            .with_property(
                "rollup_points",
                PropertyValue::Unsigned(self.rolled_up),
                "collector",
            );
        if let Some(unit) = self.unit {
            row = row.with_property("unit", PropertyValue::Text(unit), "collector");
        }
        for (key, value) in self.labels {
            row.properties.insert(key, (value, "client".to_string()));
        }
        match self.layout {
            None => {
                row = row.with_property("value", PropertyValue::Float(self.value), "collector");
            }
            Some(layout) => {
                row = row
                    .with_property("histogram_bounds", PropertyValue::Text(layout), "collector")
                    .with_property(
                        "histogram_counts",
                        PropertyValue::Text(join_unsigned(&self.counts)),
                        "collector",
                    )
                    .with_property(
                        "histogram_count",
                        PropertyValue::Unsigned(self.count),
                        "collector",
                    )
                    .with_property("histogram_sum", PropertyValue::Float(self.sum), "collector");
            }
        }
        // The series key is recomputed from what the rolled row carries, so a
        // rate over rolled points groups exactly as it does over raw ones.
        let key = derived_series_key(&row);
        row.properties.insert(
            "series_key".to_string(),
            (PropertyValue::Text(key), "collector".to_string()),
        );
        row
    }
}

// ---------------------------------------------------------------------------
// Row building
// ---------------------------------------------------------------------------

fn counter_row(name: &str, key: &Key, end_at: i64, value: f64, project_id: [u8; 16]) -> EventRow {
    let mut row = metric_row(name, end_at, project_id, "counter");
    row = row
        .with_property("monotonic", PropertyValue::Boolean(true), "collector")
        .with_property("value", PropertyValue::Float(value), "collector")
        .with_property("start_at", PropertyValue::Integer(key.bucket), "collector")
        .with_property("end_at", PropertyValue::Integer(end_at), "collector")
        .with_property(
            "service",
            PropertyValue::Text(key.service.clone()),
            "collector",
        )
        .with_property(
            "operation",
            PropertyValue::Text(key.operation.clone()),
            "collector",
        );
    row.service_name = Some(key.service.clone());
    let series = derived_series_key(&row);
    row.properties.insert(
        "series_key".to_string(),
        (PropertyValue::Text(series), "collector".to_string()),
    );
    row
}

fn histogram_row(key: &Key, end_at: i64, signals: &Signals, project_id: [u8; 16]) -> EventRow {
    let mut row = metric_row(DURATION, end_at, project_id, "histogram");
    row = row
        .with_property("monotonic", PropertyValue::Boolean(false), "collector")
        .with_property("unit", PropertyValue::Text("s".into()), "collector")
        .with_property("start_at", PropertyValue::Integer(key.bucket), "collector")
        .with_property("end_at", PropertyValue::Integer(end_at), "collector")
        .with_property(
            "histogram_bounds",
            PropertyValue::Text(join_floats(DURATION_BOUNDS)),
            "collector",
        )
        .with_property(
            "histogram_counts",
            PropertyValue::Text(join_unsigned(&signals.counts)),
            "collector",
        )
        .with_property(
            "histogram_count",
            PropertyValue::Unsigned(signals.requests),
            "collector",
        )
        .with_property(
            "histogram_sum",
            PropertyValue::Float(signals.sum),
            "collector",
        )
        .with_property(
            "service",
            PropertyValue::Text(key.service.clone()),
            "collector",
        )
        .with_property(
            "operation",
            PropertyValue::Text(key.operation.clone()),
            "collector",
        );
    row.service_name = Some(key.service.clone());
    let series = derived_series_key(&row);
    row.properties.insert(
        "series_key".to_string(),
        (PropertyValue::Text(series), "collector".to_string()),
    );
    row
}

fn metric_row(name: &str, occurred_at: i64, project_id: [u8; 16], kind: &str) -> EventRow {
    let mut row = EventRow::new(
        derived_id(name, occurred_at),
        "metric-point",
        name,
        occurred_at,
    );
    row.project_id = project_id;
    row.received_at = occurred_at;
    row.with_property("metric_name", PropertyValue::Text(name.into()), "collector")
        .with_property("metric_kind", PropertyValue::Text(kind.into()), "collector")
        // A derived row says so. Nothing produced it: TallyOwl computed it from
        // rows that are still there, so a count of what an application sent
        // must be able to leave it out.
        .with_property("derived", PropertyValue::Text(DERIVED.into()), "collector")
        // A rollup is a delta: it describes a window that closed. See the
        // module note.
        .with_property(
            "temporality",
            PropertyValue::Text("delta".into()),
            "collector",
        )
}

/// A stable identifier for a derived row.
///
/// It is derived from the name and the window rather than generated, so a
/// rollup that runs twice over the same window commits the same identifier and
/// deduplicates to one logical row. A random identifier would double every
/// signal on a retry.
fn derived_id(name: &str, occurred_at: i64) -> [u8; 16] {
    let mut id = [0u8; 16];
    let hash = blake3::hash(format!("{name}\u{1}{occurred_at}").as_bytes());
    id.copy_from_slice(&hash.as_bytes()[..16]);
    id
}

/// The series identity of a derived row, from the labels it carries.
fn derived_series_key(row: &EventRow) -> String {
    let kind = text(row, "metric_kind").unwrap_or_default();
    let mut key = kind;
    for (name, value) in label_properties(row) {
        key.push('\u{1}');
        key.push_str(&name);
        key.push('\u{2}');
        key.push_str(&display(&value));
    }
    key
}

/// The properties of a metric row that are labels rather than the point's own
/// fields.
fn label_properties(row: &EventRow) -> Vec<(String, PropertyValue)> {
    row.properties
        .iter()
        .filter(|(key, _)| !crate::project::METRIC_FIELDS.contains(&key.as_str()))
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "rollup_resolution_ms" | "rollup_points" | "derived"
            )
        })
        .map(|(key, (value, _))| (key.clone(), value.clone()))
        .collect()
}

fn display(value: &PropertyValue) -> String {
    match value {
        PropertyValue::Text(text) => text.clone(),
        PropertyValue::Integer(v) => v.to_string(),
        PropertyValue::Unsigned(v) => v.to_string(),
        PropertyValue::Float(v) => crate::project::format_float(*v),
        PropertyValue::Boolean(v) => v.to_string(),
        PropertyValue::Decimal(v) => v.clone(),
        PropertyValue::Bytes(v) => tallyowl_store::row::hex(v),
        PropertyValue::Null => String::new(),
    }
}

fn text(row: &EventRow, key: &str) -> Option<String> {
    match row.properties.get(key) {
        Some((PropertyValue::Text(value), _)) => Some(value.clone()),
        _ => None,
    }
}

fn integer(row: &EventRow, key: &str) -> Option<i64> {
    match row.properties.get(key) {
        Some((PropertyValue::Integer(value), _)) => Some(*value),
        Some((PropertyValue::Unsigned(value), _)) => Some(*value as i64),
        _ => None,
    }
}

fn number(row: &EventRow, key: &str) -> Option<f64> {
    match row.properties.get(key)? {
        (PropertyValue::Float(value), _) => Some(*value),
        (PropertyValue::Integer(value), _) => Some(*value as f64),
        (PropertyValue::Unsigned(value), _) => Some(*value as f64),
        _ => None,
    }
}

fn join_floats(values: &[f64]) -> String {
    values
        .iter()
        .map(|v| crate::project::format_float(*v))
        .collect::<Vec<String>>()
        .join(",")
}

fn join_unsigned(values: &[u64]) -> String {
    values
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<String>>()
        .join(",")
}

fn parse_unsigned(text: &str) -> Vec<u64> {
    text.split(',')
        .filter(|part| !part.is_empty())
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: [u8; 16] = [9; 16];
    const BASE: i64 = 1_785_628_800_000;

    fn span(
        id: u8,
        service: &str,
        operation: &str,
        at: i64,
        duration_ms: f64,
        error: bool,
    ) -> EventRow {
        let mut row = EventRow::new([id; 16], "span", operation, at);
        row.project_id = PROJECT;
        row.service_name = Some(service.to_string());
        row.with_property("operation", PropertyValue::Text(operation.into()), "client")
            .with_property("duration_ms", PropertyValue::Float(duration_ms), "client")
            .with_property(
                "status",
                PropertyValue::Text(if error { "error".into() } else { "ok".into() }),
                "client",
            )
    }

    fn find<'a>(rows: &'a [EventRow], name: &str) -> &'a EventRow {
        rows.iter()
            .find(|row| row.name == name)
            .expect("the rollup holds this series")
    }

    #[test]
    fn spans_become_a_request_count_an_error_count_and_a_duration_histogram() {
        let rows = vec![
            span(1, "checkout", "GET /cart", BASE, 20.0, false),
            span(2, "checkout", "GET /cart", BASE + 100, 40.0, false),
            span(3, "checkout", "GET /cart", BASE + 200, 300.0, true),
        ];
        let out = golden_signals(&rows, 60_000, PROJECT);
        assert_eq!(out.len(), 3, "three series for one operation in one window");

        assert_eq!(number(find(&out, REQUESTS), "value"), Some(3.0));
        assert_eq!(number(find(&out, ERRORS), "value"), Some(1.0));

        let duration = find(&out, DURATION);
        assert_eq!(
            text(duration, "histogram_bounds").unwrap(),
            join_floats(DURATION_BOUNDS)
        );
        assert_eq!(number(duration, "histogram_count"), Some(3.0));
        // 0.02 + 0.04 + 0.3 seconds.
        assert!((number(duration, "histogram_sum").unwrap() - 0.36).abs() < 1e-9);
        let counts = parse_unsigned(&text(duration, "histogram_counts").unwrap());
        // One at or below 0.025, two at or below 0.05, all three at or below 0.5.
        assert_eq!(counts[3], 1);
        assert_eq!(counts[4], 2);
        assert_eq!(counts[7], 3);
    }

    #[test]
    fn two_operations_of_one_service_stay_apart() {
        let rows = vec![
            span(1, "checkout", "GET /cart", BASE, 10.0, false),
            span(2, "checkout", "POST /order", BASE, 10.0, false),
        ];
        let out = golden_signals(&rows, 60_000, PROJECT);
        assert_eq!(out.len(), 6);
        let operations: Vec<String> = out
            .iter()
            .filter(|row| row.name == REQUESTS)
            .filter_map(|row| text(row, "operation"))
            .collect();
        assert!(operations.contains(&"GET /cart".to_string()));
        assert!(operations.contains(&"POST /order".to_string()));
    }

    #[test]
    fn two_windows_produce_two_rollups() {
        let rows = vec![
            span(1, "checkout", "GET /cart", BASE, 10.0, false),
            span(2, "checkout", "GET /cart", BASE + 120_000, 10.0, false),
        ];
        let out = golden_signals(&rows, 60_000, PROJECT);
        let requests: Vec<&EventRow> = out.iter().filter(|row| row.name == REQUESTS).collect();
        assert_eq!(requests.len(), 2);
        assert_ne!(
            integer(requests[0], "start_at"),
            integer(requests[1], "start_at")
        );
    }

    #[test]
    fn a_rollup_is_a_delta_over_its_own_window() {
        let out = golden_signals(
            &[span(1, "checkout", "GET /cart", BASE + 5_000, 10.0, false)],
            60_000,
            PROJECT,
        );
        let requests = find(&out, REQUESTS);
        assert_eq!(text(requests, "temporality").as_deref(), Some("delta"));
        assert_eq!(integer(requests, "start_at"), Some(BASE));
        assert_eq!(integer(requests, "end_at"), Some(BASE + 60_000));
    }

    #[test]
    fn running_the_same_window_twice_gives_the_same_identifiers() {
        // A rollup that ran twice must deduplicate to one logical row rather
        // than doubling every signal.
        let rows = vec![span(1, "checkout", "GET /cart", BASE, 10.0, false)];
        let first = golden_signals(&rows, 60_000, PROJECT);
        let second = golden_signals(&rows, 60_000, PROJECT);
        assert_eq!(first[0].event_id, second[0].event_id);
    }

    #[test]
    fn a_span_with_no_service_still_produces_a_signal() {
        // The service nobody named is the one most likely to be misconfigured.
        let mut row = span(1, "x", "GET /cart", BASE, 10.0, false);
        row.service_name = None;
        let out = golden_signals(&[row], 60_000, PROJECT);
        assert_eq!(
            text(find(&out, REQUESTS), "service").as_deref(),
            Some("unknown")
        );
    }

    #[test]
    fn a_row_that_is_not_a_span_contributes_nothing() {
        let mut event = EventRow::new([1; 16], "event", "signed-up", BASE);
        event.project_id = PROJECT;
        assert!(golden_signals(&[event], 60_000, PROJECT).is_empty());
    }

    #[test]
    fn saturation_is_not_derived_from_a_span() {
        // A span says nothing about how full a queue was, and a signal named
        // "saturation" that guessed would read as measured.
        let out = golden_signals(
            &[span(1, "checkout", "GET /cart", BASE, 10.0, false)],
            60_000,
            PROJECT,
        );
        assert!(!out.iter().any(|row| row.name.contains("saturation")));
    }

    // ---- downsampling ------------------------------------------------------

    fn delta_counter(id: u8, at: i64, route: &str, value: f64) -> EventRow {
        let mut row = EventRow::new([id; 16], "metric-point", "requests_total", at);
        row.project_id = PROJECT;
        row.with_property(
            "metric_name",
            PropertyValue::Text("requests_total".into()),
            "client",
        )
        .with_property(
            "metric_kind",
            PropertyValue::Text("counter".into()),
            "client",
        )
        .with_property("temporality", PropertyValue::Text("delta".into()), "client")
        .with_property(
            "series_key",
            PropertyValue::Text(format!("counter\u{1}route\u{2}{route}")),
            "client",
        )
        .with_property("route", PropertyValue::Text(route.into()), "client")
        .with_property("start_at", PropertyValue::Integer(at - 60_000), "client")
        .with_property("end_at", PropertyValue::Integer(at), "client")
        .with_property("value", PropertyValue::Float(value), "client")
    }

    #[test]
    fn a_minute_of_deltas_becomes_one_hourly_point() {
        let rows: Vec<EventRow> = (0..10)
            .map(|n| delta_counter(n as u8, BASE + n * 60_000, "/a", 2.0))
            .collect();
        let out = downsample(&rows, 3_600_000, PROJECT);
        assert_eq!(out.rows.len(), 1);
        assert_eq!(number(&out.rows[0], "value"), Some(20.0));
        // The row says its resolution, so a chart can show the approximation.
        assert_eq!(
            integer(&out.rows[0], "rollup_resolution_ms"),
            Some(3_600_000)
        );
        assert_eq!(number(&out.rows[0], "rollup_points"), Some(10.0));
    }

    #[test]
    fn two_series_downsample_apart() {
        let rows = vec![
            delta_counter(1, BASE, "/a", 2.0),
            delta_counter(2, BASE + 60_000, "/b", 5.0),
            delta_counter(3, BASE + 120_000, "/a", 3.0),
        ];
        let out = downsample(&rows, 3_600_000, PROJECT);
        assert_eq!(out.rows.len(), 2);
        let values: Vec<Option<f64>> = out.rows.iter().map(|row| number(row, "value")).collect();
        assert!(values.contains(&Some(5.0)) && values.contains(&Some(5.0)));
    }

    #[test]
    fn a_cumulative_point_is_left_alone_because_two_levels_do_not_add() {
        let mut row = delta_counter(1, BASE, "/a", 5.0);
        row.properties.insert(
            "temporality".to_string(),
            (
                PropertyValue::Text("cumulative".into()),
                "client".to_string(),
            ),
        );
        assert!(downsample(&[row], 3_600_000, PROJECT).rows.is_empty());
    }

    #[test]
    fn a_gauge_is_left_alone_and_counted() {
        let mut row = delta_counter(1, BASE, "/a", 5.0);
        row.properties.insert(
            "metric_kind".to_string(),
            (PropertyValue::Text("gauge".into()), "client".to_string()),
        );
        let out = downsample(&[row], 3_600_000, PROJECT);
        assert!(out.rows.is_empty());
        assert_eq!(out.skipped_gauges, 1);
    }

    #[test]
    fn a_histogram_downsamples_bucket_by_bucket() {
        let rows: Vec<EventRow> = (0..2)
            .map(|n| {
                let mut row = delta_counter(n as u8, BASE + n * 60_000, "/a", 0.0);
                row.properties.remove("value");
                row.properties.insert(
                    "histogram_bounds".to_string(),
                    (PropertyValue::Text("1,5".into()), "client".to_string()),
                );
                row.properties.insert(
                    "histogram_counts".to_string(),
                    (PropertyValue::Text("1,3".into()), "client".to_string()),
                );
                row.properties.insert(
                    "histogram_count".to_string(),
                    (PropertyValue::Unsigned(3), "client".to_string()),
                );
                row.properties.insert(
                    "histogram_sum".to_string(),
                    (PropertyValue::Float(4.0), "client".to_string()),
                );
                row
            })
            .collect();
        let out = downsample(&rows, 3_600_000, PROJECT);
        assert_eq!(out.rows.len(), 1);
        assert_eq!(
            text(&out.rows[0], "histogram_counts").as_deref(),
            Some("2,6")
        );
        assert_eq!(number(&out.rows[0], "histogram_count"), Some(6.0));
        assert_eq!(number(&out.rows[0], "histogram_sum"), Some(8.0));
    }

    #[test]
    fn a_bucket_layout_that_changed_inside_the_window_is_left_at_the_finer_resolution() {
        // A merged shape nothing observed reads exactly like a real one, so the
        // rollup refuses rather than rebucketing. The finer points remain the
        // answer.
        let mut first = delta_counter(1, BASE, "/a", 0.0);
        first.properties.remove("value");
        first.properties.insert(
            "histogram_bounds".to_string(),
            (PropertyValue::Text("1,5".into()), "client".to_string()),
        );
        first.properties.insert(
            "histogram_counts".to_string(),
            (PropertyValue::Text("1,3".into()), "client".to_string()),
        );
        let mut second = first.clone();
        second.event_id = [2; 16];
        second.properties.insert(
            "histogram_bounds".to_string(),
            (PropertyValue::Text("2,8".into()), "client".to_string()),
        );

        let out = downsample(&[first, second], 3_600_000, PROJECT);
        assert!(out.rows.is_empty());
        assert_eq!(out.skipped_layouts, 1);
    }

    #[test]
    fn a_downsampled_row_keeps_a_series_key_that_a_rate_can_group_by() {
        let rows = vec![delta_counter(1, BASE, "/a", 2.0)];
        let out = downsample(&rows, 3_600_000, PROJECT);
        let key = text(&out.rows[0], "series_key").expect("a series key");
        assert!(key.contains("route"), "{key}");
        assert!(key.contains("/a"), "{key}");
    }

    #[test]
    fn a_downsample_of_nothing_produces_nothing() {
        assert!(downsample(&[], 3_600_000, PROJECT).rows.is_empty());
    }

    // -----------------------------------------------------------------------
    // The pass that follows a watermark
    // -----------------------------------------------------------------------

    const HOUR: i64 = 3_600_000;

    fn directory(name: &str) -> std::path::PathBuf {
        let base = std::env::var("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target"));
        let path = base
            .join("head-tests")
            .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    fn settings() -> DownsampleSettings {
        DownsampleSettings {
            resolution_ms: HOUR,
            detailed_ms: 0,
            lateness_grace_ms: 5 * 60_000,
        }
    }

    fn store_with_project(path: &std::path::Path) -> tallyowl_store::SegmentedStore {
        let store = tallyowl_store::SegmentedStore::open(path).expect("the store opens");
        store
            .catalog()
            .put_project(&tallyowl_store::control::Project {
                project_id: PROJECT,
                workspace_id: [8; 16],
                name: "shop".into(),
                description: None,
                created_at: BASE,
            })
            .expect("the project is stored");
        store
    }

    #[test]
    fn every_window_that_closed_while_the_pass_was_away_is_rolled_up() {
        use tallyowl_store::Store;
        // BASE is on an hour boundary. The clock is the `now` argument.
        assert_eq!(BASE.rem_euclid(HOUR), 0);
        let path = directory("downsample-watermark");
        let store = store_with_project(&path);
        store
            .commit(
                [7; 16],
                [1; 16],
                vec![
                    delta_counter(1, BASE + 60_000, "/cart", 2.0),
                    delta_counter(2, BASE + HOUR + 60_000, "/cart", 3.0),
                    delta_counter(3, BASE + 3 * HOUR + 60_000, "/cart", 5.0),
                ],
            )
            .expect("the points commit");

        // The first run rolls the newest closed window and no history.
        let first = downsample_due(&store, settings(), BASE + HOUR + 10 * 60_000);
        assert_eq!((first.windows, first.points), (1, 1), "{first:?}");

        // Inside the grace, the window that just closed waits.
        let early = downsample_due(&store, settings(), BASE + 2 * HOUR + 60_000);
        assert_eq!(early.windows, 0, "{early:?}");

        // The head stops, and starts again three hours later. The old pass
        // rolled only the window that had just closed, so the second and the
        // fourth hour were holes once the detailed points expired.
        drop(store);
        let store = tallyowl_store::SegmentedStore::open(&path).expect("the store opens again");
        let later = downsample_due(&store, settings(), BASE + 4 * HOUR + 10 * 60_000);
        assert_eq!(later.windows, 3, "{later:?}");
        assert_eq!(later.points, 2, "the second and the fourth hour");
        assert!(later.failures.is_empty(), "{later:?}");

        // A run that comes again at once has nothing to do.
        let again = downsample_due(&store, settings(), BASE + 4 * HOUR + 10 * 60_000);
        assert_eq!(again.windows, 0);
        assert_eq!(
            store.catalog().rolled_up_through(PROJECT).unwrap(),
            Some(BASE + 4 * HOUR)
        );
    }

    #[test]
    fn a_rollup_is_not_rolled_up_again_with_the_next_window() {
        // The rolled-up point of one hour sits on the first instant of the next.
        let first = downsample(
            &[delta_counter(1, BASE + 60_000, "/cart", 2.0)],
            HOUR,
            PROJECT,
        );
        assert_eq!(first.rows.len(), 1);
        assert_eq!(first.rows[0].occurred_at, BASE + HOUR);

        let mut next_window = first.rows.clone();
        next_window.push(delta_counter(2, BASE + HOUR + 60_000, "/cart", 3.0));
        let second = downsample(&next_window, HOUR, PROJECT);
        assert_eq!(second.rows.len(), 1);
        assert_eq!(
            number(&second.rows[0], "value"),
            Some(3.0),
            "the first hour was added in"
        );
    }

    #[test]
    fn a_long_absence_catches_up_over_several_runs_and_skips_what_has_expired() {
        let path = directory("downsample-catch-up");
        let store = store_with_project(&path);
        store
            .catalog()
            .mark_rolled_up_through(PROJECT, BASE)
            .expect("a watermark from before the absence");

        let now = BASE + 1_000 * HOUR + 10 * 60_000;
        let run = downsample_due(&store, settings(), now);
        assert_eq!(run.windows, MAX_WINDOWS_EACH_RUN);

        // With a detailed retention of one day, a window whose finer points
        // are gone is not read at all.
        let bounded = DownsampleSettings {
            detailed_ms: 24 * HOUR,
            ..settings()
        };
        let run = downsample_due(&store, bounded, now);
        assert_eq!(run.windows, 23, "{run:?}");
        assert_eq!(
            store.catalog().rolled_up_through(PROJECT).unwrap(),
            Some(BASE + 1_000 * HOUR)
        );
    }
}
