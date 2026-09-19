//! Distributed planning, partial aggregation, and the rules about being wrong.
//!
//! `docs/QUERY.md` section 10 draws the line: a storage node evaluates time and
//! project pruning, segment pruning, exact index lookup, filters, column
//! selection, partial aggregate states, and local sort and limit. The
//! coordinator merges partial states, sorts and limits globally, joins bounded
//! inputs, and finalizes domain operators. **The coordinator never pulls raw
//! rows to compute an aggregate.** It pulls rows only for a detail query, a
//! trace assembly, or an exact lookup.
//!
//! # The three rules that make a distributed answer honest
//!
//! **A partial result is never the default.** `docs/QUERY.md` section 9: a
//! missing tablet causes a typed incomplete-result error, and a caller must ask
//! for partial mode explicitly. [`Plan::allow_partial`] is that request, and
//! [`merge`] refuses without it.
//!
//! **A partial result can never mark itself complete.** Completeness is
//! multiplied across parts, never added: one incomplete part makes the whole
//! incomplete, for ever, and no later merge can undo it.
//!
//! **The watermark is the smallest, not the largest.** A result is only as
//! current as its least current part. Reporting the largest would let a
//! dashboard believe it had seen a write that one tablet had not applied.
//!
//! # Fan-out has a maximum
//!
//! A query that needs more tablets than `query.maxFanOut` fails with a typed
//! error rather than opening a thousand connections. `docs/QUERY.md` section 10
//! requires it, and the refusal names both numbers so an operator can decide
//! whether to raise the limit or narrow the query.
//!
//! # An exact lookup at high cardinality
//!
//! `AGENTS.md`: "Do not silently drop, coalesce, or reject a value because it
//! has high cardinality." Across tablets that means the lookup goes to every
//! tablet that could hold the value and the answers are concatenated, not
//! sampled. A tablet prunes with its own locator, which is what stops the fan
//! out being a full scan; the rows themselves decide, because a fingerprint
//! prunes and never answers.

use std::collections::BTreeMap;

use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_store::row::EventRow;
use tallyowl_store::TimeBasis;

use crate::topology::{Generation, TabletName, Topology};

/// How current a read has to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consistency {
    /// Route to a voting replica at or past the requested watermark.
    Committed,
    /// A read replica whose watermark is inside the caller's tolerance.
    BoundedStale { max_staleness_ms: i64 },
}

/// What kind of partial state a tablet is being asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartialKind {
    /// A count. The partial state is an integer.
    Count,
    /// A count for each time bucket. The partial state is a bucket map, and
    /// merging is addition on a shared bucket start.
    Trend { bucket_ms: i64 },
    /// Rows. This is a detail query, and it is one of the three forms that may
    /// move rows at all.
    Rows { max_rows: usize },
    /// An exact lookup on a correlation value. Also allowed to move rows.
    Lookup { column: String, value: Vec<u8> },
    /// A general aggregate, computed on the tablet.
    ///
    /// **The plan travels as bytes and this module never reads it.** A storage
    /// node runs the coordinator's own aggregation over its own rows, so there
    /// is one implementation of a sum rather than two: L102 is what a second
    /// one costs. The partial states come back as bytes for the same reason,
    /// and the coordinator's aggregation merges them.
    Aggregate { plan: Vec<u8> },
}

impl PartialKind {
    /// Whether this form moves rows. `docs/QUERY.md` section 10 names the three
    /// that do, and an aggregate is not one of them.
    pub fn moves_rows(&self) -> bool {
        matches!(self, PartialKind::Rows { .. } | PartialKind::Lookup { .. })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            PartialKind::Count => "count",
            PartialKind::Trend { .. } => "trend",
            PartialKind::Rows { .. } => "rows",
            PartialKind::Lookup { .. } => "lookup",
            PartialKind::Aggregate { .. } => "aggregate",
        }
    }
}

/// One query, planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub project_id: [u8; 16],
    pub range_start: i64,
    pub range_end: i64,
    pub basis: TimeBasis,
    pub kind: PartialKind,
    pub event_name: Option<String>,
    pub consistency: Consistency,
    /// The watermark a `committed` read requires. Zero means "whatever the
    /// tablet has".
    pub require_watermark: u64,
    /// A partial result is never the default. See the module note.
    pub allow_partial: bool,
    /// Every tablet this query must ask.
    pub tablets: Vec<TabletName>,
    pub generation: Generation,
}

/// How many tablets one query may fan out to before it is refused.
///
/// The default matches a cell that has grown well past what one query should
/// touch at once. `docs/QUERY.md` section 10 makes it configurable.
pub const DEFAULT_MAX_FAN_OUT: usize = 256;

