//! Parquet export.
//!
//! `docs/STORAGE.md` section 13: Parquet export is a stable optional product
//! feature. The exporter reads native TallyOwl pages and performs a bounded
//! conversion, and DuckDB can query the resulting files directly.
//!
//! # Why this is a separate crate
//!
//! `AGENTS.md`: "Arrow and Parquet are optional export dependencies, not
//! dependencies of the always-on collector, storage, query, or dashboard path."
//! A minimal build never loads them, and D1 keeps the native format rather than
//! adopting Parquet as the storage contract. This crate is the only place those
//! libraries appear.
//!
//! # What an export is, and what it is not
//!
//! **Exports apply visible tombstones.** An export that carried erased rows out
//! of the installation would make the erasure meaningless the moment somebody
//! opened the file. The rows come from the store's own query path, which
//! filters them, so this cannot be forgotten at a call site.
//!
//! **An export is a copy, not a tier.** Cold data is still part of the logical
//! database; an exported file is not. STORAGE.md section 13 keeps those apart on
//! purpose, and nothing here removes anything.
//!
//! # The manifest
//!
//! Section 13 asks an export to carry schemas, policy, checksums, and the
//! deletion high-water mark. [`ExportManifest`] is that, written beside the
//! Parquet file, so somebody who finds the pair in a year knows what they hold
//! and what was already erased when it was made.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::builder::{
    BooleanBuilder, Float64Builder, Int64Builder, StringBuilder, UInt64Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

use tallyowl_store::row::{EventRow, PropertyValue};
use tallyowl_store::space::{Point, Space};
use tallyowl_store::{Store, StoreError, TimeBasis};

/// Room one row is assumed to want in the export.
///
/// D17 measured 39.75 bytes for one event in a native segment. Parquet with
/// Zstandard lands near that, and this rounds well above it so that the check
/// refuses early rather than late.
const ROW_BYTES_ESTIMATE: u64 = 128;

/// Why an export failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportError {
    pub message: String,
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExportError {}

impl From<StoreError> for ExportError {
    fn from(error: StoreError) -> ExportError {
        ExportError {
            message: error.to_string(),
        }
    }
}

fn failed(what: &str, error: impl std::fmt::Display) -> ExportError {
    ExportError {
        message: format!("The export could not {what}: {error}"),
    }
}

/// What one export produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportManifest {
    pub project_id: [u8; 16],
    pub range_start: i64,
    pub range_end: i64,
    pub basis: &'static str,
    pub rows: usize,
    /// Columns, in the order the file holds them.
    pub columns: Vec<String>,
    /// The tombstone generation the rows were filtered at. An export made at an
    /// older generation may hold rows a later erasure covers, and this is what
    /// says so.
    pub tombstone_generation: u64,
    /// The commit watermark the export applies to.
    pub commit_watermark: u64,
    pub file_bytes: u64,
    pub relative_path: String,
}

