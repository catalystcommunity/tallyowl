//! Copying sealed segments between nodes, and proving each one on arrival.
//!
//! `docs/STORAGE.md` section 6 puts segments on the transfer path: a move
//! "copies sealed segments, catches up committed WAL positions, verifies
//! checksums, then changes placement generation". L087 built the stage machine
//! in [`crate::movement`] and the parity check, and left the copy itself to be
//! driven by an operator. This module is the driver.
//!
//! # Why this is what unblocked the log
//!
//! A raft log is purged behind a snapshot. TallyOwl's tablet snapshot carries
//! the marks and **not** the rows (L087), because a snapshot holding a whole
//! tablet's rows would make one message as large as the tablet. So until a
//! replica could catch up some other way, purging the log would have taken the
//! only copy of entries a lagging replica still needed, and the log grew without
//! bound: `docs/BENCHMARKS.md` section 19 measured a replicated tablet at about
//! five times the disk of an unreplicated one, nearly all of it log.
//!
//! The other way is this. A snapshot now seals first, so everything it covers
//! is in a segment, and a replica behind the purge point copies segments and
//! then replays the log from the snapshot. L095 is what that buys.
//!
//! # Three rules
//!
//! **Nothing the sender says is trusted.** The digest travels, and the receiver
//! recomputes it from the bytes that arrived. The manifest does not travel at
//! all: [`tallyowl_store::SegmentedStore::install_segment`] derives it from the
//! segment, which is self-describing.
//!
//! **A copy is safe to repeat.** A segment the target already holds under the
//! same content address is not published twice, so a transfer that stopped
//! halfway is finished by running it again.
//!
//! **A copy onto a target that already holds data for the tablet reconciles.**
//! Two replicas that applied the same entries build differently shaped
//! segments, so their content addresses do not match and there is no honest way
//! to tell a copied **segment** from one the target built itself. L097 read
//! that as needing a marker in the segment format; L147 says why it does not.
//! A segment was the wrong unit. A row carries a producer-assigned `event_id`,
//! and the target keeps the rows it does not already hold, so the overlap is
//! counted once.
//!
//! The two cases this exists for — a new replica and a tablet movement — still
//! start from nothing and still take the files whole, which is the cheap path.
//! The reconcile is for the third case, which used to be refused: a replica
//! rebuilt onto a node that already holds part of the tablet.

use std::sync::Arc;

use tallyowl_cluster_api::types::{
    SegmentList, SegmentListRequest, SegmentSummary, SegmentTransfer, SegmentTransferRequest,
};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_rpc::Client;
use tallyowl_store::SegmentedStore;

use crate::movement::{Movement, SegmentCopy, Stage};

/// How much of a segment travels in one message.
///
/// Well under the 64 MiB frame bound in [`crate::raft::network`], so a segment
/// at the home profile's 32 to 64 MiB target crosses in a handful of messages
/// and no single message is near the limit.
pub const DEFAULT_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// The segments one node holds for one tablet, in both directions.
///
/// A node is a source for a tablet it holds and a target for one it is taking
/// on, and it is usually both for different tablets, so this is one trait
/// rather than two.
pub trait TabletSegments: Send + Sync + 'static {
    /// Every sealed segment for one tablet, and how far this node has applied.
    fn list(&self, tablet: &str) -> Result<SegmentList, TallyOwlError>;

    /// Part of one sealed segment.
    fn read(
        &self,
        tablet: &str,
        segment_id: &str,
        offset: u64,
        max_bytes: u64,
    ) -> Result<SegmentTransfer, TallyOwlError>;

    /// Adopt one whole segment that arrived from another node.
    fn install(&self, tablet: &str, bytes: Vec<u8>) -> Result<Adopted, TallyOwlError>;

    /// Adopt a segment onto a tablet this node already holds part of.
    ///
    /// It keeps the rows this node does not already have and drops the rest, so
    /// the overlap is counted once. See L147, and
    /// `tallyowl_store::SegmentedStore::reconcile_segment`.
    fn reconcile(&self, tablet: &str, bytes: Vec<u8>) -> Result<Adopted, TallyOwlError>;

    /// Copy everything this node holds for one tablet into a directory.
    ///
    /// This is what makes a cluster snapshot a **backup** rather than a
    /// description of one. Until L090 the snapshot carried the state machine's
    /// marks and none of the rows, so a restore from it could only ever have
    /// put the marks back.
    fn snapshot_into(
        &self,
        tablet: &str,
        into: &std::path::Path,
        at: i64,
    ) -> Result<TakenSnapshot, TallyOwlError>;

    /// Check a snapshot directory without changing anything.
    fn verify_snapshot(
        &self,
        from: &std::path::Path,
    ) -> Result<tallyowl_store::snapshot::RestoreReport, TallyOwlError>;

    /// Adopt every segment a snapshot directory names.
    ///
    /// It refuses onto a tablet this node already holds data for, and it stays
    /// a refusal where a copy became a reconcile. A restore puts a whole
    /// installation back; mixing one into a node that already holds another is
    /// not an overlap to reconcile, it is two installations in one directory.
    /// FAILURE_MODES.md section 11 procedure 3.
    fn restore_from(
        &self,
        tablet: &str,
        from: &std::path::Path,
    ) -> Result<CopyReport, TallyOwlError>;
}

