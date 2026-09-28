//! A durable consensus log and applied state, on the engine the catalog uses.
//!
//! D27 measured openraft against in-memory storage and named what that left
//! open: "behaviour against durable storage, where every append pays an fsync".
//! This is that storage. A voter that acknowledges an entry has written it and
//! made it durable, because a quorum receipt that survives no restart is not a
//! receipt.
//!
//! # Why redb rather than the append log
//!
//! The append log in `tallyowl-store` is a stream: it appends, it seals, and it
//! reclaims from the front. A consensus log also has to **truncate from the
//! back**, when a new leader overwrites entries a previous one had not
//! committed. A stream cannot do that without rewriting itself. D3 already
//! measured redb for the catalog and accepted it, and a keyed table gives
//! truncate, purge, and range read for nothing.
//!
//! # The two durability rules
//!
//! **An append is durable before it is acknowledged.** `append` commits with
//! immediate durability and only then reports completion, so the entry survives
//! a kill between the acknowledgement and the next write.
//!
//! **The vote is durable before it is used.** A node that voted, restarted, and
//! forgot could vote twice in one term, which is how two leaders appear in one
//! term. `save_vote` commits before it returns.

// openraft's storage traits fix the error type, and it is a large one. Every
// function here that returns it does so because a trait method hands it on.
#![allow(clippy::result_large_err)]

use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use openraft::storage::{LogFlushed, LogState, RaftLogStorage, RaftStateMachine, Snapshot};
use openraft::{
    BasicNode, Entry, EntryPayload, LogId, OptionalSend, RaftLogId, RaftLogReader,
    RaftSnapshotBuilder, SnapshotMeta, StorageError, StoredMembership, Vote,
};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use super::machine::GroupMachine;
use super::{NodeId, TypeConfig};

/// The consensus log: index to encoded entry.
const LOG: TableDefinition<u64, Vec<u8>> = TableDefinition::new("raft_log");
/// Everything that is one value: the vote, the committed log ID, the applied
/// log ID, the membership, and the snapshot.
const META: TableDefinition<&str, Vec<u8>> = TableDefinition::new("raft_meta");

const VOTE: &str = "vote";
const COMMITTED: &str = "committed";
const APPLIED: &str = "applied";
const MEMBERSHIP: &str = "membership";
const SNAPSHOT_META: &str = "snapshot_meta";
const SNAPSHOT_DATA: &str = "snapshot_data";
const PURGED: &str = "purged";
/// The state machine's whole state, as of [`APPLIED`].
///
/// **It is written in the transaction that writes `APPLIED`**, so the two can
/// never disagree. A controller, a directory, and a tablet's marks live in
/// memory, and openraft replays nothing at or below the applied position: a
/// machine that was not written here would come back from a restart holding
/// only its last snapshot while claiming everything after it. See
/// [`GroupStorage::open`].
const MACHINE_STATE: &str = "machine_state";

/// An applied position, and the machine state that was written with it.
type AppliedPair = (Option<LogId<NodeId>>, Option<Vec<u8>>);

/// How many bytes of entries one append carries. See
/// [`RaftLogReader::limited_get_log_entries`].
pub const MAX_APPEND_BYTES: usize = 4 * 1024 * 1024;

/// How long a group waits after a snapshot that could not be prepared before it
/// prepares another.
const SNAPSHOT_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

/// What an operator needs to see about one group's storage path.
#[derive(Default)]
pub struct StorageCounters {
    /// Snapshots that could not be prepared. The log is not purged while this
    /// climbs, so it is the early sign of a consensus log filling a disk.
    pub snapshot_failures: AtomicU64,
    pub last_snapshot_failure: Mutex<Option<String>>,
    retry_snapshot_after: Mutex<Option<std::time::Instant>>,
}

/// The durable state one group keeps.
pub struct GroupStorage {
    database: Arc<Database>,
    machine: Arc<dyn GroupMachine>,
    /// How many snapshots this node has built, so each gets a distinct name.
    /// It is not part of the replicated state and does not have to survive a
    /// restart; a restarted node that reuses a name is harmless because the
    /// name is qualified by the last applied index.
    snapshot_count: Arc<Mutex<u64>>,
    counters: Arc<StorageCounters>,
}

