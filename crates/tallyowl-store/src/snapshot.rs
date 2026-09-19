//! Snapshot, restore, and rebuild.
//!
//! `docs/STORAGE.md` section 13 and `docs/FAILURE_MODES.md` section 11.
//!
//! # A snapshot pins
//!
//! The catalog and control generation, the log checkpoint, the required segment
//! identifiers and their checksums, the tombstone generation, and the format
//! metadata. A worker copies immutable segments, and only new segments need
//! copying after the first snapshot.
//!
//! # A restore verifies
//!
//! **Restore verifies each manifest and payload checksum before it publishes
//! the snapshot. A missing or corrupt file causes a visible failure, and
//! restore never silently skips a file.** That sentence is section 13's and it
//! is the whole contract: a restore that quietly dropped a segment would leave
//! an installation that looks healthy and answers wrongly.
//!
//! # The erasure ledger travels with it
//!
//! Section 9 rule 4: a restore cannot resurrect an erased end user. The ledger
//! is part of a snapshot and is applied on restore before anything is queryable.
//!
//! # A rebuild is not a restore
//!
//! Rebuilding the segment catalog by scanning manifests covers **one** of the
//! twelve things the catalog holds. Section 7 lists the nine it does not, and
//! [`rebuild`] reports exactly what came back and what did not rather than
//! leaving an operator to discover it.

use std::path::{Path, PathBuf};

use crate::catalog::{Catalog, Manifest};
use crate::segment;
use crate::store::StoreError;

/// What a snapshot pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub taken_at: i64,
    pub manifest_generation: u64,
    pub tombstone_generation: u64,
    pub commit_watermark: u64,
    pub log_checkpoint: u64,
    /// Every segment the snapshot needs, with the address that identifies it.
    pub segments: Vec<([u8; 16], [u8; 32])>,
    /// How many erasure records travelled with it.
    pub erasures: usize,
}

impl Snapshot {
    /// The snapshot's own description, written beside the files.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("# TallyOwl snapshot\n");
        out.push_str(&format!("taken_at: {}\n", self.taken_at));
        out.push_str(&format!(
            "manifest_generation: {}\n",
            self.manifest_generation
        ));
        out.push_str(&format!(
            "tombstone_generation: {}\n",
            self.tombstone_generation
        ));
        out.push_str(&format!("commit_watermark: {}\n", self.commit_watermark));
        out.push_str(&format!("log_checkpoint: {}\n", self.log_checkpoint));
        out.push_str(&format!("erasures: {}\n", self.erasures));
        out.push_str(&format!("segments: {}\n", self.segments.len()));
        for (segment_id, address) in &self.segments {
            out.push_str(&format!(
                "segment: {} {}\n",
                crate::row::hex(segment_id),
                crate::row::hex(address)
            ));
        }
        out
    }
}

/// Take a snapshot of a data directory into `into`.
///
/// Only new segments are copied. A segment already in the destination with the
/// right content address is left alone, which is what makes the second snapshot
/// much cheaper than the first.
pub fn take(from: &Path, into: &Path, taken_at: i64) -> Result<Snapshot, StoreError> {
    let catalog = Catalog::open(from.join("catalog"))?;
    take_with(from, into, taken_at, &catalog)
}

