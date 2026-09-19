//! The store contract, and the directory-backed implementation Phase 1 uses.
//!
//! The contract is the part that lasts. `DirectoryStore` is deliberately the
//! simplest physical format that keeps every promise the contract makes, so
//! Phase 3 replaces it with the segment format in `docs/SEGMENT_FORMAT.md`
//! without moving the seam.
//!
//! # Durability
//!
//! `commit` appends every row, then the receipt, then calls `fsync` before it
//! returns. A caller that received a commit therefore keeps its data across an
//! abrupt process kill, which is the claim `docs/DELIVERY.md` section 5 makes
//! and the one the failure tests exercise. The receipt is written last: a crash
//! between the rows and the receipt replays the batch, and deduplication makes
//! the replay one logical commit. A crash in the other order would lose data
//! while reporting success.
//!
//! # Deduplication
//!
//! A lost acknowledgement can cause a duplicate delivery, and TallyOwl never
//! claims exactly-once transport. `commit` therefore deduplicates on
//! `(source_id, batch_id)` and returns the prior receipt, so a retry gives one
//! logical commit. See DELIVERY.md sections 1 and 6.
//!
//! # The implementation
//!
//! `crates/tallyowl-store/src/segmented.rs`. Phase 1 had a directory of JSON
//! lines behind this trait; Phase 3 replaced it with the append log, the
//! catalog, and immutable segments, and the trait did not move. That was the
//! point of putting a real contract here on the first day rather than a stub.

use crate::row::EventRow;

/// Why a store operation failed. The store speaks its own error type; the head
/// translates it into the taxonomy in CONVENTIONS.md section 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The store could not reach or write its data directory.
    Unavailable(String),
    /// The stored bytes did not read back as what they claimed to be. A query
    /// over damaged data returns `incomplete-result` and names what it could
    /// not read. It never silently returns a smaller answer. See D57.
    Damaged(String),
    /// The caller asked for something the contract does not allow.
    InvalidArgument(String),
    /// The device has no room for this write.
    ///
    /// This is separate from [`StoreError::Unavailable`] because the two need
    /// different answers from an operator. Unavailable is usually a moment;
    /// this one does not clear until somebody frees space or the retention
    /// policy does. `docs/FAILURE_MODES.md` section 10 gives the behaviour at
    /// each point, and the message names which point refused.
    Exhausted(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Unavailable(m)
            | StoreError::Damaged(m)
            | StoreError::InvalidArgument(m)
            | StoreError::Exhausted(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for StoreError {}

/// What one commit produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    pub accepted: u64,
    pub committed_at: i64,
    /// The commit watermark after this commit. A query states the watermark its
    /// result applies to. See D18.
    pub commit_watermark: u64,
    /// True when this batch ID had already committed and the store returned the
    /// prior receipt rather than writing the rows again.
    pub deduplicated: bool,
}

/// A durable record that one batch committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub source_id: [u8; 16],
    pub batch_id: [u8; 16],
    pub accepted: u64,
    pub committed_at: i64,
    pub commit_watermark: u64,
}

/// Which time fact a query counts by. TallyOwl keeps three and never collapses
/// them, so a query says which one it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeBasis {
    OccurredAt,
    ReceivedAt,
    CommittedAt,
}

/// What a scan read, and whether it read all of it.
///
/// The flag travels with the rows rather than beside them. A caller that had to
/// ask for it separately would forget, and the failure that produces is the one
/// `docs/FAILURE_MODES.md` section 2 ranks worst: a smaller answer presented as
/// a complete one.
#[derive(Debug, Clone, PartialEq)]
pub struct Scanned {
    pub rows: Vec<EventRow>,
    /// True when the store could not read part of the range.
    pub incomplete: bool,
}

/// A count for each time bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trend {
    pub basis: TimeBasis,
    pub bucket_ms: i64,
    /// Bucket start to count, in time order.
    pub buckets: Vec<(i64, u64)>,
    pub total: u64,
    /// The watermark this result applies to.
    pub commit_watermark: u64,
    /// True when the store could not read part of the range. A caller must not
    /// present an incomplete answer as a complete one.
    pub incomplete: bool,
}

/// The store contract. Phase 3 replaced the implementation and not this trait,
/// which is what `docs/PLAN.md` Phase 1 said would happen.
/// What every part of a store said about one aggregate.
///
/// The states are opaque here. See [`Store::partial_aggregates`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialAggregates {
    /// One encoded partial state for each part that answered.
    pub states: Vec<Vec<u8>>,
    /// False when a part could not answer. A merge never makes it true again,
    /// and the caller refuses rather than reporting a smaller number.
    pub complete: bool,
    /// What could not be read, in the words an operator can act on.
    pub unreadable: Vec<String>,
}

pub trait Store: Send + Sync {
    /// Commit one batch. Repeating a batch ID gives one logical commit.
    fn commit(
        &self,
        source_id: [u8; 16],
        batch_id: [u8; 16],
        rows: Vec<EventRow>,
    ) -> Result<CommitOutcome, StoreError>;

    /// The receipt for a batch that already committed, when one exists.
    fn receipt(&self, source_id: [u8; 16], batch_id: [u8; 16]) -> Option<Receipt>;