impl Clone for GroupStorage {
    fn clone(&self) -> GroupStorage {
        GroupStorage {
            database: Arc::clone(&self.database),
            machine: Arc::clone(&self.machine),
            snapshot_count: Arc::clone(&self.snapshot_count),
            counters: Arc::clone(&self.counters),
        }
    }
}

impl Debug for GroupStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GroupStorage")
    }
}

impl GroupStorage {
    /// Open, or create, the durable state for one group.
    pub fn open(
        path: &std::path::Path,
        machine: Arc<dyn GroupMachine>,
    ) -> Result<GroupStorage, String> {
        GroupStorage::open_with_cache(
            path,
            machine,
            crate::groups::DEFAULT_LOG_CACHE_BYTES as usize,
        )
    }

    /// The same, with the memory this one file may use as a cache. See
    /// [`crate::groups::DEFAULT_LOG_CACHE_BYTES`] for why it is not the
    /// engine's default.
    pub fn open_with_cache(
        path: &std::path::Path,
        machine: Arc<dyn GroupMachine>,
        cache_bytes: usize,
    ) -> Result<GroupStorage, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "The consensus directory {} could not be made: {e}",
                    parent.display()
                )
            })?;
        }
        let mut builder = Database::builder();
        builder.set_cache_size(cache_bytes);
        let database = builder.create(path).map_err(|e| {
            format!(
                "The consensus log at {} could not be opened: {e}",
                path.display()
            )
        })?;
        let storage = GroupStorage {
            database: Arc::new(database),
            machine,
            snapshot_count: Arc::new(Mutex::new(0)),
            counters: Arc::new(StorageCounters::default()),
        };
        // Create both tables once, so every later read can assume they exist
        // and a read on a fresh database is not an error.
        let write = storage.write()?;
        {
            write.open_table(LOG).map_err(io_message)?;
            write.open_table(META).map_err(io_message)?;
        }
        write.commit().map_err(io_message)?;

        // A restart takes the machine back to exactly where `APPLIED` says it
        // is, because the two were written together.
        //
        // A file written before that record existed has only its snapshot. It
        // installs that, and [`RaftStateMachine::applied_state`] then reports
        // the snapshot's position rather than `APPLIED`, so openraft applies
        // the log after the snapshot again. Every machine here gives the same
        // result for an entry applied twice, which is what makes that safe.
        match storage.machine_state()? {
            Some(state) => storage.machine.install(&state)?,
            None => {
                if let Some(data) = storage.meta_raw(SNAPSHOT_DATA)? {
                    storage.machine.install(&data)?;
                }
            }
        }
        Ok(storage)
    }

    /// The machine state that was written with `APPLIED`, when there is one.
    ///
    /// An empty record counts as none: a machine that could not encode itself
    /// wrote nothing worth restoring, and replaying from the snapshot is the
    /// safe answer to that.
    fn machine_state(&self) -> Result<Option<Vec<u8>>, String> {
        Ok(self
            .meta_raw(MACHINE_STATE)?
            .filter(|state| !state.is_empty()))
    }

    /// An in-memory group, for a test and for the home profile's single-voter
    /// tablet, which has a consensus group of one and no peer to talk to.
    pub fn in_memory(machine: Arc<dyn GroupMachine>) -> Result<GroupStorage, String> {
        let database = Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .map_err(|e| format!("An in-memory consensus log could not be made: {e}"))?;
        let storage = GroupStorage {
            database: Arc::new(database),
            machine,
            snapshot_count: Arc::new(Mutex::new(0)),
            counters: Arc::new(StorageCounters::default()),
        };
        let write = storage.write()?;
        {
            write.open_table(LOG).map_err(io_message)?;
            write.open_table(META).map_err(io_message)?;
        }
        write.commit().map_err(io_message)?;
        Ok(storage)
    }

    pub fn machine(&self) -> Arc<dyn GroupMachine> {
        Arc::clone(&self.machine)
    }

    fn write(&self) -> Result<redb::WriteTransaction, String> {
        let mut transaction = self.database.begin_write().map_err(io_message)?;
        // Every consensus write is a durability promise, so none of them may be
        // deferred. See the module note.
        transaction
            .set_durability(redb::Durability::Immediate)
            .map_err(|e| {
                format!("The consensus log would not promise durability, so this node must not acknowledge anything: {e}")
            })?;
        Ok(transaction)
    }

    fn meta_raw(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let read = self.database.begin_read().map_err(io_message)?;
        let table = read.open_table(META).map_err(io_message)?;
        Ok(table
            .get(key)
            .map_err(io_message)?
            .map(|value| value.value().clone()))
    }

    fn meta<T: for<'a> serde::Deserialize<'a>>(&self, key: &str) -> Result<Option<T>, String> {
        match self.meta_raw(key)? {
            None => Ok(None),
            Some(bytes) => super::decode(&bytes).map(Some),
        }
    }

    fn put_meta<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<(), String> {
        let bytes = super::encode(value)?;
        self.put_meta_raw(key, bytes)
    }

    fn put_meta_raw(&self, key: &str, bytes: Vec<u8>) -> Result<(), String> {
        let write = self.write()?;
        {
            let mut table = write.open_table(META).map_err(io_message)?;
            table.insert(key, bytes).map_err(io_message)?;
        }
        write.commit().map_err(io_message)
    }

    /// The applied position and the machine state written with it, from one
    /// read transaction.
    fn applied_pair(&self) -> Result<AppliedPair, String> {
        let read = self.database.begin_read().map_err(io_message)?;
        let table = read.open_table(META).map_err(io_message)?;
        let applied = match table.get(APPLIED).map_err(io_message)? {
            None => None,
            Some(bytes) => super::decode::<Option<LogId<NodeId>>>(&bytes.value())?,
        };
        let state = table
            .get(MACHINE_STATE)
            .map_err(io_message)?
            .map(|bytes| bytes.value().clone())
            .filter(|state| !state.is_empty());
        Ok((applied, state))
    }

    /// Run the machine's preparation, unless the last one failed a moment ago.
    ///
    /// The snapshot policy asks again on every apply once it is exceeded, and a
    /// tablet's preparation is a seal. A disk at its reserve would otherwise be
    /// asked to build a segment for every batch it takes.
    fn before_snapshot_guarded(&self) -> Result<(), String> {
        {
            let retry = self
                .counters
                .retry_snapshot_after
                .lock()
                .expect("snapshot retry");
            if retry.is_some_and(|at| std::time::Instant::now() < at) {
                return Err(
                    "The last snapshot failed a moment ago, so this one was not tried.".into(),
                );
            }
        }
        let result = self.machine.before_snapshot();
        let mut retry = self
            .counters
            .retry_snapshot_after
            .lock()
            .expect("snapshot retry");
        *retry = match &result {
            Ok(()) => None,
            Err(_) => Some(std::time::Instant::now() + SNAPSHOT_RETRY_AFTER),
        };
        result
    }

    /// The snapshot this node already holds, or one that claims nothing.
    ///
    /// openraft ignores a snapshot that is not ahead of the one it knows, so
    /// either answer leaves its state, and the log, exactly where they were.
    fn previous_snapshot(&self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        let meta: Option<SnapshotMeta<NodeId, BasicNode>> =
            self.meta(SNAPSHOT_META).map_err(storage_error)?;
        let data = self
            .meta_raw(SNAPSHOT_DATA)
            .map_err(storage_error)?
            .unwrap_or_default();
        Ok(Snapshot {
            meta: meta.unwrap_or_default(),
            snapshot: Box::new(Cursor::new(data)),
        })
    }

    /// Rewrite this node's durable state to name it as the group's only voter.
    ///
    /// `docs/FAILURE_MODES.md` section 6.2. **The algorithm must be stopped.**
    /// It answers how many log entries were dropped.
    ///
    /// What is kept: everything applied, and every entry after it that is an
    /// ordinary command. Those may have been committed by the voters that were
    /// lost, and the survivor commits them alone when it starts. What is
    /// dropped: the log from the first voter-set change that was never applied,
    /// because openraft takes its voter set from the newest one in the log and
    /// that one would bring the lost voters back.
    pub fn force_single_voter(&self, node: NodeId, address: &str) -> Result<u64, String> {
        let applied: Option<LogId<NodeId>> = self.meta(APPLIED)?.flatten();
        let after = applied.map(|id| id.index + 1).unwrap_or(0);

        let write = self.write()?;
        let dropped;
        {
            let mut log = write.open_table(LOG).map_err(io_message)?;
            let mut first_change = None;
            for row in log.range(after..).map_err(io_message)? {
                let (index, value) = row.map_err(io_message)?;
                let entry: Entry<TypeConfig> = super::decode(&read_value(&value.value())?)?;
                if matches!(entry.payload, EntryPayload::Membership(_)) {
                    first_change = Some(index.value());
                    break;
                }
            }
            let doomed: Vec<u64> = match first_change {
                None => Vec::new(),
                Some(from) => log
                    .range(from..)
                    .map_err(io_message)?
                    .map(|row| row.map(|(index, _)| index.value()))
                    .collect::<Result<_, _>>()
                    .map_err(io_message)?,
            };
            dropped = doomed.len() as u64;
            for index in doomed {
                log.remove(index).map_err(io_message)?;
            }

            let mut voters = std::collections::BTreeSet::new();
            voters.insert(node);
            let mut nodes = std::collections::BTreeMap::new();
            nodes.insert(
                node,
                BasicNode {
                    addr: address.to_string(),
                },
            );
            let membership =
                StoredMembership::new(applied, openraft::Membership::new(vec![voters], nodes));
            let mut meta = write.open_table(META).map_err(io_message)?;
            meta.insert(MEMBERSHIP, super::encode(&membership)?)
                .map_err(io_message)?;
        }
        write.commit().map_err(io_message)?;
        Ok(dropped)
    }

    /// Make a file what a release before [`MACHINE_STATE`] left behind.
    #[cfg(test)]
    pub(crate) fn forget_machine_state(path: &std::path::Path) -> Result<(), String> {
        let database = Database::create(path).map_err(io_message)?;
        let write = database.begin_write().map_err(io_message)?;
        {
            let mut table = write.open_table(META).map_err(io_message)?;
            table.remove(MACHINE_STATE).map_err(io_message)?;
        }
        write.commit().map_err(io_message)
    }

    /// What went wrong on this group's storage path without stopping it.
    pub fn counters(&self) -> Arc<StorageCounters> {
        Arc::clone(&self.counters)
    }

    fn entry(&self, index: u64) -> Result<Option<Entry<TypeConfig>>, String> {
        let read = self.database.begin_read().map_err(io_message)?;
        let table = read.open_table(LOG).map_err(io_message)?;
        match table.get(index).map_err(io_message)? {
            None => Ok(None),
            Some(bytes) => super::decode(&read_value(&bytes.value())?).map(Some),
        }
    }

    fn last_index(&self) -> Result<Option<u64>, String> {
        let read = self.database.begin_read().map_err(io_message)?;
        let table = read.open_table(LOG).map_err(io_message)?;
        let last = table.last().map_err(io_message)?;
        Ok(last.map(|(key, _)| key.value()))
    }
}

