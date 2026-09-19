//! Removing one person, everywhere they are.
//!
//! `docs/PLAN.md` Phase 8: "per-user erasure across detailed data, derived
//! state, local segments, cold objects, and caches." `AGENTS.md` gives the
//! rules and `docs/STORAGE.md` section 11 gives the mechanism.
//!
//! # What makes this harder than deleting rows
//!
//! **A person has more than one identifier.** They signed in on three devices,
//! so there are three anonymous identifiers and one known one, and an alias may
//! have merged two known ones. A predicate that named only the identifier the
//! request carried would leave the other timelines behind, and the person would
//! still be there under a name nobody looked for.
//! [`crate::identity::Identity::every_identifier_of`] is what finds the rest.
//!
//! **A tombstone is a standing predicate.** `AGENTS.md`: "It also hides
//! matching data that arrives after the erasure request." Telemetry for an
//! erased person can still be in a collector queue when the erasure lands, so
//! the predicate stays active until its horizon rather than being applied once.
//!
//! **Derived state is derived from raw, so it is covered by the same
//! predicate** — but only if the derived rows carry the identity. A rollup that
//! dropped the end-user property would survive its own source, which is why
//! [`plan`] returns one predicate for each identifier rather than one for the
//! request, and why the report says how many predicates it wrote.
//!
//! # What an erasure does not do
//!
//! It does not rewrite a segment on the spot. `AGENTS.md`: deletion "physically
//! reclaims local data by rewriting only affected bounded segments", and that
//! rewrite happens in compaction, which already rewrites cold segments. The
//! erasure is visible immediately because a read applies the predicate; the
//! bytes go when compaction next passes. The cold tier erases by destroying key
//! material, never by rewriting every intersecting object.

use tallyowl_obs::error::TallyOwlError;
use tallyowl_store::catalog::Tombstone;
use tallyowl_store::row::EventRow;
use tallyowl_store::Store;

use crate::identity::{Identity, ANONYMOUS_ID, END_USER_ID};

/// Who or what is being removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// One person, by any identifier they are known by.
    EndUser(String),
    /// A named set of events.
    Events(Vec<[u8; 16]>),
    /// Everything in a time range.
    Range(i64, i64),
}

/// What an erasure request asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub project_id: [u8; 16],
    pub target: Target,
    pub reason: String,
    /// How long the predicate keeps hiding late arrivals.
    pub horizon_ms: i64,
    pub requested_at: i64,
}

/// How long a predicate keeps hiding what arrives late, by default.
///
/// A collector holds a queue and a retry backoff, and a batch can be days old
/// when it finally lands. Thirty days is far longer than any of those and it is
/// a bound rather than for ever, which `docs/STORAGE.md` section 11 asks for.
pub const DEFAULT_HORIZON_MS: i64 = 30 * 86_400_000;

/// What an erasure did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub request_id: String,
    /// The predicates written. One for each identifier the person is known by.
    pub predicates: usize,
    /// Every identifier the erasure covered, so the record says what was
    /// removed rather than only who asked.
    pub identifiers: Vec<String>,
    pub tombstone_generation: u64,
    pub accepted_at: i64,
}