/// What a snapshot of one tablet pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakenSnapshot {
    pub segments: usize,
    pub erasures: usize,
    pub commit_watermark: u64,
    pub total_bytes: u64,
}

/// What adopting one segment did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adopted {
    pub segment_id: String,
    pub row_count: u64,
    pub byte_count: u64,
    /// True when this node already held the same content and published nothing.
    pub already_held: bool,
}

// ---------------------------------------------------------------------------
// A node that has a store
// ---------------------------------------------------------------------------

/// [`TabletSegments`] over a local store.
///
/// One head holds one tablet in this build, so the tablet name is checked
/// rather than used to select a store. A node that held several would hold
/// several of these.
pub struct StoreSegments {
    tablet: String,
    store: Arc<SegmentedStore>,
    /// What this node has applied, as the caller can see it. A store does not
    /// know its own consensus position, so the head supplies it.
    applied: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl StoreSegments {
    pub fn new(tablet: impl Into<String>, store: Arc<SegmentedStore>) -> StoreSegments {
        StoreSegments {
            tablet: tablet.into(),
            store,
            applied: Arc::new(|| 0),
        }
    }

    /// Say how this node reports its applied position.
    pub fn reporting_applied(
        mut self,
        applied: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> StoreSegments {
        self.applied = applied;
        self
    }

    pub fn store(&self) -> Arc<SegmentedStore> {
        Arc::clone(&self.store)
    }

    fn check_tablet(&self, tablet: &str) -> Result<(), TallyOwlError> {
        if tablet == self.tablet {
            return Ok(());
        }
        Err(TallyOwlError::new(
            ErrorCode::NotFound,
            format!(
                "This node holds `{}` and not `{tablet}`. It moved, or it was never placed here.",
                self.tablet
            ),
        ))
    }
}

fn store_failure(e: tallyowl_store::StoreError) -> TallyOwlError {
    match e {
        tallyowl_store::StoreError::InvalidArgument(m) => TallyOwlError::invalid_argument(m),
        tallyowl_store::StoreError::Exhausted(m) => {
            TallyOwlError::new(ErrorCode::ResourceExhausted, m)
        }
        tallyowl_store::StoreError::Damaged(m) => TallyOwlError::internal(m),
        tallyowl_store::StoreError::Unavailable(m) => TallyOwlError::unavailable(m),
    }
}

impl TabletSegments for StoreSegments {
    fn list(&self, tablet: &str) -> Result<SegmentList, TallyOwlError> {
        self.check_tablet(tablet)?;
        let manifests = self.store.manifests().map_err(store_failure)?;
        let mut segments = Vec::with_capacity(manifests.len());
        for manifest in &manifests {
            // The digest is over the file as it sits on disk, which is what
            // travels. The content address inside the segment covers the body
            // and not the prologue, so the two are different numbers and this
            // is deliberately the one that matches the transfer.
            let bytes = self
                .store
                .read_segment_bytes(&manifest.segment_id)
                .map_err(store_failure)?;
            segments.push(SegmentSummary {
                segment_id: tallyowl_store::row::hex(&manifest.segment_id),
                total_bytes: bytes.len() as u64,
                digest: crate::movement::digest(&bytes).to_vec(),
                row_count: manifest.row_count,
                occurred_start: manifest.occurred_range.0,
                occurred_end: manifest.occurred_range.1,
            });
        }
        Ok(SegmentList {
            tablet: tablet.to_string(),
            segments,
            generation: 0,
            applied_index: (self.applied)(),
        })
    }

    fn read(
        &self,
        tablet: &str,
        segment_id: &str,
        offset: u64,
        max_bytes: u64,
    ) -> Result<SegmentTransfer, TallyOwlError> {
        self.check_tablet(tablet)?;
        let id = parse_segment_id(segment_id)?;
        let bytes = self.store.read_segment_bytes(&id).map_err(store_failure)?;
        let total = bytes.len() as u64;
        if offset > total {
            return Err(TallyOwlError::invalid_argument(format!(
                "Segment `{segment_id}` holds {total} bytes and the request starts at {offset}."
            )));
        }
        let take = max_bytes.clamp(1, DEFAULT_CHUNK_BYTES).min(total - offset);
        let from = offset as usize;
        let to = from + take as usize;
        let last = to as u64 >= total;
        Ok(SegmentTransfer {
            segment_id: segment_id.to_string(),
            offset,
            data: bytes[from..to].to_vec(),
            total_bytes: total,
            last,
            // The digest is over the whole segment and travels on the last
            // chunk, so a receiver that has all of it can prove all of it.
            whole_digest: last.then(|| crate::movement::digest(&bytes).to_vec()),
        })
    }

    fn install(&self, tablet: &str, bytes: Vec<u8>) -> Result<Adopted, TallyOwlError> {
        self.check_tablet(tablet)?;
        let installed = self.store.install_segment(bytes).map_err(store_failure)?;
        Ok(Adopted {
            segment_id: tallyowl_store::row::hex(&installed.segment_id),
            row_count: installed.row_count,
            byte_count: installed.byte_count,
            already_held: installed.already_held,
        })
    }

    fn reconcile(&self, tablet: &str, bytes: Vec<u8>) -> Result<Adopted, TallyOwlError> {
        self.check_tablet(tablet)?;
        let installed = self.store.reconcile_segment(bytes).map_err(store_failure)?;
        Ok(Adopted {
            segment_id: tallyowl_store::row::hex(&installed.segment_id),
            row_count: installed.row_count,
            byte_count: installed.byte_count,
            already_held: installed.already_held,
        })
    }

    fn snapshot_into(
        &self,
        tablet: &str,
        into: &std::path::Path,
        at: i64,
    ) -> Result<TakenSnapshot, TallyOwlError> {
        self.check_tablet(tablet)?;
        // The store's own snapshot seals first, so everything acknowledged is
        // in a segment the snapshot names rather than only in the log.
        let taken = self.store.snapshot(into, at).map_err(store_failure)?;
        Ok(TakenSnapshot {
            segments: taken.segments.len(),
            erasures: taken.erasures,
            commit_watermark: taken.commit_watermark,
            total_bytes: directory_bytes(into),
        })
    }

    fn verify_snapshot(
        &self,
        from: &std::path::Path,
    ) -> Result<tallyowl_store::snapshot::RestoreReport, TallyOwlError> {
        tallyowl_store::snapshot::verify(from).map_err(store_failure)
    }

    fn restore_from(
        &self,
        tablet: &str,
        from: &std::path::Path,
    ) -> Result<CopyReport, TallyOwlError> {
        self.check_tablet(tablet)?;

        // Verify the whole snapshot before anything is published. A restore
        // that skipped a damaged file would give wrong answers and say nothing.
        let report = self.verify_snapshot(from)?;
        if !report.is_complete() {
            let mut said = String::from("The snapshot was not restored and nothing changed.");
            for name in &report.segments_missing {
                said.push_str(&format!(
                    " This file is named in the snapshot and is not there: {name}."
                ));
            }
            for name in &report.segments_damaged {
                said.push_str(&format!(" This file is damaged: {name}."));
            }
            said.push_str(" Use another copy of the snapshot.");
            return Err(TallyOwlError::new(ErrorCode::FailedPrecondition, said));
        }

        let held = self.store.manifests().map_err(store_failure)?;
        if !held.is_empty() {
            return Err(TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                format!(
                    "This node already holds {} sealed segments for `{tablet}`, so restoring onto it would mix two installations together. Stop the head, move the data directory aside, and run `tallyowl-head restore <directory>`, which is FAILURE_MODES.md section 11 procedure 3.",
                    held.len()
                ),
            ));
        }

        let mut copied = CopyReport::default();
        for (name, path) in tallyowl_store::snapshot::segment_files(from).map_err(store_failure)? {
            let bytes = std::fs::read(&path).map_err(|e| {
                TallyOwlError::internal(format!(
                    "The snapshot file for {name} could not be read: {e}"
                ))
            })?;
            let adopted = self.install(tablet, bytes)?;
            if adopted.already_held {
                copied.segments_already_held += 1;
            } else {
                copied.segments_copied += 1;
                copied.rows += adopted.row_count;
                copied.bytes += adopted.byte_count;
            }
        }
        Ok(copied)
    }
}

/// How much one directory holds, one level deep.
fn directory_bytes(path: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.metadata() {
                Ok(meta) if meta.is_dir() => stack.push(entry.path()),
                Ok(meta) => total += meta.len(),
                Err(_) => {}
            }
        }
    }
    total
}