impl ExportManifest {
    /// The manifest as text, written beside the file.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("# TallyOwl export\n");
        out.push_str(&format!("project: {}\n", hex(&self.project_id)));
        out.push_str(&format!("range_start: {}\n", self.range_start));
        out.push_str(&format!("range_end: {}\n", self.range_end));
        out.push_str(&format!("basis: {}\n", self.basis));
        out.push_str(&format!("rows: {}\n", self.rows));
        out.push_str(&format!("columns: {}\n", self.columns.join(", ")));
        out.push_str(&format!(
            "tombstone_generation: {}\n",
            self.tombstone_generation
        ));
        out.push_str(&format!("commit_watermark: {}\n", self.commit_watermark));
        out.push_str(&format!("file_bytes: {}\n", self.file_bytes));
        out.push_str(&format!("file: {}\n", self.relative_path));
        out
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// How many envelope columns lead every export. The property columns follow.
const ENVELOPE_COLUMNS: usize = 12;

/// Which columns an export writes.
///
/// The envelope columns are fixed. A property becomes a column when the rows
/// hold it, which keeps a file readable without a schema registry: a column that
/// is there means the data was there.
fn schema_for(rows: &[EventRow]) -> Schema {
    let mut fields = vec![
        Field::new("event_id", DataType::Utf8, false),
        Field::new("batch_id", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("occurred_at", DataType::Int64, false),
        Field::new("received_at", DataType::Int64, false),
        Field::new("committed_at", DataType::Int64, false),
        Field::new("session_id", DataType::Utf8, true),
        Field::new("request_id", DataType::Utf8, true),
        Field::new("trace_id", DataType::Utf8, true),
        Field::new("service_name", DataType::Utf8, true),
        Field::new("release", DataType::Utf8, true),
    ];

    let mut properties: std::collections::BTreeMap<String, DataType> =
        std::collections::BTreeMap::new();
    for row in rows {
        for (key, (value, _)) in &row.properties {
            let kind = match value {
                PropertyValue::Boolean(_) => DataType::Boolean,
                PropertyValue::Integer(_) => DataType::Int64,
                PropertyValue::Unsigned(_) => DataType::UInt64,
                PropertyValue::Float(_) => DataType::Float64,
                // A decimal keeps its exact digits as text. Parquet's decimal
                // type takes a fixed precision and scale, and choosing one for
                // a value TallyOwl accepted at any scale would silently round
                // somebody's money. Text is lossless, and DuckDB casts it when
                // a query asks.
                _ => DataType::Utf8,
            };
            properties
                .entry(format!("p_{key}"))
                .and_modify(|held| {
                    // One name holding two types becomes text, which is the one
                    // representation that carries both. D20 keeps typed
                    // variants in storage; a flat file cannot.
                    if *held != kind {
                        *held = DataType::Utf8;
                    }
                })
                .or_insert(kind);
        }
    }

    for (name, kind) in properties {
        fields.push(Field::new(name, kind, true));
    }
    Schema::new(fields)
}

fn build_batch(schema: &Schema, rows: &[EventRow]) -> Result<RecordBatch, ExportError> {
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());

    for field in schema.fields() {
        let name = field.name().as_str();
        let array: ArrayRef = match name {
            "event_id" => text_column(rows, |row| Some(hex(&row.event_id))),
            "batch_id" => text_column(rows, |row| Some(hex(&row.batch_id))),
            "kind" => text_column(rows, |row| Some(row.kind.clone())),
            "name" => text_column(rows, |row| Some(row.name.clone())),
            "occurred_at" => number_column(rows, |row| row.occurred_at),
            "received_at" => number_column(rows, |row| row.received_at),
            "committed_at" => number_column(rows, |row| row.committed_at),
            "session_id" => text_column(rows, |row| row.session_id.clone()),
            "request_id" => text_column(rows, |row| row.request_id.clone()),
            "trace_id" => text_column(rows, |row| row.trace_id.map(|id| hex(&id))),
            "service_name" => text_column(rows, |row| row.service_name.clone()),
            "release" => text_column(rows, |row| row.release.clone()),
            other => {
                let key = other.strip_prefix("p_").unwrap_or(other).to_string();
                property_column(rows, &key, field.data_type())
            }
        };
        arrays.push(array);
    }

    RecordBatch::try_new(Arc::new(schema.clone()), arrays).map_err(|e| failed("be built", e))
}

fn text_column(rows: &[EventRow], pick: impl Fn(&EventRow) -> Option<String>) -> ArrayRef {
    let mut builder = StringBuilder::new();
    for row in rows {
        match pick(row) {
            Some(value) => builder.append_value(value),
            None => builder.append_null(),
        }
    }
    Arc::new(builder.finish())
}

fn number_column(rows: &[EventRow], pick: impl Fn(&EventRow) -> i64) -> ArrayRef {
    let mut builder = Int64Builder::new();
    for row in rows {
        builder.append_value(pick(row));
    }
    Arc::new(builder.finish())
}

fn property_column(rows: &[EventRow], key: &str, kind: &DataType) -> ArrayRef {
    fn held<'a>(row: &'a EventRow, key: &str) -> Option<&'a PropertyValue> {
        row.properties.get(key).map(|(value, _)| value)
    }

    match kind {
        DataType::Boolean => {
            let mut builder = BooleanBuilder::new();
            for row in rows {
                match held(row, key) {
                    Some(PropertyValue::Boolean(v)) => builder.append_value(*v),
                    _ => builder.append_null(),
                }
            }
            Arc::new(builder.finish())
        }
        DataType::Int64 => {
            let mut builder = Int64Builder::new();
            for row in rows {
                match held(row, key) {
                    Some(PropertyValue::Integer(v)) => builder.append_value(*v),
                    _ => builder.append_null(),
                }
            }
            Arc::new(builder.finish())
        }
        DataType::UInt64 => {
            let mut builder = UInt64Builder::new();
            for row in rows {
                match held(row, key) {
                    Some(PropertyValue::Unsigned(v)) => builder.append_value(*v),
                    _ => builder.append_null(),
                }
            }
            Arc::new(builder.finish())
        }
        DataType::Float64 => {
            let mut builder = Float64Builder::new();
            for row in rows {
                match held(row, key) {
                    Some(PropertyValue::Float(v)) => builder.append_value(*v),
                    _ => builder.append_null(),
                }
            }
            Arc::new(builder.finish())
        }
        _ => {
            let mut builder = StringBuilder::new();
            for row in rows {
                match held(row, key) {
                    Some(value) => builder.append_value(value.to_display()),
                    None => builder.append_null(),
                }
            }
            Arc::new(builder.finish())
        }
    }
}

