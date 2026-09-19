//! A store that commits through a tablet group.
//!
//! `docs/STORAGE.md` section 5 gives the multi-voter write sequence, and this
//! is it:
//!
//! 1. the tablet leader validates and assigns the next log position;
//! 2. voting replicas append the frame;
//! 3. consensus commits after the configured quorum persists it;
//! 4. the leader gets an additional remote copy if the policy requires it;
//! 5. the leader returns the durable receipt;
//! 6. segment construction proceeds asynchronously from the committed log.
//!
//! Steps 1 to 3 are [`crate::groups::GroupRegistry::propose`]. Step 4 is
//! `await_remote_copy`. Steps 5 and 6 are the local store, which every replica
//! runs behind its own state machine.
//!
//! # Why this is a `Store`
//!
//! The head, the query executor, and the reference application all speak to
//! [`tallyowl_store::Store`], and Phase 1 put a real contract there on the
//! first day so that Phase 3 could replace the implementation without moving
//! the seam. Phase 7 does the same thing again: a replicated tablet is a store
//! whose commit goes through consensus, and every caller above it is unchanged.
//! That is what makes "the reference application runs unchanged against the
//! replicated installation" an achievable exit criterion rather than a rewrite.
//!
//! # Reads
//!
//! A read is answered from this node's own store, because a replica that
//! applied the entry holds the rows. What a read must not do is claim to be
//! more current than it is: `applied_index` is the watermark this replica has
//! reached, and [`crate::query`] refuses a `committed` read against a replica
//! that has not reached the requested watermark.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_store::row::EventRow;
use tallyowl_store::{CommitOutcome, Receipt, Scanned, Store, StoreError, TimeBasis, Trend};

use crate::groups::{GroupKey, GroupRegistry};
use crate::raft::machine::{Outcome, TabletCommand};
use crate::topology::{Member, MemberRole, ReceiptPolicy};

/// How long a `remote-one` write waits for its remote copy before it refuses.
///
/// It refuses rather than acknowledging. A policy that quietly degraded to
/// `local-quorum` under a slow link would make the receipt a lie, and a receipt
/// is the only thing a caller has.
pub const REMOTE_COPY_TIMEOUT: Duration = Duration::from_secs(10);

/// One tablet, as a store.
pub struct ReplicatedStore {
    registry: Arc<GroupRegistry>,
    group: GroupKey,
    local: Arc<dyn Store>,
    policy: ReceiptPolicy,
    /// The region this tablet writes in. A `remote-one` receipt needs a durable
    /// copy outside it.
    write_region: String,
    remote_copy_timeout: Duration,
    /// Where a read goes when the cell has more than one tablet.
    ///
    /// **Absent means "this node's own replica holds all of it"**, which is
    /// true of every installation with one tablet and of no installation with
    /// two. A head that read locally in a two-tablet cell would answer from one
    /// tablet and would not know it was short, which is the failure
    /// `docs/FAILURE_MODES.md` section 2 ranks worst.
    fan_out: Option<Arc<dyn crate::fanout::TabletReads>>,
}

impl ReplicatedStore {
    pub fn new(
        registry: Arc<GroupRegistry>,
        tablet: impl Into<String>,
        local: Arc<dyn Store>,
        policy: ReceiptPolicy,
        write_region: impl Into<String>,
    ) -> ReplicatedStore {
        ReplicatedStore {
            registry,
            group: GroupKey::Tablet(tablet.into()),
            local,
            policy,
            write_region: write_region.into(),
            remote_copy_timeout: REMOTE_COPY_TIMEOUT,
            fan_out: None,
        }
    }

    pub fn with_remote_copy_timeout(mut self, timeout: Duration) -> ReplicatedStore {
        self.remote_copy_timeout = timeout;
        self
    }

    /// Read across every readable tablet rather than only this node's.
    pub fn reading_across(
        mut self,
        fan_out: Arc<dyn crate::fanout::TabletReads>,
    ) -> ReplicatedStore {
        self.fan_out = Some(fan_out);
        self
    }

    pub fn group(&self) -> &GroupKey {
        &self.group
    }

    pub fn policy(&self) -> ReceiptPolicy {
        self.policy
    }

    /// The local store this replica reads from.
    pub fn local(&self) -> Arc<dyn Store> {
        Arc::clone(&self.local)
    }

    /// The log index this replica has applied.
    pub fn applied_index(&self) -> u64 {
        self.registry.applied_index(&self.group)
    }

    pub fn is_leader(&self) -> bool {
        self.registry.is_leader(&self.group)
    }

    pub fn leader(&self) -> Option<String> {
        self.registry.leader(&self.group)
    }

