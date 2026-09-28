//! The state machines the three group kinds run.
//!
//! A group's state machine is the part TallyOwl owns. Consensus decides *which*
//! commands run and *in what order*; this decides what each one means.
//!
//! # The rule every one of these holds
//!
//! **Applying the same entry twice gives what applying it once gives.** A state
//! machine is fed from a log, and a log is replayed after a restart and after a
//! snapshot install. A machine that counted, appended, or generated an
//! identifier on each apply would diverge between replicas, and consensus would
//! not notice: it agrees on the log, not on the result.
//!
//! The tablet machine gets this from the store's own deduplication on
//! `(source_id, batch_id)`, which Phase 1 built and every phase since has
//! relied on. The controller and directory machines get it by returning "no
//! change" rather than an error when a command is already applied.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use tallyowl_store::row::EventRow;
use tallyowl_store::Store;

use crate::directory::{Directory, DirectoryCommand};
use crate::topology::{ControllerCommand, Topology};

/// What a group's state machine must do.
///
/// It is deliberately narrow. Everything a machine needs to be correct under
/// consensus is here, and nothing else is.
pub trait GroupMachine: Send + Sync + 'static {
    /// Apply one committed command. The answer is encoded, and empty when the
    /// machine has none.
    ///
    /// **There are two ways a command can go wrong, and they are not the same.**
    ///
    /// A command that no replica could apply — it does not decode, it names a
    /// tablet that does not exist — returns its reason as the outcome. Consensus
    /// already committed it and every replica reaches the same refusal, so the
    /// replicas stay in the same state.
    ///
    /// A failure that belongs to this replica alone — its disk is full, its
    /// device returned an error — returns `Err`. Marking that entry applied
    /// would skip it on this replica for ever while every other replica holds
    /// it, and nothing would ever say so. The group stops on this node instead,
    /// and the entry is applied when the node comes back.
    fn apply(&self, index: u64, payload: &[u8]) -> Result<Vec<u8>, String>;

    /// Whether this replica can take a write now.
    ///
    /// A tablet answers for its store. A follower that cannot write refuses new
    /// entries before it acknowledges them, so it never counts towards a quorum
    /// for a batch it cannot hold.
    fn is_writable(&self) -> bool {
        true
    }

    /// The whole state, encoded, for a snapshot.
    fn snapshot(&self) -> Vec<u8>;

    /// Replace the whole state from a snapshot.
    fn install(&self, snapshot: &[u8]) -> Result<(), String>;

    /// What a snapshot sent to a peer carries, given the state it stands for.
    ///
    /// The state a machine writes beside its applied position is written on
    /// every apply, so it has to be small. A peer that installs a snapshot may
    /// need more than that, and this is where a machine adds it. A failure here
    /// fails the snapshot, and the log behind it is not purged.
    fn snapshot_for_peer(&self, state: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(state)
    }

    /// Install a snapshot the leader sent, because this replica fell behind
    /// what the leader still keeps log for.
    ///
    /// `index` is the log position the snapshot stands for and `peers` are the
    /// addresses of the group's members. **A machine whose snapshot does not
    /// carry everything must fetch the rest here, and fail if it cannot.** Once
    /// this returns, the replica reports itself level with `index`, counts
    /// towards a quorum, and answers reads.
    fn install_from_peer(
        &self,
        snapshot: &[u8],
        _index: u64,
        _peers: &[String],
    ) -> Result<(), String> {
        self.install(snapshot)
    }

    /// Make everything this snapshot will cover reachable some other way.
    ///
    /// **A snapshot is what permits the log behind it to be purged**, so this
    /// runs first and a failure here fails the snapshot rather than purging
    /// anyway. A tablet seals its store, because a replica that catches up by
    /// segment copy receives only what is in a segment; the controller and the
    /// directory carry their whole state in the snapshot and have nothing to
    /// do here.
    fn before_snapshot(&self) -> Result<(), String> {
        Ok(())
    }
}