/// Build the predicates one request needs.
///
/// `rows` is what the identity graph is built from. A caller passes the
/// project's `identify` and `alias` rows; passing everything works and costs a
/// scan.
pub fn plan(request: &Request, identity: &Identity) -> Result<Vec<Tombstone>, TallyOwlError> {
    if request.reason.trim().is_empty() {
        return Err(TallyOwlError::invalid_argument(
            "An erasure needs a reason. It is written to the audit record and it is what somebody reads a year from now.",
        ));
    }
    let horizon = request
        .requested_at
        .saturating_add(if request.horizon_ms > 0 {
            request.horizon_ms
        } else {
            DEFAULT_HORIZON_MS
        });

    let base = |property: Option<(String, String)>, event_ids: Vec<[u8; 16]>, range| Tombstone {
        tombstone_id: [0u8; 16],
        generation: 0,
        project_id: request.project_id,
        event_ids,
        property,
        // An always-keep error survives even when its trace is dropped, but an
        // erasure is not a sampling decision: a person asked to be removed, and
        // no kind is excepted. D35's exception is for the tail rules.
        except_kinds: Vec::new(),
        range,
        requested_at: request.requested_at,
        horizon,
        reason: request.reason.clone(),
    };

    let mut out = Vec::new();
    match &request.target {
        Target::EndUser(who) => {
            // Every identifier this person is known by, not only the one the
            // request carried.
            for identifier in identity.every_identifier_of(who) {
                for column in [END_USER_ID, ANONYMOUS_ID] {
                    out.push(base(
                        Some((column.to_string(), identifier.clone())),
                        Vec::new(),
                        None,
                    ));
                }
            }
        }
        Target::Events(ids) => {
            if ids.is_empty() {
                return Err(TallyOwlError::invalid_argument(
                    "This erasure names no events. Name the events, the person, or the time range to remove.",
                ));
            }
            out.push(base(None, ids.clone(), None));
        }
        Target::Range(start, end) => {
            if end <= start {
                return Err(TallyOwlError::invalid_argument(
                    "An erasure range ends before it starts.",
                ));
            }
            out.push(base(None, Vec::new(), Some((*start, *end))));
        }
    }

    // Each predicate needs its own identifier, because the erasure ledger is
    // keyed by it and two predicates under one identifier would be one.
    for (index, tombstone) in out.iter_mut().enumerate() {
        tombstone.tombstone_id = identifier_for(request, index);
    }
    Ok(out)
}

/// A stable identifier for one predicate of one request.
///
/// **Stable, so a repeated request is one erasure.** A random identifier would
/// write a second ledger record for the same removal every time somebody
/// retried, and an erasure that a retry duplicates is an erasure nobody can
/// count.
fn identifier_for(request: &Request, index: usize) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tallyowl-erasure\x01");
    hasher.update(&request.project_id);
    match &request.target {
        Target::EndUser(who) => {
            hasher.update(b"end-user");
            hasher.update(who.as_bytes());
        }
        Target::Events(ids) => {
            hasher.update(b"events");
            for id in ids {
                hasher.update(id);
            }
        }
        Target::Range(start, end) => {
            hasher.update(b"range");
            hasher.update(&start.to_le_bytes());
            hasher.update(&end.to_le_bytes());
        }
    }
    hasher.update(&(index as u64).to_le_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    out
}

/// Run one erasure against a store.
///
/// Every predicate is committed. **A partial erasure is a failure**: a person
/// removed from two of their three timelines is still there, so a failure part
/// way through returns the error and names how many predicates had already
/// applied, rather than reporting success.
pub fn run(
    store: &dyn Store,
    request: &Request,
    identity: &Identity,
) -> Result<Report, TallyOwlError> {
    let predicates = plan(request, identity)?;
    let identifiers: Vec<String> = predicates
        .iter()
        .filter_map(|t| t.property.as_ref().map(|(_, value)| value.clone()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut generation = 0;
    for (applied, tombstone) in predicates.iter().enumerate() {
        generation = store.erase(tombstone).map_err(|e| {
            TallyOwlError::internal(format!(
                "This erasure stopped after {applied} of {} predicates, so the person is removed from part of their data and not all of it. Send it again; the predicates have stable identifiers, so a repeat is one erasure. The reason it stopped: {e}",
                predicates.len()
            ))
        })?;
    }

    Ok(Report {
        request_id: tallyowl_store::row::hex(&identifier_for(request, 0)),
        predicates: predicates.len(),
        identifiers,
        tombstone_generation: generation,
        accepted_at: request.requested_at,
    })
}

/// Whether one row is one this erasure would remove.
///
/// It exists so that a caller can check its own derived state — a cache, a
/// saved result — against the same rule the store applies, rather than writing
/// a second one that drifts.
pub fn hides(predicates: &[Tombstone], row: &EventRow) -> bool {
    predicates.iter().any(|predicate| predicate.hides(row))
}
