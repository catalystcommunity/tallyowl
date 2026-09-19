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

/// Export one project's rows over one range to a Parquet file.
///
/// The rows come from the store's own query path, so visible tombstones are
/// already applied.
pub fn export_events(
    store: &dyn Store,
    request: &ExportRequest,
) -> Result<ExportManifest, ExportError> {
    let ExportRequest {
        project_id,
        range_start,
        range_end,
        basis,
        into,
        tombstone_generation,
        reserve_bytes,
    } = request;
    let (project_id, tombstone_generation) = (*project_id, *tombstone_generation);
    let (range_start, range_end, basis) = (*range_start, *range_end, *basis);
    let into: &Path = into;
    let scanned = store.scan(project_id, range_start, range_end, basis)?;
    if scanned.incomplete {
        // An export is a compatibility contract: somebody reads the file later
        // and believes it holds the range it names. An export that quietly
        // dropped the rows it could not read would be a wrong answer that
        // outlives this process.
        return Err(ExportError {
            message: "The export did not run. Some of the stored data for this range could not \
                      be read, so the file would hold fewer rows than the range it names. Repair \
                      or restore the damaged segment first."
                .to_string(),
        });
    }
    let rows = scanned.rows;

    if let Some(parent) = into.parent() {
        std::fs::create_dir_all(parent).map_err(|e| failed("create its directory", e))?;
    }

    // FAILURE_MODES.md section 10, export: fail the export, and never let an
    // export displace live data. An export is the one write here that nobody is
    // waiting on and that nothing depends on, so it is the first to give way.
    //
    // The estimate is the rows in hand at their in-memory size. Parquet is
    // smaller than that in every measured case, so this refuses a little early
    // rather than filling the device and then failing.
    let estimate = rows.len() as u64 * ROW_BYTES_ESTIMATE;
    let space = Space::new(
        into.parent().unwrap_or_else(|| Path::new(".")),
        *reserve_bytes,
    );
    space.check_bulk(Point::Export, estimate)?;

    let schema = schema_for(&rows);
    let columns: Vec<String> = schema
        .fields()
        .iter()
        .map(|field| field.name().clone())
        .collect();

    let file = File::create(into).map_err(|e| failed("create its file", e))?;
    let properties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .build();
    let mut writer = ArrowWriter::try_new(file, Arc::new(schema.clone()), Some(properties))
        .map_err(|e| failed("be started", e))?;

    if !rows.is_empty() {
        writer
            .write(&build_batch(&schema, &rows)?)
            .map_err(|e| failed("be written", e))?;
    }
    writer.close().map_err(|e| failed("be finished", e))?;

    let file_bytes = std::fs::metadata(into)
        .map_err(|e| failed("be measured", e))?
        .len();

    let manifest = ExportManifest {
        project_id,
        range_start,
        range_end,
        basis: match basis {
            TimeBasis::OccurredAt => "occurred_at",
            TimeBasis::ReceivedAt => "received_at",
            TimeBasis::CommittedAt => "committed_at",
        },
        rows: rows.len(),
        columns,
        tombstone_generation,
        commit_watermark: store.commit_watermark(),
        file_bytes,
        relative_path: into
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
    };

    std::fs::write(manifest_path_for(into), manifest.to_text())
        .map_err(|e| failed("write its description", e))?;

    Ok(manifest)
}

/// Where the description of an export lives, beside the file it describes.
pub fn manifest_path_for(parquet: &Path) -> PathBuf {
    parquet.with_extension("manifest.txt")
}