/// Run one piece of durable I/O without occupying a consensus worker.
///
/// Every group on a node shares two async workers, and every function here ends
/// in an fsync, a store commit, or a seal. Run in place, two slow devices would
/// stop every heartbeat and every election timer on the node, and the groups
/// that were healthy would start elections because of the ones that were slow.
/// `block_in_place` hands the worker's other tasks to another thread first.
fn blocking<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

fn io_message(e: impl std::fmt::Display) -> String {
    format!("The consensus log could not be used: {e}")
}

// ---------------------------------------------------------------------------
// How an entry sits on disk
// ---------------------------------------------------------------------------

/// What marks a framed log value.
///
/// A value written before this frame existed is bare CBOR, and an
/// `Entry<TypeConfig>` encodes as a CBOR map or array, so it can never begin
/// with 0x54 — major type 2, a byte string. That is what makes the two forms
/// tellable apart without a migration, and [`read_value`] reads either.
const LOG_FRAME: [u8; 4] = *b"TOL1";
const FRAME_PLAIN: u8 = 0;
const FRAME_ZSTD: u8 = 1;

/// Below this, compression is not worth the processor time.
///
/// The same threshold the delivery queue uses in `tallyowl-collector`, for the
/// same reason: a small entry is already close to its floor and a payload that
/// grew would cost bytes and cost a reader a decompression for nothing.
const COMPRESS_ABOVE_BYTES: usize = 1024;