fn parse_segment_id(text: &str) -> Result<[u8; 16], TallyOwlError> {
    let bytes = tallyowl_store::row::from_hex(text).ok_or_else(|| {
        TallyOwlError::invalid_argument(format!(
            "`{text}` is not a segment identifier. One is 32 hexadecimal characters."
        ))
    })?;
    bytes.try_into().map_err(|_| {
        TallyOwlError::invalid_argument(format!(
            "`{text}` is not a segment identifier. One is 32 hexadecimal characters."
        ))
    })
}

// ---------------------------------------------------------------------------
// Pulling from a peer
// ---------------------------------------------------------------------------

/// Where a copy pulls from.
pub trait SegmentReader: Send + Sync {
    fn list(&self, tablet: &str) -> Result<SegmentList, TallyOwlError>;
    fn fetch(
        &self,
        tablet: &str,
        segment_id: &str,
        offset: u64,
        max_bytes: u64,
    ) -> Result<SegmentTransfer, TallyOwlError>;
}

/// A peer, over the node-to-node service.
pub struct PeerSegments {
    client: Arc<Client>,
}

impl PeerSegments {
    pub fn new(client: Arc<Client>) -> PeerSegments {
        PeerSegments { client }
    }

    /// Open a connection to one address.
    pub fn at(address: impl Into<String>) -> PeerSegments {
        PeerSegments {
            client: Arc::new(Client::new(
                address.into(),
                crate::raft::network::MAX_FRAME_BYTES,
            )),
        }
    }

