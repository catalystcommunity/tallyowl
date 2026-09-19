//! Moving a tablet without stopping it, and proving the copy before trusting it.
//!
//! `docs/STORAGE.md` section 6 gives the sequence a move follows: "a move
//! copies sealed segments, catches up committed WAL positions, verifies
//! checksums, then changes placement generation." The order is the whole
//! design. Placement changes **last**, after the copy is proved, so a move that
//! fails halfway leaves the source owning the data and nothing points at an
//! incomplete replica.
//!
//! # Parity is checked, not assumed
//!
//! Every transferred segment carries a BLAKE3 digest and the receiver
//! reproduces it. A transfer whose digest does not match is discarded, not
//! installed, and the move stops with the segment named. `docs/DECISIONS.md`
//! D44 gives BLAKE3 as the content address for exactly this: a checksum that
//! prunes is not a checksum that proves.
//!
//! # Why the source stays readable
//!
//! Section 6 step 7: "Keep old ownership readable until active requests drain."
//! The source becomes [`TabletState::Draining`], which answers reads and
//! refuses writes, and is retired only after the safety period. A query planner
//! that dropped a draining tablet from its fan-out would return a smaller
//! answer during every movement, which is the failure `docs/FAILURE_MODES.md`
//! section 2 ranks worst.

use std::collections::BTreeMap;

use tallyowl_obs::error::{ErrorCode, TallyOwlError};

use crate::topology::{ControllerCommand, Member, NodeName, TabletName, TabletState};

/// One sealed segment, as it moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentCopy {
    pub segment_id: String,
    pub total_bytes: u64,
    /// BLAKE3 over the whole segment, as the source computed it.
    pub digest: [u8; 32],
}

/// What a move has reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Nothing has moved yet.
    Planned,
    /// Sealed segments are being copied.
    CopyingSegments,
    /// Segments are copied and verified; the log is catching up.
    CatchingUp,
    /// The copy is proved. Placement can change.
    Verified,
    /// Placement changed. The source is draining.
    Published,
    /// The source is gone.
    Finished,
    /// It stopped, and the reason says where.
    Stopped,
}

/// One tablet movement, in progress.
#[derive(Debug, Clone)]
pub struct Movement {
    pub tablet: TabletName,
    pub away_from: NodeName,
    pub onto: Member,
    pub stage: Stage,
    /// Segments the source says exist, and whether the target has proved each.
    segments: BTreeMap<String, Verification>,
    /// The committed log position the target must reach before the copy counts
    /// as caught up.
    pub catch_up_to: u64,
    pub target_applied: u64,
    pub stopped_because: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verification {
    Expected,
    Proved,
    Failed,
}

impl Movement {
    pub fn plan(
        tablet: impl Into<TabletName>,
        away_from: impl Into<NodeName>,
        onto: Member,
        segments: &[SegmentCopy],
        catch_up_to: u64,
    ) -> Movement {
        Movement {
            tablet: tablet.into(),
            away_from: away_from.into(),
            onto,
            stage: Stage::Planned,
            segments: segments
                .iter()
                .map(|s| (s.segment_id.clone(), Verification::Expected))
                .collect(),
            catch_up_to,
            target_applied: 0,
            stopped_because: None,
        }
    }

    pub fn start(&mut self) {
        if self.stage == Stage::Planned {
            self.stage = if self.segments.is_empty() {
                Stage::CatchingUp
            } else {
                Stage::CopyingSegments
            };
        }
    }

    /// Record that one segment arrived, and check its parity.
    ///
    /// The digest is recomputed here from the bytes that arrived rather than
    /// taken from the message. A digest a sender asserted proves nothing about
    /// what the receiver holds.
    pub fn segment_arrived(
        &mut self,
        segment: &SegmentCopy,
        received: &[u8],
    ) -> Result<(), TallyOwlError> {
        let actual: [u8; 32] = *blake3::hash(received).as_bytes();
        if actual != segment.digest {
            self.segments
                .insert(segment.segment_id.clone(), Verification::Failed);
            self.stop(format!(
                "Segment `{}` did not arrive intact, so the copy was discarded rather than installed. The move stopped and `{}` still owns the data.",
                segment.segment_id, self.away_from
            ));
            return Err(TallyOwlError::new(
                ErrorCode::Internal,
                self.stopped_because.clone().unwrap_or_default(),
            ));
        }
        if received.len() as u64 != segment.total_bytes {
            self.segments
                .insert(segment.segment_id.clone(), Verification::Failed);
            self.stop(format!(
                "Segment `{}` arrived with {} bytes and the source said {}. The move stopped.",
                segment.segment_id,
                received.len(),
                segment.total_bytes
            ));
            return Err(TallyOwlError::new(
                ErrorCode::Internal,
                self.stopped_because.clone().unwrap_or_default(),
            ));
        }
        self.segments
            .insert(segment.segment_id.clone(), Verification::Proved);
        if self.every_segment_proved() && self.stage == Stage::CopyingSegments {
            self.stage = Stage::CatchingUp;
        }
        Ok(())
    }

    /// Record how far the target has applied.
    pub fn caught_up_to(&mut self, applied: u64) {
        self.target_applied = applied;
        if self.stage == Stage::CatchingUp && applied >= self.catch_up_to {
            self.stage = Stage::Verified;
        }
    }

    fn every_segment_proved(&self) -> bool {
        self.segments.values().all(|v| *v == Verification::Proved)
    }

    pub fn proved_segments(&self) -> usize {
        self.segments
            .values()
            .filter(|v| **v == Verification::Proved)
            .count()
    }

    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    fn stop(&mut self, because: String) {
        self.stage = Stage::Stopped;
        self.stopped_because = Some(because);
    }

    /// The placement change, once the copy is proved.
    ///
    /// This is the only thing that publishes the move, and it refuses before
    /// [`Stage::Verified`]. A caller cannot skip the proof by calling this
    /// early, which is the point of the stage machine rather than a boolean.
    pub fn publish(&mut self) -> Result<Vec<ControllerCommand>, TallyOwlError> {
        match self.stage {
            Stage::Verified => {}
            Stage::Stopped => {
                return Err(TallyOwlError::new(
                    ErrorCode::FailedPrecondition,
                    self.stopped_because.clone().unwrap_or_else(|| {
                        "This movement stopped and cannot be published.".to_string()
                    }),
                ))
            }
            _ => {
                return Err(TallyOwlError::new(
                    ErrorCode::FailedPrecondition,
                    format!(
                        "`{}` is not proved on `{}` yet: {} of {} segments verified, and the target has applied {} of {}. Placement changes only after the copy is proved.",
                        self.tablet,
                        self.onto.node,
                        self.proved_segments(),
                        self.segment_count(),
                        self.target_applied,
                        self.catch_up_to
                    ),
                ))
            }
        }
        self.stage = Stage::Published;
        Ok(vec![
            ControllerCommand::MoveReplica {
                tablet: self.tablet.clone(),
                away_from: self.away_from.clone(),
                onto: self.onto.clone(),
            },
            ControllerCommand::SetTabletState {
                tablet: self.tablet.clone(),
                state: TabletState::Active,
            },
        ])
    }

    /// Finish, after old requests have drained.
    pub fn finish(&mut self) -> Result<(), TallyOwlError> {
        if self.stage != Stage::Published {
            return Err(TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                "A movement finishes only after its placement change is published.".to_string(),
            ));
        }
        self.stage = Stage::Finished;
        Ok(())
    }
}

/// The digest of one segment, as both ends compute it.
pub fn digest(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}
