//! The append log: the final storage acceptance boundary.
//!
//! `docs/STORAGE.md` section 3.1. Each record is a length-delimited,
//! checksummed frame, and the single-node path fsyncs the frame before it sends
//! an acknowledgement.
//!
//! # Group commit is not an optimization
//!
//! A measurement put the device ceiling at 186 fsync operations each second.
//! Group commit with a 2 millisecond linger then reached 16,923 durable frames
//! each second from 134 of them. The same path without a linger reached 503.
//! See D47 and BENCHMARKS.md section 13.
//!
//! The committer obeys three rules, and the first is the one a prototype got
//! wrong and reported no gain at all:
//!
//! - **it releases the lock while it writes and calls fsync**, so other writers
//!   accumulate into the next group;
//! - it waits a configurable linger before it seals a group, defaulting to
//!   2 milliseconds;
//! - it bounds a group by bytes and by count, so a large group cannot exhaust
//!   memory or hold a caller past its deadline.
//!
//! A longer linger is worse, not better. At 10 milliseconds the same benchmark
//! lost half its throughput and doubled its latency.
//!
//! # Recovery
//!
//! The scanner truncates only a torn final frame, replays complete frames after
//! the catalog checkpoint, and resumes. Nothing acknowledged is lost, which is
//! what D53 means by "a process crash is not a recovery event".

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::segment::format::{get_u32, get_u64, page_checksum, put_u32, put_u64, FormatError};

/// The first bytes of every append log, so a file that is not one is refused
/// rather than parsed.
pub const WAL_MAGIC: &[u8; 8] = b"TOWLWAL1";

/// A frame header: payload length, log position, and a checksum over the
/// payload.
pub const FRAME_HEADER_BYTES: usize = 20;

/// D47. The default linger, and the measured best of the four modes.
pub const DEFAULT_LINGER: Duration = Duration::from_millis(2);

/// Reclaim a prefix smaller than half the file once it reaches this size.
///
/// Without a floor, a log whose backlog never shrinks would keep a prefix for
/// ever waiting to reach half. Four megabytes is small enough that the copy is
/// one sequential write and large enough that it is rare.
pub const MIN_RECLAIM_BYTES: usize = 4 * 1024 * 1024;

/// How long a reclamation waits for a group commit to finish before it gives
/// up. A group commit is one write and one fsync, so this is several orders of
/// magnitude above the expected wait and only a broken device reaches it.
pub const RECLAIM_WAIT: Duration = Duration::from_secs(5);

/// A group has a byte bound and a count bound as well as the time bound, so a
/// large group cannot exhaust memory or hold a caller past its deadline.
pub const DEFAULT_MAX_GROUP_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_MAX_GROUP_FRAMES: u64 = 4_096;

/// What one appended frame holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The monotonically assigned tablet log position.
    pub position: u64,
    pub payload: Vec<u8>,
}

/// Why an append or a recovery failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalError {
    /// The device refused the write. A caller must not acknowledge.
    Unavailable(String),
    /// The bytes on disk are not the bytes that were written.
    Damaged(String),
}

impl std::fmt::Display for WalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalError::Unavailable(m) | WalError::Damaged(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for WalError {}

impl From<FormatError> for WalError {
    fn from(error: FormatError) -> WalError {
        match error {
            FormatError::Damaged(m) | FormatError::Unsupported(m) => WalError::Damaged(m),
            FormatError::Incomplete(m) => WalError::Damaged(m),
        }
    }
}

/// How a group commit behaves. Every value is configurable, and the defaults
/// are the measured ones.
#[derive(Debug, Clone, Copy)]
pub struct GroupCommit {
    pub linger: Duration,
    pub max_group_bytes: usize,
    pub max_group_frames: u64,
    /// Reclaim a prefix smaller than half the file once it reaches this size.
    /// Zero reclaims whatever is covered, whenever it is asked.
    pub min_reclaim_bytes: usize,
}

impl Default for GroupCommit {
    fn default() -> GroupCommit {
        GroupCommit {
            linger: DEFAULT_LINGER,
            max_group_bytes: DEFAULT_MAX_GROUP_BYTES,
            max_group_frames: DEFAULT_MAX_GROUP_FRAMES,
            min_reclaim_bytes: MIN_RECLAIM_BYTES,
        }
    }
}

/// The append log's internal state, for a caller that is diagnosing a stall.
///
/// Every field is one a person reading L131 would want. **`committing` with an
/// unchanging `durable_before` over several seconds is the signature to look
/// for**: it means a committer took a group and has not come back, which is a
/// device or a lock outside this file rather than the protocol inside it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalState {
    pub committing: bool,
    pub reclaim_wanted: u64,
    pub pending_bytes: u64,
    pub pending_frames: u64,
    pub write_offset: u64,
    pub durable_upto: u64,
    pub durable_before: u64,
    pub next_position: u64,
    pub failure: Option<String>,
}

impl WalState {
    /// One line, for a log field.
    pub fn to_line(&self) -> String {
        format!(
            "committing={} reclaim_wanted={} pending_bytes={} pending_frames={} write_offset={} durable_upto={} durable_before={} next_position={} failure={}",
            self.committing,
            self.reclaim_wanted,
            self.pending_bytes,
            self.pending_frames,
            self.write_offset,
            self.durable_upto,
            self.durable_before,
            self.next_position,
            self.failure.as_deref().unwrap_or("none")
        )
    }
}

/// What one group commit did, for the metrics `docs/STORAGE.md` section 14 asks
/// for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WalStatistics {
    pub frames: u64,
    pub bytes: u64,
    pub fsyncs: u64,
    pub largest_group: u64,
}

struct Shared {
    /// The open log.
    ///
    /// It is behind an `Arc` so that a committer can clone the handle, release
    /// the lock, and then write and fsync. Holding the lock across the fsync
    /// stops every other caller adding to the next group, which is the whole
    /// mechanism. See `append`.
    file: Arc<File>,
    /// Bytes accepted and not yet made durable.
    pending: Vec<u8>,
    pending_frames: u64,
    /// Where the next write lands.
    write_offset: u64,
    /// Every byte below this offset is durable.
    ///
    /// **A byte offset is file-relative, and the file gets rewritten.**
    /// `reclaim_through` and `truncate_to_empty` both move this backwards,
    /// because the new file is shorter. Nothing may wait on it. See
    /// `durable_position`.
    durable_upto: u64,
    /// The first position that is **not** yet durable. Every position below it
    /// is on disk.
    ///
    /// **This is what a caller waits on**, and the reason is that a position is
    /// logical where an offset is physical. A reclamation rewrites the file and
    /// resets `durable_upto` to the start of the new one; a caller waiting on
    /// an absolute offset it captured before the rewrite would then wait for an
    /// offset the file can never reach again, for ever. A position is assigned
    /// once, never reused, and never rewritten. See L131.
    ///
    /// It is an **exclusive** bound because positions start at zero, so there
    /// is no value of an inclusive one that means "nothing is durable yet".
    durable_before: u64,
    committing: bool,
    /// How many callers are waiting to rewrite the file.
    ///
    /// A committer stands down when this is not zero and its own frame is
    /// durable, and a new caller waits rather than taking the role. Both are
    /// what stop a busy log from starving its own reclamation.
    reclaim_wanted: u64,
    next_position: u64,
    statistics: WalStatistics,
    /// Set when the device refused a write, and never cleared.
    ///
    /// FAILURE_MODES.md section 10: stop accepting writes. A log that failed
    /// once holds a gap, and a frame written after the gap is unreachable to
    /// recovery, so accepting one would be acknowledging data nobody can read
    /// back. Reopening the log is what clears this, because reopening truncates
    /// to the last complete frame.
    failure: Option<String>,
    /// Test-only fault injection: refuse the next write as a device would.
    ///
    /// `docs/FAILURE_MODES.md` requires a test for the append-log refusal, and
    /// filling a real device to get one is not a test anybody runs. This is a
    /// fault injected into the real code path, not a mock of the storage
    /// interface, which `AGENTS.md` forbids for a different reason.
    #[cfg(test)]
    fail_next_write: bool,
}

