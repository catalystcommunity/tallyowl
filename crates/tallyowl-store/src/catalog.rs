//! The embedded transactional catalog.
//!
//! `docs/STORAGE.md` section 3.3 gives the contents and the ordered key
//! prefixes. D3 measured the engine and accepted it: a deduplication lookup on
//! the ingest hot path costs 0.91 microseconds, a manifest prefix scan reads
//! 2.9 million rows each second, and a durable commit is bounded by the device
//! rather than by the engine.
//!
//! # What a rebuild restores, and what it does not
//!
//! **The catalog's byte format is not TallyOwl's recovery contract.** Every
//! retained segment has a self-contained manifest, so a repair command rebuilds
//! the segment catalog by scanning them.
//!
//! That covers one of the twelve things the catalog holds. `docs/FAILURE_MODES.md`
//! section 7 lists the nine it does not, and two of those matter beyond
//! inconvenience: **lost tombstones resurrect erased data**, and **lost receipts
//! duplicate on retry**.
//!
//! The erasure ledger is therefore durable independently of the catalog and
//! survives a rebuild, because an erasure that a rebuild can undo is not an
//! erasure. See section 9 and D59.
//!
//! # Values are versioned canonical CBOR
//!
//! Not a structure dump from whichever library happened to be linked. A
//! snapshot outlives an engine change, which is the reason section 3.3 gives.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};

use crate::cbor::{self, MapBuilder, Value};
use crate::keys::{Cipher, KeyError, ProjectKey, RootKey};
use crate::locator::{Locator, LocatorRun};
use crate::row::hex;

/// The one table. Ordered key prefixes give the structure, which is what
/// section 3.3 describes and what a prefix scan needs.
const CATALOG: TableDefinition<&str, &[u8]> = TableDefinition::new("catalog");

/// Every stored locator run lives under this prefix.
const LOCATOR_PREFIX: &str = "locator/";

/// The maintained sum of every stored locator run's value length. It moves
/// in the same transaction as the run keys it counts (`write_durable`,
/// `remove_durable`), so it can never disagree with the stored runs. It is
/// deliberately outside the `locator/` range so it does not count itself.
const LOCATOR_BYTES_KEY: &str = "tablet/0000/locator_bytes";

/// The independently durable erasure ledger.
///
/// It is a second file rather than a second table, so a catalog that is lost or
/// rebuilt cannot take the ledger with it. Section 9 rule 3.
const ERASURE_LEDGER: TableDefinition<&str, &[u8]> = TableDefinition::new("erasure");

/// The format version this catalog writes. Startup refuses an unknown
/// incompatible version rather than guessing.
pub const CATALOG_VERSION: u64 = 1;

/// Why a catalog operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    Unavailable(String),
    Damaged(String),
    Unsupported(String),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogError::Unavailable(m)
            | CatalogError::Damaged(m)
            | CatalogError::Unsupported(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for CatalogError {}

impl From<KeyError> for CatalogError {
    fn from(error: KeyError) -> CatalogError {
        match error {
            // A key that is gone is the ordinary result of an erasure rather
            // than a fault, and it is not something a retry fixes.
            KeyError::Destroyed(m) => CatalogError::Unsupported(m),
            KeyError::RootKey(m) => CatalogError::Unsupported(m),
            KeyError::Damaged(m) => CatalogError::Damaged(m),
        }
    }
}

fn unavailable(what: &str, error: impl std::fmt::Display) -> CatalogError {
    CatalogError::Unavailable(format!("The stored index could not {what}: {error}"))
}

/// One committed batch's durable receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub source_id: [u8; 16],
    pub batch_id: [u8; 16],
    pub accepted: u64,
    pub committed_at: i64,
    pub commit_watermark: u64,
    /// The first append-log position this batch occupies.
    pub log_position: u64,
}

/// One segment, as `docs/SEGMENT_FORMAT.md` section 12 describes a manifest.
///
/// The manifest is authoritative. A directory path is a convenience and never
/// carries query meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub segment_id: [u8; 16],
    pub content_address: [u8; 32],
    pub tablet_id: u64,
    pub virtual_shard: u64,
    pub workspace_id: [u8; 16],
    pub project_id: [u8; 16],
    pub kinds: Vec<String>,
    pub occurred_range: (i64, i64),
    pub received_range: (i64, i64),
    pub committed_range: (i64, i64),
    pub log_range: (u64, u64),
    pub row_count: u64,
    pub byte_count: u64,
    /// The generation that published it.
    pub generation: u64,
    /// `local` today. The cold tier adds an object key here.
    pub tier: String,
    pub relative_path: String,
}

/// A standing predicate that hides matching rows.
///
/// A tombstone is not only a filter over data that already exists. Telemetry for
/// an erased end user can still be in a collector queue when the erasure lands,
/// so the predicate stays active and a late arrival that matches never becomes
/// visible. See STORAGE.md section 11 and D28.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tombstone {
    pub tombstone_id: [u8; 16],
    pub generation: u64,
    pub project_id: [u8; 16],
    /// Every event this tombstone names, when it names events.
    pub event_ids: Vec<[u8; 16]>,
    /// A property key and value that every matching row carries, such as an
    /// end-user identifier.
    ///
    /// The key may also name one of the row's own correlation columns, such as
    /// `trace_id` or `session_id`. A tail-sampling decision names a trace, and
    /// a trace ID is a column rather than a property.
    pub property: Option<(String, String)>,
    /// Telemetry kinds this predicate does **not** hide.
    ///
    /// D35: an always-keep error survives even when the tail rules drop its
    /// trace. The exclusion is part of the predicate rather than a list of
    /// event IDs, because a predicate also covers the late arrivals that no
    /// list could name.
    pub except_kinds: Vec<String>,
    /// A bounded time range, when the request named one.
    pub range: Option<(i64, i64)>,
    pub requested_at: i64,
    /// Until when the predicate stays active for late arrivals.
    pub horizon: i64,
    pub reason: String,
}

impl Tombstone {
    /// Whether this predicate hides one row.
    pub fn hides(&self, row: &crate::row::EventRow) -> bool {
        if row.project_id != self.project_id {
            return false;
        }
        if !self.event_ids.is_empty() && !self.event_ids.contains(&row.event_id) {
            return false;
        }
        if self.except_kinds.iter().any(|kind| kind == &row.kind) {
            return false;
        }
        if let Some((key, value)) = &self.property {
            match correlation_of(row, key) {
                Some(held) if held == *value => {}
                Some(_) => return false,
                None => match row.properties.get(key) {
                    Some((held, _)) if held.to_display() == *value => {}
                    _ => return false,
                },
            }
        }
        if let Some((start, end)) = self.range {
            if row.occurred_at < start || row.occurred_at >= end {
                return false;
            }
        }
        true
    }