    /// Every row for one project in a time range, by one time basis.
    ///
    /// The result says whether it is complete. A query over damaged data must
    /// return `incomplete-result` rather than a smaller number, and it can only
    /// do that if the scan tells it.
    fn scan(
        &self,
        project_id: [u8; 16],
        range_start: i64,
        range_end: i64,
        basis: TimeBasis,
    ) -> Result<Scanned, StoreError>;

    /// One event by its ID. This is the exact lookup that must stay exact at
    /// high cardinality.
    fn lookup_event(&self, event_id: [u8; 16]) -> Result<Option<EventRow>, StoreError>;

    /// Every row that carries one exact correlation value.
    ///
    /// This is the lookup a trace assembly and a request timeline need, and it
    /// is the one `AGENTS.md` says must stay exact: "Do not silently drop,
    /// coalesce, or reject a value because it has high cardinality."
    ///
    /// `column` is one of the correlation names in `segment::schema`, or a
    /// property name with the property prefix. The locator prunes to candidate
    /// segments; the rows themselves decide, because a fingerprint prunes and
    /// never answers.
    fn lookup_correlated(&self, column: &str, value: &[u8]) -> Result<Scanned, StoreError>;

    /// A count for each time bucket, pushed down to the store.
    ///
    /// The executor can produce the same answer from a scan. This exists
    /// because a distributed query needs a partial state it can merge rather
    /// than a row set it has to move, and the shape of that push-down should
    /// not appear for the first time in Phase 7.
    fn trend(
        &self,
        project_id: [u8; 16],
        range_start: i64,
        range_end: i64,
        basis: TimeBasis,
        bucket_ms: i64,
        name: Option<&str>,
    ) -> Result<Trend, StoreError>;

    /// Ask every part of this store for the partial state of one aggregate.
    ///
    /// **The plan and the states are bytes this contract never reads.** D25
    /// keeps the store contract small, and an aggregate is query algebra rather
    /// than storage: a store that understood measures would be a second place
    /// the algebra lived. The caller encodes its own plan, every part runs the
    /// caller's own aggregation over its own rows, and the caller merges what
    /// comes back. There is therefore one implementation of a sum.
    ///
    /// `None` means this store has no parts to ask — a single-node installation
    /// answers from its own rows, which is what it did before this existed. The
    /// default is `None`, so an implementation that does not fan out needs no
    /// code at all.
    fn partial_aggregates(
        &self,
        _plan: &[u8],
        _project_id: [u8; 16],
        _range_start: i64,
        _range_end: i64,
        _basis: TimeBasis,
    ) -> Result<Option<PartialAggregates>, StoreError> {
        Ok(None)
    }

    /// The current commit watermark.
    fn commit_watermark(&self) -> u64;

    /// How many rows this store holds. An operational report reads it; a query
    /// does not, because a query states its own time range.
    fn row_count(&self) -> usize;

    /// Why this store cannot read part of what it holds.
    ///
    /// A query over a damaged range answers `incomplete-result` rather than a
    /// smaller number, and FAILURE_MODES.md procedure 6 requires that the
    /// damaged part be **named**. Without this the refusal is true and
    /// unactionable, which cost a day of guessing. A store with nothing wrong
    /// returns nothing.
    fn unreadable(&self) -> Vec<String> {
        Vec::new()
    }

    /// Whether the store can accept a commit right now. A head that cannot
    /// write must fail readiness rather than accept data it would discard.
    fn is_writable(&self) -> bool;

    /// Put everything committed so far into a sealed segment.
    ///
    /// **A replica that catches up by segment copy receives only what is in a
    /// segment.** A consensus snapshot that permits the log behind it to be
    /// purged therefore has to seal first, or the rows between the newest
    /// segment and the snapshot would be on no replica that catches up that
    /// way, and the log that held them would be gone. `docs/STORAGE.md`
    /// section 6 puts segments on the transfer path, so this is what makes the
    /// two halves meet.
    ///
    /// Answers whether anything sealed. A store with an empty open buffer
    /// seals nothing and says so.
    ///
    /// There is no default. Both implementations answer it, because a default
    /// that quietly did nothing would let a snapshot claim coverage it did not
    /// have, which is the same class of failure as a receipt that acknowledges
    /// an uncommitted write.
    fn seal_now(&self) -> Result<bool, StoreError>;

    /// Hide everything one standing predicate names, and keep hiding it.
    ///
    /// **A tombstone is a standing predicate, not a one-time action.**
    /// `AGENTS.md`: "Deletion becomes visible through tombstones immediately"
    /// and "it also hides matching data that arrives after the erasure
    /// request." A row for an erased end user can still be in a collector queue
    /// when the erasure lands, so the predicate stays active.
    ///
    /// It is on the contract rather than on the implementation because a
    /// replicated tablet has to **propose** an erasure rather than apply it
    /// locally: a replica that missed a tombstone would answer a query with
    /// data an erasure removed, which is worse than any wrong number.
    ///
    /// Answers the visible tombstone generation after this one applied.
    fn erase(&self, tombstone: &crate::catalog::Tombstone) -> Result<u64, StoreError>;

    /// The tombstone generation a read applies.
    fn tombstone_generation(&self) -> Result<u64, StoreError>;
}