/// The zstd level. D17 measured this level for segment pages and found the step
/// to a higher one bought little.
const ZSTD_LEVEL: i32 = 3;

/// Put one encoded entry into the shape the log holds.
///
/// **A consensus log is a second full copy of every batch** (L095). Compressing
/// it is L095's option 2: it does not change the shape of the problem, which is
/// what the purge fixes, and it is a large constant off the part that remains.
/// The delivery queue already compresses a batch for the same reason, so the
/// precedent and the codec both exist.
fn frame_value(encoded: Vec<u8>) -> Vec<u8> {
    let (tag, body) = if encoded.len() > COMPRESS_ABOVE_BYTES {
        match zstd::encode_all(encoded.as_slice(), ZSTD_LEVEL) {
            Ok(squeezed) if squeezed.len() < encoded.len() => (FRAME_ZSTD, squeezed),
            _ => (FRAME_PLAIN, encoded),
        }
    } else {
        (FRAME_PLAIN, encoded)
    };
    let mut out = Vec::with_capacity(LOG_FRAME.len() + 1 + body.len());
    out.extend_from_slice(&LOG_FRAME);
    out.push(tag);
    out.extend_from_slice(&body);
    out
}

/// Read one log value back, in either form.
fn read_value(stored: &[u8]) -> Result<Vec<u8>, String> {
    if stored.len() < LOG_FRAME.len() + 1 || stored[..LOG_FRAME.len()] != LOG_FRAME {
        // Written before the frame existed. See [`LOG_FRAME`].
        return Ok(stored.to_vec());
    }
    let tag = stored[LOG_FRAME.len()];
    let body = &stored[LOG_FRAME.len() + 1..];
    match tag {
        FRAME_PLAIN => Ok(body.to_vec()),
        FRAME_ZSTD => zstd::decode_all(body).map_err(|e| {
            format!("A consensus log entry could not be decompressed, so this node cannot read its own log: {e}")
        }),
        other => Err(format!(
            "A consensus log entry is marked with form {other}, which this build does not know. It was written by a newer release."
        )),
    }
}