/// What one export asks for.
///
/// A record rather than eight arguments. Two of them are timestamps and two are
/// byte counts, and a caller that transposed a pair would produce a plausible
/// wrong export rather than a compile error.
#[derive(Debug, Clone)]
pub struct ExportRequest {
    pub project_id: [u8; 16],
    pub range_start: i64,
    pub range_end: i64,
    pub basis: TimeBasis,
    /// Where the file goes.
    pub into: PathBuf,
    /// The tombstone generation the export applies, for its manifest.
    pub tombstone_generation: u64,
    /// `storage.reserveBytes`. An export never takes space from live data.
    pub reserve_bytes: u64,
}

/// The first width of one export window, and the narrowest and the widest.
///
/// **An export holds one window of rows at a time.** The store answers a scan
/// with every row of the range it was asked for, so the range an export asks
/// for is what bounds its memory. One hour is a guess; the width then follows
/// the data, halving after a window that held more than [`WINDOW_ROWS`] and
/// doubling after one that held few.
const FIRST_WINDOW_MS: i64 = 3_600_000;
const NARROWEST_WINDOW_MS: i64 = 60_000;
const WIDEST_WINDOW_MS: i64 = 86_400_000;

/// How many rows one window aims to stay under.
const WINDOW_ROWS: usize = 250_000;

/// How many windows one export may take.
///
/// At the widest window this is about 270 years. It exists so that a range of
/// `0..i64::MAX` is refused in words and does not loop until somebody notices.
const MOST_WINDOWS: u64 = 100_000;