/// Plan one query against a topology.
///
/// Every tablet that is not retired is asked, including a draining one. A
/// draining tablet still answers reads until old requests stop, and leaving it
/// out is how a movement would silently shorten an answer.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    topology: &Topology,
    project_id: [u8; 16],
    range_start: i64,
    range_end: i64,
    basis: TimeBasis,
    kind: PartialKind,
    event_name: Option<String>,
    consistency: Consistency,
    allow_partial: bool,
    max_fan_out: usize,
) -> Result<Plan, TallyOwlError> {
    let tablets: Vec<TabletName> = topology
        .readable_tablets()
        .into_iter()
        .map(|t| t.name.clone())
        .collect();
    if tablets.len() > max_fan_out {
        return Err(TallyOwlError::new(
            ErrorCode::BudgetExceeded,
            format!(
                "This query needs {} tablets and the fan-out limit is {max_fan_out}. Narrow the time range, or raise `query.maxFanOut` if this installation can carry the wider query.",
                tablets.len()
            ),
        ));
    }
    Ok(Plan {
        project_id,
        range_start,
        range_end,
        basis,
        kind,
        event_name,
        consistency,
        require_watermark: 0,
        allow_partial,
        tablets,
        generation: topology.generation,
    })
}

/// A time range one tablet could not answer for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingRange {
    pub tablet: TabletName,
    pub range_start: i64,
    pub range_end: i64,
}

/// What one tablet computed.
#[derive(Debug, Clone, PartialEq)]
pub struct Partial {
    pub tablet: TabletName,
    pub commit_watermark: u64,
    pub freshness_ms: i64,
    /// False when this tablet could not read part of its own range. It never
    /// becomes true again in a merge.
    pub complete: bool,
    pub missing: Vec<MissingRange>,
    pub count: u64,
    /// Bucket start to count. Merging is addition on a shared bucket start.
    pub buckets: BTreeMap<i64, u64>,
    pub rows: Vec<EventRow>,
    /// The partial state of every group this tablet aggregated, encoded. Opaque
    /// here; see [`PartialKind::Aggregate`].
    pub aggregate_state: Option<Vec<u8>>,
    pub scanned_segments: u64,
    pub scanned_bytes: u64,
    /// True when this tablet's answer overlaps a range that unsafe recovery
    /// marked. `docs/FAILURE_MODES.md` section 6.2 requires it to reach the
    /// result and the explain output.
    pub degraded: bool,
}

impl Partial {
    pub fn for_tablet(tablet: impl Into<TabletName>) -> Partial {
        Partial {
            tablet: tablet.into(),
            commit_watermark: 0,
            freshness_ms: 0,
            complete: true,
            missing: Vec::new(),
            count: 0,
            buckets: BTreeMap::new(),
            rows: Vec::new(),
            aggregate_state: None,
            scanned_segments: 0,
            scanned_bytes: 0,
            degraded: false,
        }
    }

    /// A tablet that did not answer at all.
    pub fn unavailable(tablet: impl Into<TabletName>, range_start: i64, range_end: i64) -> Partial {
        let tablet = tablet.into();
        Partial {
            complete: false,
            missing: vec![MissingRange {
                tablet: tablet.clone(),
                range_start,
                range_end,
            }],
            ..Partial::for_tablet(tablet)
        }
    }
}

/// The merged answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Merged {
    pub count: u64,
    pub buckets: Vec<(i64, u64)>,
    pub rows: Vec<EventRow>,
    /// One partial state for each tablet that answered an aggregate, in the
    /// order the tablets were asked. The coordinator's aggregation merges them.
    pub aggregate_states: Vec<Vec<u8>>,
    /// The smallest watermark across every part. See the module note.
    pub commit_watermark: u64,
    /// The largest staleness across every part, for the same reason.
    pub freshness_ms: i64,
    pub complete: bool,
    pub missing: Vec<MissingRange>,
    pub scanned_segments: u64,
    pub scanned_bytes: u64,
    /// True when any part's answer overlaps a degraded range.
    pub degraded: bool,
    /// What an operator reads in the explain output when a part was degraded or
    /// missing. Empty when everything answered and nothing was marked.
    pub warnings: Vec<String>,
}