/// openraft wants one error type for storage. Every failure here is a device or
/// an encoding failure, and both mean the same thing to a group: this node
/// cannot be trusted to hold the log, so it must not acknowledge anything.
fn storage_error(message: String) -> StorageError<NodeId> {
    StorageError::IO {
        source: openraft::StorageIOError::write(&std::io::Error::other(message)),
    }
}

impl RaftLogReader<TypeConfig> for GroupStorage {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<NodeId>> {
        blocking(move || {
            let read = self
                .database
                .begin_read()
                .map_err(|e| storage_error(io_message(e)))?;
            let table = read
                .open_table(LOG)
                .map_err(|e| storage_error(io_message(e)))?;
            let mut entries = Vec::new();
            for row in table
                .range(range)
                .map_err(|e| storage_error(io_message(e)))?
            {
                let (_, value) = row.map_err(|e| storage_error(io_message(e)))?;
                let plain = read_value(&value.value()).map_err(storage_error)?;
                entries.push(super::decode(&plain).map_err(storage_error)?);
            }
            Ok(entries)
        })
    }

    /// The entries one append carries, bounded by their size and not only by
    /// their number.
    ///
    /// A replicated batch is hundreds of kilobytes, so a few hundred of them
    /// are past the frame limit, and well before that they are more than a
    /// follower can write and answer inside the time openraft waits for an
    /// append. The leader then sent the same range on every tick and never
    /// learned that any of it arrived. At least one entry always travels, which
    /// the trait requires and which is what lets an entry larger than the
    /// budget through at all.
    async fn limited_get_log_entries(
        &mut self,
        start: u64,
        end: u64,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<NodeId>> {
        blocking(move || {
            let read = self
                .database
                .begin_read()
                .map_err(|e| storage_error(io_message(e)))?;
            let table = read
                .open_table(LOG)
                .map_err(|e| storage_error(io_message(e)))?;
            let mut entries = Vec::new();
            let mut bytes = 0usize;
            for row in table
                .range(start..end)
                .map_err(|e| storage_error(io_message(e)))?
            {
                let (_, value) = row.map_err(|e| storage_error(io_message(e)))?;
                let plain = read_value(&value.value()).map_err(storage_error)?;
                if !entries.is_empty() && bytes + plain.len() > MAX_APPEND_BYTES {
                    break;
                }
                bytes += plain.len();
                entries.push(super::decode(&plain).map_err(storage_error)?);
            }
            Ok(entries)
        })
    }
}

impl RaftLogStorage<TypeConfig> for GroupStorage {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<NodeId>> {
        blocking(move || {
            let purged: Option<LogId<NodeId>> = self.meta(PURGED).map_err(storage_error)?;
            let last = match self.last_index().map_err(storage_error)? {
                None => None,
                Some(index) => self
                    .entry(index)
                    .map_err(storage_error)?
                    .map(|entry| *entry.get_log_id()),
            };
            Ok(LogState {
                last_purged_log_id: purged,
                last_log_id: last.or(purged),
            })
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), StorageError<NodeId>> {
        blocking(move || self.put_meta(VOTE, vote).map_err(storage_error))
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError<NodeId>> {
        blocking(move || self.meta(VOTE).map_err(storage_error))
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<NodeId>>,
    ) -> Result<(), StorageError<NodeId>> {
        blocking(move || self.put_meta(COMMITTED, &committed).map_err(storage_error))
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<NodeId>>, StorageError<NodeId>> {
        blocking(move || Ok(self.meta(COMMITTED).map_err(storage_error)?.flatten()))
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
    {
        blocking(move || {
            let write = self.write().map_err(storage_error)?;
            {
                let mut table = write
                    .open_table(LOG)
                    .map_err(|e| storage_error(io_message(e)))?;
                for entry in entries {
                    let index = entry.get_log_id().index;
                    let encoded = super::encode(&entry).map_err(storage_error)?;
                    table
                        .insert(index, frame_value(encoded))
                        .map_err(|e| storage_error(io_message(e)))?;
                }
            }
            // The commit is what makes it durable. The callback is what tells
            // consensus it may count this node towards a quorum, so it comes
            // strictly after. The other order would acknowledge a write that a kill
            // could still lose.
            write.commit().map_err(|e| storage_error(io_message(e)))?;
            callback.log_io_completed(Ok(()));
            Ok(())
        })
    }

    async fn truncate(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        blocking(move || {
            let write = self.write().map_err(storage_error)?;
            {
                let mut table = write
                    .open_table(LOG)
                    .map_err(|e| storage_error(io_message(e)))?;
                let doomed: Vec<u64> = table
                    .range(log_id.index..)
                    .map_err(|e| storage_error(io_message(e)))?
                    .map(|row| row.map(|(key, _)| key.value()))
                    .collect::<Result<_, _>>()
                    .map_err(|e| storage_error(io_message(e)))?;
                for index in doomed {
                    table
                        .remove(index)
                        .map_err(|e| storage_error(io_message(e)))?;
                }
            }
            write.commit().map_err(|e| storage_error(io_message(e)))
        })
    }

    async fn purge(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        blocking(move || {
            let write = self.write().map_err(storage_error)?;
            {
                let mut table = write
                    .open_table(LOG)
                    .map_err(|e| storage_error(io_message(e)))?;
                let doomed: Vec<u64> = table
                    .range(..=log_id.index)
                    .map_err(|e| storage_error(io_message(e)))?
                    .map(|row| row.map(|(key, _)| key.value()))
                    .collect::<Result<_, _>>()
                    .map_err(|e| storage_error(io_message(e)))?;
                for index in doomed {
                    table
                        .remove(index)
                        .map_err(|e| storage_error(io_message(e)))?;
                }
                let mut meta = write
                    .open_table(META)
                    .map_err(|e| storage_error(io_message(e)))?;
                let encoded = super::encode(&log_id).map_err(storage_error)?;
                meta.insert(PURGED, encoded)
                    .map_err(|e| storage_error(io_message(e)))?;
            }
            write.commit().map_err(|e| storage_error(io_message(e)))
        })
    }
}

impl RaftSnapshotBuilder<TypeConfig> for GroupStorage {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        blocking(move || {
            // **The position and the state are read together, and first.** openraft
            // builds a snapshot on its own task while entries keep applying, so a
            // position read after the seal could name entries the seal did not
            // cover, and a state read apart from its position could hold commands
            // the position does not claim. `apply` writes the two in one
            // transaction, so one read sees a pair that agrees.
            let (applied, state) = self.applied_pair().map_err(storage_error)?;

            // A snapshot is what permits the log behind it to be purged, so the
            // machine gets to make its state reachable another way first. A tablet
            // seals, and the seal starts after the position above was read, so it
            // covers every row that position claims.
            //
            // **A failure here is not a storage failure.** openraft stops the whole
            // group on a storage error, and a disk near its reserve would then stop
            // every replica of a tablet at the same log index. The previous
            // snapshot is the honest answer: it moves nothing, so nothing is
            // purged, and the log keeps growing until the seal works again.
            if let Err(reason) = self.before_snapshot_guarded() {
                self.counters
                    .snapshot_failures
                    .fetch_add(1, Ordering::Relaxed);
                *self
                    .counters
                    .last_snapshot_failure
                    .lock()
                    .expect("snapshot failure") = Some(reason);
                return self.previous_snapshot();
            }

            let membership: StoredMembership<NodeId, BasicNode> = self
                .meta(MEMBERSHIP)
                .map_err(storage_error)?
                .unwrap_or_default();
            let data = match self
                .machine
                .snapshot_for_peer(state.unwrap_or_else(|| self.machine.snapshot()))
            {
                Ok(data) => data,
                Err(reason) => {
                    self.counters
                        .snapshot_failures
                        .fetch_add(1, Ordering::Relaxed);
                    *self
                        .counters
                        .last_snapshot_failure
                        .lock()
                        .expect("snapshot failure") = Some(reason);
                    return self.previous_snapshot();
                }
            };

            let count = {
                let mut count = self.snapshot_count.lock().expect("snapshot count");
                *count += 1;
                *count
            };
            let meta = SnapshotMeta {
                last_log_id: applied,
                last_membership: membership,
                snapshot_id: format!("{}-{count}", applied.map(|l| l.index).unwrap_or(0)),
            };
            // One transaction, so a kill cannot pair new metadata with old data.
            let write = self.write().map_err(storage_error)?;
            {
                let mut table = write
                    .open_table(META)
                    .map_err(|e| storage_error(io_message(e)))?;
                table
                    .insert(SNAPSHOT_META, super::encode(&meta).map_err(storage_error)?)
                    .map_err(|e| storage_error(io_message(e)))?;
                table
                    .insert(SNAPSHOT_DATA, data.clone())
                    .map_err(|e| storage_error(io_message(e)))?;
            }
            write.commit().map_err(|e| storage_error(io_message(e)))?;
            Ok(Snapshot {
                meta,
                snapshot: Box::new(Cursor::new(data)),
            })
        })
    }
}

impl RaftStateMachine<TypeConfig> for GroupStorage {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<NodeId>>, StoredMembership<NodeId, BasicNode>), StorageError<NodeId>>
    {
        blocking(move || {
            let membership = self
                .meta(MEMBERSHIP)
                .map_err(storage_error)?
                .unwrap_or_default();
            // The machine is where `APPLIED` says only if its state was written
            // with it. A file from before that record holds a machine restored from
            // its snapshot, so the honest position is the snapshot's, and openraft
            // applies the log after it again. See [`GroupStorage::open`].
            if self.machine_state().map_err(storage_error)?.is_none() {
                let snapshot: Option<SnapshotMeta<NodeId, BasicNode>> =
                    self.meta(SNAPSHOT_META).map_err(storage_error)?;
                return Ok((snapshot.and_then(|meta| meta.last_log_id), membership));
            }
            let applied = self.meta(APPLIED).map_err(storage_error)?.flatten();
            Ok((applied, membership))
        })
    }

    async fn apply<I>(
        &mut self,
        entries: I,
    ) -> Result<Vec<super::GroupResponse>, StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
    {
        blocking(move || {
            let mut answers = Vec::new();
            let mut applied: Option<LogId<NodeId>> = None;
            let mut membership: Option<StoredMembership<NodeId, BasicNode>> = None;

            for entry in entries {
                let log_id = *entry.get_log_id();
                applied = Some(log_id);
                let outcome = match entry.payload {
                    // A failure that is this replica's alone stops the group here.
                    // Nothing below runs, so `APPLIED` does not move past an entry
                    // this replica does not hold, and a restart applies it again.
                    EntryPayload::Normal(request) => self
                        .machine
                        .apply(log_id.index, &request.payload)
                        .map_err(storage_error)?,
                    EntryPayload::Membership(new) => {
                        membership = Some(StoredMembership::new(Some(log_id), new));
                        Vec::new()
                    }
                    // A blank entry is what a new leader writes to establish its
                    // term. It carries no application meaning.
                    EntryPayload::Blank => Vec::new(),
                };
                answers.push(super::GroupResponse {
                    applied_index: log_id.index,
                    outcome,
                });
            }

            // One durable write for the whole apply batch, and the machine's state
            // goes in it. openraft applies nothing at or below `APPLIED` after a
            // restart, so a machine that lives in memory has to be written with
            // the position that describes it, in the same transaction, or a
            // restart loses every command since the last snapshot.
            if applied.is_some() || membership.is_some() {
                let state = self.machine.snapshot();
                let write = self.write().map_err(storage_error)?;
                {
                    let mut table = write
                        .open_table(META)
                        .map_err(|e| storage_error(io_message(e)))?;
                    if let Some(applied) = applied {
                        let encoded = super::encode(&Some(applied)).map_err(storage_error)?;
                        table
                            .insert(APPLIED, encoded)
                            .map_err(|e| storage_error(io_message(e)))?;
                        table
                            .insert(MACHINE_STATE, state)
                            .map_err(|e| storage_error(io_message(e)))?;
                    }
                    if let Some(membership) = membership {
                        let encoded = super::encode(&membership).map_err(storage_error)?;
                        table
                            .insert(MEMBERSHIP, encoded)
                            .map_err(|e| storage_error(io_message(e)))?;
                    }
                }
                write.commit().map_err(|e| storage_error(io_message(e)))?;
            }
            Ok(answers)
        })
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<NodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<NodeId>> {
        blocking(move || {
            let data = snapshot.into_inner();
            let index = meta.last_log_id.map(|id| id.index).unwrap_or(0);
            let peers: Vec<String> = meta
                .last_membership
                .nodes()
                .map(|(_, node)| node.addr.clone())
                .collect();
            // A machine whose snapshot does not carry everything fetches the
            // rest here, and a failure fails the install: nothing below runs,
            // so this replica keeps reporting the position it really holds.
            self.machine
                .install_from_peer(&data, index, &peers)
                .map_err(storage_error)?;
            // What a restart restores is the machine's own state, which is
            // smaller than what a peer was sent. See
            // `GroupMachine::snapshot_for_peer`.
            let state = self.machine.snapshot();
            // One transaction. Four would let a kill pair a new position with old
            // data, and a node that restarted there would claim entries its machine
            // never saw.
            let write = self.write().map_err(storage_error)?;
            {
                let mut table = write
                    .open_table(META)
                    .map_err(|e| storage_error(io_message(e)))?;
                for (key, bytes) in [
                    (
                        APPLIED,
                        super::encode(&meta.last_log_id).map_err(storage_error)?,
                    ),
                    (
                        MEMBERSHIP,
                        super::encode(&meta.last_membership).map_err(storage_error)?,
                    ),
                    (SNAPSHOT_META, super::encode(meta).map_err(storage_error)?),
                    (MACHINE_STATE, state),
                    (SNAPSHOT_DATA, data),
                ] {
                    table
                        .insert(key, bytes)
                        .map_err(|e| storage_error(io_message(e)))?;
                }
            }
            write.commit().map_err(|e| storage_error(io_message(e)))
        })
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<NodeId>> {
        blocking(move || {
            let Some(meta): Option<SnapshotMeta<NodeId, BasicNode>> =
                self.meta(SNAPSHOT_META).map_err(storage_error)?
            else {
                return Ok(None);
            };
            let data = self
                .meta_raw(SNAPSHOT_DATA)
                .map_err(storage_error)?
                .unwrap_or_default();
            Ok(Some(Snapshot {
                meta,
                snapshot: Box::new(Cursor::new(data)),
            }))
        })
    }
}