    /// Whether this predicate names anything at all.
    ///
    /// An exclusion alone names nothing: "hide everything except errors" over a
    /// whole project is not something an erasure request should reach by
    /// accident.
    ///
    /// A tombstone with no events, no property, and no range would hide a whole
    /// project. That is a real operation and it is not one an erasure request
    /// should reach by accident, so the caller checks this before it commits
    /// one and asks for the project explicitly.
    pub fn names_something(&self) -> bool {
        !self.event_ids.is_empty() || self.property.is_some() || self.range.is_some()
    }
}

/// The transactional catalog.
pub struct Catalog {
    database: Database,
    ledger: Database,
    directory: PathBuf,
}

impl Catalog {
    /// Open, creating what is missing.
    pub fn open(directory: impl AsRef<Path>) -> Result<Catalog, CatalogError> {
        let directory = directory.as_ref().to_path_buf();
        std::fs::create_dir_all(&directory).map_err(|e| unavailable("be created", e))?;

        let database = Database::create(directory.join("catalog.redb"))
            .map_err(|e| unavailable("be opened", e))?;
        // A second file, so a lost or rebuilt catalog cannot take the erasure
        // ledger with it.
        let ledger = Database::create(directory.join("erasure.redb"))
            .map_err(|e| unavailable("be opened", e))?;

        let catalog = Catalog {
            database,
            ledger,
            directory,
        };
        catalog.check_version()?;
        Ok(catalog)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn check_version(&self) -> Result<(), CatalogError> {
        match self.read("format/catalog")? {
            None => self.write_durable(&[(
                "format/catalog".to_string(),
                cbor::encode(
                    &MapBuilder::new()
                        .put("v", Value::Unsigned(CATALOG_VERSION))
                        .build(),
                ),
            )]),
            Some(bytes) => {
                let value = decode(&bytes)?;
                let held = value.field("v").and_then(|v| v.as_unsigned()).unwrap_or(0);
                if held != CATALOG_VERSION {
                    return Err(CatalogError::Unsupported(format!(
                        "This data directory was written by a different version of TallyOwl \
                         and this one cannot read it. It says version {held}, and this software \
                         reads version {CATALOG_VERSION}."
                    )));
                }
                Ok(())
            }
        }
    }

    // -----------------------------------------------------------------------
    // The low-level transaction seam
    // -----------------------------------------------------------------------

    pub(crate) fn read(&self, key: &str) -> Result<Option<Vec<u8>>, CatalogError> {
        let transaction = self
            .database
            .begin_read()
            .map_err(|e| unavailable("be read", e))?;
        let table = match transaction.open_table(CATALOG) {
            Ok(table) => table,
            // A database with no table yet holds nothing, which is the ordinary
            // state on the first open.
            Err(_) => return Ok(None),
        };
        Ok(table
            .get(key)
            .map_err(|e| unavailable("be read", e))?
            .map(|value| value.value().to_vec()))
    }

    /// Write several keys in one durable transaction.
    ///
    /// One transaction is what makes a receipt and a log position atomic, and
    /// what makes a generation publish atomically. A query sees the old
    /// generation or the new one, never a mixture.
    pub(crate) fn write_durable(&self, entries: &[(String, Vec<u8>)]) -> Result<(), CatalogError> {
        let mut transaction = self
            .database
            .begin_write()
            .map_err(|e| unavailable("be written", e))?;
        // `Immediate` fsyncs on commit. That is what a durable receipt requires,
        // and D3 measured that the device bounds it rather than the engine.
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|e| unavailable("be written", e))?;
        {
            let mut table = transaction
                .open_table(CATALOG)
                .map_err(|e| unavailable("be written", e))?;
            // The locator-byte total moves in the same transaction as the run
            // keys it counts, here and in `remove_durable`, so the gauge can
            // never disagree with the stored runs. The sampler used to sum the
            // stored values on every read — 2.7 GB touched every ten seconds on
            // a consolidated soak-aged catalog.
            let mut delta: i128 = 0;
            for (key, value) in entries {
                if key.starts_with(LOCATOR_PREFIX) {
                    let old = table
                        .get(key.as_str())
                        .map_err(|e| unavailable("be written", e))?
                        .map(|held| held.value().len() as i128)
                        .unwrap_or(0);
                    delta += value.len() as i128 - old;
                }
                table
                    .insert(key.as_str(), value.as_slice())
                    .map_err(|e| unavailable("be written", e))?;
            }
            Self::shift_locator_bytes(&mut table, delta)?;
        }
        transaction
            .commit()
            .map_err(|e| unavailable("be written", e))
    }

