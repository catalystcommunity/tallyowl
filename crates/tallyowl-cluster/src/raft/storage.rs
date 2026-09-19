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

use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
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

/// The durable state one group keeps.
pub struct GroupStorage {
    database: Arc<Database>,
    machine: Arc<dyn GroupMachine>,
    /// How many snapshots this node has built, so each gets a distinct name.
    /// It is not part of the replicated state and does not have to survive a
    /// restart; a restarted node that reuses a name is harmless because the
    /// name is qualified by the last applied index.
    snapshot_count: Arc<Mutex<u64>>,
}

impl Clone for GroupStorage {
    fn clone(&self) -> GroupStorage {
        GroupStorage {
            database: Arc::clone(&self.database),
            machine: Arc::clone(&self.machine),
            snapshot_count: Arc::clone(&self.snapshot_count),
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "The consensus directory {} could not be made: {e}",
                    parent.display()
                )
            })?;
        }
        let database = Database::create(path).map_err(|e| {
            format!(
                "The consensus log at {} could not be opened: {e}",
                path.display()
            )
        })?;
        let storage = GroupStorage {
            database: Arc::new(database),
            machine,
            snapshot_count: Arc::new(Mutex::new(0)),
        };
        // Create both tables once, so every later read can assume they exist
        // and a read on a fresh database is not an error.
        let write = storage.write()?;
        {
            write.open_table(LOG).map_err(io_message)?;
            write.open_table(META).map_err(io_message)?;
        }
        write.commit().map_err(io_message)?;

        // A restart replays the state machine from its snapshot. The log after
        // it is replayed by openraft, which knows how far the machine got.
        if let Some(data) = storage.meta_raw(SNAPSHOT_DATA)? {
            storage.machine.install(&data)?;
        }
        Ok(storage)
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
    }
}

impl RaftLogStorage<TypeConfig> for GroupStorage {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<NodeId>> {
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
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), StorageError<NodeId>> {
        self.put_meta(VOTE, vote).map_err(storage_error)
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError<NodeId>> {
        self.meta(VOTE).map_err(storage_error)
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<NodeId>>,
    ) -> Result<(), StorageError<NodeId>> {
        self.put_meta(COMMITTED, &committed).map_err(storage_error)
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<NodeId>>, StorageError<NodeId>> {
        Ok(self.meta(COMMITTED).map_err(storage_error)?.flatten())
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
    {
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
    }

    async fn truncate(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
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
    }

    async fn purge(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
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
    }
}

impl RaftSnapshotBuilder<TypeConfig> for GroupStorage {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        // A snapshot is what permits the log behind it to be purged, so the
        // machine gets to make its state reachable another way first. A tablet
        // seals; a failure here fails the snapshot and the log stays.
        self.machine.before_snapshot().map_err(storage_error)?;

        let applied: Option<LogId<NodeId>> = self.meta(APPLIED).map_err(storage_error)?.flatten();
        let membership: StoredMembership<NodeId, BasicNode> = self
            .meta(MEMBERSHIP)
            .map_err(storage_error)?
            .unwrap_or_default();
        let data = self.machine.snapshot();

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
        self.put_meta(SNAPSHOT_META, &meta).map_err(storage_error)?;
        self.put_meta_raw(SNAPSHOT_DATA, data.clone())
            .map_err(storage_error)?;
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

impl RaftStateMachine<TypeConfig> for GroupStorage {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<NodeId>>, StoredMembership<NodeId, BasicNode>), StorageError<NodeId>>
    {
        let applied = self.meta(APPLIED).map_err(storage_error)?.flatten();
        let membership = self
            .meta(MEMBERSHIP)
            .map_err(storage_error)?
            .unwrap_or_default();
        Ok((applied, membership))
    }

    async fn apply<I>(
        &mut self,
        entries: I,
    ) -> Result<Vec<super::GroupResponse>, StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
    {
        let mut answers = Vec::new();
        let mut applied: Option<LogId<NodeId>> = None;
        let mut membership: Option<StoredMembership<NodeId, BasicNode>> = None;

        for entry in entries {
            let log_id = *entry.get_log_id();
            applied = Some(log_id);
            let outcome = match entry.payload {
                EntryPayload::Normal(request) => self.machine.apply(log_id.index, &request.payload),
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

        // One durable write for the whole apply batch. The state machine's own
        // durability is its business: the store fsyncs a commit before it
        // returns, and a mark that is lost is re-applied from the log.
        if applied.is_some() || membership.is_some() {
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
        let data = snapshot.into_inner();
        self.machine.install(&data).map_err(storage_error)?;
        self.put_meta(APPLIED, &meta.last_log_id)
            .map_err(storage_error)?;
        self.put_meta(MEMBERSHIP, &meta.last_membership)
            .map_err(storage_error)?;
        self.put_meta(SNAPSHOT_META, meta).map_err(storage_error)?;
        self.put_meta_raw(SNAPSHOT_DATA, data)
            .map_err(storage_error)?;
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<NodeId>> {
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
    }
}