/// Merge every tablet's partial state into one answer.
///
/// A missing part is a typed `incomplete-result` refusal unless the caller
/// asked for partial mode. That is the default `docs/QUERY.md` section 9
/// states, and it is the default here rather than a flag somebody has to
/// remember.
pub fn merge(plan: &Plan, parts: Vec<Partial>) -> Result<Merged, TallyOwlError> {
    let mut merged = Merged {
        count: 0,
        buckets: Vec::new(),
        rows: Vec::new(),
        aggregate_states: Vec::new(),
        // Start at the maximum and take the smallest, so a merge of nothing
        // does not claim an impossible watermark.
        commit_watermark: u64::MAX,
        freshness_ms: 0,
        complete: true,
        missing: Vec::new(),
        scanned_segments: 0,
        scanned_bytes: 0,
        degraded: false,
        warnings: Vec::new(),
    };
    let mut buckets: BTreeMap<i64, u64> = BTreeMap::new();

    for part in &parts {
        // Completeness is multiplied, never added.
        merged.complete = merged.complete && part.complete;
        merged.degraded = merged.degraded || part.degraded;
        merged.count += part.count;
        merged.scanned_segments += part.scanned_segments;
        merged.scanned_bytes += part.scanned_bytes;
        merged.commit_watermark = merged.commit_watermark.min(part.commit_watermark);
        merged.freshness_ms = merged.freshness_ms.max(part.freshness_ms);
        merged.missing.extend(part.missing.iter().cloned());
        for (start, count) in &part.buckets {
            *buckets.entry(*start).or_insert(0) += count;
        }
        if plan.kind.moves_rows() {
            merged.rows.extend(part.rows.iter().cloned());
        } else if !part.rows.is_empty() {
            // A tablet that sent rows for an aggregate broke the pushdown
            // contract. Saying so is better than quietly using them, because
            // the numbers would be right and the design would be wrong.
            merged.warnings.push(format!(
                "`{}` sent rows for a `{}`, which does not move rows. They were not used.",
                part.tablet,
                plan.kind.as_str()
            ));
        }
        if let Some(state) = &part.aggregate_state {
            merged.aggregate_states.push(state.clone());
        }
        if part.degraded {
            merged.warnings.push(format!(
                "`{}` holds a range that unsafe recovery marked degraded. An acknowledged write may be missing from this answer, and no component can say which.",
                part.tablet
            ));
        }
    }

    if parts.is_empty() {
        merged.commit_watermark = 0;
    }
    merged.buckets = buckets.into_iter().collect();

    // A part that never arrived at all. The caller is meant to build a
    // `Partial::unavailable` for a tablet that did not answer, and a caller that
    // forgot would otherwise get a smaller answer marked complete — which is the
    // failure `docs/FAILURE_MODES.md` section 2 ranks worst. Counting the parts
    // against the plan catches it here rather than in a dashboard.
    let answered: std::collections::BTreeSet<&str> =
        parts.iter().map(|p| p.tablet.as_str()).collect();
    for tablet in &plan.tablets {
        if !answered.contains(tablet.as_str()) {
            merged.complete = false;
            merged.missing.push(MissingRange {
                tablet: tablet.clone(),
                range_start: plan.range_start,
                range_end: plan.range_end,
            });
        }
    }

    if !merged.complete {
        if !plan.allow_partial {
            let names: Vec<&str> = merged.missing.iter().map(|m| m.tablet.as_str()).collect();
            return Err(TallyOwlError::new(
                ErrorCode::IncompleteResult,
                format!(
                    "This answer would be missing data from {} and would therefore be smaller than the truth. Ask again with partial results allowed if a smaller answer is useful, and the result will name what is missing. Missing: {}.",
                    if names.len() == 1 { "one tablet" } else { "some tablets" },
                    names.join(", ")
                ),
            ));
        }
        merged.warnings.push(format!(
            "This is a partial answer. {} of {} tablets did not answer, so every number here is at most the truth and never more.",
            merged.missing.len(),
            plan.tablets.len()
        ));
    }

    // A row-moving query still respects its own bound after the merge, because
    // each tablet applied the limit locally and the union can exceed it.
    if let PartialKind::Rows { max_rows } = plan.kind {
        merged.rows.truncate(max_rows);
    }
    Ok(merged)
}

/// Whether a replica may answer this plan.
///
/// `docs/QUERY.md` section 9 and D18. A `committed` read needs a voting replica
/// at or past the requested watermark; a `bounded-stale` read needs a replica
/// inside the caller's tolerance. A replica that fails the test is not asked,
/// and the coordinator counts it as missing rather than reading it anyway.
pub fn replica_satisfies(
    plan: &Plan,
    is_voter: bool,
    applied_watermark: u64,
    staleness_ms: i64,
) -> Result<(), TallyOwlError> {
    match plan.consistency {
        Consistency::Committed => {
            if !is_voter {
                return Err(TallyOwlError::new(
                    ErrorCode::FailedPrecondition,
                    "A `committed` read has to be answered by a voting replica, and this one does not vote. A read replica can answer a `bounded-stale` read instead.".to_string(),
                ));
            }
            if applied_watermark < plan.require_watermark {
                return Err(TallyOwlError::new(
                    ErrorCode::FailedPrecondition,
                    format!(
                        "This replica has reached watermark {applied_watermark} and the read asked for {}. It would answer with less than the caller already knows was written.",
                        plan.require_watermark
                    ),
                ));
            }
            Ok(())
        }
        Consistency::BoundedStale { max_staleness_ms } => {
            if staleness_ms > max_staleness_ms {
                return Err(TallyOwlError::new(
                    ErrorCode::FailedPrecondition,
                    format!(
                        "This replica is {staleness_ms} ms behind and the read allows {max_staleness_ms} ms."
                    ),
                ));
            }
            Ok(())
        }
    }
}