/// What a replicated command produced, in the shape every machine answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    /// The command applied and moved the state to this generation.
    Applied { generation: u64 },
    /// The command applied and changed nothing, which is what a replay does.
    Unchanged { generation: u64 },
    /// The command was committed and could not be applied. The reason travels
    /// so that the caller learns it rather than timing out.
    Refused { reason: String },
    /// A tablet commit.
    Committed {
        accepted: u64,
        committed_at: i64,
        commit_watermark: u64,
        deduplicated: bool,
    },
}

// ---------------------------------------------------------------------------
// The cell controller
// ---------------------------------------------------------------------------

/// The cell controller quorum's state machine: the topology.
pub struct ControllerMachine {
    state: Arc<Mutex<Topology>>,
}

impl ControllerMachine {
    pub fn new(state: Arc<Mutex<Topology>>) -> ControllerMachine {
        ControllerMachine { state }
    }

    pub fn topology(&self) -> Topology {
        self.state.lock().expect("topology").clone()
    }

    pub fn shared(&self) -> Arc<Mutex<Topology>> {
        Arc::clone(&self.state)
    }
}

impl GroupMachine for ControllerMachine {
    fn apply(&self, _index: u64, payload: &[u8]) -> Result<Vec<u8>, String> {
        let command: ControllerCommand = match super::decode(payload) {
            Ok(command) => command,
            Err(reason) => return Ok(encode_outcome(&Outcome::Refused { reason })),
        };
        let mut topology = self.state.lock().expect("topology");
        let before = topology.generation;
        let outcome = match topology.apply(&command) {
            Ok(generation) if generation == before => Outcome::Unchanged { generation },
            Ok(generation) => Outcome::Applied { generation },
            Err(e) => Outcome::Refused {
                reason: e.to_string(),
            },
        };
        Ok(encode_outcome(&outcome))
    }

    fn snapshot(&self) -> Vec<u8> {
        super::encode(&*self.state.lock().expect("topology")).unwrap_or_default()
    }

