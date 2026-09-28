//! The native store: the append log, the catalog, and immutable segments.
//!
//! This is the `Store` implementation `docs/PLAN.md` Phase 3 asks for, and it
//! replaces the Phase 1 directory-backed one without moving the seam.
//!
//! # The write path
//!
//! `docs/STORAGE.md` section 5, in order, and the order is the whole guarantee:
//!
//! 1. look up the stable batch ID in the receipt index;
//! 2. append a checksummed frame and fsync;
//! 3. atomically record the receipt and the log position in the catalog;
//! 4. return a committed receipt;
//! 5. segment committed ranges;
//! 6. publish complete segments in one catalog generation;
//! 7. advance the checkpoint and remove covered log ranges.
//!
//! Steps 2 and 3 are what make a crash safe. The frame is durable before the
//! receipt exists, so a crash between them replays the batch and deduplication
//! makes the replay one logical commit. A receipt written first would lose data
//! while reporting success.
//!
//! # The read path
//!
//! A query reads the open buffer and the live segments of one manifest
//! generation, and applies the visible tombstone generation. A tombstone is a
//! standing predicate rather than a one-time action, so a row that arrives after
//! an erasure and matches it never becomes visible.
//!
//! # What is not here yet
//!
//! The tablet locator, compaction, and the cold tier. `docs/IMPLEMENTATION_LOG.md`
//! L022 says where each goes. Until the locator exists an exact lookup opens
//! every candidate segment, which is correct and does not scale; the segment
//! filters already make most of those opens cheap.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::catalog::{Catalog, CatalogError, Manifest, Tombstone, TombstoneSet};
use crate::locator::Locator;
use crate::row::EventRow;
use crate::row_codec;
use crate::segment::format::FormatError;
use crate::segment::{self, Segment, SegmentWriter};
use crate::space::{Point, Space};
use crate::store::{CommitOutcome, Receipt, Scanned, Store, StoreError, TimeBasis, Trend};
use crate::wal::{GroupCommit, Wal, WalError};

/// How the store decides when to seal a segment.
///
/// STORAGE.md section 3.2 and D17: a microsegment seals at 1 second or 8 MiB,
/// and the home profile targets a 32 to 64 MiB segment. A test overrides both,
/// because a test that had to write 32 MiB to see one segment would be a
/// benchmark rather than a test.
#[derive(Debug, Clone, Copy)]
pub struct Sealing {
    /// Seal once the open buffer holds this many rows.
    pub max_open_rows: usize,
    /// Seal once the open buffer has held anything for this long.
    pub max_open_ms: i64,
    /// `verify-on-read` is the default, and D57 gives the three levels.
    pub verify_on_read: bool,
    /// Space held back so recovery can still write. `storage.reserveBytes`.
    /// FAILURE_MODES.md section 10.
    pub reserve_bytes: u64,
}

impl Default for Sealing {
    fn default() -> Sealing {
        Sealing {
            // Roughly a 32 MiB segment at the measured 39.75 bytes for each
            // event, which is the home-profile target D17 names.
            max_open_rows: 800_000,
            max_open_ms: 60_000,
            verify_on_read: true,
            reserve_bytes: crate::space::DEFAULT_RESERVE_BYTES,
        }
    }
}

/// The least a sealed row can cost on disk, for the question a seal asks the
/// device before it builds anything. The measured figure is about 40 bytes for
/// each event; this is deliberately far below it, because the exact check
/// still follows the build and this one only has to catch a device with no
/// room at all.
const SMALLEST_SEALED_ROW_BYTES: u64 = 8;

/// Room an erasure record needs. Generous on purpose: refusing an erasure
/// because the estimate was tight would be the wrong way to be wrong.
const ERASURE_RECORD_BYTES: u64 = 64 * 1024;

/// What adopting one transferred segment did.
///
/// `already_held` is what makes a transfer safe to repeat: a segment this
/// store already holds under the same content address is not published twice,
/// so a copy that stopped halfway is finished by running it again rather than
/// by an operator working out where it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Installed {
    pub segment_id: [u8; 16],
    pub row_count: u64,
    pub byte_count: u64,
    pub already_held: bool,
}

/// The native store.
pub struct SegmentedStore {
    directory: PathBuf,
    catalog: Catalog,
    wal: Arc<Wal>,
    sealing: Sealing,
    space: Space,
    state: Mutex<State>,
    /// Excludes a seal from the window between a commit's log append and the
    /// moment its rows reach the open buffer.
    ///
    /// **A seal computes the log range it covers from the log's own position**,
    /// and a commit is not instantaneous: it appends its frame, records its
    /// receipt, and only then puts its rows in the open buffer. A seal that
    /// drained inside that window would publish a segment claiming to cover the
    /// frame while holding none of its rows, and advance the checkpoint past
    /// it. The rows would still be in memory, so nothing would look wrong — but
    /// a process that died before the next seal would lose an **acknowledged**
    /// batch, which is the one thing `docs/DELIVERY.md` section 3 says cannot
    /// happen.
    ///
    /// A commit holds this for reading, so any number of them proceed at once.
    /// A seal holds it for writing across its drain alone, which is a few
    /// microseconds under the state lock and not the file writes.
    ///
    /// The lock order is always this gate, then `state`. Both paths take them
    /// in that order and neither holds this across a call that takes it again.
    append_gate: std::sync::RwLock<()>,
    /// One seal at a time.
    ///
    /// **Two seals can publish out of order, and the checkpoint cannot tell.**
    /// A seal drains the open buffer under `append_gate`, releases it, and then
    /// spends the expensive part building and writing files. A second seal can
    /// start in that window, drain the rows that arrived since, and reach the
    /// catalog first. `checkpoint` is the highest `log_range` end any published
    /// manifest names, so it would then step over the first seal's range — and
    /// `reclaim_through` would remove the only durable copy of rows that are
    /// still nothing but memory. A crash there loses an acknowledged batch.
    ///
    /// The lock order is this, then `append_gate`, then `state`. A commit takes
    /// this only after it has released the gate, which is why the inline seal in
    /// `commit` sits outside that block.
    sealing_turn: Mutex<()>,
    /// The combined locator, keyed by the manifest generation that built it.
    ///
    /// A lookup reads the generation — a sub-microsecond catalog read, D3 —
    /// and reuses the combination while it matches, instead of decoding and
    /// sorting every stored run per call, which L165 records as the cost that
    /// held a query for hours. A seal publishes its runs and its generation in
    /// one transaction, so the generation key is exact there; compaction
    /// replaces runs after its generation has already moved, so it
    /// invalidates this explicitly. The memory held is the combined locator
    /// itself. The lock is held across a rebuild so two concurrent misses
    /// build once.
    locator_cache: Mutex<Option<(u64, Arc<Locator>)>>,
    /// Seals that did not publish, for the metric that reports them. A store
    /// whose seals fail keeps answering from the log and keeps growing it, so
    /// this is the early sign of an append log that will not shrink.
    seal_failures: std::sync::atomic::AtomicU64,
    /// Reads that met a stored part that would not read back.
    integrity_failures: std::sync::atomic::AtomicU64,
    /// Compactions by outcome, restarts, and rows erased, for the sampler.
    compactions: [std::sync::atomic::AtomicU64; 3],
    compaction_restarts: std::sync::atomic::AtomicU64,
    rows_erased: std::sync::atomic::AtomicU64,
}

struct State {
    /// Rows committed to the log and not yet sealed into a segment. STORAGE.md
    /// section 9: a query can read recent data from a committed-log read view,
    /// so dashboard freshness does not depend on the segment size.
    open: Vec<EventRow>,
    /// Every workspace and project the open buffer holds rows for. A seal
    /// writes one segment for each, and it has to name those segments at the
    /// moment it drains, so the count is kept as rows arrive rather than
    /// worked out under the lock.
    open_projects: BTreeSet<([u8; 16], [u8; 16])>,
    /// When the open buffer took its first row.
    opened_at: i64,
    /// The log position the open buffer starts at.
    open_from: u64,
    watermark: u64,
    /// Rows a seal has drained and not yet published.
    ///
    /// **They are still the newest data this store holds, and a query has to
    /// see them.** A seal used to move them into a local variable, and for the
    /// whole build, write, and publish no query could: up to a minute of the
    /// newest data was missing from an answer that did not say so.
    sealing: Vec<SealingBatch>,
    /// Segments already opened, so a query does not re-read a file it just read.
    cached: HashMap<[u8; 16], Arc<Segment>>,
    /// What could not be read, by what it is. A query that needed one of these
    /// marks itself incomplete rather than returning a smaller answer.
    ///
    /// It is keyed, so one damaged file is one entry however many queries meet
    /// it, and an entry goes when the segment opens again or is retired.
    unreadable: BTreeMap<String, String>,
}

/// Rows between the open buffer and a published segment.
struct SealingBatch {
    /// The segments this seal will publish, all in one transaction. A reader
    /// that finds any of them in the catalog reads the segments and skips
    /// these rows; one that does not reads these rows. It is never both and
    /// never neither.
    segment_ids: Vec<[u8; 16]>,
    rows: Arc<Vec<EventRow>>,
}

/// One segment a seal will write: the workspace and project it holds, and the
/// identifier it will carry.
type PlannedSegment = (([u8; 16], [u8; 16]), [u8; 16]);

/// A sealing batch as a reader takes it: the identifiers, and the shared rows.
type SharedBatch = (Vec<[u8; 16]>, Arc<Vec<EventRow>>);

/// The key `State::unreadable` holds a segment under.
fn unreadable_key(segment_id: &[u8; 16]) -> String {
    format!("segment/{}", crate::row::hex(segment_id))
}

/// What a read found in memory: the open buffer and every batch a seal holds.
struct MemoryView {
    open: Vec<EventRow>,
    sealing: Vec<(Vec<[u8; 16]>, Vec<EventRow>)>,
}

impl SegmentedStore {
    /// The combined locator for the current manifest generation.
    fn cached_locator(&self) -> Result<Arc<Locator>, StoreError> {
        let generation = self.catalog.generation()?;
        let mut cache = self.locator_cache.lock().expect("locator cache lock");
        if let Some((held, locator)) = cache.as_ref() {
            if *held == generation {
                return Ok(Arc::clone(locator));
            }
        }
        let built = Arc::new(self.catalog.locator()?);
        *cache = Some((generation, Arc::clone(&built)));
        Ok(built)
    }

