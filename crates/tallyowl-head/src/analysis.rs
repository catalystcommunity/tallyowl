//! Funnel, retention, path, and timeline.
//!
//! `docs/QUERY.md` section 12 states each one's input, its rules, and its
//! result: "A simple aggregate cannot express these questions. Each domain
//! operator has a typed input, a bounded cost, and a defined result."
//!
//! # The three properties every operator here holds
//!
//! **A result is exact or it is refused.** D21 forbids becoming approximate on
//! its own, and none of these estimates. A funnel that would exceed its guard
//! fails and names the guard and the observed value, which is
//! `docs/QUERY.md` section 14.
//!
//! **A result is explainable.** Every count here is a count of correlation keys
//! that a person could reproduce by hand from the same rows, and the rules that
//! decide are the ones section 12 writes down rather than ones this module
//! invented. The exit criterion for Phase 8 is "funnel and retention fixtures
//! have explainable exact results", and "explainable" is a property of the
//! rules and not of the output format.
//!
//! **An incomplete scan is never presented as a complete answer.** Every
//! operator takes the scan's own `incomplete` flag and carries it into the
//! result. The caller refuses unless partial mode was asked for, exactly as the
//! general algebra does.
//!
//! # Where the cost guards are
//!
//! [`Guards`] holds them and every operator checks before it works rather than
//! after. `docs/QUERY.md` section 14: "A query that exceeds a budget fails with
//! a typed error. The error names the budget and gives the observed value." A
//! guard checked afterwards has already paid the cost it exists to avoid.

use std::collections::{BTreeMap, BTreeSet};

use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_store::row::{EventRow, PropertyValue};

use crate::expr::Prepared;
use crate::identity::{Basis, Identity, Resolution};

/// What a domain operator refuses to exceed.
///
/// Each of these bounds work that grows faster than the range does. A funnel is
/// linear in rows and quadratic in nothing; a path is exponential in depth; a
/// retention matrix is cohorts by periods. The two that can explode have the
/// tighter guards, and the guard names itself in its refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guards {
    pub max_steps: usize,
    pub max_periods: usize,
    pub max_path_depth: usize,
    pub max_path_nodes: usize,
    pub max_correlation_keys: usize,
    pub max_timeline_rows: usize,
}

impl Default for Guards {
    fn default() -> Guards {
        Guards {
            // A funnel of more than this is a report rather than a question,
            // and each step is a pass over the rows of every key.
            max_steps: 20,
            // Daily periods over three years.
            max_periods: 1_100,
            // A path tree branches, so depth is the number that matters.
            max_path_depth: 12,
            max_path_nodes: 10_000,
            // The number of distinct people or sessions one operator holds in
            // memory at once. `docs/QUERY.md` section 14 names coordinator
            // memory as a budget, and this is its unit for these operators.
            max_correlation_keys: 2_000_000,
            max_timeline_rows: 10_000,
        }
    }
}

fn over_budget(
    budget: &str,
    observed: impl std::fmt::Display,
    limit: impl std::fmt::Display,
) -> TallyOwlError {
    TallyOwlError::new(
        ErrorCode::BudgetExceeded,
        format!(
            "This analysis needs {observed} {budget} and the limit is {limit}. Narrow it, or raise the limit if this installation can carry the wider question."
        ),
    )
    .retryable(false)
}

/// What every operator here reports beside its numbers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Coverage {
    /// True when the scan under this answer could not read part of its range.
    pub incomplete: bool,
    /// How many distinct correlation keys took part.
    pub keys: usize,
    /// How many rows carried no identity and could not take part.
    pub rows_without_identity: usize,
}

// ---------------------------------------------------------------------------
// Funnel
// ---------------------------------------------------------------------------

/// One step of a funnel.
pub struct Step {
    pub name: String,
    pub matches: Prepared,
    /// An exclusion step voids a sequence when it occurs inside the window.
    pub exclusion: bool,
}

/// What a funnel asks.
pub struct FunnelQuestion {
    pub basis: Basis,
    pub resolution: Resolution,
    pub window_ms: i64,
    /// Ordered mode requires the steps in order; unordered mode does not.
    pub ordered: bool,
    /// Group the answer by one property, when the caller asked.
    pub breakdown: Option<String>,
}