/// Take a snapshot using a catalog that is already open.
///
/// One process owns one data directory, and the catalog lock enforces it. A
/// running node therefore cannot call [`take`] against its own directory, so
/// [`crate::segmented::SegmentedStore::snapshot`] passes its own catalog in
/// here. [`take`] is the offline path, for a directory nothing is serving.
pub fn take_with(
    from: &Path,
    into: &Path,
    taken_at: i64,
    catalog: &Catalog,
) -> Result<Snapshot, StoreError> {
    let manifests = catalog.manifests()?;

    std::fs::create_dir_all(into.join("segments")).map_err(|e| {
        StoreError::Unavailable(format!("The snapshot directory could not be created: {e}"))
    })?;

    let mut segments = Vec::with_capacity(manifests.len());
    for manifest in &manifests {
        let source = from.join(&manifest.relative_path);
        let destination = into.join(&manifest.relative_path);

        // A segment is immutable and content-addressed, so one already there
        // under the right address is the same segment.
        let already = std::fs::read(&destination)
            .ok()
            .filter(|bytes| bytes.len() > segment::format::PROLOGUE_BYTES)
            .map(|bytes| {
                segment::format::content_address(&bytes[segment::format::PROLOGUE_BYTES..])
                    == manifest.content_address
            })
            .unwrap_or(false);

        if !already {
            let bytes = std::fs::read(&source).map_err(|e| {
                StoreError::Unavailable(format!(
                    "The stored file for {} could not be read: {e}",
                    crate::row::hex(&manifest.segment_id)
                ))
            })?;
            std::fs::write(&destination, &bytes).map_err(|e| {
                StoreError::Unavailable(format!("A snapshot file could not be written: {e}"))
            })?;
        }
        segments.push((manifest.segment_id, manifest.content_address));
    }

    // The catalog and the erasure ledger both travel. The ledger is what stops
    // a restore from resurrecting an erased end user.
    for name in ["catalog.redb", "erasure.redb"] {
        let source = from.join("catalog").join(name);
        if source.is_file() {
            std::fs::create_dir_all(into.join("catalog")).ok();
            std::fs::copy(&source, into.join("catalog").join(name)).map_err(|e| {
                StoreError::Unavailable(format!("The stored index could not be copied: {e}"))
            })?;
        }
    }

    // The log travels too, and it travels **last**.
    //
    // Rows that are acknowledged but not yet in a segment live only in the log.
    // A snapshot that copied segments and the catalog alone would drop them and
    // still look complete, which FAILURE_MODES.md section 2 ranks below a
    // stopped request.
    //
    // The order matters when the directory is being written to. Copying the log
    // last means the copy can hold a frame whose receipt did not travel: that
    // batch replays, its rows are present, and a retry of it would count twice.
    // The other order loses the rows outright. Duplicated rows are recoverable
    // and lost rows are not.
    copy_directory(&from.join("wal"), &into.join("wal"))?;

    let (commit_watermark, log_checkpoint) = catalog.watermark()?;
    let snapshot = Snapshot {
        taken_at,
        manifest_generation: catalog.generation()?,
        tombstone_generation: catalog.tombstone_generation()?,
        commit_watermark,
        log_checkpoint,
        segments,
        erasures: catalog.erasure_ledger()?.len(),
    };

    std::fs::write(into.join("SNAPSHOT"), snapshot.to_text()).map_err(|e| {
        StoreError::Unavailable(format!(
            "The snapshot description could not be written: {e}"
        ))
    })?;
    Ok(snapshot)
}

/// What a restore found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub segments_restored: usize,
    /// Segments the snapshot names and the files do not hold, or that did not
    /// verify. A restore with any of these does not publish.
    pub segments_missing: Vec<String>,
    pub segments_damaged: Vec<String>,
    pub erasures_restored: usize,
}

impl RestoreReport {
    pub fn is_complete(&self) -> bool {
        self.segments_missing.is_empty() && self.segments_damaged.is_empty()
    }
}

/// Check a snapshot without changing anything.
///
/// This is the half of [`restore`] that reads, and it is separate because a
/// damaged backup is otherwise discovered at the worst moment there is: the
/// outage that needs it. An operator runs this any time, and a restore runs it
/// again before it writes.
///
/// The report counts what verified and names what did not.
pub fn verify(from: &Path) -> Result<RestoreReport, StoreError> {
    let mut report = RestoreReport::default();

    if !from.join("SNAPSHOT").is_file() {
        return Err(StoreError::InvalidArgument(format!(
            "There is no TallyOwl snapshot in {}. A snapshot directory holds a file named SNAPSHOT.",
            from.display()
        )));
    }

    let source_catalog = Catalog::open(from.join("catalog"))?;
    for manifest in &source_catalog.manifests()? {
        let path = from.join(&manifest.relative_path);
        let name = crate::row::hex(&manifest.segment_id);
        let Ok(bytes) = std::fs::read(&path) else {
            report.segments_missing.push(name);
            continue;
        };
        match segment::open(bytes, true) {
            Ok(segment) if segment.verify().is_ok() => report.segments_restored += 1,
            _ => report.segments_damaged.push(name),
        }
    }
    Ok(report)
}