    fn call(&self, op: &str, payload: Vec<u8>) -> Result<Vec<u8>, TallyOwlError> {
        let response = self
            .client
            .call(crate::raft::network::REPLICATION_SERVICE, op, payload)
            .map_err(|e| {
                TallyOwlError::unavailable(format!("`{op}` did not reach the peer: {e}"))
            })?;
        if response.variant.as_deref() == Some("ServiceError") {
            return Err(decode_service_error(&response.payload));
        }
        Ok(response.payload)
    }
}

/// Read a refusal back as the error it is, rather than as a decode failure.
pub fn read_refusal(payload: &[u8]) -> TallyOwlError {
    decode_service_error(payload)
}

fn decode_service_error(payload: &[u8]) -> TallyOwlError {
    use tallyowl_cluster_api::codec::decode_service_error;
    use tallyowl_cluster_api::types::ErrorCode as Wire;
    match decode_service_error(payload) {
        Err(e) => TallyOwlError::internal(format!("The peer's refusal could not be read: {e}")),
        Ok(error) => TallyOwlError::new(
            match error.code {
                Wire::InvalidArgument => ErrorCode::InvalidArgument,
                Wire::Unauthenticated => ErrorCode::Unauthenticated,
                Wire::PermissionDenied => ErrorCode::PermissionDenied,
                Wire::NotFound => ErrorCode::NotFound,
                Wire::AlreadyExists => ErrorCode::AlreadyExists,
                Wire::ResourceExhausted => ErrorCode::ResourceExhausted,
                Wire::FailedPrecondition => ErrorCode::FailedPrecondition,
                Wire::Unavailable => ErrorCode::Unavailable,
                Wire::SchemaUnsupported => ErrorCode::SchemaUnsupported,
                Wire::BudgetExceeded => ErrorCode::BudgetExceeded,
                Wire::IncompleteResult => ErrorCode::IncompleteResult,
                Wire::Internal => ErrorCode::Internal,
            },
            error.message,
        ),
    }
}

impl SegmentReader for PeerSegments {
    fn list(&self, tablet: &str) -> Result<SegmentList, TallyOwlError> {
        use tallyowl_cluster_api::codec::{decode_segment_list, encode_segment_list_request};
        let payload = encode_segment_list_request(&SegmentListRequest {
            tablet: tablet.to_string(),
        });
        let answer = self.call("list-segments", payload)?;
        decode_segment_list(&answer)
            .map_err(|e| TallyOwlError::internal(format!("A segment list could not be read: {e}")))
    }