/// One step's result.
#[derive(Debug, Clone, PartialEq)]
pub struct StepResult {
    pub name: String,
    /// Distinct correlation keys that reached this step.
    pub reached: u64,
    /// Milliseconds from the first step to this one, for the keys that reached
    /// it. Empty on the first step, which is zero by definition.
    pub conversion_times_ms: Vec<i64>,
}

impl StepResult {
    /// The median conversion time, which is what a person reads first.
    ///
    /// Exact rather than estimated: the times are held, so there is no reason
    /// to approximate, and D21 forbids becoming approximate on its own.
    pub fn median_ms(&self) -> Option<i64> {
        if self.conversion_times_ms.is_empty() {
            return None;
        }
        let mut sorted = self.conversion_times_ms.clone();
        sorted.sort_unstable();
        Some(sorted[sorted.len() / 2])
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FunnelResult {
    pub steps: Vec<StepResult>,
    /// One result for each value of the breakdown dimension, when asked.
    pub by_dimension: BTreeMap<String, Vec<StepResult>>,
    pub coverage: Coverage,
}

/// Run a funnel over already-scanned rows.
///
/// **The first matching sequence for one correlation key counts once.**
/// `docs/QUERY.md` section 12.1. A person who completed the flow twice is one
/// conversion, because the question is how many people converted.
pub fn funnel(
    rows: &[EventRow],
    incomplete: bool,
    identity: &Identity,
    steps: &[Step],
    question: &FunnelQuestion,
    guards: &Guards,
) -> Result<FunnelResult, TallyOwlError> {
    if steps.is_empty() {
        return Err(TallyOwlError::invalid_argument(
            "A funnel needs at least one step. Name the events a person passes through.",
        ));
    }
    if steps.len() > guards.max_steps {
        return Err(over_budget("steps", steps.len(), guards.max_steps));
    }
    if steps.iter().all(|step| step.exclusion) {
        return Err(TallyOwlError::invalid_argument(
            "Every step of this funnel is an exclusion, so nothing can enter it. Add the step a person starts at.",
        ));
    }
    if question.window_ms <= 0 {
        return Err(TallyOwlError::invalid_argument(
            "A funnel needs a window. Without one, two events years apart would count as one sequence.",
        ));
    }

    let mut by_key: BTreeMap<String, Vec<&EventRow>> = BTreeMap::new();
    let mut coverage = Coverage {
        incomplete,
        ..Coverage::default()
    };
    for row in rows {
        match identity.key_for(row, question.basis, question.resolution) {
            None => coverage.rows_without_identity += 1,
            Some(key) => {
                if !by_key.contains_key(&key) && by_key.len() >= guards.max_correlation_keys {
                    return Err(over_budget(
                        match question.basis {
                            Basis::EndUser => "end users",
                            Basis::Session => "sessions",
                            Basis::Group => "groups",
                        },
                        format!("more than {}", guards.max_correlation_keys),
                        guards.max_correlation_keys,
                    ));
                }
                by_key.entry(key).or_default().push(row);
            }
        }
    }
    coverage.keys = by_key.len();

    let real: Vec<&Step> = steps.iter().filter(|step| !step.exclusion).collect();
    let exclusions: Vec<&Step> = steps.iter().filter(|step| step.exclusion).collect();

    let mut totals: Vec<StepResult> = real
        .iter()
        .map(|step| StepResult {
            name: step.name.clone(),
            reached: 0,
            conversion_times_ms: Vec::new(),
        })
        .collect();
    let mut by_dimension: BTreeMap<String, Vec<StepResult>> = BTreeMap::new();

    for (_key, mut owned) in by_key {
        owned.sort_by_key(|row| (row.occurred_at, row.event_id));
        let Some(walked) = walk(&owned, &real, &exclusions, question) else {
            continue;
        };

        // The breakdown value is taken from the row that entered the funnel, so
        // a person who changed plan mid-flow is counted in the plan they
        // started in. Counting them in both would make the parts sum to more
        // than the whole.
        let bucket = question.breakdown.as_ref().map(|name| {
            property_text(owned[walked.entered_at_index], name)
                .unwrap_or_else(|| "(not set)".to_string())
        });

        for (index, reached_at) in walked.reached.iter().enumerate() {
            let Some(at) = reached_at else { break };
            totals[index].reached += 1;
            if index > 0 {
                totals[index]
                    .conversion_times_ms
                    .push(at - walked.reached[0].expect("the first step was reached"));
            }
            if let Some(bucket) = &bucket {
                let held = by_dimension.entry(bucket.clone()).or_insert_with(|| {
                    real.iter()
                        .map(|step| StepResult {
                            name: step.name.clone(),
                            reached: 0,
                            conversion_times_ms: Vec::new(),
                        })
                        .collect()
                });
                held[index].reached += 1;
                if index > 0 {
                    held[index]
                        .conversion_times_ms
                        .push(at - walked.reached[0].expect("the first step was reached"));
                }
            }
        }
    }

    Ok(FunnelResult {
        steps: totals,
        by_dimension,
        coverage,
    })
}

struct Walked {
    /// When each step was reached, or `None` from the first one that was not.
    reached: Vec<Option<i64>>,
    entered_at_index: usize,
}

/// Walk one correlation key's rows through the steps.
///
/// Ordered mode requires the steps in order, so it takes the earliest row that
/// matches step 1 and then the earliest row after it that matches step 2, and
/// so on. Unordered mode asks only whether each step happened inside the
/// window.
///
/// **The two modes open the window at different rows, and they have to.**
/// Ordered opens at the first row matching step 1, because in ordered mode
/// nothing before step 1 is part of the sequence. Unordered opens at the first
/// row matching **any** step, because in unordered mode it is: a person who did
/// step 2 and then step 1 has done both, and a window anchored on step 1 would
/// have closed before it started.
fn walk(
    rows: &[&EventRow],
    steps: &[&Step],
    exclusions: &[&Step],
    question: &FunnelQuestion,
) -> Option<Walked> {
    // Where the sequence starts. "The first matching sequence for one
    // correlation key counts once", `docs/QUERY.md` section 12.1.
    let entered_at_index = if question.ordered {
        rows.iter().position(|row| steps[0].matches.keeps(row))?
    } else {
        rows.iter()
            .position(|row| steps.iter().any(|step| step.matches.keeps(row)))?
    };
    let opened_at = rows[entered_at_index].occurred_at;
    let closes_at = opened_at.saturating_add(question.window_ms);

    // An exclusion voids a sequence when it occurs inside the window.
    if exclusions.iter().any(|excluded| {
        rows.iter().any(|row| {
            row.occurred_at >= opened_at
                && row.occurred_at < closes_at
                && excluded.matches.keeps(row)
        })
    }) {
        return None;
    }

    let mut reached: Vec<Option<i64>> = vec![None; steps.len()];
    reached[0] = Some(opened_at);

    if question.ordered {
        let mut after = entered_at_index;
        for (index, step) in steps.iter().enumerate().skip(1) {
            let found = rows
                .iter()
                .enumerate()
                .skip(after + 1)
                .find(|(_, row)| {
                    row.occurred_at >= opened_at
                        && row.occurred_at < closes_at
                        && step.matches.keeps(row)
                })
                .map(|(at, row)| (at, row.occurred_at));
            match found {
                Some((at, when)) => {
                    reached[index] = Some(when);
                    after = at;
                }
                None => break,
            }
        }
    } else {
        // Every step, including the first, because the window may have opened
        // on a later step and the first one may be anywhere inside it.
        for (index, step) in steps.iter().enumerate() {
            let when = rows
                .iter()
                .filter(|row| row.occurred_at >= opened_at && row.occurred_at < closes_at)
                .filter(|row| step.matches.keeps(row))
                .map(|row| row.occurred_at)
                .min();
            match when {
                Some(when) => reached[index] = Some(when),
                // A gap still stops the walk, because a step nobody reached
                // cannot be followed by one they did: the funnel counts a
                // sequence, and section 12.1 gives no way to skip a step.
                None => {
                    // Nothing after a gap, and nothing at the gap either.
                    for later in reached.iter_mut().skip(index) {
                        *later = None;
                    }
                    break;
                }
            }
        }
        reached[0]?;
    }

    Some(Walked {
        reached,
        entered_at_index,
    })
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

/// The period a retention matrix counts in.
///
/// **A period is a calendar period in the range's timezone**, so a month is the
/// month it is and a day across a daylight-saving boundary is 23 or 25 hours.
/// L110 made a month 28 days for want of a timezone database and said plainly
/// that a monthly retention matrix built on it was not calendar-accurate; L134
/// built the calendar. See [`crate::calendar`].
pub type Period = crate::calendar::Unit;

/// Where a retention matrix counts from, and in what zone.
#[derive(Debug, Clone, Copy)]
pub struct Periods {
    pub unit: Period,
    pub zone: crate::calendar::Zone,
}

impl Periods {
    /// Which period `at` falls in, counted from the epoch.
    fn index(&self, epoch: i64, at: i64) -> usize {
        self.zone.periods_between(epoch, at, self.unit).max(0) as usize
    }

    /// Where a period index starts, as a UTC millisecond.
    fn start(&self, epoch: i64, index: usize) -> i64 {
        self.zone.advance(epoch, self.unit, index as i64)
    }
}

/// What a retention matrix asks.
pub struct RetentionQuestion {
    pub initial: Prepared,
    pub returning: Prepared,
    pub period: Periods,
    pub periods: usize,
    /// First-time semantics: only a key whose initial event is its first one in
    /// the whole range enters a cohort.
    pub first_time_only: bool,
    pub basis: Basis,
    pub resolution: Resolution,
    /// Where period zero starts. Every cohort key is measured from here, so two
    /// runs of the same query give the same matrix.
    pub epoch: i64,
}

/// One cohort's row of the matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct Cohort {
    /// The period index this cohort started in, from the epoch.
    pub period: usize,
    pub started_at: i64,
    /// How many keys entered.
    pub size: u64,
    /// How many of them returned in each later period, index 0 being the period
    /// they started in.
    pub returned: Vec<u64>,
}

impl Cohort {
    /// The share that returned in one period, as a fraction.
    pub fn rate(&self, period: usize) -> f64 {
        match (self.size, self.returned.get(period)) {
            (0, _) | (_, None) => 0.0,
            (size, Some(returned)) => *returned as f64 / size as f64,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RetentionResult {
    pub period: String,
    pub cohorts: Vec<Cohort>,
    pub coverage: Coverage,
}

/// Build a cohort-by-period retention matrix.
pub fn retention(
    rows: &[EventRow],
    incomplete: bool,
    identity: &Identity,
    question: &RetentionQuestion,
    guards: &Guards,
) -> Result<RetentionResult, TallyOwlError> {
    if question.periods == 0 {
        return Err(TallyOwlError::invalid_argument(
            "A retention matrix needs at least one period.",
        ));
    }
    if question.periods > guards.max_periods {
        return Err(over_budget("periods", question.periods, guards.max_periods));
    }

    let mut coverage = Coverage {
        incomplete,
        ..Coverage::default()
    };

    // What each key did, in time order.
    let mut by_key: BTreeMap<String, Vec<&EventRow>> = BTreeMap::new();
    for row in rows {
        match identity.key_for(row, question.basis, question.resolution) {
            None => coverage.rows_without_identity += 1,
            Some(key) => {
                if !by_key.contains_key(&key) && by_key.len() >= guards.max_correlation_keys {
                    return Err(over_budget(
                        "correlation keys",
                        format!("more than {}", guards.max_correlation_keys),
                        guards.max_correlation_keys,
                    ));
                }
                by_key.entry(key).or_default().push(row);
            }
        }
    }
    coverage.keys = by_key.len();

    let mut cohorts: BTreeMap<usize, Cohort> = BTreeMap::new();
    for (_key, mut owned) in by_key {
        owned.sort_by_key(|row| (row.occurred_at, row.event_id));

        let Some(entered) = owned.iter().find(|row| question.initial.keeps(row)) else {
            continue;
        };
        if question.first_time_only && !std::ptr::eq(*entered, owned[0]) {
            // Its initial event is not its first event in the range, so this
            // key was already active before the cohort it would join. Counting
            // it would make an established person look like a new one.
            continue;
        }
        let started = entered.occurred_at;
        let cohort_period = question.period.index(question.epoch, started);

        let held = cohorts.entry(cohort_period).or_insert_with(|| Cohort {
            period: cohort_period,
            started_at: question.period.start(question.epoch, cohort_period),
            size: 0,
            returned: vec![0; question.periods],
        });
        held.size += 1;

        // Which later periods this key came back in. A key that returned twice
        // in one period counts once for that period, because the question is
        // how many people came back.
        let mut seen: BTreeSet<usize> = BTreeSet::new();
        for row in &owned {
            if row.occurred_at < started || !question.returning.keeps(row) {
                continue;
            }
            let offset = question
                .period
                .index(question.epoch, row.occurred_at)
                .saturating_sub(cohort_period);
            if offset < question.periods {
                seen.insert(offset);
            }
        }
        for offset in seen {
            held.returned[offset] += 1;
        }
    }

    Ok(RetentionResult {
        period: question.period.unit.as_str().to_string(),
        cohorts: cohorts.into_values().collect(),
        coverage,
    })
}

// ---------------------------------------------------------------------------
// Path
// ---------------------------------------------------------------------------

/// What a path tree asks.
pub struct PathQuestion {
    pub anchor: Prepared,
    /// Previous walks backwards from the anchor; next walks forwards.
    pub forwards: bool,
    pub depth: usize,
    pub min_frequency: u64,
    /// Collapse a repeated node into one, so a person refreshing a page ten
    /// times is one step rather than ten.
    pub collapse_loops: bool,
    pub basis: Basis,
    pub resolution: Resolution,
}

/// One node of the bounded tree.
#[derive(Debug, Clone, PartialEq)]
pub struct PathNode {
    /// The semantic name of the page or interaction.
    pub name: String,
    pub depth: usize,
    pub count: u64,
    pub children: Vec<PathNode>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PathResult {
    pub root: Option<PathNode>,
    pub coverage: Coverage,
    /// True when a branch was dropped for being below the minimum frequency.
    /// A tree that silently dropped one would read as complete.
    pub pruned: bool,
}

/// Build a bounded tree of what came before or after an anchor.
pub fn path(
    rows: &[EventRow],
    incomplete: bool,
    identity: &Identity,
    question: &PathQuestion,
    guards: &Guards,
) -> Result<PathResult, TallyOwlError> {
    if question.depth == 0 {
        return Err(TallyOwlError::invalid_argument(
            "A path needs a depth of at least one step.",
        ));
    }
    if question.depth > guards.max_path_depth {
        return Err(over_budget(
            "steps of depth",
            question.depth,
            guards.max_path_depth,
        ));
    }

    let mut coverage = Coverage {
        incomplete,
        ..Coverage::default()
    };
    let mut by_key: BTreeMap<String, Vec<&EventRow>> = BTreeMap::new();
    for row in rows {
        match identity.key_for(row, question.basis, question.resolution) {
            None => coverage.rows_without_identity += 1,
            Some(key) => by_key.entry(key).or_default().push(row),
        }
    }
    coverage.keys = by_key.len();

    // Each key contributes at most one sequence, from its first anchor.
    let mut sequences: Vec<Vec<String>> = Vec::new();
    for (_key, mut owned) in by_key {
        owned.sort_by_key(|row| (row.occurred_at, row.event_id));
        let Some(anchor_at) = owned.iter().position(|row| question.anchor.keeps(row)) else {
            continue;
        };
        let mut names: Vec<String> = if question.forwards {
            owned[anchor_at..]
                .iter()
                .map(|row| node_name(row))
                .collect()
        } else {
            owned[..=anchor_at]
                .iter()
                .rev()
                .map(|row| node_name(row))
                .collect()
        };
        if question.collapse_loops {
            names.dedup();
        }
        names.truncate(question.depth + 1);
        if names.len() > 1 {
            sequences.push(names);
        }
    }

    if sequences.is_empty() {
        return Ok(PathResult {
            root: None,
            coverage,
            pruned: false,
        });
    }

    let root_name = sequences[0][0].clone();
    let mut nodes = 0usize;
    let mut pruned = false;
    let root = build_node(
        &root_name,
        0,
        &sequences.iter().map(|s| s.as_slice()).collect::<Vec<_>>(),
        question,
        guards,
        &mut nodes,
        &mut pruned,
    )?;

    Ok(PathResult {
        root: Some(root),
        coverage,
        pruned,
    })
}

fn build_node(
    name: &str,
    depth: usize,
    sequences: &[&[String]],
    question: &PathQuestion,
    guards: &Guards,
    nodes: &mut usize,
    pruned: &mut bool,
) -> Result<PathNode, TallyOwlError> {
    *nodes += 1;
    if *nodes > guards.max_path_nodes {
        return Err(over_budget("nodes", *nodes, guards.max_path_nodes));
    }

    let mut children: Vec<PathNode> = Vec::new();
    if depth < question.depth {
        let mut grouped: BTreeMap<String, Vec<&[String]>> = BTreeMap::new();
        for sequence in sequences {
            if sequence.len() > depth + 1 {
                grouped
                    .entry(sequence[depth + 1].clone())
                    .or_default()
                    .push(sequence);
            }
        }
        for (child_name, held) in grouped {
            if (held.len() as u64) < question.min_frequency {
                // A branch below the minimum is dropped, and the result says a
                // branch was dropped. A tree that pruned in silence would read
                // as the whole picture.
                *pruned = true;
                continue;
            }
            children.push(build_node(
                &child_name,
                depth + 1,
                &held,
                question,
                guards,
                nodes,
                pruned,
            )?);
        }
    }
    children.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));

    Ok(PathNode {
        name: name.to_string(),
        depth,
        count: sequences.len() as u64,
        children,
    })
}

/// The semantic name of one node.
///
/// `docs/QUERY.md` section 12.3: "a node is a semantic page or a semantic
/// interaction". A route or a target, never a selector.
fn node_name(row: &EventRow) -> String {
    property_text(row, "route")
        .or_else(|| property_text(row, "target"))
        .unwrap_or_else(|| row.name.clone())
}

// ---------------------------------------------------------------------------
// Timeline
// ---------------------------------------------------------------------------

/// What a timeline asks.
pub struct TimelineQuestion {
    /// One of these. A timeline is for one person or one session.
    pub end_user_id: Option<String>,
    pub session_id: Option<String>,
    pub kinds: Vec<String>,
    pub limit: usize,
    /// Where to resume, as an occurred time and an event ID. Section 13 makes a
    /// cursor deterministic by appending the event ID to the sort.
    pub after: Option<(i64, [u8; 16])>,
    pub resolution: Resolution,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimelineResult {
    pub rows: Vec<EventRow>,
    /// Where a caller resumes. Absent when there is nothing more.
    pub next_after: Option<(i64, [u8; 16])>,
    pub coverage: Coverage,
}

/// The merged envelopes for one end user or session, in event-time order.
pub fn timeline(
    rows: &[EventRow],
    incomplete: bool,
    identity: &Identity,
    question: &TimelineQuestion,
    guards: &Guards,
) -> Result<TimelineResult, TallyOwlError> {
    if question.end_user_id.is_none() && question.session_id.is_none() {
        return Err(TallyOwlError::invalid_argument(
            "A timeline is for one end user or one session. Name which.",
        ));
    }
    let limit = question.limit.clamp(1, guards.max_timeline_rows);

    let wanted_kinds: BTreeSet<&str> = question.kinds.iter().map(String::as_str).collect();
    // A person's canonical identifier, so a timeline asked for by any of their
    // identifiers finds all of it.
    let wanted_user = question
        .end_user_id
        .as_ref()
        .map(|id| identity.canonical(id));

    let mut found: Vec<EventRow> = rows
        .iter()
        .filter(|row| wanted_kinds.is_empty() || wanted_kinds.contains(row.kind.as_str()))
        .filter(|row| match (&wanted_user, &question.session_id) {
            (Some(who), _) => identity
                .who(row, question.resolution)
                .is_some_and(|found| &found == who),
            (None, Some(session)) => row.session_id.as_deref() == Some(session.as_str()),
            (None, None) => false,
        })
        .cloned()
        .collect();

    found.sort_by_key(|row| (row.occurred_at, row.event_id));
    if let Some((at, event_id)) = question.after {
        found.retain(|row| (row.occurred_at, row.event_id) > (at, event_id));
    }

    let more = found.len() > limit;
    found.truncate(limit);
    let next_after = more
        .then(|| found.last().map(|row| (row.occurred_at, row.event_id)))
        .flatten();

    Ok(TimelineResult {
        coverage: Coverage {
            incomplete,
            keys: 1,
            rows_without_identity: 0,
        },
        rows: found,
        next_after,
    })
}

fn property_text(row: &EventRow, key: &str) -> Option<String> {
    match row.properties.get(key) {
        Some((PropertyValue::Text(value), _)) if !value.is_empty() => Some(value.clone()),
        Some((other, _)) => Some(other.to_display()).filter(|text| !text.is_empty()),
        None => None,
    }
}