    pub(crate) fn remove_durable(&self, keys: &[String]) -> Result<(), CatalogError> {
        let mut transaction = self
            .database
            .begin_write()
            .map_err(|e| unavailable("be written", e))?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|e| unavailable("be written", e))?;
        {
            let mut table = transaction
                .open_table(CATALOG)
                .map_err(|e| unavailable("be written", e))?;
            let mut delta: i128 = 0;
            for key in keys {
                let removed = table
                    .remove(key.as_str())
                    .map_err(|e| unavailable("be written", e))?;
                if key.starts_with(LOCATOR_PREFIX) {
                    if let Some(held) = removed {
                        delta -= held.value().len() as i128;
                    }
                }
            }
            Self::shift_locator_bytes(&mut table, delta)?;
        }
        transaction
            .commit()
            .map_err(|e| unavailable("be written", e))
    }

    /// Move the maintained locator-byte total by `delta`, inside the caller's
    /// open transaction.
    fn shift_locator_bytes(
        table: &mut redb::Table<&str, &[u8]>,
        delta: i128,
    ) -> Result<(), CatalogError> {
        if delta == 0 {
            return Ok(());
        }
        let held = table
            .get(LOCATOR_BYTES_KEY)
            .map_err(|e| unavailable("be written", e))?
            .map(|value| {
                decode(value.value())
                    .map(|decoded| field(&decoded, "b") as i128)
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let total = (held + delta).max(0) as u64;
        table
            .insert(
                LOCATOR_BYTES_KEY,
                cbor::encode(&MapBuilder::new().put("b", Value::Unsigned(total)).build())
                    .as_slice(),
            )
            .map_err(|e| unavailable("be written", e))?;
        Ok(())
    }

    pub(crate) fn scan(&self, prefix: &str) -> Result<Vec<(String, Vec<u8>)>, CatalogError> {
        let transaction = self
            .database
            .begin_read()
            .map_err(|e| unavailable("be read", e))?;
        let table = match transaction.open_table(CATALOG) {
            Ok(table) => table,
            Err(_) => return Ok(Vec::new()),
        };
        // The next prefix, so the range covers exactly this one.
        let mut upper = prefix.to_string();
        upper.push('\u{10ffff}');
        let mut out = Vec::new();
        for row in table
            .range(prefix..upper.as_str())
            .map_err(|e| unavailable("be read", e))?
        {
            let (key, value) = row.map_err(|e| unavailable("be read", e))?;
            out.push((key.value().to_string(), value.value().to_vec()));
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Receipts
    // -----------------------------------------------------------------------

    fn receipt_key(source_id: [u8; 16], batch_id: [u8; 16]) -> String {
        format!("receipt/{}/{}", hex(&source_id), hex(&batch_id))
    }

    /// The receipt for a batch that already committed.
    ///
    /// This is the deduplication lookup on the ingest hot path, and D3 measured
    /// it at 0.91 microseconds.
    pub fn receipt(
        &self,
        source_id: [u8; 16],
        batch_id: [u8; 16],
    ) -> Result<Option<Receipt>, CatalogError> {
        let Some(bytes) = self.read(&Self::receipt_key(source_id, batch_id))? else {
            return Ok(None);
        };
        let value = decode(&bytes)?;
        Ok(Some(Receipt {
            source_id,
            batch_id,
            accepted: field(&value, "n"),
            committed_at: signed(&value, "at"),
            commit_watermark: field(&value, "wm"),
            log_position: field(&value, "log"),
        }))
    }

    /// Record a receipt and the commit watermark in one transaction.
    ///
    /// STORAGE.md section 5 step 4: atomically record the receipt and the log
    /// position. A crash between them would either duplicate on retry or lose
    /// the position.
    pub fn commit_receipt(&self, receipt: &Receipt) -> Result<(), CatalogError> {
        let value = MapBuilder::new()
            .put("n", Value::Unsigned(receipt.accepted))
            .put("at", Value::integer(receipt.committed_at))
            .put("wm", Value::Unsigned(receipt.commit_watermark))
            .put("log", Value::Unsigned(receipt.log_position))
            .build();
        self.write_durable(&[
            (
                Self::receipt_key(receipt.source_id, receipt.batch_id),
                cbor::encode(&value),
            ),
            (
                "tablet/0000/watermark".to_string(),
                cbor::encode(
                    &MapBuilder::new()
                        .put("wm", Value::Unsigned(receipt.commit_watermark))
                        .put("log", Value::Unsigned(receipt.log_position))
                        .build(),
                ),
            ),
        ])
    }

    /// The current commit watermark and the highest log position it covers.
    pub fn watermark(&self) -> Result<(u64, u64), CatalogError> {
        let Some(bytes) = self.read("tablet/0000/watermark")? else {
            return Ok((0, 0));
        };
        let value = decode(&bytes)?;
        Ok((field(&value, "wm"), field(&value, "log")))
    }

    /// How many receipts the catalog holds. `docs/DELIVERY.md` section 6
    /// requires the deduplication window to outlive the retry window, so this is
    /// an operational figure rather than a query one.
    pub fn receipt_count(&self) -> Result<usize, CatalogError> {
        Ok(self.scan("receipt/")?.len())
    }

    /// Remove every receipt committed before `before_ms`, and report how many
    /// went.
    ///
    /// **This is the deduplication window, and it is half of D36.** A receipt is
    /// what makes a repeated batch ID one logical commit, so a receipt that
    /// expires while a retry of that batch is still possible turns the retry
    /// into a second logical commit that no query can remove afterwards. D36
    /// states the rule: `dedup_window >= max_outage_buffer + max_replay_window +
    /// safety`. `crates/tallyowl-config/src/validate.rs` refuses a configuration
    /// that breaks it, so this function can expire without checking again.
    ///
    /// Until this existed the window was unbounded, which satisfied D36
    /// trivially and grew the catalog for ever. See L044 and L052.
    pub fn expire_receipts(&self, before_ms: i64) -> Result<usize, CatalogError> {
        let expired: Vec<String> = self
            .scan("receipt/")?
            .into_iter()
            .filter_map(|(key, bytes)| {
                let value = decode(&bytes).ok()?;
                (signed(&value, "at") < before_ms).then_some(key)
            })
            .collect();
        if expired.is_empty() {
            return Ok(0);
        }
        self.remove_durable(&expired)?;
        Ok(expired.len())
    }

    // -----------------------------------------------------------------------
    // Segments and generations
    // -----------------------------------------------------------------------

    fn segment_key(generation: u64, segment_id: [u8; 16]) -> String {
        format!("segment/0000/{generation:016x}/{}", hex(&segment_id))
    }

    /// Publish segments in one catalog transaction.
    ///
    /// FAILURE_MODES.md section 8.1 rule 2: a query sees the old generation or
    /// the new one, never a mixture. That is why this takes a list rather than
    /// one manifest.
    pub fn publish(&self, manifests: &[Manifest]) -> Result<u64, CatalogError> {
        self.publish_with_locator(manifests, &Locator::new())
    }

    /// Publish segments and the locator runs that describe them, together.
    ///
    /// FAILURE_MODES.md section 8.4 rule 1: a locator run is published in the
    /// same catalog transaction as the segments it describes, so a generation
    /// never holds a run that disagrees with its segments.
    pub fn publish_with_locator(
        &self,
        manifests: &[Manifest],
        locator: &Locator,
    ) -> Result<u64, CatalogError> {
        let generation = self.generation()? + 1;
        let mut entries: Vec<(String, Vec<u8>)> = manifests
            .iter()
            .map(|manifest| {
                let mut manifest = manifest.clone();
                manifest.generation = generation;
                (
                    Self::segment_key(generation, manifest.segment_id),
                    cbor::encode(&manifest_to_cbor(&manifest)),
                )
            })
            .collect();

        for run in locator.runs() {
            if run.is_empty() {
                continue;
            }
            entries.push((
                format!("locator/0000/{:016x}/{generation:016x}", run.bucket()),
                run.encode(),
            ));
        }

        entries.push((
            "tablet/0000/generation".to_string(),
            cbor::encode(
                &MapBuilder::new()
                    .put("g", Value::Unsigned(generation))
                    .build(),
            ),
        ));
        self.write_durable(&entries)?;
        Ok(generation)
    }

    /// Every locator run, combined bucket by bucket and sealed once.
    ///
    /// The one-merge-per-run version of this re-sorted every accumulated
    /// entry once per stored run, which is quadratic in runs and stood three
    /// heads' watchers still for hours on a soak-aged locator. L165.
    pub fn locator(&self) -> Result<Locator, CatalogError> {
        let mut runs = Vec::new();
        for (_, bytes) in self.scan("locator/")? {
            runs.push(LocatorRun::decode(&bytes).map_err(|e| {
                CatalogError::Damaged(format!("A stored index run could not be read. {e}"))
            })?);
        }
        Ok(Locator::from_all_runs(runs))
    }

    /// The bytes every stored locator run occupies, without combining them.
    ///
    /// The metrics sampler reads this. Building the whole locator to report
    /// its size is what put the sampler inside an hours-long merge (L165),
    /// and summing the stored values on every read still touched 2.7 GB
    /// every ten seconds on a consolidated soak-aged catalog. The total is a
    /// maintained counter now, moved in the same transaction as every run
    /// write and removal; this reads one small key. A catalog written before
    /// the counter existed pays one full sum, inside a write transaction so
    /// the total it stores is exact, and never pays it again.
    pub fn locator_bytes(&self) -> Result<u64, CatalogError> {
        if let Some(bytes) = self.read(LOCATOR_BYTES_KEY)? {
            return Ok(field(&decode(&bytes)?, "b"));
        }
        // Migration: sum and store inside one write transaction, so the
        // stored total cannot race a concurrent run write.
        let mut transaction = self
            .database
            .begin_write()
            .map_err(|e| unavailable("be written", e))?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|e| unavailable("be written", e))?;
        let total;
        {
            let mut table = transaction
                .open_table(CATALOG)
                .map_err(|e| unavailable("be written", e))?;
            let mut upper = LOCATOR_PREFIX.to_string();
            upper.push('\u{10ffff}');
            let mut sum = 0u64;
            for row in table
                .range(LOCATOR_PREFIX..upper.as_str())
                .map_err(|e| unavailable("be read", e))?
            {
                let (_, value) = row.map_err(|e| unavailable("be read", e))?;
                sum += value.value().len() as u64;
            }
            total = sum;
            table
                .insert(
                    LOCATOR_BYTES_KEY,
                    cbor::encode(&MapBuilder::new().put("b", Value::Unsigned(total)).build())
                        .as_slice(),
                )
                .map_err(|e| unavailable("be written", e))?;
        }
        transaction
            .commit()
            .map_err(|e| unavailable("be written", e))?;
        Ok(total)
    }

    /// Replace every locator run with the ones given, in one transaction.
    ///
    /// Compaction combines runs incrementally, and this is where the combined
    /// set lands. Writing before removing keeps a query from ever seeing a
    /// window with no runs at all.
    pub fn replace_locator(&self, locator: &Locator) -> Result<(), CatalogError> {
        let existing: Vec<String> = self
            .scan("locator/")?
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        let generation = self.generation()?;
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        for run in locator.runs() {
            if run.is_empty() {
                continue;
            }
            entries.push((
                format!("locator/0000/{:016x}/{generation:016x}", run.bucket()),
                run.encode(),
            ));
        }
        self.write_durable(&entries)?;
        // A key that was just rewritten must not then be removed.
        let stale: Vec<String> = existing
            .into_iter()
            .filter(|key| !entries.iter().any(|(written, _)| written == key))
            .collect();
        self.remove_durable(&stale)
    }

    /// The current manifest generation.
    pub fn generation(&self) -> Result<u64, CatalogError> {
        let Some(bytes) = self.read("tablet/0000/generation")? else {
            return Ok(0);
        };
        Ok(field(&decode(&bytes)?, "g"))
    }

    /// Every live segment manifest.
    ///
    /// A segment stays live until a later generation replaces it, and this
    /// returns the newest entry for each segment ID.
    pub fn manifests(&self) -> Result<Vec<Manifest>, CatalogError> {
        let mut newest: BTreeMap<[u8; 16], Manifest> = BTreeMap::new();
        for (_, bytes) in self.scan("segment/")? {
            let manifest = manifest_from_cbor(&decode(&bytes)?)?;
            newest
                .entry(manifest.segment_id)
                .and_modify(|held| {
                    if manifest.generation > held.generation {
                        *held = manifest.clone();
                    }
                })
                .or_insert(manifest);
        }
        // A retired segment carries a generation and no rows, which is how a
        // compaction says "this one is gone" without deleting the record a
        // pinned query may still be reading.
        Ok(newest
            .into_values()
            .filter(|manifest| !manifest.tier.is_empty())
            .collect())
    }

    /// Retire segments and publish replacements in one transaction.
    ///
    /// FAILURE_MODES.md section 8.1 rule 2 again. A compaction that published
    /// its replacements and then retired the sources would let a query see both.
    pub fn swap(&self, retire: &[[u8; 16]], publish: &[Manifest]) -> Result<u64, CatalogError> {
        let generation = self.generation()? + 1;
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();

        for segment_id in retire {
            // An empty tier marks the segment retired at this generation. The
            // record stays so a query that pinned an older generation can still
            // find what it resolved.
            let retired = MapBuilder::new()
                .put("id", Value::Bytes(segment_id.to_vec()))
                .put("g", Value::Unsigned(generation))
                .put("tier", Value::text(""))
                .build();
            entries.push((
                Self::segment_key(generation, *segment_id),
                cbor::encode(&retired),
            ));
        }
        for manifest in publish {
            let mut manifest = manifest.clone();
            manifest.generation = generation;
            entries.push((
                Self::segment_key(generation, manifest.segment_id),
                cbor::encode(&manifest_to_cbor(&manifest)),
            ));
        }
        entries.push((
            "tablet/0000/generation".to_string(),
            cbor::encode(
                &MapBuilder::new()
                    .put("g", Value::Unsigned(generation))
                    .build(),
            ),
        ));
        self.write_durable(&entries)?;
        Ok(generation)
    }

    /// Rebuild the segment catalog by scanning manifests.
    ///
    /// STORAGE.md section 3.3 and FAILURE_MODES.md procedure 5. **This covers
    /// the segment catalog only.** Receipts, tombstones, configuration,
    /// credentials, and saved dashboards are in no segment manifest, and the
    /// caller is told so explicitly rather than discovering it.
    pub fn rebuild_from_manifests(&self, manifests: &[Manifest]) -> Result<u64, CatalogError> {
        let existing: Vec<String> = self
            .scan("segment/")?
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        self.remove_durable(&existing)?;
        self.publish(manifests)
    }

    // -----------------------------------------------------------------------
    // Tombstones and the erasure ledger
    // -----------------------------------------------------------------------

    /// Commit a tombstone and advance the visible generation.
    ///
    /// **The tombstone is durable before the erasure is acknowledged.** The
    /// acknowledgement is a statement to a person that their data is gone, and a
    /// crash after it and before the durable write would make that statement
    /// false. See FAILURE_MODES.md section 9 rules 1 and 2.
    ///
    /// The ledger entry is written first and in its own database, so a catalog
    /// rebuild cannot undo the erasure.
    pub fn commit_tombstone(&self, tombstone: &Tombstone) -> Result<u64, CatalogError> {
        let generation = self.tombstone_generation()? + 1;
        let mut tombstone = tombstone.clone();
        tombstone.generation = generation;
        let encoded = cbor::encode(&tombstone_to_cbor(&tombstone));

        // The independently durable ledger goes first. An erasure that a
        // rebuild can undo is not an erasure.
        self.write_ledger(&tombstone.tombstone_id, &encoded)?;

        self.write_durable(&[
            (
                format!(
                    "tombstone/0000/{generation:016x}/{}",
                    hex(&tombstone.tombstone_id)
                ),
                encoded,
            ),
            (
                "tablet/0000/tombstone-generation".to_string(),
                cbor::encode(
                    &MapBuilder::new()
                        .put("g", Value::Unsigned(generation))
                        .build(),
                ),
            ),
        ])?;
        Ok(generation)
    }

    pub fn tombstone_generation(&self) -> Result<u64, CatalogError> {
        let Some(bytes) = self.read("tablet/0000/tombstone-generation")? else {
            return Ok(0);
        };
        Ok(field(&decode(&bytes)?, "g"))
    }

    /// Every active predicate.
    pub fn tombstones(&self) -> Result<Vec<Tombstone>, CatalogError> {
        self.scan("tombstone/")?
            .into_iter()
            .map(|(_, bytes)| tombstone_from_cbor(&decode(&bytes)?))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Tail-sampling decisions
    //
    // D35: a span that arrives after the grace period cannot change a decision
    // that was already applied. That rule needs a durable record of what was
    // applied, so a restart does not decide a trace a second time.
    //
    // Only a *kept* decision strictly needs one: a dropped trace has a
    // tombstone, and a tombstone is already a standing predicate that covers
    // its own late arrivals. Both are recorded anyway, because the projector
    // needs to tell "decided and kept" from "not decided yet", and one shape
    // for both is easier to reason about than two.
    // -----------------------------------------------------------------------

    /// Record what the tail rules decided for one trace.
    pub fn record_tail_decision(
        &self,
        trace_id: [u8; 16],
        keep: bool,
        at: i64,
    ) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!("projector/tail/0000/{}", hex(&trace_id)),
            cbor::encode(
                &MapBuilder::new()
                    .put("keep", Value::Bool(keep))
                    .put("at", Value::integer(at))
                    .build(),
            ),
        )])
    }

    /// What was decided for one trace, and when, when anything was.
    pub fn tail_decision(&self, trace_id: [u8; 16]) -> Result<Option<(bool, i64)>, CatalogError> {
        let Some(bytes) = self.read(&format!("projector/tail/0000/{}", hex(&trace_id)))? else {
            return Ok(None);
        };
        let value = decode(&bytes)?;
        Ok(Some((
            value.field("keep").and_then(Value::as_bool).unwrap_or(true),
            signed(&value, "at"),
        )))
    }

    /// How many traces have a recorded decision. An operational report reads it.
    pub fn tail_decision_count(&self) -> Result<usize, CatalogError> {
        Ok(self.scan("projector/tail/")?.len())
    }

    // -----------------------------------------------------------------------
    // Generation pins
    // -----------------------------------------------------------------------

    /// Pin a manifest generation for a running query.
    ///
    /// FAILURE_MODES.md section 8.1 rule 1: a query pins the generation it
    /// resolved, and a pinned generation's segments are never deleted. Rule 4
    /// bounds the damage a leaked pin can do: a pin that outlives the grace
    /// period, because a process died holding it, expires.
    pub fn pin(&self, generation: u64, at: i64) -> Result<[u8; 16], CatalogError> {
        let pin_id = pin_identifier(at);
        self.write_durable(&[(
            format!("pin/0000/{}", hex(&pin_id)),
            cbor::encode(
                &MapBuilder::new()
                    .put("g", Value::Unsigned(generation))
                    .put("at", Value::integer(at))
                    .build(),
            ),
        )])?;
        Ok(pin_id)
    }

    /// Release a pin. A query that finished holds nothing.
    pub fn release_pin(&self, pin_id: [u8; 16]) -> Result<(), CatalogError> {
        self.remove_durable(&[format!("pin/0000/{}", hex(&pin_id))])
    }

    /// Every pin, as a generation and when it was taken.
    pub fn pins(&self) -> Result<Vec<(u64, i64)>, CatalogError> {
        self.scan("pin/")?
            .into_iter()
            .map(|(_, bytes)| {
                let value = decode(&bytes)?;
                Ok((field(&value, "g"), signed(&value, "at")))
            })
            .collect()
    }

    /// Remove pins older than `max_age`, and say how many went.
    ///
    /// A leaked pin must not retain storage forever. This is rule 4, and the
    /// bound is what makes it a delay rather than a leak.
    pub fn expire_pins(&self, now: i64, max_age_ms: i64) -> Result<usize, CatalogError> {
        let stale: Vec<String> = self
            .scan("pin/")?
            .into_iter()
            .filter_map(|(key, bytes)| {
                let value = decode(&bytes).ok()?;
                (now - signed(&value, "at") > max_age_ms).then_some(key)
            })
            .collect();
        let count = stale.len();
        if count > 0 {
            self.remove_durable(&stale)?;
        }
        Ok(count)
    }

    /// The oldest generation any query is still holding, when one is.
    pub fn oldest_pinned_generation(&self) -> Result<Option<u64>, CatalogError> {
        Ok(self.pins()?.into_iter().map(|(g, _)| g).min())
    }

    // -----------------------------------------------------------------------
    // Segment encryption keys, per D61
    // -----------------------------------------------------------------------

    fn key_prefix(project_id: [u8; 16]) -> String {
        format!("key/project/{}/", hex(&project_id))
    }

    /// The key generation a new segment for this project writes under, creating
    /// one when the project has none.
    ///
    /// A key is generated locally and stored wrapped. The root key never leaves
    /// configuration and never encrypts a segment itself.
    pub fn current_key(
        &self,
        project_id: [u8; 16],
        root: &RootKey,
    ) -> Result<Cipher, CatalogError> {
        if let Some(cipher) = self.key_at_generation(project_id, root, None)? {
            return Ok(cipher);
        }
        self.rotate_key(project_id, root)
    }

    /// Write a new key generation for this project.
    ///
    /// Rotation leaves already-written objects readable: each segment names the
    /// generation it used, so an older one still opens until retention expires
    /// it or compaction rewrites it.
    pub fn rotate_key(&self, project_id: [u8; 16], root: &RootKey) -> Result<Cipher, CatalogError> {
        let generation = self
            .key_generations(project_id)?
            .into_iter()
            .max()
            .map(|held| held + 1)
            .unwrap_or(1);
        let key = ProjectKey::generate()?;
        let wrapped = root.wrap(project_id, generation, &key)?;
        self.write_durable(&[(
            format!("{}{generation:08x}", Self::key_prefix(project_id)),
            wrapped,
        )])?;
        Ok(Cipher::new(key, generation))
    }

    /// Every generation this project still has a key for.
    pub fn key_generations(&self, project_id: [u8; 16]) -> Result<Vec<u32>, CatalogError> {
        Ok(self
            .scan(&Self::key_prefix(project_id))?
            .into_iter()
            .filter_map(|(key, _)| u32::from_str_radix(key.rsplit('/').next()?, 16).ok())
            .collect())
    }

    /// One generation's key, or the newest when none is named.
    ///
    /// Returns nothing when the project has no key at that generation, which is
    /// the ordinary result after a destruction.
    pub fn key_at_generation(
        &self,
        project_id: [u8; 16],
        root: &RootKey,
        generation: Option<u32>,
    ) -> Result<Option<Cipher>, CatalogError> {
        let generation = match generation {
            Some(named) => named,
            None => match self.key_generations(project_id)?.into_iter().max() {
                Some(newest) => newest,
                None => return Ok(None),
            },
        };
        let Some(wrapped) =
            self.read(&format!("{}{generation:08x}", Self::key_prefix(project_id)))?
        else {
            return Ok(None);
        };
        let key = root.unwrap_key(project_id, generation, &wrapped)?;
        Ok(Some(Cipher::new(key, generation)))
    }

    /// Destroy every key generation for a project.
    ///
    /// This erases the whole project's protected data instantly and cannot be
    /// undone. An object-store reader without the key reads nothing useful, and
    /// neither does this installation.
    ///
    /// The destruction goes to the erasure ledger first, which is durable
    /// independently of the catalog, so a catalog restore cannot bring the key
    /// back. FAILURE_MODES.md section 9 rule 3: an erasure that a rebuild can
    /// undo is not an erasure.
    pub fn destroy_project_keys(
        &self,
        project_id: [u8; 16],
        requested_at: i64,
        reason: &str,
    ) -> Result<usize, CatalogError> {
        let generations = self.key_generations(project_id)?;
        if generations.is_empty() {
            return Ok(0);
        }

        // A destruction is an erasure, so it lands in the ledger before the
        // keys go and before anybody is told it happened.
        let mut ledger_id = [0u8; 16];
        ledger_id[..16].copy_from_slice(&project_id);
        let record = MapBuilder::new()
            .put("id", Value::Bytes(project_id.to_vec()))
            .put("g", Value::Unsigned(0))
            .put("pr", Value::Bytes(project_id.to_vec()))
            .put("ev", Value::Array(Vec::new()))
            .put("at", Value::integer(requested_at))
            .put("hz", Value::integer(i64::MAX))
            .put(
                "why",
                Value::text(format!("The project's key was destroyed. {reason}")),
            )
            .build();
        self.write_ledger(&ledger_id, &cbor::encode(&record))?;

        let keys: Vec<String> = generations
            .iter()
            .map(|generation| format!("{}{generation:08x}", Self::key_prefix(project_id)))
            .collect();
        self.remove_durable(&keys)?;
        Ok(generations.len())
    }

    /// How many projects hold a key. A capacity report states this rather than
    /// naming the projects, because which project is encrypted is not a secret
    /// and which end user is in it would be.
    pub fn encrypted_project_count(&self) -> Result<usize, CatalogError> {
        let mut projects: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (key, _) in self.scan("key/project/")? {
            if let Some(project) = key.split('/').nth(2) {
                projects.insert(project.to_string());
            }
        }
        Ok(projects.len())
    }

    fn write_ledger(&self, id: &[u8; 16], encoded: &[u8]) -> Result<(), CatalogError> {
        let mut transaction = self
            .ledger
            .begin_write()
            .map_err(|e| unavailable("be written", e))?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(|e| unavailable("be written", e))?;
        {
            let mut table = transaction
                .open_table(ERASURE_LEDGER)
                .map_err(|e| unavailable("be written", e))?;
            table
                .insert(hex(id).as_str(), encoded)
                .map_err(|e| unavailable("be written", e))?;
        }
        transaction
            .commit()
            .map_err(|e| unavailable("be written", e))
    }

    /// Every erasure this installation has ever acknowledged.
    ///
    /// The ledger travels with a snapshot and with a restore, so a restore
    /// cannot resurrect an erased end user. See FAILURE_MODES.md section 9
    /// rule 4 and THREAT_MODEL.md section 5.
    pub fn erasure_ledger(&self) -> Result<Vec<Tombstone>, CatalogError> {
        let transaction = self
            .ledger
            .begin_read()
            .map_err(|e| unavailable("be read", e))?;
        let table = match transaction.open_table(ERASURE_LEDGER) {
            Ok(table) => table,
            Err(_) => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for row in table.iter().map_err(|e| unavailable("be read", e))? {
            let (_, value) = row.map_err(|e| unavailable("be read", e))?;
            out.push(tombstone_from_cbor(&decode(value.value())?)?);
        }
        Ok(out)
    }

    /// Put every ledger entry back into the tombstone set.
    ///
    /// Procedure 5 step 2: a rebuild without snapshots restores the erasure
    /// ledger, because tombstones are otherwise gone.
    pub fn restore_tombstones_from_ledger(&self) -> Result<usize, CatalogError> {
        let entries = self.erasure_ledger()?;
        let mut highest = self.tombstone_generation()?;
        let mut writes = Vec::new();
        for tombstone in &entries {
            highest = highest.max(tombstone.generation);
            writes.push((
                format!(
                    "tombstone/0000/{:016x}/{}",
                    tombstone.generation,
                    hex(&tombstone.tombstone_id)
                ),
                cbor::encode(&tombstone_to_cbor(tombstone)),
            ));
        }
        if !writes.is_empty() {
            writes.push((
                "tablet/0000/tombstone-generation".to_string(),
                cbor::encode(&MapBuilder::new().put("g", Value::Unsigned(highest)).build()),
            ));
            self.write_durable(&writes)?;
        }
        Ok(entries.len())
    }
}