/// The segment files one snapshot names, in catalog order.
///
/// The catalog is what names them, not the directory listing. A file that a
/// crash left beside a snapshot is not part of it, and adopting one would put
/// data into an installation that no manifest describes.
pub fn segment_files(from: &Path) -> Result<Vec<(String, PathBuf)>, StoreError> {
    let catalog = Catalog::open(from.join("catalog"))?;
    Ok(catalog
        .manifests()?
        .into_iter()
        .map(|manifest| {
            (
                crate::row::hex(&manifest.segment_id),
                from.join(&manifest.relative_path),
            )
        })
        .collect())
}

/// Restore a snapshot into a data directory.
///
/// **Every manifest and payload checksum is verified before anything is
/// published.** A missing or corrupt file causes a visible failure, and this
/// never silently skips a file: a partial restore would leave an installation
/// that looks healthy and answers wrongly, which FAILURE_MODES.md section 2
/// ranks above a stopped request.
pub fn restore(from: &Path, into: &Path) -> Result<RestoreReport, StoreError> {
    // Verify before publishing. Everything below this point is a copy; nothing
    // above it has touched the destination.
    let report = verify(from)?;
    if !report.is_complete() {
        // A restore that published anyway would produce an installation that
        // answers wrongly and says nothing.
        return Ok(report);
    }
    let mut report = report;

    let source_catalog = Catalog::open(from.join("catalog"))?;
    let manifests = source_catalog.manifests()?;

    for child in ["catalog", "segments"] {
        std::fs::create_dir_all(into.join(child)).map_err(|e| {
            StoreError::Unavailable(format!("The data directory could not be created: {e}"))
        })?;
    }
    for manifest in &manifests {
        std::fs::copy(
            from.join(&manifest.relative_path),
            into.join(&manifest.relative_path),
        )
        .map_err(|e| {
            StoreError::Unavailable(format!("A stored file could not be restored: {e}"))
        })?;
    }
    for name in ["catalog.redb", "erasure.redb"] {
        let source = from.join("catalog").join(name);
        if source.is_file() {
            std::fs::copy(&source, into.join("catalog").join(name)).map_err(|e| {
                StoreError::Unavailable(format!("The stored index could not be restored: {e}"))
            })?;
        }
    }
    // Rows that were acknowledged but not yet in a segment are in the log, and
    // a restore that left them behind would lose acknowledged data.
    copy_directory(&from.join("wal"), &into.join("wal"))?;

    // Section 9 rule 4: the erasure ledger travels with a restore, so a restore
    // cannot make an erased end user visible again.
    let restored = Catalog::open(into.join("catalog"))?;
    report.erasures_restored = restored.restore_tombstones_from_ledger()?;
    Ok(report)
}

/// What a rebuild put back, and what it did not.
///
/// The second half matters more. FAILURE_MODES.md section 7 lists nine things a
/// rebuild cannot restore, and two of them are not merely inconvenient: lost
/// tombstones resurrect erased data, and lost receipts duplicate on retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildReport {
    pub segments_found: usize,
    pub segments_unreadable: Vec<String>,
    pub generation: u64,
    /// Erasure records recovered from the independently durable ledger.
    pub erasures_recovered: usize,
    /// What a rebuild does not restore, in the words an operator needs.
    pub not_restored: Vec<&'static str>,
}