    /// What this write satisfied, for the receipt.
    pub fn satisfied(&self, commit_watermark: u64) -> SatisfiedReceipt {
        let members = self.registry.members(&self.group);
        SatisfiedReceipt {
            policy: self.policy,
            commit_watermark,
            voters: members
                .iter()
                .filter(|m| m.role == MemberRole::Voter)
                .count(),
            durable_copies: members.len(),
        }
    }

    /// Wait for at least one durable copy outside the write region.
    ///
    /// This is step 4 of the sequence. It reads what consensus already knows
    /// about each follower's progress rather than asking the follower, because
    /// the leader's replication state is the only place that is authoritative
    /// about what a follower persisted.
    fn await_remote_copy(&self, index: u64) -> Result<(), TallyOwlError> {
        let remote: Vec<String> = self
            .registry
            .members(&self.group)
            .into_iter()
            .filter(|m| m.region != self.write_region)
            .map(|m| m.node)
            .collect();
        if remote.is_empty() {
            return Err(TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                format!(
                    "This tablet's policy is `remote-one` and it has no replica outside `{}`, so no write can satisfy it. Add a replica in another region or change the policy.",
                    self.write_region
                ),
            ));
        }
        let deadline = Instant::now() + self.remote_copy_timeout;
        loop {
            if self.registry.replicated_to(&self.group, &remote, index) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(TallyOwlError::unavailable(format!(
                    "The write committed locally and no replica outside `{}` had it after {} seconds, so it is not acknowledged. Send it again; the batch ID makes a retry one logical commit.",
                    self.write_region,
                    self.remote_copy_timeout.as_secs()
                )));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn store_error(error: TallyOwlError) -> StoreError {
    match error.code {
        ErrorCode::ResourceExhausted => StoreError::Exhausted(error.message),
        ErrorCode::InvalidArgument => StoreError::InvalidArgument(error.message),
        _ => StoreError::Unavailable(error.message),
    }
}

impl Store for ReplicatedStore {
    fn commit(
        &self,
        source_id: [u8; 16],
        batch_id: [u8; 16],
        rows: Vec<EventRow>,
    ) -> Result<CommitOutcome, StoreError> {
        let command = TabletCommand::Commit {
            source_id,
            batch_id,
            rows: crate::raft::machine::encode_rows(&rows),
        };
        let payload = crate::raft::encode(&command).map_err(StoreError::InvalidArgument)?;
        let outcome = self
            .registry
            .propose(&self.group, payload)
            .map_err(store_error)?;

        match outcome {
            Outcome::Committed {
                accepted,
                committed_at,
                commit_watermark,
                deduplicated,
            } => {
                if self.policy == ReceiptPolicy::RemoteOne && !deduplicated {
                    self.await_remote_copy(self.applied_index())
                        .map_err(store_error)?;
                }
                Ok(CommitOutcome {
                    accepted,
                    committed_at,
                    commit_watermark,
                    deduplicated,
                })
            }
            Outcome::Refused { reason } => Err(StoreError::Unavailable(reason)),
            other => Err(StoreError::Unavailable(format!(
                "The tablet answered a commit with something that is not a commit: {other:?}"
            ))),
        }
    }

    fn receipt(&self, source_id: [u8; 16], batch_id: [u8; 16]) -> Option<Receipt> {
        self.local.receipt(source_id, batch_id)
    }

    fn scan(
        &self,
        project_id: [u8; 16],
        range_start: i64,
        range_end: i64,
        basis: TimeBasis,
    ) -> Result<Scanned, StoreError> {
        match &self.fan_out {
            Some(across) => across.scan(project_id, range_start, range_end, basis),
            None => self.local.scan(project_id, range_start, range_end, basis),
        }
    }

    fn lookup_event(&self, event_id: [u8; 16]) -> Result<Option<EventRow>, StoreError> {
        // An event ID is a correlation value like any other, so a multi-tablet
        // installation asks every tablet for it. Looking only locally would
        // answer "no such event" for an event another tablet holds, which reads
        // as data loss.
        match &self.fan_out {
            None => self.local.lookup_event(event_id),
            Some(across) => {
                let found = across
                    .lookup_correlated(tallyowl_store::segment::schema::EVENT_ID, &event_id)?;
                if found.incomplete && found.rows.is_empty() {
                    return Err(StoreError::Damaged(
                        "This lookup could not reach every tablet, so `not found` would not be the truth.".to_string(),
                    ));
                }
                Ok(found.rows.into_iter().next())
            }
        }
    }

    fn lookup_correlated(&self, column: &str, value: &[u8]) -> Result<Scanned, StoreError> {
        match &self.fan_out {
            Some(across) => across.lookup_correlated(column, value),
            None => self.local.lookup_correlated(column, value),
        }
    }

    fn partial_aggregates(
        &self,
        plan: &[u8],
        project_id: [u8; 16],
        range_start: i64,
        range_end: i64,
        basis: TimeBasis,
    ) -> Result<Option<tallyowl_store::store::PartialAggregates>, StoreError> {
        // A one-tablet installation has nothing to fan out to, and answering
        // `None` is what tells the caller to use its own rows. That is the
        // path a home installation takes, and it is the one every test runs.
        let Some(across) = &self.fan_out else {
            return Ok(None);
        };
        across
            .partial_aggregates(plan, project_id, range_start, range_end, basis)
            .map(Some)
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
        match &self.fan_out {
            Some(across) => {
                across.trend(project_id, range_start, range_end, basis, bucket_ms, name)
            }
            None => self
                .local
                .trend(project_id, range_start, range_end, basis, bucket_ms, name),
        }
    }

    fn commit_watermark(&self) -> u64 {
        self.local.commit_watermark()
    }

    fn row_count(&self) -> usize {
        self.local.row_count()
    }

    fn unreadable(&self) -> Vec<String> {
        // Both halves. This node's own damaged segments, and the tablets the
        // last read could not reach. A refusal that named neither would be true
        // and unactionable, which FAILURE_MODES.md procedure 6 forbids.
        let mut reasons = self.local.unreadable();
        if let Some(across) = &self.fan_out {
            reasons.extend(across.unreadable());
        }
        reasons
    }

    fn is_writable(&self) -> bool {
        // Three separate things have to be true, and a caller that asked only
        // the device would accept a write this tablet cannot commit.
        self.local.is_writable()
            && self.registry.holds(&self.group)
            && self.registry.leader(&self.group).is_some()
    }

    fn erase(&self, tombstone: &tallyowl_store::catalog::Tombstone) -> Result<u64, StoreError> {
        // **An erasure is proposed, not applied here.** A replica that missed a
        // tombstone would answer a query with data an erasure removed, and
        // `AGENTS.md` makes a tombstone a standing predicate that also hides
        // what arrives later. Both of those need every replica to hold it.
        let command = TabletCommand::Tombstone {
            predicate: tallyowl_store::catalog::encode_tombstone(tombstone),
        };
        let payload = crate::raft::encode(&command).map_err(StoreError::InvalidArgument)?;
        match self
            .registry
            .propose(&self.group, payload)
            .map_err(store_error)?
        {
            Outcome::Applied { generation } | Outcome::Unchanged { generation } => Ok(generation),
            Outcome::Refused { reason } => Err(StoreError::InvalidArgument(reason)),
            other => Err(StoreError::Unavailable(format!(
                "The tablet answered an erasure with something that is not an erasure: {other:?}"
            ))),
        }
    }

    fn tombstone_generation(&self) -> Result<u64, StoreError> {
        self.local.tombstone_generation()
    }

    fn seal_now(&self) -> Result<bool, StoreError> {
        // Sealing is local. Each replica seals its own store from the entries
        // it applied, so two replicas may hold the same rows in differently
        // shaped segments and neither is wrong. `docs/STORAGE.md` section 5
        // puts segment construction after the committed log for exactly this
        // reason, which is why a seal is not a replicated command.
        self.local.seal_now()
    }
}

/// What a receipt reports about the policy it satisfied.
///
/// `docs/CELLS.md` section 8: "Each receipt gives its satisfied policy and
/// commit watermark." A receipt that named the configured policy rather than
/// the satisfied one would be worthless during a degradation, which is exactly
/// when somebody reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SatisfiedReceipt {
    pub policy: ReceiptPolicy,
    pub commit_watermark: u64,
    pub voters: usize,
    pub durable_copies: usize,
}

/// Decide whether a policy is legal for a replica set, and say why when it is
/// not.
///
/// This is the same rule [`crate::topology`] enforces on a change. It is here
/// as well because a tablet whose voter set changed underneath a configured
/// policy must refuse the write rather than acknowledge it under a policy that
/// no longer applies.
pub fn policy_is_legal(policy: ReceiptPolicy, members: &[Member]) -> Result<(), TallyOwlError> {
    let voters = members
        .iter()
        .filter(|m| m.role == MemberRole::Voter)
        .count();
    match policy {
        ReceiptPolicy::LocalOne if voters > 1 => Err(TallyOwlError::new(
            ErrorCode::FailedPrecondition,
            format!(
                "This tablet has {voters} voters and `local-one` would acknowledge a write before the group commits it. Use `local-quorum` or `remote-one`."
            ),
        )),
        ReceiptPolicy::RemoteOne => {
            let regions: std::collections::BTreeSet<&str> =
                members.iter().map(|m| m.region.as_str()).collect();
            if regions.len() < 2 {
                Err(TallyOwlError::new(
                    ErrorCode::FailedPrecondition,
                    "`remote-one` needs a replica outside the write region and this tablet has replicas in one region only."
                        .to_string(),
                ))
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}