/// One tablet's append log.
pub struct Wal {
    path: PathBuf,
    settings: GroupCommit,
    shared: Mutex<Shared>,
    durable: Condvar,
}

impl Wal {
    /// Open, replaying what is there.
    ///
    /// A torn final frame is truncated, which is the only truncation recovery
    /// performs. Every complete frame before it survives.
    pub fn open(path: impl AsRef<Path>, settings: GroupCommit) -> Result<Arc<Wal>, WalError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                WalError::Unavailable(format!(
                    "The data directory {} could not be created: {e}",
                    parent.display()
                ))
            })?;
        }

        let recovered = recover(&path)?;

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| {
                WalError::Unavailable(format!(
                    "The append log {} could not be opened: {e}",
                    path.display()
                ))
            })?;

        // Cut the torn tail. Recovery truncates only a torn final frame.
        file.set_len(recovered.durable_bytes).map_err(|e| {
            WalError::Unavailable(format!("The append log could not be trimmed: {e}"))
        })?;
        if recovered.durable_bytes == 0 {
            file.write_all_at(WAL_MAGIC, 0).map_err(|e| {
                WalError::Unavailable(format!("The append log could not be started: {e}"))
            })?;
            file.sync_data().map_err(|e| {
                WalError::Unavailable(format!("The append log could not be flushed to disk: {e}"))
            })?;
        }
        let write_offset = recovered.durable_bytes.max(WAL_MAGIC.len() as u64);

        Ok(Arc::new(Wal {
            path,
            settings,
            shared: Mutex::new(Shared {
                file: Arc::new(file),
                pending: Vec::with_capacity(settings.max_group_bytes.min(1 << 20)),
                pending_frames: 0,
                write_offset,
                durable_upto: write_offset,
                // Everything recovery replayed is on disk, so the first
                // position that is not durable is the next one to be assigned.
                durable_before: recovered.next_position,
                committing: false,
                reclaim_wanted: 0,
                next_position: recovered.next_position,
                statistics: WalStatistics::default(),
                failure: None,
                #[cfg(test)]
                fail_next_write: false,
            }),
            durable: Condvar::new(),
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one payload and return only after it is durable.
    ///
    /// One fsync may make many frames durable, and each caller returns after its
    /// own bytes are durable and not before. That is what keeps the durability
    /// contract intact while removing one physical sync for each caller.
    ///
    /// # Why this cannot wait for ever
    ///
    /// L131 records an intermittent hang that nobody explained, and the
    /// explanation matters more than the fix, so it is written here where it
    /// stays true. A caller waits only inside the loop below, and it leaves that
    /// loop when either of two things holds:
    ///
    /// 1. its own position is durable, or the log has failed; or
    /// 2. `committing` is false, in which case it becomes the committer itself.
    ///
    /// So an unbounded wait needs `committing` to stay true with no committer
    /// making progress. **A committer never waits.** It writes, it syncs, and
    /// every path out of its loop clears `committing` and wakes the others,
    /// including the failure path. It acquires no other lock while it holds this
    /// one. `append` is therefore free of any wait cycle of its own, and a hang
    /// that involves the append log has to involve a lock outside it.
    ///
    /// That is the argument the earlier work was missing, and it is why two
    /// changes could each move the hang rate without either one being the cause:
    /// both changed how long this function holds the lock, and neither changed
    /// whether it can wait.
    pub fn append(&self, payload: &[u8]) -> Result<u64, WalError> {
        let mut shared = self.shared.lock().expect("append log lock");

        // A log that could not make a write durable stops accepting.
        // FAILURE_MODES.md section 10: stop accepting writes, and never accept a
        // write that cannot be made durable. Taking a position here would leave
        // a caller waiting for bytes nothing will ever write.
        if let Some(failure) = &shared.failure {
            return Err(WalError::Unavailable(failure.clone()));
        }

        let position = shared.next_position;
        shared.next_position += 1;
        let frame = encode_frame(position, payload);
        shared.pending.extend_from_slice(&frame);
        shared.pending_frames += 1;

        // **Wait on the position, never on a byte offset.** See
        // `Shared::durable_position`: a reclamation rewrites the file and moves
        // every offset backwards, and a caller holding an offset from before
        // the rewrite would wait for one the file can never reach again.
        loop {
            if position < shared.durable_before {
                return Ok(position);
            }
            if let Some(failure) = &shared.failure {
                return Err(WalError::Unavailable(failure.clone()));
            }
            // **A waiting reclamation goes first.** Without this clause a
            // steady stream of callers hands the committer role straight from
            // one to the next and a reclamation never sees `committing` false.
            // That is not a rare interleaving: it is what a busy log does, and
            // it is measured below.
            if !shared.committing && shared.reclaim_wanted == 0 {
                // Nobody is going to make these bytes durable. Take it on.
                break;
            }
            shared = self.durable.wait(shared).expect("append log lock");
        }

        shared.committing = true;

        // The linger is what makes a group large. Without it a group holds only
        // what arrived while the previous fsync was in flight, and the measured
        // difference is 503 frames each second against 16,923.
        if !self.settings.linger.is_zero() {
            drop(shared);
            std::thread::sleep(self.settings.linger);
            shared = self.shared.lock().expect("append log lock");
        }

        loop {
            // **Stand down for a reclamation, once this caller's own frame is
            // safe.** A committer keeps the role while anything is pending, so
            // on a busy log one committer holds it without a break and nothing
            // else ever gets the file to itself. Measured: six callers appending
            // without a pause produced **no reclamation at all** in twenty-five
            // seconds, and the log kept every byte it had ever taken. Yielding
            // here costs the next caller one handover.
            if shared.reclaim_wanted > 0 && position < shared.durable_before {
                shared.committing = false;
                self.durable.notify_all();
                return Ok(position);
            }

            // Take at most one bounded group, so a large group cannot exhaust
            // memory or hold a caller past its deadline.
            let take = if shared.pending.len() > self.settings.max_group_bytes
                || shared.pending_frames > self.settings.max_group_frames
            {
                let cut = frame_boundary_before(&shared.pending, self.settings.max_group_bytes);
                let taken = shared.pending.drain(..cut).collect::<Vec<u8>>();
                // **Decrement, or the bound latches on for ever.** This branch
                // used to leave `pending_frames` alone, so the first group that
                // crossed the frame bound made the condition permanently true
                // and every later group was byte-bounded however small it was.
                let frames = frames_in(&taken);
                shared.pending_frames = shared.pending_frames.saturating_sub(frames);
                taken
            } else {
                shared.pending_frames = 0;
                std::mem::take(&mut shared.pending)
            };

            if take.is_empty() {
                shared.committing = false;
                self.durable.notify_all();
                // **Nothing is pending, so this caller's frame either went out
                // in a group or it never will.** This used to return `Ok`
                // whatever the state was, which reported a durable append to a
                // caller whose frame a failed group had already dropped. A
                // receipt for data that is not on disk is the one failure
                // `docs/DELIVERY.md` section 3 says cannot happen.
                if position < shared.durable_before {
                    return Ok(position);
                }
                return Err(WalError::Unavailable(match &shared.failure {
                    Some(failure) => failure.clone(),
                    None => "We could not make this data durable, so we did not accept it. \
                             The append log has nothing left to write and this frame is not \
                             on disk."
                        .to_string(),
                }));
            }

            // The highest position this group carries. Read from the bytes
            // rather than tracked beside them, so the two cannot disagree.
            let covers = last_position_in(&take);

            let at = shared.write_offset;
            shared.write_offset += take.len() as u64;
            // The frames in **this** group. It used to read `pending_frames`
            // after the take, which is what was left behind rather than what
            // went out, and which the unbounded branch had just set to zero.
            let group = frames_in(&take);
            shared.statistics.largest_group = shared.statistics.largest_group.max(group);

            // Release the lock across the expensive part. A committer that
            // holds it prevents accumulation and defeats the mechanism, which
            // is exactly the mistake that made a prototype report no gain:
            // `docs/BENCHMARKS.md` records 503 frames each second against
            // 16,923 once it was fixed.
            //
            // **The handle is cloned so that the write and the sync happen with
            // the lock released.** This used to drop the lock and immediately
            // take it again to reach `shared.file`, which held it across the
            // fsync and did the very thing the paragraph above warns about. An
            // `Arc<File>` costs a refcount and lets the bytes go out while other
            // callers keep filling the next group. See L131.
            let file = Arc::clone(&shared.file);
            #[cfg(test)]
            let refuse = std::mem::take(&mut shared.fail_next_write);
            drop(shared);
            let outcome = file.write_all_at(&take, at).and_then(|()| file.sync_data());
            #[cfg(test)]
            let outcome = match refuse {
                true => Err(std::io::Error::other("the device refused the write")),
                false => outcome,
            };
            shared = self.shared.lock().expect("append log lock");

            if let Err(error) = outcome {
                // **The whole log stops here, and this is the second half of a
                // defect rather than caution.** The group this committer took is
                // gone from `pending`, and it carried other callers' frames as
                // well as this one's. Those callers are waiting, and before this
                // they woke, found nothing pending, and returned `Ok` — a
                // success receipt for bytes no device ever took.
                //
                // A gap in the file is the other half. The next group would
                // write past the range this one failed on, and recovery stops at
                // the first frame that does not check out, so everything after
                // the gap would be acknowledged and unreadable.
                //
                // FAILURE_MODES.md section 10, append log: stop accepting writes
                // and never accept a write that cannot be made durable. The log
                // refuses every later append until the process reopens it, and
                // `failure` is what the readiness report reads.
                let message = format!(
                    "We could not make this data durable, so we did not accept it. {error}"
                );
                shared.failure = Some(message.clone());
                shared.pending.clear();
                shared.pending_frames = 0;
                shared.committing = false;
                self.durable.notify_all();
                return Err(WalError::Unavailable(message));
            }

            let end = at + take.len() as u64;
            shared.statistics.fsyncs += 1;
            shared.statistics.bytes += take.len() as u64;
            if end > shared.durable_upto {
                shared.durable_upto = end;
            }
            if let Some(covers) = covers {
                shared.durable_before = shared.durable_before.max(covers + 1);
            }
            self.durable.notify_all();

            if position < shared.durable_before && shared.pending.is_empty() {
                shared.committing = false;
                self.durable.notify_all();
                return Ok(position);
            }
        }
    }

    /// The next position this log will assign.
    pub fn next_position(&self) -> u64 {
        self.shared.lock().expect("append log lock").next_position
    }

    /// Every durable byte written so far.
    pub fn durable_bytes(&self) -> u64 {
        self.shared.lock().expect("append log lock").durable_upto
    }

    pub fn statistics(&self) -> WalStatistics {
        self.shared.lock().expect("append log lock").statistics
    }

    /// Everything the append log's internal state says about itself.
    ///
    /// **This exists for one reason: L131.** An intermittent hang was found by
    /// somebody noticing that four shells were still running, and the state
    /// that would have explained it was inside a mutex nothing could read.
    /// `append` is now proved unable to wait indefinitely, so a future stall
    /// involves a lock outside this file — and the first question anybody will
    /// ask is whether the log was the thing that was stuck. This answers it in
    /// one call, from outside, without a debugger.
    ///
    /// It takes the lock, so a caller that is diagnosing a stall learns
    /// something either way: an answer says the log is not held, and no answer
    /// says it is.
    pub fn state(&self) -> WalState {
        let shared = self.shared.lock().expect("append log lock");
        WalState {
            committing: shared.committing,
            reclaim_wanted: shared.reclaim_wanted,
            pending_bytes: shared.pending.len() as u64,
            pending_frames: shared.pending_frames,
            write_offset: shared.write_offset,
            durable_upto: shared.durable_upto,
            durable_before: shared.durable_before,
            next_position: shared.next_position,
            failure: shared.failure.clone(),
        }
    }

    /// Why the log stopped accepting writes, when it has.
    ///
    /// FAILURE_MODES.md section 10 asks for readiness to fail before the device
    /// is full, and this is what a health report reads to say so in words a
    /// person can act on.
    pub fn failure(&self) -> Option<String> {
        self.shared.lock().expect("append log lock").failure.clone()
    }

    /// Make the next group commit fail, as a device that refused a write would.
    #[cfg(test)]
    fn fail_next_write(&self) {
        self.shared.lock().expect("append log lock").fail_next_write = true;
    }

    /// Replay every complete frame at or after `from`.
    ///
    /// The catalog checkpoint decides `from`, so a replay after a crash covers
    /// exactly the range a segment has not yet taken.
    pub fn replay(&self, from: u64) -> Result<Vec<Frame>, WalError> {
        let frames = recover(&self.path)?.frames;
        Ok(frames.into_iter().filter(|f| f.position >= from).collect())
    }

    /// Remove every frame at or below `through_position` and keep the rest.
    ///
    /// STORAGE.md section 5 step 7: advance the checkpoint and remove covered
    /// log ranges. **Covered ranges, not the whole file.** A seal publishes the
    /// rows it captured, and a commit that lands while that seal is building
    /// sits above the published range. Its frame is the only durable copy of an
    /// acknowledged batch until a later seal takes it, so removing the whole
    /// file would lose data that TallyOwl reported as committed.
    ///
    /// The rewrite is atomic: a new file is written beside this one and renamed
    /// over it. A crash before the rename leaves the old log, which replays
    /// frames a segment already holds, and deduplication makes that harmless. A
    /// crash after it leaves the new one. There is no state in between.
    ///
    /// Returns the bytes reclaimed, which is zero when there is nothing to do.
    pub fn reclaim_through(&self, through_position: u64) -> Result<u64, WalError> {
        let mut shared = self.shared.lock().expect("append log lock");

        // **A group in flight is the one thing a rewrite must not race.** Its
        // committer captured an absolute offset and a handle to the old file
        // before it released the lock, and it is writing there now. A rewrite
        // would rename a new file over that one and swap the handle, so the
        // bytes would land in an inode nothing can open, and the committer would
        // then report them durable.
        //
        // **Pending bytes are not that.** A pending frame has no offset yet; it
        // gets one when a group takes it, from `write_offset`, which this
        // rewrite resets. Refusing while anything was pending as well was the
        // first half of the defect below.
        //
        // **The second half was stepping aside and hoping.** This used to return
        // zero and leave it to the next seal, and on a busy log the next seal
        // found the same thing, for ever: a committer keeps the role while
        // callers keep arriving, so `committing` never went false. Six callers
        // appending without a pause produced no reclamation at all in
        // twenty-five seconds and the log kept 100 percent of what it had taken.
        // An append log that never shrinks while the installation is busy is the
        // failure L095 already paid for once.
        //
        // So it asks, and waits. `append` stands a committer down once that
        // committer's own frame is durable, and holds back a new one, so the
        // wait is bounded by one group commit.
        shared.reclaim_wanted += 1;
        while shared.committing {
            let (guard, timed_out) = self
                .durable
                .wait_timeout(shared, RECLAIM_WAIT)
                .expect("append log lock");
            shared = guard;
            if timed_out.timed_out() {
                // A committer that has not finished a group in this long is a
                // device problem, not a scheduling one. Reclamation is never
                // urgent, so it gives up rather than holding a caller.
                shared.reclaim_wanted -= 1;
                self.durable.notify_all();
                return Ok(0);
            }
        }
        // Nothing else can run until this releases the lock, so the count comes
        // down here rather than after the rewrite. **The wake matters**: a
        // caller that parked because a reclamation was waiting has nothing else
        // coming to wake it, and leaving that out wedged every writer on the
        // first run of this code.
        shared.reclaim_wanted -= 1;
        self.durable.notify_all();

        let magic = WAL_MAGIC.len() as u64;
        if shared.durable_upto <= magic {
            return Ok(0);
        }

        let mut bytes = vec![0u8; (shared.durable_upto - magic) as usize];
        shared
            .file
            .read_exact_at(&mut bytes, magic)
            .map_err(|e| WalError::Unavailable(format!("The append log could not be read: {e}")))?;

        // Walk to the first frame this reclamation must keep.
        let mut at = 0usize;
        while at < bytes.len() {
            let length = get_u32(&bytes, at)? as usize;
            let position = get_u64(&bytes, at + 4)?;
            if position > through_position {
                break;
            }
            let next = at + FRAME_HEADER_BYTES + length;
            if next > bytes.len() {
                // A torn tail. `open` truncates it; this leaves it alone rather
                // than copying a partial frame into the new file.
                break;
            }
            at = next;
        }
        if at == 0 {
            return Ok(0);
        }

        // **Rewrite only when the prefix is worth the copy.**
        //
        // A seal runs often and reclaims the range it just published, which is
        // usually small next to the backlog behind it. Rewriting on every seal
        // therefore copies the whole tail each time, and the cost is quadratic
        // in the backlog: the first load run after this function landed put the
        // head at about 15 batches each second, against a collector taking
        // 37,000 events each second, because it was moving tens of megabytes
        // for each seal.
        //
        // Waiting until the reclaimable prefix is at least half the file makes
        // the copy amortised: each byte is moved at most once for each doubling,
        // so the total work is linear. The cost of waiting is bounded and
        // stated: the log holds at most twice what it needs.
        let tail_bytes = bytes.len() - at;
        if at < tail_bytes && at < self.settings.min_reclaim_bytes {
            return Ok(0);
        }

        let tail = &bytes[at..];
        let temporary = self.path.with_extension("reclaiming");
        {
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&temporary)
                .map_err(|e| {
                    WalError::Unavailable(format!("The append log could not be rewritten: {e}"))
                })?;
            file.write_all_at(WAL_MAGIC, 0).map_err(|e| {
                WalError::Unavailable(format!("The append log could not be rewritten: {e}"))
            })?;
            file.write_all_at(tail, magic).map_err(|e| {
                WalError::Unavailable(format!("The append log could not be rewritten: {e}"))
            })?;
            file.sync_data().map_err(|e| {
                WalError::Unavailable(format!("The append log could not be flushed to disk: {e}"))
            })?;
        }

        std::fs::rename(&temporary, &self.path).map_err(|e| {
            WalError::Unavailable(format!("The append log could not be replaced: {e}"))
        })?;
        // The rename itself has to reach the device, or a crash could leave the
        // directory entry pointing at the old file.
        if let Some(parent) = self.path.parent() {
            if let Ok(directory) = File::open(parent) {
                let _ = directory.sync_all();
            }
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|e| {
                WalError::Unavailable(format!(
                    "The append log {} could not be reopened: {e}",
                    self.path.display()
                ))
            })?;
        shared.file = Arc::new(file);
        shared.write_offset = magic + tail.len() as u64;
        shared.durable_upto = shared.write_offset;
        Ok(at as u64)
    }

    /// Remove the log once every frame it holds is covered by a published
    /// segment. STORAGE.md section 5 step 8: advance the checkpoint and
    /// eventually remove covered files.
    pub fn truncate_to_empty(&self) -> Result<(), WalError> {
        let mut shared = self.shared.lock().expect("append log lock");
        // The same rule `reclaim_through` obeys, and for the same reason: a
        // committer that released the lock is writing at an offset this would
        // cut away, and it would report those bytes durable.
        if shared.committing || !shared.pending.is_empty() {
            return Err(WalError::Unavailable(
                "The append log cannot be emptied while it is taking writes.".to_string(),
            ));
        }
        shared.file.set_len(WAL_MAGIC.len() as u64).map_err(|e| {
            WalError::Unavailable(format!("The append log could not be trimmed: {e}"))
        })?;
        shared.file.sync_data().map_err(|e| {
            WalError::Unavailable(format!("The append log could not be flushed to disk: {e}"))
        })?;
        shared.write_offset = WAL_MAGIC.len() as u64;
        shared.durable_upto = shared.write_offset;
        Ok(())
    }
}

fn encode_frame(position: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
    put_u32(&mut out, payload.len() as u32);
    put_u64(&mut out, position);
    put_u64(&mut out, page_checksum(payload));
    out.extend_from_slice(payload);
    out
}

/// The position of the last whole frame in `bytes`, when there is one.
///
/// A group is a byte range of whole frames in ascending position order, so the
/// last frame's position is the highest the group carries. It is read from the
/// bytes rather than tracked alongside them, because two records of one fact
/// drift and this one decides when a caller is told its data is durable.
fn frames_in(bytes: &[u8]) -> u64 {
    let mut at = 0usize;
    let mut count = 0u64;
    while let Ok(length) = get_u32(bytes, at) {
        let next = at + FRAME_HEADER_BYTES + length as usize;
        if next > bytes.len() {
            break;
        }
        count += 1;
        at = next;
    }
    count
}

fn last_position_in(bytes: &[u8]) -> Option<u64> {
    let mut at = 0usize;
    let mut last = None;
    loop {
        let Ok(length) = get_u32(bytes, at) else {
            return last;
        };
        let Ok(position) = get_u64(bytes, at + 4) else {
            return last;
        };
        let next = at + FRAME_HEADER_BYTES + length as usize;
        if next > bytes.len() {
            return last;
        }
        last = Some(position);
        at = next;
    }
}

/// The largest whole number of frames inside `limit` bytes.
fn frame_boundary_before(bytes: &[u8], limit: usize) -> usize {
    let mut at = 0;
    loop {
        let Ok(length) = get_u32(bytes, at) else {
            return at;
        };
        let next = at + FRAME_HEADER_BYTES + length as usize;
        if next > bytes.len() || next > limit {
            // Always take at least one frame, so a frame larger than the bound
            // still makes progress rather than stalling forever.
            return if at == 0 { next.min(bytes.len()) } else { at };
        }
        at = next;
    }
}

struct Recovered {
    frames: Vec<Frame>,
    /// How many bytes hold complete frames. A torn tail beyond this is cut.
    durable_bytes: u64,
    next_position: u64,
}

/// Read every complete frame, and say where the complete part ends.
fn recover(path: &Path) -> Result<Recovered, WalError> {
    if !path.exists() {
        return Ok(Recovered {
            frames: Vec::new(),
            durable_bytes: 0,
            next_position: 0,
        });
    }

    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|e| {
            WalError::Unavailable(format!(
                "The append log {} could not be read: {e}",
                path.display()
            ))
        })?;

    if bytes.len() < WAL_MAGIC.len() {
        // A file that never reached its own magic holds nothing, which is the
        // ordinary result of a crash during creation.
        return Ok(Recovered {
            frames: Vec::new(),
            durable_bytes: 0,
            next_position: 0,
        });
    }
    if &bytes[..WAL_MAGIC.len()] != WAL_MAGIC {
        return Err(WalError::Damaged(format!(
            "The file {} is where the append log should be and is not one.",
            path.display()
        )));
    }

    let mut frames = Vec::new();
    let mut at = WAL_MAGIC.len();
    let mut complete = at;
    let mut next_position = 0u64;

    while at < bytes.len() {
        let Ok(length) = get_u32(&bytes, at) else {
            break;
        };
        let length = length as usize;
        let payload_at = at + FRAME_HEADER_BYTES;
        if payload_at + length > bytes.len() {
            // A torn final frame. Everything before it survives.
            break;
        }
        let position = get_u64(&bytes, at + 4)?;
        let checksum = get_u64(&bytes, at + 12)?;
        let payload = &bytes[payload_at..payload_at + length];

        if page_checksum(payload) != checksum {
            // A frame that does not match its checksum ends the log. A crash
            // during a write is the ordinary cause, and a later frame written
            // after it would be from a different life of the process.
            break;
        }

        frames.push(Frame {
            position,
            payload: payload.to_vec(),
        });
        next_position = next_position.max(position + 1);
        at = payload_at + length;
        complete = at;
    }

    Ok(Recovered {
        frames,
        durable_bytes: complete as u64,
        next_position,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn directory(name: &str) -> PathBuf {
        // A real directory on real storage. Section 9 of the implementation
        // prompt forbids benchmarking storage on tmpfs, and a correctness test
        // uses the same path shape so the two stay honest.
        let base = std::env::var("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("target"));
        let path = base
            .join("wal-tests")
            .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a place to work");
        path
    }

    fn open(path: &Path) -> Arc<Wal> {
        Wal::open(path.join("tablet.wal"), GroupCommit::default()).expect("the log opens")
    }

    /// A log that reclaims whatever is covered, whenever it is asked. The
    /// amortisation threshold has its own test; these are about what the
    /// rewrite keeps and what it removes.
    fn open_eager(path: &Path) -> Arc<Wal> {
        Wal::open(
            path.join("tablet.wal"),
            GroupCommit {
                min_reclaim_bytes: 0,
                ..GroupCommit::default()
            },
        )
        .expect("the log opens")
    }

    #[test]
    fn an_appended_frame_is_durable_when_append_returns() {
        let place = directory("durable");
        let wal = open(&place);
        assert_eq!(wal.append(b"one").unwrap(), 0);
        assert_eq!(wal.append(b"two").unwrap(), 1);

        // Reopening is what a restart does. Nothing acknowledged is lost.
        drop(wal);
        let reopened = open(&place);
        let frames = reopened.replay(0).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].payload, b"one");
        assert_eq!(frames[1].position, 1);
        assert_eq!(reopened.next_position(), 2);
    }

    #[test]
    fn a_position_rises_and_never_repeats() {
        let place = directory("positions");
        let wal = open(&place);
        for expected in 0..50u64 {
            assert_eq!(wal.append(b"x").unwrap(), expected);
        }
        drop(wal);
        // A restart continues the sequence rather than starting over, or a
        // segment would be written twice under one position.
        let reopened = open(&place);
        assert_eq!(reopened.append(b"x").unwrap(), 50);
    }

    #[test]
    fn a_torn_final_frame_is_truncated_and_everything_before_it_survives() {
        // STORAGE.md section 5: the recovery scanner truncates only a torn
        // final frame.
        let place = directory("torn");
        let path = place.join("tablet.wal");
        {
            let wal = Wal::open(&path, GroupCommit::default()).unwrap();
            for n in 0..10u8 {
                wal.append(&[n; 64]).unwrap();
            }
        }

        // A crash part way through the next frame.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&[0u8; 30]);
        std::fs::write(&path, &bytes).unwrap();

        let wal = Wal::open(&path, GroupCommit::default()).unwrap();
        let frames = wal.replay(0).unwrap();
        assert_eq!(frames.len(), 10, "every complete frame survived");
        assert_eq!(wal.next_position(), 10);

        // And the log is usable again: the torn bytes are gone rather than
        // sitting in front of the next append.
        wal.append(b"after").unwrap();
        assert_eq!(wal.replay(0).unwrap().len(), 11);
    }

    #[test]
    fn a_frame_whose_checksum_does_not_match_ends_the_log() {
        // A crash during a write is the ordinary cause. A frame written after
        // it would be from a different life of the process, so recovery stops
        // rather than skipping past.
        let place = directory("checksum");
        let path = place.join("tablet.wal");
        {
            let wal = Wal::open(&path, GroupCommit::default()).unwrap();
            for n in 0..5u8 {
                wal.append(&[n; 32]).unwrap();
            }
        }
        let mut bytes = std::fs::read(&path).unwrap();
        // The third frame's payload, past the magic and two whole frames.
        let third = WAL_MAGIC.len() + 2 * (FRAME_HEADER_BYTES + 32) + FRAME_HEADER_BYTES;
        bytes[third] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        let wal = Wal::open(&path, GroupCommit::default()).unwrap();
        assert_eq!(wal.replay(0).unwrap().len(), 2);
    }

    #[test]
    fn a_replay_starts_at_the_checkpoint() {
        let place = directory("checkpoint");
        let wal = open(&place);
        for n in 0..10u8 {
            wal.append(&[n]).unwrap();
        }
        let frames = wal.replay(7).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].position, 7);
    }

    #[test]
    fn one_fsync_makes_many_frames_durable() {
        // The property D47 measured. Without it the device ceiling of 186 fsync
        // operations each second is the whole throughput.
        let place = directory("group");
        let wal = Wal::open(
            place.join("tablet.wal"),
            GroupCommit {
                linger: Duration::from_millis(2),
                ..GroupCommit::default()
            },
        )
        .unwrap();

        let written = Arc::new(AtomicU64::new(0));
        let mut threads = Vec::new();
        for _ in 0..16 {
            let wal = Arc::clone(&wal);
            let written = Arc::clone(&written);
            threads.push(std::thread::spawn(move || {
                for _ in 0..20 {
                    wal.append(&[7u8; 256]).unwrap();
                    written.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let frames = written.load(Ordering::Relaxed);
        assert_eq!(frames, 320);
        let statistics = wal.statistics();
        assert!(
            statistics.fsyncs < frames,
            "{} fsync operations for {frames} frames is one for each",
            statistics.fsyncs
        );
        // And every frame is there, which is the part that must not be traded
        // for the throughput.
        assert_eq!(wal.replay(0).unwrap().len(), 320);
    }

    #[test]
    fn every_position_is_distinct_under_concurrent_appends() {
        // A repeated position would make two batches one in the catalog.
        let place = directory("concurrent");
        let wal = open(&place);
        let mut threads = Vec::new();
        for _ in 0..8 {
            let wal = Arc::clone(&wal);
            threads.push(std::thread::spawn(move || {
                (0..25)
                    .map(|_| wal.append(b"x").unwrap())
                    .collect::<Vec<_>>()
            }));
        }
        let mut positions: Vec<u64> = threads
            .into_iter()
            .flat_map(|t| t.join().unwrap())
            .collect();
        positions.sort_unstable();
        let before = positions.len();
        positions.dedup();
        assert_eq!(before, positions.len(), "two appends shared a position");
        assert_eq!(positions.len(), 200);
    }

    #[test]
    fn a_group_stays_inside_its_byte_bound() {
        // A large group cannot exhaust memory or hold a caller past its
        // deadline, which is the third of D47's three rules.
        let place = directory("bounded");
        let wal = Wal::open(
            place.join("tablet.wal"),
            GroupCommit {
                linger: Duration::from_millis(0),
                max_group_bytes: 4 * 1024,
                max_group_frames: 8,
                ..GroupCommit::default()
            },
        )
        .unwrap();
        for _ in 0..50 {
            wal.append(&[1u8; 1_024]).unwrap();
        }
        assert_eq!(wal.replay(0).unwrap().len(), 50);
    }

    #[test]
    fn a_file_that_is_not_an_append_log_is_refused() {
        let place = directory("foreign");
        let path = place.join("tablet.wal");
        std::fs::write(&path, b"this is not an append log at all").unwrap();
        let Err(failure) = Wal::open(&path, GroupCommit::default()) else {
            panic!("a file that is not an append log was accepted as one");
        };
        assert!(matches!(failure, WalError::Damaged(_)));
        assert!(failure.to_string().contains("is not one"));
    }

    #[test]
    fn a_log_cut_before_its_own_magic_starts_over_rather_than_refusing() {
        // A crash during creation is the ordinary cause, and there is nothing
        // to lose, so this is not a failure a person needs to see.
        let place = directory("stub");
        let path = place.join("tablet.wal");
        std::fs::write(&path, b"TOW").unwrap();
        let wal = Wal::open(&path, GroupCommit::default()).unwrap();
        assert_eq!(wal.replay(0).unwrap().len(), 0);
        wal.append(b"first").unwrap();
        assert_eq!(wal.replay(0).unwrap().len(), 1);
    }

    #[test]
    fn an_empty_payload_is_a_frame_like_any_other() {
        let place = directory("empty");
        let wal = open(&place);
        wal.append(b"").unwrap();
        wal.append(b"after").unwrap();
        let frames = wal.replay(0).unwrap();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].payload.is_empty());
    }

    #[test]
    fn a_covered_log_is_trimmed_and_keeps_its_positions() {
        // STORAGE.md section 5 step 8. The positions must not restart, or a
        // replay after the next crash would collide with a published segment.
        let place = directory("trim");
        let wal = open(&place);
        for _ in 0..5 {
            wal.append(b"x").unwrap();
        }
        wal.truncate_to_empty().unwrap();
        assert_eq!(wal.replay(0).unwrap().len(), 0);
        assert_eq!(
            wal.append(b"after").unwrap(),
            5,
            "a trimmed log continues its positions"
        );
    }

    // -----------------------------------------------------------------------
    // Reclaiming a covered prefix
    // -----------------------------------------------------------------------

    #[test]
    fn reclaiming_a_prefix_keeps_every_frame_above_it() {
        // The property the store depends on. A seal publishes the rows it
        // captured and reclaims only that range; a commit that landed while the
        // seal was building sits above it, and its frame is the only durable
        // copy of an acknowledged batch until a later seal takes it.
        let place = directory("reclaim-prefix");
        let wal = open_eager(&place);
        for n in 0..6u8 {
            wal.append(&[n; 32]).expect("the append is durable");
        }
        let before = wal.durable_bytes();

        let reclaimed = wal.reclaim_through(2).expect("the prefix goes");
        assert!(reclaimed > 0, "something was reclaimed");
        assert!(wal.durable_bytes() < before, "the log got smaller");

        let kept = wal.replay(0).expect("the log replays");
        let positions: Vec<u64> = kept.iter().map(|frame| frame.position).collect();
        assert_eq!(positions, vec![3, 4, 5], "only the covered prefix went");
        assert_eq!(kept[0].payload, vec![3u8; 32], "and the payloads survived");
        assert_eq!(
            wal.next_position(),
            6,
            "a reclaimed log keeps counting where it was"
        );
    }

    #[test]
    fn a_reclaimed_log_reads_back_the_same_way_after_a_restart() {
        let place = directory("reclaim-restart");
        {
            let wal = open_eager(&place);
            for n in 0..6u8 {
                wal.append(&[n; 32]).expect("the append is durable");
            }
            wal.reclaim_through(3).expect("the prefix goes");
            wal.append(&[9; 32]).expect("the append is durable");
        }

        let reopened = open(&place);
        let frames = reopened.replay(0).expect("the log replays");
        let positions: Vec<u64> = frames.iter().map(|frame| frame.position).collect();
        assert_eq!(positions, vec![4, 5, 6]);
        assert_eq!(
            reopened.next_position(),
            7,
            "the position counter survives the rewrite"
        );
    }

    #[test]
    fn reclaiming_a_range_that_already_went_leaves_the_log_alone() {
        // A seal reclaims through the checkpoint each time it runs, and the
        // checkpoint does not move when a seal publishes nothing. Asking twice
        // for the same range must be free rather than rewrite the file again.
        let place = directory("reclaim-again");
        let wal = open_eager(&place);
        for n in 0..3u8 {
            wal.append(&[n; 16]).expect("the append is durable");
        }
        wal.reclaim_through(0).expect("the first frame goes");
        let after_first = wal.durable_bytes();

        assert_eq!(wal.reclaim_through(0).expect("no work"), 0);
        assert_eq!(wal.durable_bytes(), after_first, "the file did not move");
        let positions: Vec<u64> = wal
            .replay(0)
            .expect("replays")
            .iter()
            .map(|frame| frame.position)
            .collect();
        assert_eq!(positions, vec![1, 2]);
    }

    #[test]
    fn reclaiming_every_frame_empties_the_log_and_keeps_its_positions() {
        let place = directory("reclaim-all");
        let wal = open_eager(&place);
        for n in 0..4u8 {
            wal.append(&[n; 16]).expect("the append is durable");
        }
        wal.reclaim_through(3).expect("everything goes");

        assert!(wal.replay(0).expect("replays").is_empty());
        assert_eq!(wal.durable_bytes(), WAL_MAGIC.len() as u64);
        let position = wal.append(&[7; 16]).expect("the log still takes writes");
        assert_eq!(position, 4, "a reclaimed log never reuses a position");
        assert_eq!(wal.replay(0).expect("replays").len(), 1);
    }

    #[test]
    fn a_small_prefix_waits_rather_than_copying_the_whole_tail_each_time() {
        // The first load run after `reclaim_through` landed put the head at
        // about 15 batches each second against a collector taking 37,000 events
        // each second, because a seal reclaims the range it just published and
        // that range is small next to the backlog behind it. Copying the tail
        // for each seal is quadratic in the backlog.
        //
        // Waiting until the prefix is half the file makes it amortised: each
        // byte moves at most once for each doubling.
        let place = directory("reclaim-amortised");
        let wal = Wal::open(
            place.join("tablet.wal"),
            GroupCommit {
                min_reclaim_bytes: 1024,
                ..GroupCommit::default()
            },
        )
        .expect("the log opens");

        for _ in 0..20 {
            wal.append(&[7; 256]).expect("the append is durable");
        }
        let before = wal.durable_bytes();

        // One frame of twenty is far below half the file and below the floor,
        // so nothing moves and the frame stays readable.
        assert_eq!(wal.reclaim_through(0).expect("no work"), 0);
        assert_eq!(wal.durable_bytes(), before, "the file was rewritten anyway");
        assert_eq!(wal.replay(0).expect("replays").len(), 20);

        // Past half, it goes.
        assert!(wal.reclaim_through(14).expect("the prefix goes") > 0);
        let positions: Vec<u64> = wal
            .replay(0)
            .expect("replays")
            .iter()
            .map(|frame| frame.position)
            .collect();
        assert_eq!(positions, vec![15, 16, 17, 18, 19]);
    }

    #[test]
    fn an_empty_log_reclaims_nothing() {
        let place = directory("reclaim-empty");
        let wal = open_eager(&place);
        assert_eq!(wal.reclaim_through(100).expect("no work"), 0);
        assert_eq!(wal.durable_bytes(), WAL_MAGIC.len() as u64);
    }

    // -----------------------------------------------------------------------
    // What a caller waits on. See L131.
    // -----------------------------------------------------------------------

    #[test]
    fn reclaiming_moves_the_byte_offset_back_and_never_the_position() {
        // The invariant the deadlock broke. A reclamation rewrites the file, so
        // every byte offset in it restarts near zero. A caller waiting on an
        // absolute offset captured before the rewrite would wait for one the
        // file can never reach again.
        //
        // The position is what a caller waits on, and it is assigned once and
        // never rewritten. This asserts both halves: the offset goes backwards
        // and the position does not.
        let wal = Wal::open(
            directory("durable-position").join("t.wal"),
            GroupCommit {
                linger: Duration::ZERO,
                ..GroupCommit::default()
            },
        )
        .expect("the log opens");

        for _ in 0..8 {
            wal.append(&[9; 64]).expect("appends");
        }
        let (offset_before, position_before) = {
            let shared = wal.shared.lock().expect("append log lock");
            (shared.durable_upto, shared.durable_before)
        };
        assert!(position_before >= 8, "every frame appended is durable");

        // Reclaim everything but the last frame, which rewrites the file.
        wal.reclaim_through(position_before - 1).expect("reclaims");

        let (offset_after, position_after) = {
            let shared = wal.shared.lock().expect("append log lock");
            (shared.durable_upto, shared.durable_before)
        };
        assert!(
            offset_after < offset_before,
            "the rewrite did not shorten the file, so this proves nothing: {offset_after} against {offset_before}"
        );
        assert_eq!(
            position_after, position_before,
            "a reclamation moved the position a caller waits on"
        );
    }

    #[test]
    fn a_quiet_log_reports_a_state_a_person_can_read() {
        // L131 was found by somebody noticing that four shells were running,
        // and the state that would have explained it was inside a mutex nothing
        // could read. This is that state, and this asserts it means something
        // rather than that it exists.
        let place = directory("state");
        let wal = open(&place);
        let quiet = wal.state();
        assert!(!quiet.committing, "an idle log claims a group is in flight");
        assert_eq!(quiet.pending_frames, 0);
        assert_eq!(quiet.next_position, 0);
        assert!(quiet.failure.is_none());

        for _ in 0..3 {
            wal.append(b"x").expect("appends");
        }
        let after = wal.state();
        assert_eq!(after.next_position, 3);
        assert_eq!(
            after.durable_before, 3,
            "three appends returned and the log says none of them is durable"
        );
        assert_eq!(after.pending_frames, 0, "something was left behind");
        assert!(
            after.to_line().contains("durable_before=3"),
            "{}",
            after.to_line()
        );
    }

    #[test]
    fn a_failed_log_says_so_in_its_state() {
        // The stall detector reads this line. A log that had stopped accepting
        // writes and did not say so in the one place a watcher looks would send
        // somebody hunting a lock that was not the problem.
        let wal = Wal::open(
            directory("state-failed").join("t.wal"),
            GroupCommit {
                linger: Duration::ZERO,
                ..GroupCommit::default()
            },
        )
        .expect("the log opens");
        wal.fail_next_write();
        assert!(wal.append(b"x").is_err());
        let state = wal.state();
        assert!(state.failure.is_some());
        assert!(!state.committing, "a failed log left a group in flight");
        assert!(
            state.to_line().contains("failure=We could not"),
            "{}",
            state.to_line()
        );
    }

    #[test]
    fn a_refused_write_stops_the_log_and_no_caller_is_told_otherwise() {
        // FAILURE_MODES.md section 10, append log: never accept a write that
        // cannot be made durable, and stop accepting writes.
        //
        // The second caller is the point. Its frame goes out inside somebody
        // else's group, so when that group fails, its bytes leave `pending`
        // without reaching the device. It then woke, found nothing pending, and
        // returned `Ok` — a success receipt for data no device took.
        let wal = Wal::open(
            directory("refused-write").join("t.wal"),
            GroupCommit {
                linger: Duration::from_millis(60),
                ..GroupCommit::default()
            },
        )
        .expect("the log opens");

        wal.fail_next_write();

        let committer = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || wal.append(b"one"))
        };
        // Long enough to be inside the committer's linger, so this one parks as
        // a waiter rather than becoming a committer of its own.
        std::thread::sleep(Duration::from_millis(20));
        let waiter = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || wal.append(b"two"))
        };

        let first = committer.join().expect("the committer returns");
        let second = waiter.join().expect("the waiter returns");
        assert!(
            first.is_err(),
            "the committer was told a refused write was durable"
        );
        assert!(
            second.is_err(),
            "a waiter in the refused group was told its data was durable"
        );
        assert!(
            wal.failure().is_some(),
            "a refused write left the log looking healthy"
        );
        assert!(
            wal.append(b"after").is_err(),
            "the log took a write after it had already failed one, so recovery \
             would stop at the gap and everything after it would be lost"
        );
    }

    #[test]
    fn emptying_the_log_while_it_is_taking_writes_is_refused() {
        // Same rule as the reclamation: a committer that released the lock is
        // writing at an offset this would cut away.
        let wal = Wal::open(
            directory("trim-busy").join("t.wal"),
            GroupCommit {
                linger: Duration::from_millis(60),
                ..GroupCommit::default()
            },
        )
        .expect("the log opens");

        let writer = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || wal.append(&[1; 64]).expect("appends"))
        };
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            wal.truncate_to_empty().is_err(),
            "the log was emptied under a group that was still in flight"
        );
        writer.join().expect("the writer returns");
        wal.truncate_to_empty().expect("a quiet log empties");
    }

    #[test]
    fn a_log_that_is_taking_writes_still_reclaims() {
        // **This is a measurement, not a rule.** Reclamation used to step aside
        // whenever anything was pending as well as whenever a group was in
        // flight, and an installation that is taking writes always has something
        // pending. The hunt for L131 measured 428 rewrites in forty seconds with
        // two writers and **none at all** with four, which means the append log
        // grew without bound for as long as the installation stayed busy.
        //
        // A pending frame has no offset yet, so a rewrite cannot hurt it. Only a
        // group in flight can be hurt, and that is the only thing refused now.
        let wal = Wal::open(
            directory("reclaim-busy").join("t.wal"),
            GroupCommit {
                linger: Duration::from_micros(200),
                min_reclaim_bytes: 0,
                ..GroupCommit::default()
            },
        )
        .expect("the log opens");

        let stop = Arc::new(AtomicU64::new(0));
        let rewrites = Arc::new(AtomicU64::new(0));
        let reclaimer = {
            let wal = Arc::clone(&wal);
            let stop = Arc::clone(&stop);
            let rewrites = Arc::clone(&rewrites);
            std::thread::spawn(move || {
                while stop.load(Ordering::Relaxed) == 0 {
                    let upto = wal.next_position().saturating_sub(1);
                    if wal.reclaim_through(upto).expect("reclaims") > 0 {
                        rewrites.fetch_add(1, Ordering::Relaxed);
                    }
                    std::thread::yield_now();
                }
            })
        };

        let mut writers = Vec::new();
        for _ in 0..6 {
            let wal = Arc::clone(&wal);
            writers.push(std::thread::spawn(move || {
                for _ in 0..200 {
                    wal.append(&[3; 128]).expect("every append returns");
                }
            }));
        }
        for writer in writers {
            writer.join().expect("no writer wedged");
        }
        stop.store(1, Ordering::Relaxed);
        reclaimer.join().expect("the reclaimer stops");

        assert!(
            rewrites.load(Ordering::Relaxed) > 0,
            "not one reclamation ran while the log was busy, so this test proves \
             nothing and the log grows for ever under load"
        );
        // **The count is not the property; the size is.** A test that only
        // counted rewrites passed against the code that never reclaimed under
        // load, because a handful of them got through in the gaps. What the
        // installation cares about is that the file does not keep everything it
        // has ever taken.
        let appended = 6 * 200 * (128 + FRAME_HEADER_BYTES) as u64;
        let held = wal.durable_bytes();
        assert!(
            held * 4 < appended,
            "the log still holds {held} bytes of the {appended} it took, so \
             reclamation is not keeping up with the writers"
        );
    }

    #[test]
    fn appending_while_a_reclamation_runs_beside_it_makes_progress() {
        // **This is a stress test and not a regression test, and the difference
        // is worth stating.** It was written to reproduce the hang L131
        // records, and it does not: it passes against the code that had the
        // defect. The window is narrower than this can drive from the outside.
        //
        // It is kept because the interaction it exercises — appends racing a
        // reclamation that rewrites the file underneath them — is the one that
        // produced the hang, and a future change that breaks it more coarsely
        // will be caught here. What actually reproduces the hang is the
        // integration test `many_concurrent_commits_all_survive` against an
        // eager store, at roughly one run in twenty before this work.
        let wal = Wal::open(
            directory("reclaim-race").join("t.wal"),
            GroupCommit {
                linger: Duration::from_micros(200),
                ..GroupCommit::default()
            },
        )
        .expect("the log opens");

        let stop = Arc::new(AtomicU64::new(0));
        let reclaimer = {
            let wal = Arc::clone(&wal);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while stop.load(Ordering::Relaxed) == 0 {
                    let upto = wal.next_position().saturating_sub(1);
                    let _ = wal.reclaim_through(upto);
                    std::thread::yield_now();
                }
            })
        };

        let mut writers = Vec::new();
        for _ in 0..6 {
            let wal = Arc::clone(&wal);
            writers.push(std::thread::spawn(move || {
                for _ in 0..150 {
                    wal.append(&[3; 128]).expect("every append returns");
                }
            }));
        }
        for writer in writers {
            writer.join().expect("no writer wedged");
        }
        stop.store(1, Ordering::Relaxed);
        reclaimer.join().expect("the reclaimer stops");
    }

    #[test]
    fn a_bounded_group_gives_back_the_frames_it_took() {
        // `pending_frames` used to be left alone on the bounded path, so the
        // first group that crossed the frame bound made the condition
        // permanently true and every later group was byte-bounded however small
        // it was. The counter has to come back down.
        let wal = Wal::open(
            directory("bounded-frames").join("t.wal"),
            GroupCommit {
                linger: Duration::ZERO,
                max_group_frames: 2,
                ..GroupCommit::default()
            },
        )
        .expect("the log opens");

        for _ in 0..12 {
            wal.append(&[4; 32]).expect("appends");
        }
        let shared = wal.shared.lock().expect("append log lock");
        assert!(shared.pending.is_empty(), "everything was flushed");
        assert_eq!(
            shared.pending_frames, 0,
            "the frame count did not come back down, so every later group is bounded for ever"
        );
        assert!(
            shared.statistics.largest_group > 0,
            "the largest group was recorded as what was left behind rather than what went out"
        );
    }
}