impl RebuildReport {
    /// The report as text, for a command line.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Rebuilt the list of stored files from {}.\n",
            count(self.segments_found, "file", "files")
        ));
        if !self.segments_unreadable.is_empty() {
            out.push_str(&format!(
                "{} could not be read and {} not in the rebuilt list:\n",
                count(self.segments_unreadable.len(), "file", "files"),
                if self.segments_unreadable.len() == 1 {
                    "is"
                } else {
                    "are"
                }
            ));
            for name in &self.segments_unreadable {
                out.push_str(&format!("  {name}\n"));
            }
        }
        out.push_str(&format!(
            "Recovered {} from the erasure record, which is kept separately.\n",
            count(self.erasures_recovered, "erasure record", "erasure records")
        ));
        out.push_str("\nThis did not restore:\n");
        for item in &self.not_restored {
            out.push_str(&format!("  {item}\n"));
        }
        out
    }
}

/// Rebuild the segment catalog by scanning the manifests on disk.
///
/// STORAGE.md section 3.3 and FAILURE_MODES.md procedure 5. The operator is
/// told what did not come back rather than discovering it.
pub fn rebuild(directory: &Path) -> Result<RebuildReport, StoreError> {
    let catalog = Catalog::open(directory.join("catalog"))?;
    let segments = directory.join("segments");

    let mut manifests: Vec<Manifest> = Vec::new();
    let mut unreadable = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&segments) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("tos") {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            let Ok(bytes) = std::fs::read(&path) else {
                unreadable.push(name);
                continue;
            };
            let byte_count = bytes.len() as u64;
            // A segment is self-describing, which is what makes this possible
            // at all. Its header holds everything a manifest needs.
            match segment::open(bytes, true) {
                Ok(found) => manifests.push(Manifest {
                    segment_id: found.header.segment_id,
                    content_address: found.content_address,
                    tablet_id: found.header.tablet_id,
                    virtual_shard: found.header.virtual_shard,
                    workspace_id: found.header.workspace_id,
                    project_id: found.header.project_id,
                    kinds: found.header.kinds.clone(),
                    occurred_range: found.header.occurred_range,
                    received_range: found.header.received_range,
                    committed_range: found.header.committed_range,
                    log_range: found.header.log_range,
                    row_count: found.header.row_count,
                    byte_count,
                    generation: 0,
                    tier: "local".into(),
                    relative_path: format!("segments/{name}"),
                }),
                Err(_) => unreadable.push(name),
            }
        }
    }

    let generation = catalog.rebuild_from_manifests(&manifests)?;
    let erasures = catalog.restore_tombstones_from_ledger()?;

    Ok(RebuildReport {
        segments_found: manifests.len(),
        segments_unreadable: unreadable,
        generation,
        erasures_recovered: erasures,
        // FAILURE_MODES.md section 7, in the words an operator needs rather
        // than the words the design uses.
        not_restored: vec![
            "Batch receipts. A delivery that was in flight may arrive twice.",
            "Workspace, project, and source settings.",
            "Keys and their permissions. Every application needs a new key.",
            "Node identity, so every node has to enrol again.",
            "Sign-in sessions. Everybody signs in again.",
            "Saved dashboards, queries, and alerts.",
            "Backup and export records.",
        ],
    })
}

/// A count with the right word beside it. A message that says "1 files" reads
/// as a defect even when the number is right.
pub fn count(number: usize, one: &str, many: &str) -> String {
    if number == 1 {
        format!("{number} {one}")
    } else {
        format!("{number} {many}")
    }
}

/// Where a snapshot's description lives.
pub fn description_path(snapshot: &Path) -> PathBuf {
    snapshot.join("SNAPSHOT")
}

/// Copy the files of one directory into another, one level deep.
fn copy_directory(from: &Path, into: &Path) -> Result<(), StoreError> {
    if !from.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(into)
        .map_err(|e| StoreError::Unavailable(format!("A directory could not be created: {e}")))?;
    let entries = std::fs::read_dir(from)
        .map_err(|e| StoreError::Unavailable(format!("A directory could not be read: {e}")))?;
    for entry in entries.flatten() {
        if entry.path().is_file() {
            std::fs::copy(entry.path(), into.join(entry.file_name()))
                .map_err(|e| StoreError::Unavailable(format!("A file could not be copied: {e}")))?;
        }
    }
    Ok(())
}