    fn fetch(
        &self,
        tablet: &str,
        segment_id: &str,
        offset: u64,
        max_bytes: u64,
    ) -> Result<SegmentTransfer, TallyOwlError> {
        use tallyowl_cluster_api::codec::{
            decode_segment_transfer, encode_segment_transfer_request,
        };
        let payload = encode_segment_transfer_request(&SegmentTransferRequest {
            tablet: tablet.to_string(),
            segment_id: segment_id.to_string(),
            offset,
            max_bytes,
        });
        let answer = self.call("fetch-segment", payload)?;
        decode_segment_transfer(&answer).map_err(|e| {
            TallyOwlError::internal(format!("A segment transfer could not be read: {e}"))
        })
    }
}

/// A [`SegmentReader`] over another node's [`TabletSegments`], with no socket.
///
/// A movement between two tablets on one node uses it, and so does a test that
/// is exercising the copy rather than the transport.
pub struct LocalReader(pub Arc<dyn TabletSegments>);

impl SegmentReader for LocalReader {
    fn list(&self, tablet: &str) -> Result<SegmentList, TallyOwlError> {
        self.0.list(tablet)
    }

    fn fetch(
        &self,
        tablet: &str,
        segment_id: &str,
        offset: u64,
        max_bytes: u64,
    ) -> Result<SegmentTransfer, TallyOwlError> {
        self.0.read(tablet, segment_id, offset, max_bytes)
    }
}

// ---------------------------------------------------------------------------
// The copy itself
// ---------------------------------------------------------------------------

/// What one copy moved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyReport {
    pub segments_copied: usize,
    pub segments_already_held: usize,
    pub rows: u64,
    pub bytes: u64,
    /// What the source had applied when it listed. A target that reaches this
    /// position holds everything the source held at list time.
    pub source_applied_index: u64,
}

/// Fetch one whole segment, in bounded chunks, and prove it.
///
/// The digest travels on the last chunk and is recomputed here from the bytes
/// that arrived. A chunk that is short, out of order, or claims a different
/// total stops the fetch with the segment named.
pub fn fetch_whole(
    from: &dyn SegmentReader,
    tablet: &str,
    summary: &SegmentSummary,
    chunk_bytes: u64,
) -> Result<Vec<u8>, TallyOwlError> {
    let mut bytes: Vec<u8> = Vec::with_capacity(summary.total_bytes as usize);
    let mut offset = 0u64;
    loop {
        let chunk = from.fetch(tablet, &summary.segment_id, offset, chunk_bytes)?;
        if chunk.offset != offset {
            return Err(TallyOwlError::internal(format!(
                "Segment `{}` answered at byte {} and the request asked for byte {offset}, so the copy stopped.",
                summary.segment_id, chunk.offset
            )));
        }
        if chunk.total_bytes != summary.total_bytes {
            return Err(TallyOwlError::internal(format!(
                "Segment `{}` was listed at {} bytes and is now {} bytes, so the copy stopped rather than mixing two versions of it.",
                summary.segment_id, summary.total_bytes, chunk.total_bytes
            )));
        }
        if chunk.data.is_empty() && !chunk.last {
            return Err(TallyOwlError::internal(format!(
                "Segment `{}` sent nothing at byte {offset} and did not say it had finished.",
                summary.segment_id
            )));
        }
        offset += chunk.data.len() as u64;
        bytes.extend_from_slice(&chunk.data);
        if chunk.last {
            break;
        }
        if offset > summary.total_bytes {
            return Err(TallyOwlError::internal(format!(
                "Segment `{}` sent more than the {} bytes it was listed at.",
                summary.segment_id, summary.total_bytes
            )));
        }
    }
    Ok(bytes)
}