    /// Forget the cached combination.
    ///
    /// Compaction replaces the stored runs after its generation has already
    /// moved, so the generation alone cannot say the cache went stale there.
    fn invalidate_locator_cache(&self) {
        *self.locator_cache.lock().expect("locator cache lock") = None;
    }

    /// Open a data directory, recovering whatever is there.
    pub fn open(directory: impl AsRef<Path>) -> Result<SegmentedStore, StoreError> {
        SegmentedStore::open_with(directory, Sealing::default(), GroupCommit::default())
    }

    /// Open, waiting up to `timeout` for another process to release the
    /// directory.
    ///
    /// **One process owns one data directory.** The catalog takes an exclusive
    /// lock, because two processes writing one directory would corrupt it, and
    /// STORAGE.md section 4 describes a single-node layout with one owner.
    ///
    /// A restart is the ordinary reason to meet that lock: a container
    /// scheduler can start the replacement before the old process has finished
    /// exiting. Failing immediately would turn a routine restart into a crash
    /// loop, and waiting forever would hide a real conflict, so this waits a
    /// bounded time and then says plainly what it found.
    pub fn open_waiting(
        directory: impl AsRef<Path>,
        timeout: std::time::Duration,
    ) -> Result<SegmentedStore, StoreError> {
        let directory = directory.as_ref();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match SegmentedStore::open(directory) {
                Ok(store) => return Ok(store),
                Err(StoreError::Unavailable(message)) if std::time::Instant::now() < deadline => {
                    // Only a lock conflict is worth waiting on. A directory
                    // that cannot be created will not start working.
                    if !message.contains("Cannot acquire lock") {
                        return Err(StoreError::Unavailable(message));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(StoreError::Unavailable(message))
                    if message.contains("Cannot acquire lock") =>
                {
                    return Err(StoreError::Unavailable(format!(
                        "Another copy of TallyOwl is already using the data directory {}. \
                         Stop it before starting this one. Two copies writing one directory \
                         would damage the stored data.",
                        directory.display()
                    )))
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// Open, waiting for the lock, with settings.
    pub fn open_waiting_with(
        directory: impl AsRef<Path>,
        timeout: std::time::Duration,
        sealing: Sealing,
    ) -> Result<SegmentedStore, StoreError> {
        let directory = directory.as_ref();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match SegmentedStore::open_with(directory, sealing, GroupCommit::default()) {
                Ok(store) => return Ok(store),
                Err(StoreError::Unavailable(message)) if std::time::Instant::now() < deadline => {
                    if !message.contains("Cannot acquire lock") {
                        return Err(StoreError::Unavailable(message));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(StoreError::Unavailable(message))
                    if message.contains("Cannot acquire lock") =>
                {
                    return Err(StoreError::Unavailable(format!(
                        "Another copy of TallyOwl is already using the data directory {}. \
                         Stop it before starting this one. Two copies writing one directory \
                         would damage the stored data.",
                        directory.display()
                    )))
                }
                Err(other) => return Err(other),
            }
        }
    }

    pub fn open_with(
        directory: impl AsRef<Path>,
        sealing: Sealing,
        group_commit: GroupCommit,
    ) -> Result<SegmentedStore, StoreError> {
        let directory = directory.as_ref().to_path_buf();
        for child in ["catalog", "wal", "segments"] {
            std::fs::create_dir_all(directory.join(child)).map_err(|e| {
                StoreError::Unavailable(format!(
                    "The data directory {} could not be created: {e}",
                    directory.display()
                ))
            })?;
        }

        let catalog = Catalog::open(directory.join("catalog"))?;
        // The highest log position a receipt names is the highest this store
        // has acknowledged, and the log needs it to tell a torn tail from
        // damage that has acknowledged data beyond it.
        let (watermark, highest_logged) = catalog.watermark()?;
        let wal = Wal::open_acknowledged(
            directory.join("wal/tablet-0000.wal"),
            group_commit,
            (watermark > 0).then_some(highest_logged),
        )?;

        let store = SegmentedStore {
            space: Space::new(&directory, sealing.reserve_bytes),
            directory,
            catalog,
            wal,
            sealing,
            append_gate: std::sync::RwLock::new(()),
            sealing_turn: Mutex::new(()),
            locator_cache: Mutex::new(None),
            seal_failures: std::sync::atomic::AtomicU64::new(0),
            integrity_failures: std::sync::atomic::AtomicU64::new(0),
            compactions: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            compaction_restarts: std::sync::atomic::AtomicU64::new(0),
            rows_erased: std::sync::atomic::AtomicU64::new(0),
            state: Mutex::new(State {
                open: Vec::new(),
                open_projects: BTreeSet::new(),
                opened_at: 0,
                open_from: 0,
                watermark: 0,
                sealing: Vec::new(),
                cached: HashMap::new(),
                unreadable: BTreeMap::new(),
            }),
        };
        store.recover()?;
        Ok(store)
    }

    /// Replay the committed log range no segment covers yet.
    ///
    /// Procedure 1 of FAILURE_MODES.md section 11: automatic, and nothing
    /// acknowledged is lost. D53 puts it more strongly — a process crash is not
    /// a recovery event, because every write is atomic and a restart loses
    /// nothing that TallyOwl acknowledged.
    fn recover(&self) -> Result<(), StoreError> {
        let (watermark, highest_logged) = self.catalog.watermark()?;
        let checkpoint = match self.catalog.log_checkpoint()? {
            Some(checkpoint) => checkpoint,
            None => {
                // A catalog written before the checkpoint was its own record.
                // The old value was the highest log range any manifest named,
                // and a segment copied from another node names that node's
                // positions, so it cannot be trusted to say what this log has
                // sealed. The whole log is replayed once instead. A frame a
                // segment already holds becomes a second physical copy of rows
                // that count once (DELIVERY.md section 6); a frame that was
                // skipped would have been an acknowledged batch lost.
                self.catalog.record_log_checkpoint(0)?;
                0
            }
        };

        // **A position is never assigned twice.** The log learns its next
        // position from the frames it holds, and a log whose every frame was
        // reclaimed holds none, so it started again at zero: below the
        // checkpoint, where the next restart did not replay it and the next
        // seal reclaimed it. Both records of how far the log has reached are
        // in the catalog, and the log starts above them.
        let derived = self
            .catalog
            .manifests()?
            .iter()
            .map(|manifest| manifest.log_range.1.saturating_add(1))
            .max()
            .unwrap_or(0);
        let floor = if watermark == 0 {
            checkpoint
        } else {
            checkpoint.max(highest_logged.saturating_add(1))
        };
        self.wal.start_at_least(floor.max(derived));

        let frames = self.wal.replay(checkpoint)?;

        let mut state = self.state.lock().expect("store lock");
        state.watermark = watermark;
        state.open_from = checkpoint;
        for frame in frames {
            match row_codec::decode_rows(&frame.payload) {
                Ok(rows) => {
                    if state.opened_at == 0 && !rows.is_empty() {
                        state.opened_at = tallyowl_obs::time::now_ms();
                    }
                    for row in &rows {
                        state
                            .open_projects
                            .insert((row.workspace_id, row.project_id));
                    }
                    state.open.extend(rows);
                }
                Err(error) => {
                    // A frame that passed its own checksum and still will not
                    // decode is a fault this software cannot repair. It is
                    // named rather than skipped quietly.
                    state
                        .unreadable
                        .insert(format!("append-log/{:020}", frame.position), error.message);
                }
            }
        }
        Ok(())
    }

    /// The log position every locally sealed segment covers up to.
    ///
    /// The catalog holds it as its own record, and only a seal moves it. See
    /// `Catalog::publish_sealed`.
    fn checkpoint(&self) -> Result<u64, StoreError> {
        Ok(self.catalog.log_checkpoint()?.unwrap_or(0))
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The space rule for this data directory.
    ///
    /// FAILURE_MODES.md section 10. A test reaches this to pretend the device
    /// is full; a service reaches it to report free space and readiness.
    pub fn space(&self) -> &Space {
        &self.space
    }

    /// Refusals since the last call, for the metric that reports them.
    pub fn take_refusals(&self) -> [u64; 6] {
        self.space.take_refusals()
    }

    /// Report the real device again after a test pretended it was full.
    pub fn stop_pretending_the_device_is_full(&self) {
        self.space.stop_pretending();
    }

    /// May a control write happen?
    ///
    /// Section 10, catalog: stop accepting control writes and serve reads. A
    /// control write is refused once the device is inside the reserve, because
    /// the reserve is there for the metadata a recovery needs and a settings
    /// change is not that.
    pub fn guard_control_write(&self) -> Result<(), StoreError> {
        if self.space.is_inside_reserve() {
            return Err(StoreError::Exhausted(format!(
                "The device holding {} has only its reserve left, so settings cannot change. \
                 Queries still answer. Free space, then try again.",
                self.directory.display()
            )));
        }
        Ok(())
    }

    /// Take a snapshot of this store into `into`.
    ///
    /// The open buffer is sealed first, so every acknowledged row is in a
    /// segment the snapshot names rather than only in the log. A running node
    /// has to snapshot itself: one process owns one data directory, so a
    /// separate command cannot open the catalog while this one holds it.
    pub fn snapshot(
        &self,
        into: impl AsRef<Path>,
        taken_at: i64,
    ) -> Result<crate::snapshot::Snapshot, StoreError> {
        self.seal()?;
        crate::snapshot::take_with(&self.directory, into.as_ref(), taken_at, &self.catalog)
    }

    /// Seal the open buffer into a segment and publish it.
    ///
    /// Steps 5 to 7 of the write sequence. A crash anywhere in here leaves the
    /// log intact, because the checkpoint only moves inside the same catalog
    /// transaction that publishes the segment.
    /// Seal only when the open buffer has reached a seal condition.
    ///
    /// **This is what a background segmenter calls.** `seal` seals whatever is
    /// open, which is right for a caller that has decided; this asks the same
    /// question `commit` asks and is a no-op when the answer is no.
    pub fn seal_if_due(&self) -> Result<Option<[u8; 16]>, StoreError> {
        {
            let state = self.state.lock().expect("store lock");
            if !self.should_seal(&state) {
                return Ok(None);
            }
        }
        self.seal()
    }

    pub fn seal(&self) -> Result<Option<[u8; 16]>, StoreError> {
        self.seal_observed(&|| {})
    }

    /// Seal, and call `while_building` after the drain and before the publish.
    ///
    /// A test reads the store from inside that window, which is the only way
    /// to show what a query sees there without racing a clock.
    fn seal_observed(&self, while_building: &dyn Fn()) -> Result<Option<[u8; 16]>, StoreError> {
        // One seal at a time. See `sealing_turn` for what two of them do to the
        // checkpoint, and through it to the append log.
        let _turn = self.sealing_turn.lock().expect("sealing turn");

        // FAILURE_MODES.md section 10, segment publish. A device that cannot
        // take even the smallest segment these rows could make is asked before
        // anything is drained or built. Near a full device every commit past
        // the seal threshold used to build a whole segment and then throw it
        // away.
        let waiting = self.state.lock().expect("store lock").open.len() as u64;
        if waiting == 0 {
            return Ok(None);
        }
        self.space
            .check_bulk(Point::SegmentPublish, waiting * SMALLEST_SEALED_ROW_BYTES)?;

        let (rows, plan, from, to, opened_at) = {
            // No commit may be between its log append and its open extend while
            // this range is computed. See `append_gate`.
            let _gate = self.append_gate.write().expect("append gate");
            let mut state = self.state.lock().expect("store lock");
            if state.open.is_empty() {
                return Ok(None);
            }
            let rows = Arc::new(std::mem::take(&mut state.open));
            // A segment holds one project, because the header prunes by project
            // and a segment that mixed two could not.
            let projects = std::mem::take(&mut state.open_projects);
            let segment_ids: Vec<[u8; 16]> = projects.iter().map(|_| new_segment_id()).collect();
            let from = state.open_from;
            let to = self.wal.next_position().saturating_sub(1);
            let opened_at = state.opened_at;
            state.open_from = to + 1;
            state.opened_at = 0;
            // The rows stay answerable while this seal builds. See
            // `State::sealing`.
            state.sealing.push(SealingBatch {
                segment_ids: segment_ids.clone(),
                rows: Arc::clone(&rows),
            });
            let plan: Vec<PlannedSegment> = projects.into_iter().zip(segment_ids).collect();
            (rows, plan, from, to, opened_at)
        };

        while_building();

        // **Every way out of here that did not publish puts the rows back.**
        // They used to go back only when the device refused for space. A write
        // that failed for any other reason dropped them, the next seal
        // published a later range, the checkpoint stepped over this one, and
        // the log reclaimed the only copy of rows TallyOwl had acknowledged.
        match self.publish_sealed(&rows, &plan, from, to) {
            Ok(written) => {
                // The catalog names the segments now. A reader that still
                // finds this batch also finds its segments, and reads those.
                let mut state = self.state.lock().expect("store lock");
                state
                    .sealing
                    .retain(|batch| !Arc::ptr_eq(&batch.rows, &rows));
                drop(state);

                // The published range is covered now, so it can go. STORAGE.md
                // section 5 step 7. This happens after the publish, so a crash
                // between them leaves a log that replays into rows a segment
                // already holds, and deduplication is what makes that harmless.
                //
                // Only the published prefix goes. A commit that landed while
                // this seal was building sits above `to`, and its frame is the
                // only durable copy of an acknowledged batch until a later seal
                // takes it.
                let checkpoint = self.checkpoint()?;
                if checkpoint > 0 {
                    self.wal.reclaim_through(checkpoint - 1)?;
                }
                Ok(written)
            }
            Err(failed) => {
                self.seal_failures
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.reopen(rows, from, opened_at);
                Err(failed)
            }
        }
    }

    /// Build, write, and publish one drained batch. Nothing here touches the
    /// open buffer, so the caller can put the rows back on any error.
    fn publish_sealed(
        &self,
        rows: &Arc<Vec<EventRow>>,
        plan: &[PlannedSegment],
        from: u64,
        to: u64,
    ) -> Result<Option<[u8; 16]>, StoreError> {
        let mut manifests = Vec::new();
        let mut locator = Locator::new();
        let mut written = None;
        // Every segment is built before any of it is written. The device may
        // refuse the whole seal, and a seal that had already written half of
        // its files would leave orphans a later rebuild would adopt.
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for ((workspace_id, project_id), segment_id) in plan {
            // The common store holds one project, and then the batch is the
            // segment. Otherwise one project's rows are copied out, written,
            // and dropped before the next, so the extra memory is one
            // project's rows and never the whole batch again.
            let copied: Vec<EventRow>;
            let project_rows: &[EventRow] = if plan.len() == 1 {
                rows.as_slice()
            } else {
                copied = rows
                    .iter()
                    .filter(|row| {
                        row.workspace_id == *workspace_id && row.project_id == *project_id
                    })
                    .cloned()
                    .collect();
                &copied
            };
            // One reference for each (value, segment) pair, which is what sets
            // the locator's size. A value on a thousand rows of this segment
            // becomes one entry when the run is sealed.
            for row in project_rows {
                index_row(&mut locator, row, *segment_id);
            }
            let mut writer = SegmentWriter::new(*segment_id, *workspace_id, *project_id);
            writer.log_range = (from, to);
            let segment = writer.write(project_rows)?;

            let relative = format!("segments/{}.tos", crate::row::hex(segment_id));
            files.push((relative.clone(), segment.bytes.clone()));

            manifests.push(Manifest {
                segment_id: *segment_id,
                content_address: segment.content_address,
                tablet_id: 0,
                virtual_shard: 0,
                workspace_id: *workspace_id,
                project_id: *project_id,
                kinds: segment.header.kinds.clone(),
                occurred_range: segment.header.occurred_range,
                received_range: segment.header.received_range,
                committed_range: segment.header.committed_range,
                log_range: (from, to),
                row_count: segment.header.row_count,
                byte_count: segment.bytes.len() as u64,
                generation: 0,
                tier: "local".into(),
                relative_path: relative,
            });
            written = Some(*segment_id);
        }

        if manifests.is_empty() {
            return Ok(None);
        }

        // FAILURE_MODES.md section 10, segment publish: keep the log range. The
        // log is the durable record until a segment replaces it, so a refusal
        // here loses nothing. The rows go back in the open buffer and the next
        // seal tries again.
        let needed: u64 = files.iter().map(|(_, bytes)| bytes.len() as u64).sum();
        self.space.check_bulk(Point::SegmentPublish, needed)?;

        for (relative, bytes) in &files {
            write_atomically(&self.directory.join(relative), bytes)?;
        }

        locator.seal();
        // One transaction: the segments, the locator runs that describe them,
        // and the checkpoint that says the log range is covered. A generation
        // never holds a run that disagrees with its segments, and the
        // checkpoint never says a range is covered before its segments exist.
        self.catalog
            .publish_sealed(&manifests, &locator, to.saturating_add(1))?;
        Ok(written)
    }

    /// Put an unpublished seal's rows back in the open buffer.
    ///
    /// The rows are still in the append log, so nothing is lost either way.
    /// This is what keeps them answerable to a query in the meantime, and what
    /// keeps the log checkpoint where it was.
    fn reopen(&self, rows: Arc<Vec<EventRow>>, from: u64, opened_at: i64) {
        let mut state = self.state.lock().expect("store lock");
        state
            .sealing
            .retain(|batch| !Arc::ptr_eq(&batch.rows, &rows));
        // A reader may still hold the batch for a moment, and then the rows are
        // copied rather than moved.
        let rows = Arc::try_unwrap(rows).unwrap_or_else(|shared| shared.as_ref().clone());
        for row in &rows {
            state
                .open_projects
                .insert((row.workspace_id, row.project_id));
        }
        // A commit may have landed while this seal was building. Those rows are
        // newer, so the refused ones go in front of them.
        let newer = std::mem::take(&mut state.open);
        state.open = rows;
        state.open.extend(newer);
        state.open_from = from;
        state.opened_at = if opened_at == 0 {
            tallyowl_obs::time::now_ms()
        } else {
            opened_at
        };
    }

    /// Whether the open buffer has reached a seal condition.
    fn should_seal(&self, state: &State) -> bool {
        if state.open.is_empty() {
            return false;
        }
        state.open.len() >= self.sealing.max_open_rows
            || tallyowl_obs::time::now_ms() - state.opened_at >= self.sealing.max_open_ms
    }

    /// The live segments a read needs, opened and cached, and what could not
    /// be opened.
    ///
    /// **The manifest decides before any file is read.** This used to open
    /// every live segment and let the caller prune afterwards, so the first
    /// query after a restart read the whole store, and it did so while holding
    /// the lock a commit needs. The catalog already holds each segment's
    /// project and time ranges, so a segment the read cannot use is never
    /// opened, and a file is read with no lock held.
    ///
    /// The second value names each wanted segment that would not open. The
    /// caller is incomplete exactly when it is not empty: a damaged segment in
    /// another project, or outside the range, is not this read's problem.
    fn segments_where(
        &self,
        wanted: &dyn Fn(&Manifest) -> bool,
    ) -> Result<(Vec<Arc<Segment>>, Vec<String>), StoreError> {
        let manifests: Vec<Manifest> = self
            .catalog
            .manifests()?
            .into_iter()
            .filter(|manifest| wanted(manifest))
            .collect();

        let mut found: HashMap<[u8; 16], Arc<Segment>> = HashMap::new();
        {
            let state = self.state.lock().expect("store lock");
            for manifest in &manifests {
                if let Some(segment) = state.cached.get(&manifest.segment_id) {
                    found.insert(manifest.segment_id, Arc::clone(segment));
                }
            }
        }

        // No lock is held across a read or a parse. A parse that panicked
        // under the store lock used to poison it, and every commit after that
        // panicked too.
        let mut opened: Vec<([u8; 16], Arc<Segment>)> = Vec::new();
        let mut failed: Vec<([u8; 16], String)> = Vec::new();
        for manifest in &manifests {
            if found.contains_key(&manifest.segment_id) {
                continue;
            }
            let path = self.directory.join(&manifest.relative_path);
            let outcome = std::fs::read(&path)
                .map_err(|error| {
                    // Procedure 6 of section 11: with no other copy the segment
                    // stays damaged, and a query over its range returns
                    // `incomplete-result` and names it.
                    format!(
                        "The stored file for {} could not be read: {error}",
                        crate::row::hex(&manifest.segment_id)
                    )
                })
                .and_then(|bytes| {
                    segment::open(bytes, self.sealing.verify_on_read)
                        .map_err(|error| error.to_string())
                });
            match outcome {
                Ok(segment) => opened.push((manifest.segment_id, Arc::new(segment))),
                Err(reason) => failed.push((manifest.segment_id, reason)),
            }
        }

        if !failed.is_empty() {
            self.integrity_failures
                .fetch_add(failed.len() as u64, std::sync::atomic::Ordering::Relaxed);
        }
        if !opened.is_empty() || !failed.is_empty() {
            let mut state = self.state.lock().expect("store lock");
            for (segment_id, segment) in &opened {
                state.cached.insert(*segment_id, Arc::clone(segment));
                state.unreadable.remove(&unreadable_key(segment_id));
            }
            for (segment_id, reason) in &failed {
                state
                    .unreadable
                    .insert(unreadable_key(segment_id), reason.clone());
            }
        }
        for (segment_id, segment) in opened {
            found.insert(segment_id, segment);
        }

        let segments = manifests
            .iter()
            .filter_map(|manifest| found.remove(&manifest.segment_id))
            .collect();
        Ok((
            segments,
            failed.into_iter().map(|(_, reason)| reason).collect(),
        ))
    }

    /// Note that one segment's pages would not read, and give the reason back.
    fn note_unreadable(&self, segment_id: &[u8; 16], reason: String) -> String {
        self.integrity_failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut state = self.state.lock().expect("store lock");
        state
            .unreadable
            .insert(unreadable_key(segment_id), reason.clone());
        reason
    }

    /// Why this store cannot read part of what it holds.
    ///
    /// A query over a damaged range answers `incomplete-result` rather than a
    /// smaller number, and a person then needs to know **which** part. Without
    /// this the refusal is true and unactionable: FAILURE_MODES.md procedure 6
    /// says the segment stays damaged and is named, and this is the naming.
    pub fn unreadable(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("store lock")
            .unreadable
            .values()
            .cloned()
            .collect()
    }

    /// How many stored parts are known to be unreadable now.
    pub fn unreadable_count(&self) -> usize {
        self.state.lock().expect("store lock").unreadable.len()
    }

    /// Seals that did not publish since the last call.
    pub fn take_seal_failures(&self) -> u64 {
        self.seal_failures
            .swap(0, std::sync::atomic::Ordering::Relaxed)
    }

    /// Reads that met a stored part that would not read, since the last call.
    pub fn take_integrity_failures(&self) -> u64 {
        self.integrity_failures
            .swap(0, std::sync::atomic::Ordering::Relaxed)
    }

    /// Rows in memory that no segment holds yet, and how long the oldest of
    /// them has waited. A seal that keeps failing shows here first.
    pub fn open_buffer(&self) -> (u64, i64) {
        let state = self.state.lock().expect("store lock");
        let rows = state.open.len()
            + state
                .sealing
                .iter()
                .map(|batch| batch.rows.len())
                .sum::<usize>();
        let age = if state.opened_at == 0 {
            0
        } else {
            (tallyowl_obs::time::now_ms() - state.opened_at).max(0)
        };
        (rows as u64, age)
    }

    /// The combined locator, for compaction to ask which segments hold a value.
    pub(crate) fn locator_view(&self) -> Result<Arc<Locator>, StoreError> {
        self.cached_locator()
    }

    /// When the oldest row that no segment holds yet was committed.
    pub(crate) fn oldest_unsealed_commit(&self) -> Option<i64> {
        let state = self.state.lock().expect("store lock");
        state
            .sealing
            .iter()
            .filter_map(|batch| batch.rows.first())
            .chain(state.open.first())
            .map(|row| row.committed_at)
            .min()
    }

    /// Count what one compaction did, for the sampler to report.
    pub(crate) fn note_compaction(
        &self,
        result: &Result<crate::compact::CompactionOutcome, StoreError>,
    ) {
        use std::sync::atomic::Ordering::Relaxed;
        match result {
            Ok(outcome) if outcome.abandoned_for_space => {
                self.compactions[1].fetch_add(1, Relaxed);
            }
            Ok(_) => {
                self.compactions[0].fetch_add(1, Relaxed);
            }
            Err(_) => {
                self.compactions[2].fetch_add(1, Relaxed);
            }
        }
        if let Ok(outcome) = result {
            self.compaction_restarts
                .fetch_add(outcome.restarts as u64, Relaxed);
            self.rows_erased.fetch_add(outcome.rows_erased, Relaxed);
        }
    }

    /// Compactions since the last call, as completed, abandoned for space, and
    /// failed; then restarts; then rows erased.
    pub fn take_compaction_counts(&self) -> ([u64; 3], u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            std::array::from_fn(|index| self.compactions[index].swap(0, Relaxed)),
            self.compaction_restarts.swap(0, Relaxed),
            self.rows_erased.swap(0, Relaxed),
        )
    }

    /// Bytes the segment cache holds.
    pub fn cached_segment_bytes(&self) -> u64 {
        self.state
            .lock()
            .expect("store lock")
            .cached
            .values()
            .map(|segment| segment.bytes.len() as u64)
            .sum()
    }

    /// Whether the append log holds a frame that would not decode. No project
    /// can be ruled out of that, so every read is incomplete while it stands.
    fn append_log_is_unreadable(&self) -> bool {
        self.state
            .lock()
            .expect("store lock")
            .unreadable
            .keys()
            .any(|key| key.starts_with("append-log/"))
    }

    /// What memory holds, captured in one section.
    ///
    /// **The order a read takes is memory first and the catalog second.** A
    /// seal publishes and then drops its batch. A read that looked at the
    /// catalog first could see neither the segments nor the batch; this way it
    /// sees the batch, or the segments, or both, and `unpublished` settles
    /// both.
    fn memory_view(&self, keep: &dyn Fn(&EventRow) -> bool) -> MemoryView {
        let (open, batches) = {
            let state = self.state.lock().expect("store lock");
            let open: Vec<EventRow> = state.open.iter().filter(|row| keep(row)).cloned().collect();
            let batches: Vec<SharedBatch> = state
                .sealing
                .iter()
                .map(|batch| (batch.segment_ids.clone(), Arc::clone(&batch.rows)))
                .collect();
            (open, batches)
        };
        // The batch is shared, so it is filtered with no lock held.
        let sealing = batches
            .into_iter()
            .map(|(segment_ids, rows)| {
                (
                    segment_ids,
                    rows.iter().filter(|row| keep(row)).cloned().collect(),
                )
            })
            .collect();
        MemoryView { open, sealing }
    }

    /// The rows of a memory view that no published segment holds.
    ///
    /// A seal publishes all of its segments in one transaction, so one of its
    /// identifiers in the catalog means all of its rows are in segments.
    fn unpublished(&self, view: MemoryView) -> Result<Vec<EventRow>, StoreError> {
        let mut rows = view.open;
        if view.sealing.is_empty() {
            return Ok(rows);
        }
        let published: std::collections::HashSet<[u8; 16]> = self
            .catalog
            .manifests()?
            .iter()
            .map(|manifest| manifest.segment_id)
            .collect();
        for (segment_ids, batch) in view.sealing {
            if !segment_ids.iter().any(|id| published.contains(id)) {
                rows.extend(batch);
            }
        }
        Ok(rows)
    }

    /// The predicates a read applies.
    fn tombstones(&self) -> Result<Arc<TombstoneSet>, StoreError> {
        Ok(self.catalog.tombstone_set()?)
    }

    /// Commit a tombstone and advance the visible generation.
    ///
    /// The acknowledgement follows the durable write and never precedes it, so
    /// a crash cannot leave a person told their data is gone when it is not.
    /// See FAILURE_MODES.md section 9.
    pub fn erase(&self, tombstone: &Tombstone) -> Result<u64, StoreError> {
        if !tombstone.names_something() {
            return Err(StoreError::InvalidArgument(
                "This erasure request names nothing to remove. Name the events, the person, \
                 or the time range to remove."
                    .to_string(),
            ));
        }
        // An erasure is a recovery write, not a control write. It is small, it
        // is a legal obligation, and FAILURE_MODES.md section 9 makes the
        // erasure record durable independently of everything else. It may
        // therefore use the reserve, and it is refused only when the device has
        // nothing left at all.
        self.space
            .check_recovery(Point::Catalog, ERASURE_RECORD_BYTES)?;
        Ok(self.catalog.commit_tombstone(tombstone)?)
    }

    pub fn tombstone_generation(&self) -> Result<u64, StoreError> {
        Ok(self.catalog.tombstone_generation()?)
    }

    /// Bytes the append log holds that no segment covers yet. A rising value
    /// means the segmenter is behind the writer.
    pub fn unsealed_bytes(&self) -> u64 {
        self.wal.durable_bytes()
    }

    /// What the append log has done, for the capacity report.
    pub fn wal_statistics(&self) -> crate::wal::WalStatistics {
        self.wal.statistics()
    }

    /// What the append log's internal state says about itself. See
    /// [`crate::wal::Wal::state`], and L131.
    ///
    /// **A caller that cannot get an answer has learned the most useful thing.**
    /// This takes the log's lock, so a stall detector that times out waiting for
    /// it knows the log is held and by whom to look for next.
    pub fn append_log_state(&self) -> crate::wal::WalState {
        self.wal.state()
    }

    /// Why the append log stopped accepting writes, when it has.
    ///
    /// FAILURE_MODES.md section 10 asks for readiness to fail before the device
    /// is full. This is the other half: readiness after it did.
    pub fn append_log_failure(&self) -> Option<String> {
        self.wal.failure()
    }

    /// How many segments this store holds.
    pub fn segment_count(&self) -> usize {
        self.catalog.manifests().map(|m| m.len()).unwrap_or(0)
    }

    /// Every row the caller can see, from the segments and the open buffer.
    fn visible_rows(
        &self,
        project_id: Option<[u8; 16]>,
        range: Option<(i64, i64, TimeBasis)>,
    ) -> Result<(Vec<EventRow>, bool), StoreError> {
        let tombstones = self.tombstones()?;
        let in_scope = |row: &EventRow| {
            project_id.is_none_or(|project| row.project_id == project)
                && range.is_none_or(|(start, end, basis)| {
                    let at = time_of(row, basis);
                    at >= start && at < end
                })
        };

        // Memory first, then the catalog. See `memory_view`.
        let memory = self.memory_view(&in_scope);
        let mut out = self.unpublished(memory)?;

        // The manifest prunes without opening a file, which is the whole
        // reason it holds a project and a time range for each basis.
        let (segments, failed) = self.segments_where(&|manifest: &Manifest| {
            project_id.is_none_or(|project| manifest.project_id == project)
                && range.is_none_or(|(start, end, basis)| {
                    let (low, high) = match basis {
                        TimeBasis::OccurredAt => manifest.occurred_range,
                        TimeBasis::ReceivedAt => manifest.received_range,
                        TimeBasis::CommittedAt => manifest.committed_range,
                    };
                    low < end && high >= start
                })
        })?;
        let mut incomplete = !failed.is_empty() || self.append_log_is_unreadable();

        for segment in segments {
            match segment.rows(self.sealing.verify_on_read) {
                Ok(rows) => out.extend(rows.into_iter().filter(|row| in_scope(row))),
                Err(error) => {
                    incomplete = true;
                    self.note_unreadable(&segment.header.segment_id, error.to_string());
                }
            }
        }

        // The visible tombstone generation, applied on read. A row that arrived
        // after the erasure and matches it is hidden here even though no
        // compaction has touched it yet, which is what makes a tombstone a
        // standing predicate rather than a one-time action.
        if !tombstones.is_empty() {
            out.retain(|row| !tombstones.hides(row));
        }
        Ok((out, incomplete))
    }
}

fn time_of(row: &EventRow, basis: TimeBasis) -> i64 {
    match basis {
        TimeBasis::OccurredAt => row.occurred_at,
        TimeBasis::ReceivedAt => row.received_at,
        TimeBasis::CommittedAt => row.committed_at,
    }
}

/// Write to a temporary name, fsync, and rename.
///
/// SEGMENT_FORMAT.md section 13. A crash can leave a complete file with no
/// catalog reference, and recovery verifies it and then adopts or removes it.
/// The catalog must never refer to a partial file, which is what the rename
/// gives.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    use std::io::Write;

    let temporary = path.with_extension("writing");
    let mut file = std::fs::File::create(&temporary).map_err(|e| {
        StoreError::Unavailable(format!(
            "The stored file {} could not be created: {e}",
            temporary.display()
        ))
    })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| {
            StoreError::Unavailable(format!("The stored file could not be written: {e}"))
        })?;
    drop(file);

    std::fs::rename(&temporary, path).map_err(|e| {
        StoreError::Unavailable(format!(
            "The stored file {} could not be put in place: {e}",
            path.display()
        ))
    })?;

    // The rename itself needs to be durable, or a crash can lose a file that
    // the catalog already refers to. A failure here fails the write: the caller
    // then publishes nothing, and a seal puts its rows back.
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| {
                StoreError::Unavailable(format!(
                    "The directory {} could not be flushed to disk, so the stored file {} \
                     might not survive a power loss: {e}",
                    parent.display(),
                    path.display()
                ))
            })?;
    }
    Ok(())
}

/// Put one row's exact-lookup values into a locator run.
///
/// **This is one function because it was two, and the two went out of step.**
/// The seal path indexed `trace_id` and `request_id` and the compaction path
/// did not. The locator *prunes*: [`Store::lookup_correlated`] skips a segment
/// that a non-empty candidate set does not name. So a compacted segment that
/// held a trace was pruned out of that trace's lookup whenever any other
/// segment still named the same trace, and the answer came back smaller with
/// nothing marked incomplete. That is the failure `docs/FAILURE_MODES.md`
/// section 2 ranks worst, and a locator has to be built in one place for the
/// same reason a checksum has to be computed in one place.
pub fn index_row(locator: &mut Locator, row: &EventRow, segment_id: [u8; 16]) {
    use crate::segment::schema;

    locator.add(schema::EVENT_ID, &row.event_id, row.occurred_at, segment_id);
    if let Some(trace) = row.trace_id {
        locator.add(schema::TRACE_ID, &trace, row.occurred_at, segment_id);
    }
    if let Some(session) = &row.session_id {
        locator.add(
            schema::SESSION_ID,
            session.as_bytes(),
            row.occurred_at,
            segment_id,
        );
    }
    if let Some(request) = &row.request_id {
        locator.add(
            schema::REQUEST_ID,
            request.as_bytes(),
            row.occurred_at,
            segment_id,
        );
    }
    // A dynamic scalar field defaults to exact `lookup` indexing, so an
    // unexpected correlation property stays instantly usable. See D20.
    for (key, (value, _)) in &row.properties {
        locator.add(
            &format!("{}{key}", schema::PROPERTY_PREFIX),
            value.to_display().as_bytes(),
            row.occurred_at,
            segment_id,
        );
    }
}

/// A segment identifier: a millisecond timestamp and a counter, so identifiers
/// sort by time and do not repeat inside one millisecond.
fn new_segment_id() -> [u8; 16] {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut out = [0u8; 16];
    out[0..8].copy_from_slice(&(tallyowl_obs::time::now_ms() as u64).to_be_bytes());
    out[8..16].copy_from_slice(&COUNTER.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    out
}

/// Whether one row carries an exact value in one column.
///
/// The locator prunes and never decides, so this is what actually answers. A
/// property is compared through its display form, which is the same form the
/// locator hashed.
fn row_carries(row: &EventRow, column: &str, value: &[u8]) -> bool {
    use crate::segment::schema;
    if let Some(key) = column.strip_prefix(schema::PROPERTY_PREFIX) {
        return row
            .properties
            .get(key)
            .is_some_and(|(held, _)| held.to_display().as_bytes() == value);
    }
    match column {
        schema::EVENT_ID => row.event_id.as_slice() == value,
        schema::BATCH_ID => row.batch_id.as_slice() == value,
        schema::SOURCE_ID => row.source_id.as_slice() == value,
        schema::TRACE_ID => row.trace_id.is_some_and(|id| id.as_slice() == value),
        schema::SESSION_ID => row
            .session_id
            .as_ref()
            .is_some_and(|id| id.as_bytes() == value),
        schema::REQUEST_ID => row
            .request_id
            .as_ref()
            .is_some_and(|id| id.as_bytes() == value),
        schema::SERVICE_NAME => row
            .service_name
            .as_ref()
            .is_some_and(|name| name.as_bytes() == value),
        schema::RELEASE => row
            .release
            .as_ref()
            .is_some_and(|release| release.as_bytes() == value),
        schema::KIND => row.kind.as_bytes() == value,
        schema::NAME => row.name.as_bytes() == value,
        _ => false,
    }
}

impl From<CatalogError> for StoreError {
    fn from(error: CatalogError) -> StoreError {
        match error {
            CatalogError::Unavailable(m) => StoreError::Unavailable(m),
            CatalogError::Damaged(m) => StoreError::Damaged(m),
            CatalogError::Unsupported(m) => StoreError::InvalidArgument(m),
        }
    }
}

impl From<WalError> for StoreError {
    fn from(error: WalError) -> StoreError {
        match error {
            WalError::Unavailable(m) => StoreError::Unavailable(m),
            WalError::Damaged(m) => StoreError::Damaged(m),
        }
    }
}

impl From<FormatError> for StoreError {
    fn from(error: FormatError) -> StoreError {
        match error {
            FormatError::Damaged(m) => StoreError::Damaged(m),
            FormatError::Incomplete(m) => StoreError::Damaged(m),
            FormatError::Unsupported(m) => StoreError::InvalidArgument(m),
        }
    }
}

impl Store for SegmentedStore {
    fn commit(
        &self,
        source_id: [u8; 16],
        batch_id: [u8; 16],
        rows: Vec<EventRow>,
    ) -> Result<CommitOutcome, StoreError> {
        // 1. The prior receipt answers first. A retry after a lost
        //    acknowledgement must give one logical commit.
        if let Some(prior) = self.catalog.receipt(source_id, batch_id)? {
            return Ok(CommitOutcome {
                accepted: prior.accepted,
                committed_at: prior.committed_at,
                commit_watermark: prior.commit_watermark,
                deduplicated: true,
            });
        }

        let committed_at = tallyowl_obs::time::now_ms();
        let stamped: Vec<EventRow> = rows
            .into_iter()
            .map(|mut row| {
                row.batch_id = batch_id;
                row.source_id = source_id;
                row.committed_at = committed_at;
                row
            })
            .collect();

        // The watermark is allocated once, under the lock, before anything
        // else. Reading it and writing it back in two separate acquisitions
        // let two concurrent commits share one number, which would make two
        // batches indistinguishable to a query that states the watermark its
        // result applies to. A test caught it.
        //
        // A commit that then fails leaves a gap. That is correct: a watermark
        // is a monotonic counter and never a dense sequence, and the catalog
        // holds the highest one that actually committed.
        // FAILURE_MODES.md section 10, append log: never accept a write that
        // cannot be made durable. This is before the watermark and before the
        // log append, so a refused batch consumes no number and leaves no
        // trace. A retry of a batch that already committed answered from its
        // receipt above and never reaches here, so a full device does not
        // break deduplication.
        let payload = row_codec::encode_rows(&stamped);
        self.space
            .check_bulk(Point::AppendLog, payload.len() as u64)?;

        let watermark = {
            let mut state = self.state.lock().expect("store lock");
            state.watermark += 1;
            state.watermark
        };

        // 2. The frame is durable before the receipt exists. A crash between
        //    them replays the batch, and deduplication makes the replay one
        //    logical commit. The other order would lose data while reporting
        //    success.
        let accepted = stamped.len() as u64;

        // The whole window from the log append to the open extend is held
        // against a seal. See `append_gate`. The guard is dropped before the
        // inline seal below, because a seal takes the same gate for writing.
        let seal_now = {
            let _gate = self.append_gate.read().expect("append gate");

            let position = self.wal.append(&payload)?;

            // 3. The receipt and the log position, atomically.
            self.catalog.commit_receipt(&crate::catalog::Receipt {
                source_id,
                batch_id,
                accepted,
                committed_at,
                commit_watermark: watermark,
                log_position: position,
            })?;

            let mut state = self.state.lock().expect("store lock");
            if state.open.is_empty() {
                state.opened_at = committed_at;
            }
            for row in &stamped {
                // A batch is nearly always one project, so this asks the set
                // only when the project changes.
                let project = (row.workspace_id, row.project_id);
                if !state.open_projects.contains(&project) {
                    state.open_projects.insert(project);
                }
            }
            state.open.extend(stamped);
            self.should_seal(&state)
        };

        // 5 to 7, when the open buffer has reached a seal condition. This is
        // synchronous here; STORAGE.md calls it asynchronous, and a background
        // segmenter is a later change that moves no contract.
        if seal_now {
            match self.seal() {
                Ok(_) => {}
                // The batch is durable in the log already. Section 10 says the
                // log is the durable record until a segment replaces it, so a
                // publish the device refused is not a failed commit. The rows
                // stay open, stay answerable, and seal when there is room.
                Err(StoreError::Exhausted(_)) => {}
                Err(other) => return Err(other),
            }
        }

        // 4. The committed receipt.
        Ok(CommitOutcome {
            accepted,
            committed_at,
            commit_watermark: watermark,
            deduplicated: false,
        })
    }

    fn receipt(&self, source_id: [u8; 16], batch_id: [u8; 16]) -> Option<Receipt> {
        self.catalog
            .receipt(source_id, batch_id)
            .ok()
            .flatten()
            .map(|held| Receipt {
                source_id: held.source_id,
                batch_id: held.batch_id,
                accepted: held.accepted,
                committed_at: held.committed_at,
                commit_watermark: held.commit_watermark,
            })
    }

    fn scan(
        &self,
        project_id: [u8; 16],
        range_start: i64,
        range_end: i64,
        basis: TimeBasis,
    ) -> Result<Scanned, StoreError> {
        if range_end < range_start {
            return Err(StoreError::InvalidArgument(
                "The time range ends before it starts.".to_string(),
            ));
        }
        let (rows, incomplete) =
            self.visible_rows(Some(project_id), Some((range_start, range_end, basis)))?;
        Ok(Scanned { rows, incomplete })
    }

    fn lookup_event(&self, event_id: [u8; 16]) -> Result<Option<EventRow>, StoreError> {
        // One read of the predicates for the whole lookup. This used to decode
        // every stored predicate again for each row it found.
        let tombstones = self.tombstones()?;

        // Memory first, because the newest data is the likeliest target of a
        // point lookup, and because that is the order that cannot miss a row a
        // seal is moving. See `memory_view`.
        let memory = self.memory_view(&|row: &EventRow| row.event_id == event_id);
        if let Some(row) = self.unpublished(memory)?.into_iter().next() {
            return Ok((!tombstones.hides(&row)).then_some(row));
        }

        // The tablet locator says which segments can hold the value, so a
        // lookup does not open every retained segment. A fingerprint prunes and
        // never decides: the rows below verify the full value.
        let candidates =
            self.cached_locator()?
                .candidates(crate::segment::schema::EVENT_ID, &event_id, None);

        let (segments, _) = self.segments_where(&|manifest: &Manifest| {
            candidates.is_empty() || candidates.contains(&manifest.segment_id)
        })?;
        for segment in segments {
            // The segment's own filter answers "no" exactly, which covers the
            // open buffer's rows that no run names yet and any locator entry
            // that a compaction has not caught up with.
            if !segment.may_hold(
                crate::segment::schema::EVENT_ID,
                &event_id,
                self.sealing.verify_on_read,
            ) {
                continue;
            }
            let rows = segment.rows(self.sealing.verify_on_read)?;
            if let Some(row) = rows.into_iter().find(|row| row.event_id == event_id) {
                return Ok((!tombstones.hides(&row)).then_some(row));
            }
        }
        Ok(None)
    }

    fn lookup_correlated(&self, column: &str, value: &[u8]) -> Result<Scanned, StoreError> {
        let tombstones = self.tombstones()?;

        // Memory first. The newest data is the likeliest target of a
        // correlation lookup, and no run names it yet.
        let memory = self.memory_view(&|row: &EventRow| row_carries(row, column, value));
        let mut found: Vec<EventRow> = self.unpublished(memory)?;

        let candidates = self.cached_locator()?.candidates(column, value, None);
        let (segments, failed) = self.segments_where(&|manifest: &Manifest| {
            candidates.is_empty() || candidates.contains(&manifest.segment_id)
        })?;
        // A damaged segment does not stop the answer; it makes it incomplete,
        // and the caller decides. D57 and D21.
        //
        // The reason is kept, because "we could not read all of it" is true
        // and unactionable on its own. `unreadable` is what names the part, and
        // FAILURE_MODES.md procedure 6 requires the name.
        let mut incomplete = !failed.is_empty() || self.append_log_is_unreadable();
        for segment in segments {
            if !segment.may_hold(column, value, self.sealing.verify_on_read) {
                continue;
            }
            match segment.rows(self.sealing.verify_on_read) {
                Ok(rows) => {
                    for row in rows {
                        if row_carries(&row, column, value) {
                            found.push(row);
                        }
                    }
                }
                Err(error) => {
                    incomplete = true;
                    self.note_unreadable(&segment.header.segment_id, error.to_string());
                }
            }
        }

        // A row that an active predicate hides is not there, whatever the
        // index said. This is the second line of defence FAILURE_MODES.md
        // section 8.2 rule 4 describes: a tombstone applied to the wrong
        // generation still hides the data on read.
        let mut visible: Vec<EventRow> = found
            .into_iter()
            .filter(|row| !tombstones.hides(row))
            .collect();
        visible.sort_by_key(|row| row.occurred_at);
        Ok(Scanned {
            rows: visible,
            incomplete,
        })
    }

    fn trend(
        &self,
        project_id: [u8; 16],
        range_start: i64,
        range_end: i64,
        basis: TimeBasis,
        bucket_ms: i64,
        name: Option<&str>,
    ) -> Result<Trend, StoreError> {
        if bucket_ms <= 0 {
            return Err(StoreError::InvalidArgument(
                "A time bucket must be longer than nothing.".to_string(),
            ));
        }
        let (rows, incomplete) =
            self.visible_rows(Some(project_id), Some((range_start, range_end, basis)))?;

        // A count is over logical events, never over physical rows. A physical
        // duplicate can exist after a pathological retry or a manual replay,
        // and a dashboard that reported a spike because a collector retried
        // would be a wrong answer. See DELIVERY.md section 6.
        let mut counts: BTreeMap<i64, u64> = BTreeMap::new();
        let mut counted: std::collections::BTreeSet<[u8; 16]> = std::collections::BTreeSet::new();
        let mut total = 0;
        for row in rows {
            if let Some(name) = name {
                if row.name != name {
                    continue;
                }
            }
            if !counted.insert(row.event_id) {
                continue;
            }
            let at = time_of(&row, basis);
            let bucket = at - at.rem_euclid(bucket_ms);
            *counts.entry(bucket).or_insert(0) += 1;
            total += 1;
        }

        Ok(Trend {
            basis,
            bucket_ms,
            buckets: counts.into_iter().collect(),
            total,
            commit_watermark: self.commit_watermark(),
            incomplete,
        })
    }

    fn commit_watermark(&self) -> u64 {
        self.state.lock().expect("store lock").watermark
    }

    fn unreadable(&self) -> Vec<String> {
        SegmentedStore::unreadable(self)
    }

    fn row_count(&self) -> usize {
        let sealed: u64 = self
            .catalog
            .manifests()
            .map(|manifests| manifests.iter().map(|m| m.row_count).sum())
            .unwrap_or(0);
        let state = self.state.lock().expect("store lock");
        sealed as usize
            + state.open.len()
            + state
                .sealing
                .iter()
                .map(|batch| batch.rows.len())
                .sum::<usize>()
    }

    fn is_writable(&self) -> bool {
        self.directory.is_dir()
    }

    fn seal_now(&self) -> Result<bool, StoreError> {
        Ok(self.seal()?.is_some())
    }

    fn erase(&self, tombstone: &Tombstone) -> Result<u64, StoreError> {
        SegmentedStore::erase(self, tombstone)
    }

    fn tombstone_generation(&self) -> Result<u64, StoreError> {
        SegmentedStore::tombstone_generation(self)
    }
}

impl SegmentedStore {
    /// Every row of one segment, for compaction and for export.
    ///
    /// A query reads only the pages it needs. This reads all of them, which is
    /// what a rewrite has to do anyway.
    pub fn read_segment_rows(&self, manifest: &Manifest) -> Result<Vec<EventRow>, StoreError> {
        let bytes = std::fs::read(self.directory.join(&manifest.relative_path)).map_err(|e| {
            StoreError::Damaged(format!(
                "The stored file for {} could not be read: {e}",
                crate::row::hex(&manifest.segment_id)
            ))
        })?;
        let segment = segment::open(bytes, self.sealing.verify_on_read)?;
        Ok(segment.rows(self.sealing.verify_on_read)?)
    }

    /// Retire segments and publish their replacements in one transaction.
    ///
    /// A compaction that published its replacements and then retired the
    /// sources would let a query see both, which double-counts every row it
    /// rewrote.
    pub fn swap_segments(
        &self,
        retire: &[[u8; 16]],
        replacements: Vec<(Manifest, Vec<EventRow>)>,
    ) -> Result<u64, StoreError> {
        let mut manifests = Vec::with_capacity(replacements.len());
        let mut locator = Locator::new();
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();

        for (source, rows) in replacements {
            let segment_id = new_segment_id();
            let mut writer = SegmentWriter::new(segment_id, source.workspace_id, source.project_id);
            writer.log_range = source.log_range;
            let segment = writer.write(&rows)?;

            for row in &rows {
                index_row(&mut locator, row, segment_id);
            }

            let relative = format!("segments/{}.tos", crate::row::hex(&segment_id));
            files.push((relative.clone(), segment.bytes.clone()));

            manifests.push(Manifest {
                segment_id,
                content_address: segment.content_address,
                kinds: segment.header.kinds.clone(),
                occurred_range: segment.header.occurred_range,
                received_range: segment.header.received_range,
                committed_range: segment.header.committed_range,
                row_count: segment.header.row_count,
                byte_count: segment.bytes.len() as u64,
                generation: 0,
                relative_path: relative,
                ..source
            });
        }

        // FAILURE_MODES.md section 10, compaction: abandon the attempt and keep
        // the source segments. Compaction is never required for correctness, so
        // the only cost of stopping is that the reclaimed bytes arrive later.
        // Nothing has been written or retired at this point.
        let needed: u64 = files.iter().map(|(_, bytes)| bytes.len() as u64).sum();
        self.space.check_bulk(Point::Compaction, needed)?;

        for (relative, bytes) in &files {
            write_atomically(&self.directory.join(relative), bytes)?;
        }

        locator.seal();

        // The time buckets whose stored runs can name a retired segment: the
        // ones the replacements fall in, and the ones the retired segments
        // covered, which matters when a segment goes with no replacement.
        let mut buckets: BTreeSet<i64> = locator.runs().map(|run| run.bucket()).collect();
        let stored = self.catalog.locator_buckets()?;
        for manifest in self.catalog.manifests()? {
            if !retire.contains(&manifest.segment_id) {
                continue;
            }
            let first = crate::locator::bucket_of(manifest.occurred_range.0);
            let last = crate::locator::bucket_of(manifest.occurred_range.1);
            buckets.extend(stored.range(first..=last).copied());
        }

        // The replacements and the runs that describe them go in together,
        // FAILURE_MODES.md section 8.4 rule 1. The runs used to follow in a
        // second step, and a replacement with no run is pruned out of every
        // exact lookup that another segment still names.
        //
        // A seal holds this turn from its drain to the moment it drops its
        // batch. Taking it here means a segment is never retired while a reader
        // can still find the batch it was sealed from, which would show that
        // reader the batch and the replacement together.
        let generation = {
            let _turn = self.sealing_turn.lock().expect("sealing turn");
            self.catalog
                .swap_with_locator(retire, &manifests, &locator)?
        };

        // The locator drops references to segments that no longer exist. A run
        // that still named a retired segment would cost a wasted candidate on
        // every probe. This is one transaction for each bucket, reading what
        // it rewrites inside the transaction, so a run a seal publishes
        // meanwhile is never removed. See `Catalog::consolidate_locator`.
        let retired: std::collections::HashSet<[u8; 16]> = retire.iter().copied().collect();
        self.catalog.consolidate_locator(&buckets, &retired)?;
        self.invalidate_locator_cache();

        // A retired segment's file cannot be read again, so the cache must not
        // keep answering from it.
        let mut state = self.state.lock().expect("store lock");
        for segment_id in retire {
            state.cached.remove(segment_id);
            state.unreadable.remove(&unreadable_key(segment_id));
        }
        Ok(generation)
    }

    /// Delete the files of segments the catalog no longer names.
    ///
    /// A file older than the grace period and named by no live manifest is one
    /// nothing can be reading. Section 8.1 rule 3 sets the wait; the caller has
    /// already checked that no query holds a pin.
    pub fn reclaim_retired_files(&self, grace_ms: i64) -> Result<usize, StoreError> {
        // A set: this is asked once for each file on disk.
        let live_manifests = self.catalog.manifests()?;
        let live: std::collections::HashSet<&str> = live_manifests
            .iter()
            .filter_map(|manifest| manifest.relative_path.rsplit('/').next())
            .collect();

        let directory = self.directory.join("segments");
        let Ok(entries) = std::fs::read_dir(&directory) else {
            return Ok(0);
        };

        let now = std::time::SystemTime::now();
        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if live.contains(name) {
                continue;
            }
            // The grace period runs from when the file was last written, which
            // is the closest thing on disk to when it was retired.
            let old_enough = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|written| now.duration_since(written).ok())
                .map(|age| age.as_millis() as i64 >= grace_ms)
                .unwrap_or(false);
            if !old_enough {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }

        // A retired segment whose file is gone can be resolved by nothing, so
        // its catalog records go too. They used to stay for ever, and every
        // read of the segment catalog decoded every segment ever written.
        let gone: Vec<[u8; 16]> = self
            .catalog
            .retired_segments()?
            .into_iter()
            .filter(|segment_id| {
                !directory
                    .join(format!("{}.tos", crate::row::hex(segment_id)))
                    .exists()
            })
            .collect();
        self.catalog.forget_retired_segments(&gone)?;

        // The cache holds only what is live. A segment opened while a swap was
        // retiring it would otherwise stay in memory for the life of the
        // process.
        let live_ids: std::collections::HashSet<[u8; 16]> = live_manifests
            .iter()
            .map(|manifest| manifest.segment_id)
            .collect();
        let mut state = self.state.lock().expect("store lock");
        state
            .cached
            .retain(|segment_id, _| live_ids.contains(segment_id));
        Ok(removed)
    }

    // -----------------------------------------------------------------------
    // Segment transfer
    //
    // `docs/STORAGE.md` section 6: a move "copies sealed segments, catches up
    // committed WAL positions, verifies checksums, then changes placement
    // generation". These three methods are the copy. The catching up is the
    // consensus log and the verifying is the content address, which a segment
    // carries in itself.
    // -----------------------------------------------------------------------

    /// Every sealed segment this store holds, as the catalog names them.
    pub fn manifests(&self) -> Result<Vec<Manifest>, StoreError> {
        Ok(self.catalog.manifests()?)
    }

    /// The bytes of one sealed segment.
    ///
    /// A caller reads a range of these and the receiver checks the whole
    /// against the content address, so a chunk that arrived wrong is caught
    /// before anything is published rather than after.
    pub fn read_segment_bytes(&self, segment_id: &[u8; 16]) -> Result<Vec<u8>, StoreError> {
        let manifest = self
            .catalog
            .manifests()?
            .into_iter()
            .find(|manifest| manifest.segment_id == *segment_id)
            .ok_or_else(|| {
                StoreError::InvalidArgument(format!(
                    "This node holds no segment named {}.",
                    crate::row::hex(segment_id)
                ))
            })?;
        std::fs::read(self.directory.join(&manifest.relative_path)).map_err(|e| {
            StoreError::Damaged(format!(
                "The stored file for {} could not be read: {e}",
                crate::row::hex(segment_id)
            ))
        })
    }

    /// Adopt one segment that arrived from another node.
    ///
    /// **Nothing the sender said is trusted.** The manifest is derived from the
    /// bytes, exactly as [`crate::snapshot::rebuild`] derives one from a file on
    /// disk, and the segment verifies itself before any of it is written. A
    /// manifest that travelled beside the bytes would let a sender publish a
    /// segment under a description that did not match its contents.
    ///
    /// A segment whose content address this store already holds is left alone
    /// and reported as already present, so a transfer that stopped halfway can
    /// simply be run again.
    /// Install a segment onto a store that may already hold some of its rows.
    ///
    /// **This is what L097 said needed a change to the segment format, and it
    /// does not.** L097's reasoning was that two replicas which applied the
    /// same entries build differently shaped segments, so their content
    /// addresses do not match and there is no honest way to tell a copied
    /// segment from one the target built itself. That is true **of segments**
    /// and it was the wrong unit to reconcile on.
    ///
    /// A row already carries the marker. Every event has a producer-assigned
    /// `event_id`, `AGENTS.md` requires that value to stay exactly retrievable
    /// at any cardinality, and the query path already counts one logical event
    /// however many physical rows carry it. So the reconcile is: keep the rows
    /// this store does not already hold, and write those.
    ///
    /// **It costs one exact lookup for each row**, which the locator makes a
    /// probe rather than a scan. That is expensive next to installing a file
    /// whole, and this is a recovery path rather than a hot one: it runs when a
    /// replica is being rebuilt onto a node that already holds part of the
    /// tablet, which is the case that used to be refused outright.
    pub fn reconcile_segment(&self, bytes: Vec<u8>) -> Result<Installed, StoreError> {
        let byte_count = bytes.len() as u64;
        let segment = segment::open_received(bytes.clone())?;

        // The whole file, when this store holds none of it. The content address
        // is the cheap answer and it is the common one.
        if !self.overlaps(&segment)? {
            return self.install_segment(bytes);
        }

        let rows = segment.rows(true)?;
        let mut keep: Vec<EventRow> = Vec::new();
        for row in rows {
            if self.lookup_event(row.event_id)?.is_none() {
                keep.push(row);
            }
        }
        if keep.is_empty() {
            // Every row was already here. That is a copy that finished, run
            // again, which `Installed::already_held` is exactly for.
            return Ok(Installed {
                segment_id: segment.header.segment_id,
                row_count: segment.header.row_count,
                byte_count,
                already_held: true,
            });
        }

        // A new segment, because the rows are a subset and a segment is
        // immutable. It carries the source's log range so that a later
        // reconcile can see the overlap the same way this one did.
        let segment_id = new_segment_id();
        let mut writer = SegmentWriter::new(
            segment_id,
            segment.header.workspace_id,
            segment.header.project_id,
        );
        writer.log_range = segment.header.log_range;
        writer.tablet_id = segment.header.tablet_id;
        let built = writer.write(&keep)?;
        let kept = keep.len() as u64;
        self.install_segment(built.bytes)?;
        Ok(Installed {
            segment_id,
            row_count: kept,
            byte_count,
            already_held: false,
        })
    }

    /// Whether this store already holds a segment covering the same log range
    /// of the same tablet.
    ///
    /// It is the cheap trigger for the expensive path, and it is deliberately
    /// generous: a false yes costs a row-by-row install, and a false no would
    /// count the overlap twice.
    fn overlaps(&self, segment: &Segment) -> Result<bool, StoreError> {
        let (from, to) = segment.header.log_range;
        Ok(self.catalog.manifests()?.iter().any(|held| {
            held.tablet_id == segment.header.tablet_id
                && held.log_range.0 <= to
                && from <= held.log_range.1
        }))
    }

    pub fn install_segment(&self, bytes: Vec<u8>) -> Result<Installed, StoreError> {
        let byte_count = bytes.len() as u64;
        let segment = segment::open_received(bytes.clone())?;

        let already = self
            .catalog
            .manifests()?
            .into_iter()
            .any(|held| held.content_address == segment.content_address);
        if already {
            return Ok(Installed {
                segment_id: segment.header.segment_id,
                row_count: segment.header.row_count,
                byte_count,
                already_held: true,
            });
        }

        let relative = format!(
            "segments/{}.tos",
            crate::row::hex(&segment.header.segment_id)
        );
        let manifest = Manifest {
            segment_id: segment.header.segment_id,
            content_address: segment.content_address,
            tablet_id: segment.header.tablet_id,
            virtual_shard: segment.header.virtual_shard,
            workspace_id: segment.header.workspace_id,
            project_id: segment.header.project_id,
            kinds: segment.header.kinds.clone(),
            occurred_range: segment.header.occurred_range,
            received_range: segment.header.received_range,
            committed_range: segment.header.committed_range,
            log_range: segment.header.log_range,
            row_count: segment.header.row_count,
            byte_count,
            generation: 0,
            tier: "local".into(),
            relative_path: relative.clone(),
        };

        // The device may refuse. It refuses before anything is written, which
        // is the same order the seal path uses.
        self.space.check_bulk(Point::SegmentPublish, byte_count)?;

        // The locator is rebuilt from the rows this segment actually holds. A
        // copied segment that arrived without locator runs would be pruned out
        // of every exact lookup that another segment already names.
        let rows = segment.rows(true)?;
        let mut locator = Locator::new();
        for row in &rows {
            index_row(&mut locator, row, segment.header.segment_id);
        }
        locator.seal();

        write_atomically(&self.directory.join(&relative), &bytes)?;
        self.catalog
            .publish_with_locator(std::slice::from_ref(&manifest), &locator)?;

        Ok(Installed {
            segment_id: manifest.segment_id,
            row_count: manifest.row_count,
            byte_count,
            already_held: false,
        })
    }
}

#[cfg(test)]
mod seal_safety {
    //! The seal is where an acknowledged row changes hands, from the append log
    //! to a segment. Each test here is one way that handover used to drop it.

    use std::path::PathBuf;

    use super::{Sealing, SegmentedStore};
    use crate::row::EventRow;
    use crate::store::{Store, TimeBasis};
    use crate::wal::GroupCommit;

    const PROJECT: [u8; 16] = [9; 16];
    const BASE_TIME: i64 = 1_785_628_800_000;

    fn place(name: &str) -> PathBuf {
        let base = std::env::var("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("target"));
        let path = base
            .join("seal-safety-tests")
            .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    /// A store that seals only when a test says so.
    fn patient(place: &PathBuf) -> SegmentedStore {
        SegmentedStore::open_with(
            place,
            Sealing {
                max_open_rows: usize::MAX,
                max_open_ms: i64::MAX,
                verify_on_read: true,
                reserve_bytes: 0,
            },
            GroupCommit {
                linger: std::time::Duration::from_millis(0),
                ..GroupCommit::default()
            },
        )
        .expect("the store opens")
    }

    fn rows(count: usize, from: u16) -> Vec<EventRow> {
        (0..count)
            .map(|index| {
                let n = from + index as u16;
                let mut row = EventRow::new([1; 16], "event", "a", BASE_TIME + i64::from(n));
                row.event_id[14..16].copy_from_slice(&n.to_be_bytes());
                row.project_id = PROJECT;
                row.session_id = Some("s-1".to_string());
                row
            })
            .collect()
    }

    fn visible(store: &SegmentedStore) -> usize {
        let scanned = store
            .scan(
                PROJECT,
                BASE_TIME - 1,
                BASE_TIME + 100_000,
                TimeBasis::OccurredAt,
            )
            .expect("the scan answers");
        assert!(!scanned.incomplete, "nothing here is damaged");
        let mut ids: Vec<[u8; 16]> = scanned.rows.iter().map(|row| row.event_id).collect();
        let physical = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), physical, "a row was answered twice");
        physical
    }

    /// Make every segment write fail the way a broken device would, without
    /// needing one: the segments directory becomes a file.
    fn break_segment_writes(place: &std::path::Path) {
        std::fs::remove_dir_all(place.join("segments")).expect("the directory goes");
        std::fs::write(place.join("segments"), b"not a directory").expect("a file takes its place");
    }

    fn mend_segment_writes(place: &std::path::Path) {
        std::fs::remove_file(place.join("segments")).expect("the file goes");
        std::fs::create_dir_all(place.join("segments")).expect("the directory is back");
    }

    #[test]
    fn a_seal_that_cannot_write_keeps_its_rows_and_the_next_seal_does_not_step_over_them() {
        let place = place("failed-write");
        {
            let store = patient(&place);
            store.commit([1; 16], [1; 16], rows(10, 0)).unwrap();

            break_segment_writes(&place);
            let failed = store.seal().expect_err("the device refused the file");
            assert!(!matches!(failed, crate::store::StoreError::Exhausted(_)));
            assert_eq!(store.take_seal_failures(), 1);
            assert_eq!(
                visible(&store),
                10,
                "the rows of a failed seal stay answerable"
            );

            mend_segment_writes(&place);
            store.commit([1; 16], [2; 16], rows(10, 100)).unwrap();
            store
                .seal()
                .expect("the device is back")
                .expect("a segment");
            assert_eq!(visible(&store), 20);
        }
        // The checkpoint must not have stepped over the range the failed seal
        // drained, and the log must not have reclaimed it.
        let store = patient(&place);
        assert_eq!(visible(&store), 20, "an acknowledged batch was lost");
    }

    #[test]
    fn a_failed_seal_loses_nothing_even_when_the_process_stops_right_after_it() {
        let place = place("failed-write-then-stop");
        {
            let store = patient(&place);
            store.commit([1; 16], [1; 16], rows(10, 0)).unwrap();
            break_segment_writes(&place);
            store.seal().expect_err("the device refused the file");
            mend_segment_writes(&place);
        }
        let store = patient(&place);
        assert_eq!(visible(&store), 10);
    }

    #[test]
    fn rows_being_sealed_stay_visible_to_every_kind_of_read() {
        let place = place("visible-while-sealing");
        let store = patient(&place);
        store.commit([1; 16], [1; 16], rows(10, 0)).unwrap();
        let wanted = rows(1, 3)[0].event_id;

        let seen = std::cell::Cell::new(false);
        store
            .seal_observed(&|| {
                // The rows have left the open buffer and no segment holds them.
                assert_eq!(visible(&store), 10, "a scan lost the rows being sealed");
                assert!(
                    store.lookup_event(wanted).unwrap().is_some(),
                    "an exact lookup lost a row being sealed"
                );
                assert_eq!(
                    store
                        .lookup_correlated(crate::segment::schema::SESSION_ID, b"s-1")
                        .unwrap()
                        .rows
                        .len(),
                    10,
                    "a correlated lookup lost the rows being sealed"
                );
                assert_eq!(store.row_count(), 10);
                seen.set(true);
            })
            .unwrap();
        assert!(seen.get());

        // And once, not twice, after the publish.
        assert_eq!(visible(&store), 10);
        assert_eq!(store.segment_count(), 1);
    }

    #[test]
    fn a_segment_from_another_node_does_not_move_this_nodes_log_checkpoint() {
        // The sender's log is far ahead of the receiver's.
        let sender_place = place("transfer-sender");
        let sender = patient(&sender_place);
        for batch in 0..30u8 {
            let mut sent = rows(1, 1_000 + u16::from(batch));
            sent[0].event_id[0] = 7;
            sender.commit([1; 16], [batch; 16], sent).unwrap();
        }
        let segment_id = sender.seal().unwrap().expect("a segment");
        let bytes = sender.read_segment_bytes(&segment_id).unwrap();

        let receiver_place = place("transfer-receiver");
        {
            let receiver = patient(&receiver_place);
            // Acknowledged, and only in the receiver's own append log.
            receiver.commit([1; 16], [1; 16], rows(10, 0)).unwrap();
            receiver.install_segment(bytes).unwrap();
            assert_eq!(visible(&receiver), 40);
        }
        let receiver = patient(&receiver_place);
        assert_eq!(
            visible(&receiver),
            40,
            "the copied segment's log range hid this node's own unsealed frames"
        );

        // And a local seal after the install still keeps what it did not take.
        receiver.commit([1; 16], [2; 16], rows(5, 200)).unwrap();
        receiver.seal().unwrap();
        receiver.commit([1; 16], [3; 16], rows(5, 300)).unwrap();
        drop(receiver);
        let receiver = patient(&receiver_place);
        assert_eq!(visible(&receiver), 50);
    }

    #[test]
    fn a_log_that_was_reclaimed_to_nothing_does_not_reuse_its_positions_after_a_restart() {
        let place = place("positions");
        {
            let store = patient(&place);
            for batch in 0..5u8 {
                store
                    .commit([1; 16], [batch; 16], rows(10, u16::from(batch) * 10))
                    .unwrap();
            }
            // Every frame is covered, so the log is emptied.
            store.seal().unwrap();
        }
        {
            // The log holds no frame to learn its next position from.
            let store = patient(&place);
            store.commit([1; 16], [9; 16], rows(10, 100)).unwrap();
            assert_eq!(visible(&store), 60);
        }
        let store = patient(&place);
        assert_eq!(
            visible(&store),
            60,
            "a frame written below the checkpoint was not replayed"
        );
        // And the seal that takes it does not reclaim a frame above it.
        store.seal().unwrap();
        store.commit([1; 16], [10; 16], rows(10, 200)).unwrap();
        drop(store);
        assert_eq!(visible(&patient(&place)), 70);
    }

    #[test]
    fn a_catalog_written_before_the_checkpoint_record_opens_and_loses_nothing() {
        let place = place("no-checkpoint-record");
        {
            let store = patient(&place);
            store.commit([1; 16], [1; 16], rows(10, 0)).unwrap();
            store.seal().unwrap();
            store.commit([1; 16], [2; 16], rows(10, 100)).unwrap();
            // What an older build left behind: segments, a log, and no record.
            store
                .catalog()
                .remove_durable(&["tablet/0000/log-checkpoint".to_string()])
                .unwrap();
            assert_eq!(store.catalog().log_checkpoint().unwrap(), None);
        }
        let store = patient(&place);
        assert_eq!(visible(&store), 20);
        assert!(
            store.catalog().log_checkpoint().unwrap().is_some(),
            "the record is written once, so the whole log is replayed once"
        );
        store.seal().unwrap();
        drop(store);
        assert_eq!(visible(&patient(&place)), 20);
    }

    #[test]
    fn one_damaged_segment_is_named_once_and_only_the_reads_that_need_it_are_incomplete() {
        let place = place("unreadable");
        let store = patient(&place);
        store.commit([1; 16], [1; 16], rows(10, 0)).unwrap();
        let damaged = store.seal().unwrap().expect("a segment");

        let mut other = rows(5, 500);
        for row in &mut other {
            row.project_id = [4; 16];
        }
        store.commit([1; 16], [2; 16], other).unwrap();
        store.seal().unwrap();

        std::fs::remove_file(place.join(format!("segments/{}.tos", crate::row::hex(&damaged))))
            .unwrap();
        // A store that had the file cached would not notice. A restart does.
        drop(store);
        let store = patient(&place);

        for _ in 0..5 {
            let scanned = store
                .scan(
                    PROJECT,
                    BASE_TIME - 1,
                    BASE_TIME + 100_000,
                    TimeBasis::OccurredAt,
                )
                .unwrap();
            assert!(scanned.incomplete);
        }
        assert_eq!(
            store.unreadable().len(),
            1,
            "one damaged file is one entry, however many reads meet it"
        );

        let elsewhere = store
            .scan(
                [4; 16],
                BASE_TIME - 1,
                BASE_TIME + 100_000,
                TimeBasis::OccurredAt,
            )
            .unwrap();
        assert_eq!(elsewhere.rows.len(), 5);
        assert!(
            !elsewhere.incomplete,
            "another project's damaged segment made this read incomplete"
        );
    }
}
