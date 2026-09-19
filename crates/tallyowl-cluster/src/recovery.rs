//! Snapshot, bootstrap, restore, and the two answers to a permanent quorum loss.
//!
//! `docs/FAILURE_MODES.md` section 6.2 states the problem: two of three voters
//! destroyed with no recovery leaves one replica holding a log that may be
//! behind the last committed entry, and there is no safe automatic answer. So
//! TallyOwl gives two deliberate ones.
//!
//! # Restore is the default
//!
//! It never loses an acknowledged write that the snapshot covers, and it loses
//! everything written after the snapshot. It is the documented path, it is what
//! [`Recovery::recommended`] returns, and D58 records the decision.
//!
//! # Unsafe recovery cannot hide its cost
//!
//! It forces a single-voter membership from the survivor's log and it can lose
//! an acknowledged write that no component can identify. Section 6.2 gives four
//! requirements and this module holds all four:
//!
//! 1. **an explicit confirmation that names the tablet** — [`UnsafeRequest`]
//!    carries the name twice and [`unsafe_recover`] refuses when they differ;
//! 2. **an audit record in the `audit` retention class** — the returned
//!    [`crate::topology::ControllerCommand::MarkDegraded`] writes one, and the
//!    topology keeps it;
//! 3. **a degraded mark on the affected time range that reaches query and
//!    explain output** — the mark is on the tablet, [`crate::query`] carries it
//!    into every merged result, and a warning names it;
//! 4. **a mark that never expires on its own** — only
//!    [`clear_degraded`] removes it, and it records who accepted the loss.
//!
//! > A fast path that hides its cost becomes the habitual path. This one cannot
//! > hide its cost.

use tallyowl_obs::error::{ErrorCode, TallyOwlError};

use crate::topology::{ControllerCommand, DegradedMark, NodeName, TabletName, Topology};

/// Which answer an operator is being offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Restore from a snapshot. The default and the documented path.
    Restore,
    /// Force a single voter from a survivor. Behind an explicit flag.
    Unsafe,
}

impl Recovery {
    /// What TallyOwl recommends when a tablet has lost its quorum for good.
    pub fn recommended() -> Recovery {
        Recovery::Restore
    }
}

/// A cluster snapshot, for backup and for restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterSnapshot {
    pub snapshot_id: String,
    pub tablets: Vec<TabletName>,
    pub taken_at: i64,
    pub total_bytes: u64,
    /// BLAKE3 over every part, in tablet order. A restore that cannot reproduce
    /// it refuses rather than restoring part of a cluster.
    pub whole_digest: [u8; 32],
}

/// Build the digest of a cluster snapshot from its parts.
///
/// The order is the order the tablets are listed in, and the tablet name goes
/// into the digest beside its bytes. Two snapshots that hold the same bytes
/// under different names are not the same snapshot.
pub fn snapshot_digest(parts: &[(TabletName, Vec<u8>)]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for (tablet, bytes) in parts {
        hasher.update(tablet.as_bytes());
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    *hasher.finalize().as_bytes()
}

/// Take a cluster snapshot.
pub fn take_snapshot(
    snapshot_id: impl Into<String>,
    at: i64,
    parts: Vec<(TabletName, Vec<u8>)>,
) -> ClusterSnapshot {
    ClusterSnapshot {
        snapshot_id: snapshot_id.into(),
        tablets: parts.iter().map(|(name, _)| name.clone()).collect(),
        taken_at: at,
        total_bytes: parts.iter().map(|(_, bytes)| bytes.len() as u64).sum(),
        whole_digest: snapshot_digest(&parts),
    }
}

/// Check a snapshot before it is restored.
///
/// A restore is the answer to a quorum loss, so it happens at the worst moment
/// somebody could discover that the backup was damaged. It is checked first.
pub fn verify_snapshot(
    snapshot: &ClusterSnapshot,
    parts: &[(TabletName, Vec<u8>)],
) -> Result<(), TallyOwlError> {
    let actual = snapshot_digest(parts);
    if actual != snapshot.whole_digest {
        return Err(TallyOwlError::new(
            ErrorCode::Internal,
            format!(
                "Snapshot `{}` does not match its own checksum, so it is not restored. Restoring it would put data of unknown correctness back into the cluster.",
                snapshot.snapshot_id
            ),
        ));
    }
    Ok(())
}

/// What an operator sends to force a single voter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsafeRequest {
    pub tablet: TabletName,
    /// The tablet name again. See requirement 1 in the module note.
    pub confirm_tablet: TabletName,
    pub survivor: NodeName,
    pub reason: String,
}