/// Copy every sealed segment of one tablet from a peer onto this node.
///
/// The target must hold no segments for the tablet. See the third rule in the
/// module note: there is no honest way to merge two replicas' segments, and a
/// merge would double the rows in the overlap.
pub fn copy_tablet(
    from: &dyn SegmentReader,
    tablet: &str,
    into: &dyn TabletSegments,
    chunk_bytes: u64,
) -> Result<CopyReport, TallyOwlError> {
    // **A target that already holds part of the tablet is reconciled rather
    // than refused.** L097 recorded this as needing a change to the segment
    // format; L147 says why it does not. The rows carry the identity, so the
    // target keeps the ones it does not already hold and the overlap is counted
    // once. A target that holds none of it takes the files whole, which is the
    // ordinary case and the cheap one.
    let held = into.list(tablet)?;
    let reconciling = !held.segments.is_empty();

    let listed = from.list(tablet)?;
    let mut report = CopyReport {
        source_applied_index: listed.applied_index,
        ..CopyReport::default()
    };

    for summary in &listed.segments {
        let bytes = fetch_whole(from, tablet, summary, chunk_bytes)?;
        // The parity check, before anything is written. A digest the sender
        // asserted proves nothing about what this node holds.
        let actual = crate::movement::digest(&bytes);
        if actual.as_slice() != summary.digest.as_slice() {
            return Err(TallyOwlError::internal(format!(
                "Segment `{}` did not arrive intact, so it was discarded rather than installed and the copy stopped.",
                summary.segment_id
            )));
        }
        if bytes.len() as u64 != summary.total_bytes {
            return Err(TallyOwlError::internal(format!(
                "Segment `{}` arrived with {} bytes and the source said {}. The copy stopped.",
                summary.segment_id,
                bytes.len(),
                summary.total_bytes
            )));
        }
        let adopted = match reconciling {
            true => into.reconcile(tablet, bytes)?,
            false => into.install(tablet, bytes)?,
        };
        if adopted.already_held {
            report.segments_already_held += 1;
        } else {
            report.segments_copied += 1;
            report.rows += adopted.row_count;
            report.bytes += adopted.byte_count;
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Driving a movement
// ---------------------------------------------------------------------------

/// Plan a movement from what the source actually holds.
pub fn plan_movement(
    from: &dyn SegmentReader,
    tablet: &str,
    away_from: impl Into<crate::topology::NodeName>,
    onto: crate::topology::Member,
    catch_up_to: u64,
) -> Result<Movement, TallyOwlError> {
    let listed = from.list(tablet)?;
    let mut segments = Vec::with_capacity(listed.segments.len());
    for summary in &listed.segments {
        segments.push(SegmentCopy {
            segment_id: summary.segment_id.clone(),
            total_bytes: summary.total_bytes,
            digest: as_digest(&summary.digest)?,
        });
    }
    Ok(Movement::plan(
        tablet,
        away_from,
        onto,
        &segments,
        catch_up_to,
    ))
}

fn as_digest(bytes: &[u8]) -> Result<[u8; 32], TallyOwlError> {
    bytes.try_into().map_err(|_| {
        TallyOwlError::internal(format!(
            "A segment digest is 32 bytes and this one is {}.",
            bytes.len()
        ))
    })
}

/// Run a planned movement's copy, and let the stage machine decide.
///
/// Every segment goes through [`Movement::segment_arrived`], which recomputes
/// the digest and stops the move with the segment named when it does not match.
/// A segment is installed only after the stage machine has accepted it, so a
/// move that stops leaves the target holding nothing it could not prove.
///
/// This does not publish. [`Movement::publish`] is a separate call and it
/// refuses before [`Stage::Verified`], which is what keeps placement changing
/// last.
pub fn drive_movement(
    movement: &mut Movement,
    from: &dyn SegmentReader,
    into: &dyn TabletSegments,
    chunk_bytes: u64,
) -> Result<CopyReport, TallyOwlError> {
    movement.start();
    let tablet = movement.tablet.clone();
    let listed = from.list(&tablet)?;
    let mut report = CopyReport {
        source_applied_index: listed.applied_index,
        ..CopyReport::default()
    };

    for summary in &listed.segments {
        let bytes = fetch_whole(from, &tablet, summary, chunk_bytes)?;
        let copy = SegmentCopy {
            segment_id: summary.segment_id.clone(),
            total_bytes: summary.total_bytes,
            digest: as_digest(&summary.digest)?,
        };
        movement.segment_arrived(&copy, &bytes)?;
        let adopted = into.install(&tablet, bytes)?;
        if adopted.already_held {
            report.segments_already_held += 1;
        } else {
            report.segments_copied += 1;
            report.rows += adopted.row_count;
            report.bytes += adopted.byte_count;
        }
    }

    if movement.stage == Stage::Stopped {
        return Err(TallyOwlError::new(
            ErrorCode::Internal,
            movement
                .stopped_because
                .clone()
                .unwrap_or_else(|| "The movement stopped.".to_string()),
        ));
    }
    Ok(report)
}