// ---------------------------------------------------------------------------
// The encodings
// ---------------------------------------------------------------------------

/// A pin identifier: the moment it was taken and a counter, so two pins in one
/// millisecond do not share a key.
fn pin_identifier(at: i64) -> [u8; 16] {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut out = [0u8; 16];
    out[0..8].copy_from_slice(&(at as u64).to_be_bytes());
    out[8..16].copy_from_slice(&COUNTER.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    out
}

fn decode(bytes: &[u8]) -> Result<Value, CatalogError> {
    cbor::decode(bytes).map_err(|e| {
        CatalogError::Damaged(format!("Part of the stored index could not be read. {e}"))
    })
}

fn field(value: &Value, name: &str) -> u64 {
    value
        .field(name)
        .and_then(|v| v.as_unsigned())
        .unwrap_or_default()
}

fn signed(value: &Value, name: &str) -> i64 {
    value
        .field(name)
        .and_then(|v| v.as_integer())
        .unwrap_or_default()
}

fn identifier(value: &Value, name: &str) -> [u8; 16] {
    value
        .field(name)
        .and_then(|v| v.as_bytes())
        .and_then(|b| <[u8; 16]>::try_from(b).ok())
        .unwrap_or([0; 16])
}

fn range(value: &Value, name: &str) -> (i64, i64) {
    let list = value.field(name).and_then(|v| v.as_array()).unwrap_or(&[]);
    (
        list.first().and_then(|v| v.as_integer()).unwrap_or(0),
        list.get(1).and_then(|v| v.as_integer()).unwrap_or(0),
    )
}

fn manifest_to_cbor(manifest: &Manifest) -> Value {
    let pair =
        |(low, high): (i64, i64)| Value::Array(vec![Value::integer(low), Value::integer(high)]);
    MapBuilder::new()
        .put("id", Value::Bytes(manifest.segment_id.to_vec()))
        .put("ca", Value::Bytes(manifest.content_address.to_vec()))
        .put("tab", Value::Unsigned(manifest.tablet_id))
        .put("vs", Value::Unsigned(manifest.virtual_shard))
        .put("ws", Value::Bytes(manifest.workspace_id.to_vec()))
        .put("pr", Value::Bytes(manifest.project_id.to_vec()))
        .put(
            "kinds",
            Value::Array(manifest.kinds.iter().map(Value::text).collect()),
        )
        .put("occ", pair(manifest.occurred_range))
        .put("rec", pair(manifest.received_range))
        .put("com", pair(manifest.committed_range))
        .put(
            "log",
            Value::Array(vec![
                Value::Unsigned(manifest.log_range.0),
                Value::Unsigned(manifest.log_range.1),
            ]),
        )
        .put("rows", Value::Unsigned(manifest.row_count))
        .put("bytes", Value::Unsigned(manifest.byte_count))
        .put("g", Value::Unsigned(manifest.generation))
        .put("tier", Value::text(&manifest.tier))
        .put("path", Value::text(&manifest.relative_path))
        .build()
}

fn manifest_from_cbor(value: &Value) -> Result<Manifest, CatalogError> {
    Ok(Manifest {
        segment_id: identifier(value, "id"),
        content_address: value
            .field("ca")
            .and_then(|v| v.as_bytes())
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .unwrap_or([0; 32]),
        tablet_id: field(value, "tab"),
        virtual_shard: field(value, "vs"),
        workspace_id: identifier(value, "ws"),
        project_id: identifier(value, "pr"),
        kinds: value
            .field("kinds")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_text().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        occurred_range: range(value, "occ"),
        received_range: range(value, "rec"),
        committed_range: range(value, "com"),
        log_range: {
            let list = value.field("log").and_then(|v| v.as_array()).unwrap_or(&[]);
            (
                list.first().and_then(|v| v.as_unsigned()).unwrap_or(0),
                list.get(1).and_then(|v| v.as_unsigned()).unwrap_or(0),
            )
        },
        row_count: field(value, "rows"),
        byte_count: field(value, "bytes"),
        generation: field(value, "g"),
        tier: value
            .field("tier")
            .and_then(|v| v.as_text())
            .unwrap_or_default()
            .to_string(),
        relative_path: value
            .field("path")
            .and_then(|v| v.as_text())
            .unwrap_or_default()
            .to_string(),
    })
}

/// One erasure predicate, in the encoding the erasure ledger holds.
///
/// It is public because a replicated tablet proposes an erasure rather than
/// applying it locally, and one codec is what stops a leader and a follower
/// reading the same predicate differently. The same reasoning put the append
/// log's own row frame on the replicated commit.
pub fn encode_tombstone(tombstone: &Tombstone) -> Vec<u8> {
    cbor::encode(&tombstone_to_cbor(tombstone))
}

/// Read one erasure predicate back.
pub fn decode_tombstone(bytes: &[u8]) -> Result<Tombstone, CatalogError> {
    let value = cbor::decode(bytes)
        .map_err(|e| CatalogError::Damaged(format!("An erasure record could not be read: {e}")))?;
    tombstone_from_cbor(&value)
}

fn tombstone_to_cbor(tombstone: &Tombstone) -> Value {
    MapBuilder::new()
        .put("id", Value::Bytes(tombstone.tombstone_id.to_vec()))
        .put("g", Value::Unsigned(tombstone.generation))
        .put("pr", Value::Bytes(tombstone.project_id.to_vec()))
        .put(
            "ev",
            Value::Array(
                tombstone
                    .event_ids
                    .iter()
                    .map(|id| Value::Bytes(id.to_vec()))
                    .collect(),
            ),
        )
        .put_some(
            "prop",
            tombstone
                .property
                .as_ref()
                .map(|(key, value)| Value::Array(vec![Value::text(key), Value::text(value)])),
        )
        .put_some(
            "range",
            tombstone
                .range
                .map(|(start, end)| Value::Array(vec![Value::integer(start), Value::integer(end)])),
        )
        .put("at", Value::integer(tombstone.requested_at))
        .put("hz", Value::integer(tombstone.horizon))
        .put("why", Value::text(&tombstone.reason))
        .put(
            "keep",
            Value::Array(tombstone.except_kinds.iter().map(Value::text).collect()),
        )
        .build()
}

fn tombstone_from_cbor(value: &Value) -> Result<Tombstone, CatalogError> {
    Ok(Tombstone {
        tombstone_id: identifier(value, "id"),
        generation: field(value, "g"),
        project_id: identifier(value, "pr"),
        event_ids: value
            .field("ev")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_bytes().and_then(|b| <[u8; 16]>::try_from(b).ok()))
                    .collect()
            })
            .unwrap_or_default(),
        property: value.field("prop").and_then(|v| {
            let list = v.as_array()?;
            Some((
                list.first()?.as_text()?.to_string(),
                list.get(1)?.as_text()?.to_string(),
            ))
        }),
        range: value.field("range").and_then(|v| {
            let list = v.as_array()?;
            Some((list.first()?.as_integer()?, list.get(1)?.as_integer()?))
        }),
        requested_at: signed(value, "at"),
        horizon: signed(value, "hz"),
        reason: value
            .field("why")
            .and_then(|v| v.as_text())
            .unwrap_or_default()
            .to_string(),
        // Absent in a record written before this field existed, which is what
        // an additive versioned encoding is for.
        except_kinds: value
            .field("keep")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_text().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// One of a row's own correlation columns, by the name a predicate uses.
///
/// A tombstone names a value, and the value can be a column or a property. A
/// predicate that could only read properties would be unable to name a trace,
/// because a trace ID is a column.
fn correlation_of(row: &crate::row::EventRow, key: &str) -> Option<String> {
    match key {
        "trace_id" => row.trace_id.map(|id| hex(&id)),
        "session_id" => row.session_id.clone(),
        "request_id" => row.request_id.clone(),
        "service_name" => row.service_name.clone(),
        "release" => row.release.clone(),
        "kind" => Some(row.kind.clone()),
        "name" => Some(row.name.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod locator_bytes_counter {
    use super::{Catalog, LOCATOR_BYTES_KEY};
    use crate::locator::{Locator, LocatorRun};

    fn place(name: &str) -> std::path::PathBuf {
        let base = std::env::var("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target"));
        let path = base
            .join("catalog-tests")
            .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the test directory exists");
        path
    }

    fn run_of(bucket: i64, entries: u16) -> LocatorRun {
        let mut run = LocatorRun::new(bucket);
        for n in 0..entries {
            run.add("p:end_user", format!("u-{n}").as_bytes(), [7; 16]);
        }
        run.seal();
        run
    }

    fn stored_sum(catalog: &Catalog) -> u64 {
        catalog
            .scan("locator/")
            .unwrap()
            .iter()
            .map(|(_, value)| value.len() as u64)
            .sum()
    }

    #[test]
    fn the_maintained_total_matches_the_stored_runs_through_every_write_path() {
        let catalog = Catalog::open(place("counter")).unwrap();
        assert_eq!(catalog.locator_bytes().unwrap(), 0);

        // Publish moves the total with the runs, in the same transaction.
        catalog
            .publish_with_locator(&[], &Locator::from_runs([run_of(1, 40)]))
            .unwrap();
        assert!(catalog.locator_bytes().unwrap() > 0);
        assert_eq!(catalog.locator_bytes().unwrap(), stored_sum(&catalog));

        catalog
            .publish_with_locator(&[], &Locator::from_runs([run_of(2, 10)]))
            .unwrap();
        assert_eq!(catalog.locator_bytes().unwrap(), stored_sum(&catalog));

        // Replacement rewrites some keys and removes the stale rest, in two
        // transactions; the total tracks both.
        catalog
            .replace_locator(&Locator::from_runs([run_of(1, 5)]))
            .unwrap();
        assert_eq!(catalog.locator_bytes().unwrap(), stored_sum(&catalog));

        // A catalog written before the counter existed: the first read sums
        // the stored runs once and stores the same number.
        catalog
            .remove_durable(&[LOCATOR_BYTES_KEY.to_string()])
            .unwrap();
        assert_eq!(catalog.locator_bytes().unwrap(), stored_sum(&catalog));
    }
}

#[cfg(test)]
mod aged_probe {
    //! A measurement, not a regression test. It runs only against a
    //! soak-aged catalog copy named by `AGED_CATALOG_DIR`, on real storage,
    //! and prints what BENCHMARKS.md wants recorded:
    //!
    //! ```sh
    //! AGED_CATALOG_DIR=run/bench/aged-catalog cargo test -p tallyowl-store \
    //!     --release aged_probe -- --ignored --nocapture
    //! ```

    use std::time::Instant;

    use super::Catalog;
    use crate::locator::Locator;

    #[test]
    #[ignore = "a measurement against a soak-aged catalog, run by hand with AGED_CATALOG_DIR set"]
    fn measure_locator_against_an_aged_catalog() {
        let directory = match std::env::var("AGED_CATALOG_DIR") {
            Ok(directory) => directory,
            Err(_) => panic!("set AGED_CATALOG_DIR to a directory holding an aged catalog.redb"),
        };
        let catalog = Catalog::open(&directory).expect("the aged catalog opens");

        let started = Instant::now();
        let stored_bytes = catalog.locator_bytes().expect("the byte sum reads");
        println!(
            "locator_bytes: {stored_bytes} bytes in {:?}",
            started.elapsed()
        );

        let started = Instant::now();
        let stored = catalog.scan("locator/").expect("the runs scan");
        let mut runs = Vec::new();
        for (_, bytes) in &stored {
            runs.push(super::LocatorRun::decode(bytes).expect("a stored run decodes"));
        }
        let entries: usize = runs.iter().map(|run| run.len()).sum();
        println!(
            "stored runs: {} holding {entries} entries, decoded in {:?}",
            runs.len(),
            started.elapsed()
        );

        // The fix: concatenate, then one seal per bucket.
        let started = Instant::now();
        let combined = Locator::from_all_runs(runs.clone());
        let sealed_once = started.elapsed();
        println!(
            "from_all_runs: {} bytes across {} buckets in {sealed_once:?}",
            combined.byte_len(),
            combined.runs().count()
        );

        // What ran before: one merge per run, each re-sorting everything so
        // far. Capped, because the whole point is that it does not finish;
        // the cap is generous enough to show the per-run growth.
        let cap = std::time::Duration::from_secs(
            std::env::var("AGED_OLD_CAP_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(120),
        );
        let started = Instant::now();
        let mut one_at_a_time = Locator::new();
        let mut processed = 0usize;
        for run in &runs {
            if started.elapsed() > cap {
                break;
            }
            one_at_a_time.merge(&Locator::from_runs([run.clone()]));
            processed += 1;
        }
        println!(
            "one merge per run: {processed} of {} runs in {:?} before the {cap:?} cap",
            runs.len(),
            started.elapsed()
        );
    }
}