/// Walk a range one window at a time, oldest first.
///
/// `visit` gets the rows of one window and answers how many it held, which is
/// what the next width follows.
fn each_window(
    store: &dyn Store,
    request: &ExportRequest,
    mut visit: impl FnMut(Vec<EventRow>) -> Result<(), ExportError>,
) -> Result<(), ExportError> {
    let too_wide = || ExportError {
        message: format!(
            "The export did not run. The range {} to {} is wider than one export can walk. \
             Export a shorter range, for example one year at a time.",
            request.range_start, request.range_end
        ),
    };
    // Refused from the arithmetic, before the first scan. The count inside the
    // loop covers a range that is legal here and dense enough to stay narrow.
    let span = request.range_end.saturating_sub(request.range_start);
    if span / WIDEST_WINDOW_MS > MOST_WINDOWS as i64 {
        return Err(too_wide());
    }
    let mut from = request.range_start;
    let mut width = FIRST_WINDOW_MS;
    let mut windows = 0u64;
    while from < request.range_end {
        windows += 1;
        if windows > MOST_WINDOWS {
            return Err(too_wide());
        }
        let to = from.saturating_add(width).min(request.range_end);
        let scanned = store.scan(request.project_id, from, to, request.basis)?;
        if scanned.incomplete {
            // An export is a compatibility contract: somebody reads the file
            // later and believes it holds the range it names. An export that
            // quietly dropped the rows it could not read would be a wrong
            // answer that outlives this process.
            return Err(ExportError {
                message: "The export did not run. Some of the stored data for this range could \
                          not be read, so the file would hold fewer rows than the range it \
                          names. Repair or restore the damaged segment first."
                    .to_string(),
            });
        }
        let held = scanned.rows.len();
        visit(scanned.rows)?;
        width = match held {
            held if held > WINDOW_ROWS => (width / 2).max(NARROWEST_WINDOW_MS),
            held if held < WINDOW_ROWS / 4 => width.saturating_mul(2).min(WIDEST_WINDOW_MS),
            _ => width,
        };
        from = to;
    }
    Ok(())
}

/// Widen `held` so that it can carry every column of `more`.
fn merge_schema(held: &mut std::collections::BTreeMap<String, DataType>, more: &Schema) {
    for field in more.fields().iter().skip(ENVELOPE_COLUMNS) {
        held.entry(field.name().clone())
            .and_modify(|kind| {
                if kind != field.data_type() {
                    *kind = DataType::Utf8;
                }
            })
            .or_insert_with(|| field.data_type().clone());
    }
}