    fn install(&self, snapshot: &[u8]) -> Result<(), String> {
        let restored: Topology = super::decode(snapshot)?;
        *self.state.lock().expect("topology") = restored;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The global directory
// ---------------------------------------------------------------------------

/// The global directory quorum's state machine.
pub struct DirectoryMachine {
    state: Arc<Mutex<Directory>>,
}

impl DirectoryMachine {
    pub fn new(state: Arc<Mutex<Directory>>) -> DirectoryMachine {
        DirectoryMachine { state }
    }

    pub fn directory(&self) -> Directory {
        self.state.lock().expect("directory").clone()
    }
}

impl GroupMachine for DirectoryMachine {
    fn apply(&self, _index: u64, payload: &[u8]) -> Result<Vec<u8>, String> {
        let command: DirectoryCommand = match super::decode(payload) {
            Ok(command) => command,
            Err(reason) => return Ok(encode_outcome(&Outcome::Refused { reason })),
        };
        let mut directory = self.state.lock().expect("directory");
        let before = directory.generation;
        let outcome = match directory.apply(&command) {
            Ok(generation) if generation == before => Outcome::Unchanged { generation },
            Ok(generation) => Outcome::Applied { generation },
            Err(e) => Outcome::Refused {
                reason: e.to_string(),
            },
        };
        Ok(encode_outcome(&outcome))
    }

    fn snapshot(&self) -> Vec<u8> {
        super::encode(&*self.state.lock().expect("directory")).unwrap_or_default()
    }

    fn install(&self, snapshot: &[u8]) -> Result<(), String> {
        let restored: Directory = super::decode(snapshot)?;
        *self.state.lock().expect("directory") = restored;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// A tablet
// ---------------------------------------------------------------------------

/// What a tablet group replicates.
///
/// A commit carries the rows. `docs/STORAGE.md` section 5 puts segment
/// construction after the committed log rather than inside it, so what is
/// replicated is the batch and not the segment: a follower builds its own
/// segments from the same rows, and two replicas that compacted differently
/// still hold the same data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TabletCommand {
    Commit {
        source_id: [u8; 16],
        batch_id: [u8; 16],
        /// The batch, in the same encoding the append log carries. Replicating
        /// the store's own frame rather than a second shape means one codec, so
        /// a leader and a follower cannot read a row differently.
        ///
        /// A byte string, not a sequence. See [`super::byte_string`] and L096.
        #[serde(with = "super::byte_string")]
        rows: Vec<u8>,
    },
    /// A tombstone is a standing predicate, so it is replicated rather than
    /// applied locally: a replica that missed it would answer a query with data
    /// an erasure removed. `AGENTS.md`, "Delivery and data".
    ///
    /// **The whole predicate travels, encoded.** An earlier shape carried a
    /// column, a value, and a generation, and applying it only advanced a
    /// number: a follower bumped its generation and hid nothing. A predicate is
    /// what hides a row, so a predicate is what has to arrive.
    Tombstone {
        /// The predicate, in the store's own encoding. One codec, so a leader
        /// and a follower cannot read an erasure differently — the same reason
        /// a commit carries the store's own frame.
        #[serde(with = "super::byte_string")]
        predicate: Vec<u8>,
    },
    /// The generation a compaction may reclaim below. Replicated so that two
    /// replicas cannot disagree about what is still readable, which is what
    /// `docs/PLAN.md` Phase 7 means by compaction generation safety.
    CompactionGeneration { generation: u64 },
}

/// A tablet's state machine: the local store.
///
/// The store already deduplicates on `(source_id, batch_id)` and already makes
/// a commit durable before it returns, so applying a replayed entry gives one
/// logical commit and nothing here has to arrange that separately.
pub struct TabletMachine {
    store: Arc<dyn Store>,
    /// The tombstone and compaction generations this replica has applied. They
    /// are part of the snapshot, so a replica that catches up by snapshot
    /// rather than by log reaches the same generations.
    marks: Mutex<TabletMarks>,
    catch_up: Option<Arc<dyn TabletCatchUp>>,
    erasures: Option<ErasureSource>,
    writable: Option<WritableProbe>,
}

/// How a tablet replica gets the rows a snapshot stands for.
///
/// A tablet snapshot carries marks and not rows, because a snapshot that held a
/// tablet's rows would be one message as large as the tablet. The rows travel
/// as sealed segments. [`crate::transfer::SegmentCatchUp`] is the real one.
pub trait TabletCatchUp: Send + Sync + 'static {
    /// Bring this replica's store up to at least `index`, from one of `peers`,
    /// and say what position the replica it copied from had reached.
    fn catch_up(&self, index: u64, peers: &[String]) -> Result<u64, String>;
}

/// Every standing erasure this replica holds, each in the store's encoding.
pub type ErasureSource = Arc<dyn Fn() -> Result<Vec<Vec<u8>>, String> + Send + Sync>;

/// Whether this replica's store can take a write now.
pub type WritableProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// One standing erasure, as a byte string. See [`super::byte_string`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Erasure(#[serde(with = "super::byte_string")] Vec<u8>);

/// What a tablet sends a peer that fell behind.
///
/// **The erasures travel here because nothing else carries them.** An erasure
/// reaches a replica as a log entry, and a replica that installs a snapshot
/// skipped the entries behind it. The segments it then copies still hold the
/// erased rows until a compaction rewrites them, so without the predicates it
/// would answer a query with data an erasure removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TabletSnapshot {
    marks: TabletMarks,
    #[serde(default)]
    erasures: Vec<Erasure>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabletMarks {
    pub tombstone_generation: u64,
    pub compaction_generation: u64,
    pub applied_index: u64,
    /// When this replica last applied a commit, in milliseconds since the
    /// epoch, as the commit itself recorded.
    ///
    /// **This is how old this replica's newest data is, not how far behind the
    /// leader it is.** A follower cannot know the second without asking the
    /// leader, and a number that claimed to be the second while being the first
    /// would tell a bounded-stale reader the opposite of the truth on an idle
    /// tablet. `docs/STORAGE.md` section 8 asks a read replica to report its
    /// exact high-water marks, and `applied_index` is that; this is beside it.
    pub last_applied_at: i64,
    /// The position of the replica this one last copied segments from.
    ///
    /// **Entries up to here may already be in this store.** A catch-up copies
    /// everything its source had sealed, and the source is usually ahead of the
    /// snapshot this replica was sent. The entries between the two then arrive
    /// by log as well. The store deduplicates a batch by its receipt, and a
    /// copied segment brings rows and no receipts, so those entries are checked
    /// row by row. Past this position nothing was copied and nothing is
    /// checked.
    #[serde(default)]
    pub copied_through: u64,
}

impl TabletMachine {
    pub fn new(store: Arc<dyn Store>) -> TabletMachine {
        TabletMachine {
            store,
            marks: Mutex::new(TabletMarks::default()),
            catch_up: None,
            erasures: None,
            writable: None,
        }
    }

    /// Say how this replica fetches rows when it installs a snapshot. Without
    /// this, a replica that would need rows refuses the snapshot.
    pub fn catching_up_with(mut self, catch_up: Arc<dyn TabletCatchUp>) -> TabletMachine {
        self.catch_up = Some(catch_up);
        self
    }

    /// Say where this replica's standing erasures are read from, so a snapshot
    /// for a peer can carry them.
    pub fn carrying_erasures_from(mut self, erasures: ErasureSource) -> TabletMachine {
        self.erasures = Some(erasures);
        self
    }

    /// Say how this replica knows its store can take a write. The store's own
    /// answer is still asked; this adds what the store contract does not
    /// report, which is the space reserve.
    pub fn writable_when(mut self, writable: WritableProbe) -> TabletMachine {
        self.writable = Some(writable);
        self
    }

    /// The rows of one batch that this store does not already hold.
    ///
    /// See [`TabletMarks::copied_through`]. `None` means every row is held.
    fn not_already_copied(
        &self,
        index: u64,
        rows: Vec<EventRow>,
    ) -> Result<Option<Vec<EventRow>>, String> {
        if index > self.marks().copied_through {
            return Ok(Some(rows));
        }
        let mut missing = Vec::with_capacity(rows.len());
        for row in rows {
            let held = self.store.lookup_event(row.event_id).map_err(|e| {
                format!("This replica could not check a batch against the segments it copied, so it stopped rather than count the batch twice: {e}")
            })?;
            if held.is_none() {
                missing.push(row);
            }
        }
        Ok((!missing.is_empty()).then_some(missing))
    }

    fn read_snapshot(snapshot: &[u8]) -> Result<TabletSnapshot, String> {
        // A snapshot from before erasures travelled is the marks alone.
        match super::decode::<TabletSnapshot>(snapshot) {
            Ok(whole) => Ok(whole),
            Err(_) => Ok(TabletSnapshot {
                marks: super::decode(snapshot)?,
                erasures: Vec::new(),
            }),
        }
    }

    fn apply_erasures(&self, erasures: &[Erasure]) -> Result<(), String> {
        for erasure in erasures {
            let tombstone = tallyowl_store::catalog::decode_tombstone(&erasure.0)
                .map_err(|e| format!("An erasure in a snapshot could not be read: {e}"))?;
            // Applying one twice is applying it once. See `apply`.
            self.store.erase(&tombstone).map_err(|e| {
                format!("An erasure in a snapshot could not be applied, so the snapshot was not installed: {e}")
            })?;
        }
        Ok(())
    }

    pub fn store(&self) -> Arc<dyn Store> {
        Arc::clone(&self.store)
    }

    pub fn marks(&self) -> TabletMarks {
        self.marks.lock().expect("tablet marks").clone()
    }
}

/// Sort a store failure into the two kinds [`GroupMachine::apply`] tells apart.
///
/// An argument the store refuses is refused by every replica, because every
/// replica was handed the same bytes. Everything else is about this node's
/// device, and another replica may well have succeeded.
fn local_failure(what: &str, e: tallyowl_store::StoreError) -> Result<Outcome, String> {
    match e {
        tallyowl_store::StoreError::InvalidArgument(reason) => Ok(Outcome::Refused { reason }),
        other => Err(format!(
            "This replica could not {what}, so it stopped rather than skipping an entry the other replicas hold: {other}"
        )),
    }
}

impl GroupMachine for TabletMachine {
    fn apply(&self, index: u64, payload: &[u8]) -> Result<Vec<u8>, String> {
        let command: TabletCommand = match super::decode(payload) {
            Ok(command) => command,
            Err(reason) => return Ok(encode_outcome(&Outcome::Refused { reason })),
        };
        let outcome = match command {
            TabletCommand::Commit {
                source_id,
                batch_id,
                rows,
            } => match tallyowl_store::row_codec::decode_rows(&rows)
                .map_err(|e| format!("A replicated batch could not be read: {e}"))
            {
                Err(reason) => Outcome::Refused { reason },
                Ok(rows) => match self.not_already_copied(index, rows)? {
                    // Every row arrived in a copied segment. That is this
                    // batch, committed once.
                    None => Outcome::Committed {
                        accepted: 0,
                        committed_at: self.marks().last_applied_at,
                        commit_watermark: self.store.commit_watermark(),
                        deduplicated: true,
                    },
                    Some(rows) => match self.store.commit(source_id, batch_id, rows) {
                        Ok(result) => {
                            self.marks.lock().expect("tablet marks").last_applied_at =
                                result.committed_at;
                            Outcome::Committed {
                                accepted: result.accepted,
                                committed_at: result.committed_at,
                                commit_watermark: result.commit_watermark,
                                deduplicated: result.deduplicated,
                            }
                        }
                        Err(e) => local_failure("commit a replicated batch", e)?,
                    },
                },
            },
            TabletCommand::Tombstone { predicate } => {
                match tallyowl_store::catalog::decode_tombstone(&predicate) {
                    Err(e) => Outcome::Refused {
                        reason: format!("A replicated erasure could not be read: {e}"),
                    },
                    Ok(tombstone) => {
                        // **The predicate is applied on every replica**, which
                        // is what makes an erasure an erasure. Applying it
                        // twice is applying it once: the erasure ledger is
                        // keyed by the tombstone identifier, so a replayed
                        // entry replaces the same record rather than adding a
                        // second one.
                        match self.store.erase(&tombstone) {
                            Err(e) => local_failure("apply a replicated erasure", e)?,
                            Ok(generation) => {
                                let mut marks = self.marks.lock().expect("tablet marks");
                                let before = marks.tombstone_generation;
                                marks.tombstone_generation = generation;
                                if generation == before {
                                    Outcome::Unchanged { generation }
                                } else {
                                    Outcome::Applied { generation }
                                }
                            }
                        }
                    }
                }
            }
            TabletCommand::CompactionGeneration { generation } => {
                let mut marks = self.marks.lock().expect("tablet marks");
                if generation <= marks.compaction_generation {
                    Outcome::Unchanged {
                        generation: marks.compaction_generation,
                    }
                } else {
                    marks.compaction_generation = generation;
                    Outcome::Applied { generation }
                }
            }
        };
        // Only now, because an entry that failed locally was not applied and
        // must not read as if it had been.
        self.marks.lock().expect("tablet marks").applied_index = index;
        Ok(encode_outcome(&outcome))
    }

    fn is_writable(&self) -> bool {
        self.store.is_writable() && self.writable.as_ref().is_none_or(|probe| probe())
    }

    fn snapshot(&self) -> Vec<u8> {
        // A tablet snapshot carries the marks. The rows themselves are the
        // store's own durable state and are transferred as segments, which is
        // what `docs/STORAGE.md` section 6 says a move copies. Putting a whole
        // tablet's rows in a consensus snapshot would make one message as large
        // as the tablet.
        super::encode(&self.marks()).unwrap_or_default()
    }

    fn install(&self, snapshot: &[u8]) -> Result<(), String> {
        let restored = TabletMachine::read_snapshot(snapshot)?;
        self.apply_erasures(&restored.erasures)?;
        *self.marks.lock().expect("tablet marks") = restored.marks;
        Ok(())
    }

    fn snapshot_for_peer(&self, state: Vec<u8>) -> Result<Vec<u8>, String> {
        let Some(erasures) = &self.erasures else {
            return Ok(state);
        };
        let whole = TabletSnapshot {
            marks: super::decode(&state)?,
            erasures: erasures()?.into_iter().map(Erasure).collect(),
        };
        super::encode(&whole)
    }

    fn install_from_peer(
        &self,
        snapshot: &[u8],
        index: u64,
        peers: &[String],
    ) -> Result<(), String> {
        let restored = TabletMachine::read_snapshot(snapshot)?;
        // **The rows first, and the install fails without them.** The snapshot
        // holds marks. A replica that installed it and fetched nothing would
        // report itself level with the leader, count towards a quorum, win an
        // election, and answer reads, while missing every row between where it
        // was and `index`.
        let held = self.marks().applied_index;
        let mut copied_through = self.marks().copied_through;
        if held < index {
            let Some(catch_up) = &self.catch_up else {
                return Err(format!(
                    "This replica is at entry {held} and was sent a snapshot at entry {index}. A tablet snapshot carries no rows and this node was not told how to copy them, so it did not install the snapshot."
                ));
            };
            copied_through = copied_through.max(catch_up.catch_up(index, peers)?);
        }
        self.apply_erasures(&restored.erasures)?;
        *self.marks.lock().expect("tablet marks") = TabletMarks {
            // This replica's own fact, not the sender's.
            copied_through,
            ..restored.marks
        };
        Ok(())
    }

    fn before_snapshot(&self) -> Result<(), String> {
        // **This is the sentence that lets the log be purged.** A snapshot says
        // "everything below here is covered", and for a tablet the cover is a
        // sealed segment rather than the snapshot's own bytes. Rows that were
        // applied and not yet sealed are in the store's own append log, which
        // is local to this replica: a replica catching up by segment copy would
        // never see them, and the consensus log that did hold them would be
        // gone.
        //
        // A failure returns, and openraft then fails the snapshot, so the log
        // keeps growing rather than losing what it holds. Growing is a disk
        // problem and losing is a data problem.
        self.store
            .seal_now()
            .map(|_| ())
            .map_err(|e| format!("This tablet could not seal before its snapshot, so the snapshot was not taken and the log was not purged: {e}"))
    }
}

/// Put a batch into the shape a replicated commit carries.
pub fn encode_rows(rows: &[EventRow]) -> Vec<u8> {
    tallyowl_store::row_codec::encode_rows(rows)
}

fn encode_outcome(outcome: &Outcome) -> Vec<u8> {
    super::encode(outcome).unwrap_or_default()
}

/// Read an outcome back. A machine that could not encode its answer gives an
/// empty one, which reads as an internal refusal rather than as success.
pub fn read_outcome(bytes: &[u8]) -> Outcome {
    super::decode(bytes).unwrap_or(Outcome::Refused {
        reason: "The state machine did not report what it did.".to_string(),
    })
}