/// What unsafe recovery did, and what it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsafeOutcome {
    pub tablet: TabletName,
    pub audit_id: String,
    pub mark: DegradedMark,
    pub commands: Vec<ControllerCommand>,
}

/// Force a single-voter membership from a survivor.
///
/// `survivor_watermark` is what the survivor's log actually reached, and
/// `range` is the time range the tablet holds. Everything the lost voters
/// committed past the watermark is gone, and the mark says so in words an
/// operator reads rather than in a status code.
pub fn unsafe_recover(
    topology: &Topology,
    request: &UnsafeRequest,
    survivor_watermark: u64,
    range: (i64, i64),
    at: i64,
) -> Result<UnsafeOutcome, TallyOwlError> {
    if request.tablet != request.confirm_tablet {
        return Err(TallyOwlError::invalid_argument(format!(
            "The confirmation names `{}` and the tablet is `{}`. Unsafe recovery can lose an acknowledged write, so it runs only when both names are the same.",
            request.confirm_tablet, request.tablet
        )));
    }
    if request.reason.trim().is_empty() {
        return Err(TallyOwlError::invalid_argument(
            "Unsafe recovery needs a reason. It is written to the audit record and it is what somebody reads a year from now."
                .to_string(),
        ));
    }
    let tablet = topology.tablet(&request.tablet).ok_or_else(|| {
        TallyOwlError::new(
            ErrorCode::NotFound,
            format!("No tablet is named `{}`.", request.tablet),
        )
    })?;
    if tablet.member(&request.survivor).is_none() {
        return Err(TallyOwlError::new(
            ErrorCode::NotFound,
            format!(
                "`{}` does not hold `{}`, so it cannot be the survivor.",
                request.survivor, request.tablet
            ),
        ));
    }

    let audit_id = format!("unsafe-{}-{at}", request.tablet);
    let mark = DegradedMark {
        since: at,
        range_start: range.0,
        range_end: range.1,
        survivor_watermark,
        reason: request.reason.clone(),
        audit_id: audit_id.clone(),
    };

    // The membership change and the mark are one intention, so they are one
    // list. A caller that applied the first and not the second would have a
    // working tablet with no record that anything was lost.
    let survivor = tablet
        .member(&request.survivor)
        .expect("checked above")
        .clone();
    let mut commands = Vec::new();
    for member in &tablet.members {
        if member.node != request.survivor {
            commands.push(ControllerCommand::RemoveReplica {
                tablet: request.tablet.clone(),
                node: member.node.clone(),
            });
        }
    }
    commands.push(ControllerCommand::AddReplica {
        tablet: request.tablet.clone(),
        member: survivor,
    });
    commands.push(ControllerCommand::MarkDegraded {
        tablet: request.tablet.clone(),
        mark: mark.clone(),
    });

    Ok(UnsafeOutcome {
        tablet: request.tablet.clone(),
        audit_id,
        mark,
        commands,
    })
}

/// Clear a degraded mark, recording who accepted the loss.
pub fn clear_degraded(
    topology: &Topology,
    tablet: &str,
    accepted_by: &str,
    reason: &str,
    at: i64,
) -> Result<ControllerCommand, TallyOwlError> {
    let found = topology.tablet(tablet).ok_or_else(|| {
        TallyOwlError::new(
            ErrorCode::NotFound,
            format!("No tablet is named `{tablet}`."),
        )
    })?;
    if found.degraded.is_none() {
        return Err(TallyOwlError::new(
            ErrorCode::FailedPrecondition,
            format!("`{tablet}` is not marked degraded."),
        ));
    }
    if accepted_by.trim().is_empty() {
        return Err(TallyOwlError::invalid_argument(
            "Clearing a degraded mark records who accepted the loss, so it needs a name."
                .to_string(),
        ));
    }
    Ok(ControllerCommand::ClearDegraded {
        tablet: tablet.to_string(),
        accepted_by: accepted_by.to_string(),
        reason: reason.to_string(),
        at,
    })
}

/// Whether a query over this range touches a degraded mark.
///
/// Half-open on both sides, the way every other range in TallyOwl is. A query
/// that ends exactly where a degraded range starts does not touch it.
pub fn overlaps_degraded(mark: &DegradedMark, range_start: i64, range_end: i64) -> bool {
    range_start < mark.range_end && mark.range_start < range_end
}