/// Export one project's rows over one range to a Parquet file.
///
/// The rows come from the store's own query path, so visible tombstones are
/// already applied.
///
/// **Two passes, one window of rows in memory at a time.** Parquet wants its
/// columns before its first row, and a property is a column only when a row
/// holds it, so the first pass reads the range to learn the columns and the
/// second writes one row group for each window. Telemetry that lands between
/// the passes with a column the first pass did not see stops the export: a file
/// that dropped a column without saying so is the wrong answer this crate
/// exists to avoid.
///
/// **An export never replaces a file.** The destination is created new, and a
/// name that is taken is refused.
pub fn export_events(
    store: &dyn Store,
    request: &ExportRequest,
) -> Result<ExportManifest, ExportError> {
    let into: &Path = &request.into;
    if request.range_end <= request.range_start {
        return Err(ExportError {
            message: "The export did not run. It needs a time range whose end is after its start."
                .to_string(),
        });
    }

    // The first pass: the columns, and how many rows there are to make room for.
    let mut properties = std::collections::BTreeMap::new();
    let mut expected: u64 = 0;
    each_window(store, request, |rows| {
        expected += rows.len() as u64;
        merge_schema(&mut properties, &schema_for(&rows));
        Ok(())
    })?;
    let mut fields: Vec<Field> = schema_for(&[])
        .fields()
        .iter()
        .map(|f| (**f).clone())
        .collect();
    for (name, kind) in &properties {
        fields.push(Field::new(name, kind.clone(), true));
    }
    let schema = Schema::new(fields);
    let columns: Vec<String> = schema
        .fields()
        .iter()
        .map(|field| field.name().clone())
        .collect();

    if let Some(parent) = into.parent() {
        std::fs::create_dir_all(parent).map_err(|e| failed("create its directory", e))?;
    }

    // FAILURE_MODES.md section 10, export: fail the export, and never let an
    // export displace live data. An export is the one write here that nobody is
    // waiting on and that nothing depends on, so it is the first to give way.
    //
    // The estimate is the rows the first pass counted at a generous size.
    // Parquet is smaller than that in every measured case, so this refuses a
    // little early rather than filling the device and then failing.
    let space = Space::new(
        into.parent().unwrap_or_else(|| Path::new(".")),
        request.reserve_bytes,
    );
    space.check_bulk(Point::Export, expected * ROW_BYTES_ESTIMATE)?;

    let taken = |e: std::io::Error, what: &Path| match e.kind() {
        std::io::ErrorKind::AlreadyExists => ExportError {
            message: format!(
                "The export did not run. A file named {} is already there, and an export never \
                 replaces a file. Choose another name, or remove the old export first.",
                what.display()
            ),
        },
        _ => failed("create its file", e),
    };
    let manifest_path = manifest_path_for(into);
    if manifest_path.exists() {
        return Err(taken(
            std::io::Error::from(std::io::ErrorKind::AlreadyExists),
            &manifest_path,
        ));
    }
    let file = File::options()
        .write(true)
        .create_new(true)
        .open(into)
        .map_err(|e| taken(e, into))?;

    let written = write_windows(store, request, &schema, file);
    let rows = match written {
        Ok(rows) => rows,
        Err(e) => {
            // This call created the file, so it is this call's to remove. Half
            // an export left behind would read as a whole one.
            let _ = std::fs::remove_file(into);
            return Err(e);
        }
    };

    let file_bytes = std::fs::metadata(into)
        .map_err(|e| failed("be measured", e))?
        .len();

    let manifest = ExportManifest {
        project_id: request.project_id,
        range_start: request.range_start,
        range_end: request.range_end,
        basis: match request.basis {
            TimeBasis::OccurredAt => "occurred_at",
            TimeBasis::ReceivedAt => "received_at",
            TimeBasis::CommittedAt => "committed_at",
        },
        rows,
        columns,
        tombstone_generation: request.tombstone_generation,
        commit_watermark: store.commit_watermark(),
        file_bytes,
        relative_path: into
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
    };

    std::fs::write(&manifest_path, manifest.to_text())
        .map_err(|e| failed("write its description", e))?;

    Ok(manifest)
}

/// The second pass: one row group for each window that held rows.
fn write_windows(
    store: &dyn Store,
    request: &ExportRequest,
    schema: &Schema,
    file: File,
) -> Result<usize, ExportError> {
    let properties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .build();
    let mut writer = ArrowWriter::try_new(file, Arc::new(schema.clone()), Some(properties))
        .map_err(|e| failed("be started", e))?;

    let mut total = 0;
    each_window(store, request, |rows| {
        if rows.is_empty() {
            return Ok(());
        }
        check_fits(schema, &schema_for(&rows))?;
        writer
            .write(&build_batch(schema, &rows)?)
            .map_err(|e| failed("be written", e))?;
        // One window is one row group, so a reader can skip by time and this
        // writer holds no more than one window.
        writer.flush().map_err(|e| failed("be written", e))?;
        total += rows.len();
        Ok(())
    })?;
    writer.close().map_err(|e| failed("be finished", e))?;
    Ok(total)
}

/// Refuse a window the columns of the first pass cannot carry.
fn check_fits(fixed: &Schema, window: &Schema) -> Result<(), ExportError> {
    for field in window.fields().iter().skip(ENVELOPE_COLUMNS) {
        let fits = fixed.field_with_name(field.name()).is_ok_and(|held| {
            held.data_type() == field.data_type() || *held.data_type() == DataType::Utf8
        });
        if !fits {
            return Err(ExportError {
                message: format!(
                    "The export did not finish. New telemetry arrived while it ran, and the \
                     property `{}` changed. Run the export again.",
                    field.name().trim_start_matches("p_")
                ),
            });
        }
    }
    Ok(())
}

/// Where the description of an export lives, beside the file it describes.
pub fn manifest_path_for(parquet: &Path) -> PathBuf {
    parquet.with_extension("manifest.txt")
}
