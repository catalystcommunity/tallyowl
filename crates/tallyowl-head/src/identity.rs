//! Who a row belongs to, and when that became true.
//!
//! `docs/DATA_MODEL.md` section 3.5 states the model and this holds it:
//!
//! - anonymous IDs are random and **project scoped**;
//! - the trusted app backend supplies each known end-user ID;
//! - `identify` links an anonymous timeline to a known end user **from that
//!   point**;
//! - `alias` is an explicit, auditable merge edge; **it does not rewrite raw
//!   events**;
//! - group associations carry validity time;
//! - profiles hold only approved traits.
//!
//! # The graph is derived, and it is derived from the rows
//!
//! `AGENTS.md`: "Make all derived projections and rollups reproducible from
//! retained raw data." This is built by reading `identify`, `alias`, and
//! `group` rows and nothing else. There is no second durable copy to fall out
//! of step with the first, and an erasure that removes a person's rows removes
//! them from the graph by the same act.
//!
//! # Event time or latest known, and why both exist
//!
//! Section 3.5: "Queries can use event-time identity or latest-known identity."
//! They answer different questions and neither is the right default for the
//! other:
//!
//! - **event time** asks who this row belonged to when it happened. A funnel
//!   over a sign-up flow needs it, because the steps before the sign-in were
//!   anonymous at the time and the answer should say so;
//! - **latest known** asks who this row belongs to now. A person's timeline
//!   needs it, because somebody looking at one end user wants everything that
//!   turned out to be theirs.
//!
//! A query names which. The default is latest known, because it is the one a
//! person asking about an end user means, and every operator that uses event
//! time says so where it asks.
//!
//! # Cross-project leakage is impossible by construction
//!
//! Every key here is scoped by project, and [`Identity::build`] takes the
//! project it is building for and ignores every row that does not carry it,
//! counting them. Two projects that both use the anonymous ID `a1` are two
//! different people, and nothing in this module can merge them.

use std::collections::{BTreeMap, BTreeSet};

use tallyowl_store::row::{EventRow, PropertyValue};

/// The property that carries a known end-user identifier.
pub const END_USER_ID: &str = "end_user_id";
/// The property that carries a project-scoped anonymous identifier.
pub const ANONYMOUS_ID: &str = "anonymous_id";

/// The telemetry kinds that change the graph.
const IDENTIFY: &str = "identify";
const ALIAS: &str = "alias";
const GROUP: &str = "group";

/// How long an alias chain may be before it is treated as a loop.
///
/// A chain longer than this is a defect in what somebody sent, not a person
/// with a thousand identities. The resolution stops and keeps the last
/// identifier it reached rather than looping.
const MAX_ALIAS_DEPTH: usize = 64;

/// Which identity a query means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Who this row belonged to when it happened.
    EventTime,
    /// Who this row belongs to now.
    LatestKnown,
}

/// What correlates rows for a domain operator.
///
/// `docs/QUERY.md` section 12.1: "the correlation basis is an end user, a
/// session, or a group".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    EndUser,
    Session,
    Group,
}

impl Basis {
    pub fn as_str(&self) -> &'static str {
        match self {
            Basis::EndUser => "end-user",
            Basis::Session => "session",
            Basis::Group => "group",
        }
    }
}

/// One `identify`: an anonymous timeline joined a known end user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub anonymous_id: String,
    pub end_user_id: String,
    pub at: i64,
    /// The row that said so, so an answer can be explained.
    pub event_id: [u8; 16],
}

/// One `alias`: two known identifiers are one person.
///
/// It is kept as a record rather than applied to the rows. Section 3.5: "alias
/// is an explicit, auditable merge edge; it does not rewrite raw events." A
/// merge that rewrote history would make a stored row disagree with the receipt
/// that acknowledged it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge {
    pub from_id: String,
    pub to_id: String,
    pub at: i64,
    pub event_id: [u8; 16],
}

/// One group association, with the time it began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub group_id: String,
    pub group_kind: Option<String>,
    pub since: i64,
}

/// The identity graph over one project.
#[derive(Debug, Clone, Default)]
pub struct Identity {
    project_id: [u8; 16],
    /// Anonymous identifier to every `identify` for it, in time order.
    links: BTreeMap<String, Vec<Link>>,
    merges: Vec<Merge>,
    /// A known identifier to the one it was merged into. Following it to a
    /// fixed point gives the canonical identifier.
    merged_into: BTreeMap<String, String>,
    /// End user to their group memberships, in time order.
    memberships: BTreeMap<String, Vec<Membership>>,
    /// End user to trait name to every value it was set to, in time order.
    ///
    /// **The history is kept rather than only the winner**, because a graph is
    /// bounded to a moment when a query reads it, and a truncation that had
    /// only the winner could not recover the value that held before it. See
    /// [`Identity::bounded_to`].
    traits: BTreeMap<String, BTreeMap<String, Vec<(PropertyValue, i64)>>>,
    /// Rows that named another project. It is always zero in an installation
    /// that works, and a test asserts that a row from elsewhere changes
    /// nothing.
    foreign_rows_ignored: usize,
    /// Which rows are already folded in, so folding more is idempotent.
    ///
    /// A materialised graph is folded once and then has newer rows applied on
    /// top, and the two sets overlap whenever a query's range reaches back
    /// behind the materialisation. Counting one `identify` twice would put two
    /// links in a list that `who` reads by its latest, which is harmless, and
    /// two merges in a list an operator reads, which is not.
    folded: BTreeSet<[u8; 16]>,
    /// The moment this graph is complete up to. Everything before it is folded
    /// in; nothing is claimed about anything at or after it.
    covered_before: i64,
}

impl Identity {
    /// Build the graph for one project from raw rows.
    ///
    /// Rows may be in any order and may come from any project. Only the named
    /// project's rows are read.
    pub fn build(project_id: [u8; 16], rows: &[EventRow]) -> Identity {
        let mut identity = Identity {
            project_id,
            covered_before: i64::MIN,
            ..Identity::default()
        };
        identity.apply(rows);
        identity
    }

    /// Fold more rows into a graph that already holds some.
    ///
    /// **This is what makes a materialised graph possible.** A graph is a fold
    /// over identity rows in time order, and every row this applies is at or
    /// after everything already folded in the case that matters — a snapshot
    /// with the newer rows put on top. A row that was already folded is
    /// skipped, so applying an overlapping range twice gives one graph.
    pub fn apply(&mut self, rows: &[EventRow]) {
        let project_id = self.project_id;
        // Time order, because an `identify` links from a point and a later one
        // for the same anonymous identifier supersedes an earlier one.
        let mut ordered: Vec<&EventRow> = rows
            .iter()
            .filter(|row| {
                if row.project_id == project_id {
                    true
                } else {
                    self.foreign_rows_ignored += 1;
                    false
                }
            })
            .filter(|row| matches!(row.kind.as_str(), IDENTIFY | ALIAS | GROUP))
            .filter(|row| !self.folded.contains(&row.event_id))
            .collect();
        ordered.sort_by_key(|row| (row.occurred_at, row.event_id));

        for row in ordered {
            self.folded.insert(row.event_id);
            match row.kind.as_str() {
                IDENTIFY => self.take_identify(row),
                ALIAS => self.take_alias(row),
                GROUP => self.take_group(row),
                _ => {}
            }
        }
    }

    /// What this graph is complete up to.
    pub fn covered_before(&self) -> i64 {
        self.covered_before
    }

    /// Say that everything before `at` is folded in.
    pub fn covering_before(mut self, at: i64) -> Identity {
        self.covered_before = at;
        self
    }

    /// The graph as it was before `at`.
    ///
    /// **A query's answer must not depend on how fresh a cache is.** A
    /// materialised graph holds every identity row the installation has, and a
    /// query over a range that ended last week has to answer with what was
    /// known then — the same answer it gave before the graph was materialised,
    /// and the same answer it gives tomorrow. This is that bound, and it is
    /// applied by removing what a fold up to `at` would never have seen rather
    /// than by teaching every reader a horizon.
    pub fn bounded_to(&self, at: i64) -> Identity {
        let mut out = Identity {
            project_id: self.project_id,
            foreign_rows_ignored: self.foreign_rows_ignored,
            covered_before: self.covered_before.min(at),
            ..Identity::default()
        };
        for (anonymous, links) in &self.links {
            let kept: Vec<Link> = links.iter().filter(|link| link.at < at).cloned().collect();
            if !kept.is_empty() {
                out.links.insert(anonymous.clone(), kept);
            }
        }
        // The merges are re-applied in time order so that `merged_into` is what
        // the fold would have produced, and not what it was before the ones
        // this removed.
        out.merges = self
            .merges
            .iter()
            .filter(|merge| merge.at < at)
            .cloned()
            .collect();
        out.merges.sort_by_key(|merge| (merge.at, merge.event_id));
        for merge in &out.merges {
            out.merged_into
                .insert(merge.from_id.clone(), merge.to_id.clone());
        }
        for (who, memberships) in &self.memberships {
            let kept: Vec<Membership> = memberships
                .iter()
                .filter(|m| m.since < at)
                .cloned()
                .collect();
            if !kept.is_empty() {
                out.memberships.insert(who.clone(), kept);
            }
        }
        for (who, held) in &self.traits {
            let mut kept: BTreeMap<String, Vec<(PropertyValue, i64)>> = BTreeMap::new();
            for (key, history) in held {
                let inside: Vec<(PropertyValue, i64)> = history
                    .iter()
                    .filter(|(_, set_at)| *set_at < at)
                    .cloned()
                    .collect();
                if !inside.is_empty() {
                    kept.insert(key.clone(), inside);
                }
            }
            if !kept.is_empty() {
                out.traits.insert(who.clone(), kept);
            }
        }
        // The fold marks are kept whole. They say which rows this graph has
        // already seen, and a bounded copy has seen the same ones.
        out.folded = self.folded.clone();
        out
    }

    pub fn project_id(&self) -> [u8; 16] {
        self.project_id
    }

    pub fn foreign_rows_ignored(&self) -> usize {
        self.foreign_rows_ignored
    }

    pub fn links(&self) -> impl Iterator<Item = &Link> {
        self.links.values().flatten()
    }

    pub fn merges(&self) -> &[Merge] {
        &self.merges
    }

    fn take_identify(&mut self, row: &EventRow) {
        // The known identifier comes from the payload, and the anonymous one
        // from the envelope. An `identify` that names neither links nothing.
        let (Some(end_user_id), Some(anonymous_id)) =
            (text(row, END_USER_ID), text(row, ANONYMOUS_ID))
        else {
            return;
        };
        self.links
            .entry(anonymous_id.clone())
            .or_default()
            .push(Link {
                anonymous_id,
                end_user_id: end_user_id.clone(),
                at: row.occurred_at,
                event_id: row.event_id,
            });
        self.take_traits(&end_user_id, row);
    }

    fn take_alias(&mut self, row: &EventRow) {
        let (Some(from_id), Some(to_id)) = (text(row, "from_id"), text(row, "to_id")) else {
            return;
        };
        if from_id == to_id {
            return;
        }
        self.merges.push(Merge {
            from_id: from_id.clone(),
            to_id: to_id.clone(),
            at: row.occurred_at,
            event_id: row.event_id,
        });
        self.merged_into.insert(from_id, to_id);
    }

    fn take_group(&mut self, row: &EventRow) {
        let Some(group_id) = text(row, "group_id") else {
            return;
        };
        // A group association belongs to whoever the row named. An anonymous
        // one is kept under the anonymous identifier, and resolution finds it
        // again after an `identify`.
        let Some(who) = text(row, END_USER_ID).or_else(|| text(row, ANONYMOUS_ID)) else {
            return;
        };
        let membership = Membership {
            group_id,
            group_kind: text(row, "group_kind"),
            since: row.occurred_at,
        };
        let held = self.memberships.entry(who).or_default();
        if !held
            .iter()
            .any(|m| m.group_id == membership.group_id && m.since <= membership.since)
        {
            held.push(membership);
        }
    }

    /// Traits from one `identify`, latest wins.
    ///
    /// A trait is a property the producer sent beside the identity, so the
    /// identity fields themselves are not traits. `docs/DATA_MODEL.md` says a
    /// profile holds "only approved traits"; the approval list is collection
    /// policy and lives in [`crate::policy`], which filters before this sees a
    /// row.
    fn take_traits(&mut self, end_user_id: &str, row: &EventRow) {
        let held = self.traits.entry(end_user_id.to_string()).or_default();
        for (key, (value, _origin)) in &row.properties {
            if matches!(key.as_str(), END_USER_ID | ANONYMOUS_ID | "span_id") {
                continue;
            }
            let history = held.entry(key.clone()).or_default();
            history.push((value.clone(), row.occurred_at));
            history.sort_by_key(|(_, at)| *at);
        }
    }

    /// The value a trait held last, at or before the end of what is folded in.
    fn latest_trait(history: &[(PropertyValue, i64)]) -> Option<&(PropertyValue, i64)> {
        history.last()
    }

    /// The canonical identifier for one known end user, following alias edges.
    pub fn canonical(&self, end_user_id: &str) -> String {
        let mut at = end_user_id.to_string();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for _ in 0..MAX_ALIAS_DEPTH {
            if !seen.insert(at.clone()) {
                // A loop. Somebody sent `a -> b` and `b -> a`; keeping the
                // identifier we are on is stable and terminates, and the
                // merges are all still on the record for an operator to read.
                break;
            }
            match self.merged_into.get(&at) {
                Some(next) if next != &at => at = next.clone(),
                _ => break,
            }
        }
        at
    }

    /// Who one row belongs to.
    ///
    /// Returns `None` when the row carries no identity at all, which a
    /// server-side metric point or a self-observation row does.
    pub fn who(&self, row: &EventRow, resolution: Resolution) -> Option<String> {
        if row.project_id != self.project_id {
            // Two projects that both use `a1` are two different people.
            return None;
        }
        if let Some(known) = text(row, END_USER_ID) {
            return Some(self.canonical(&known));
        }
        let anonymous = text(row, ANONYMOUS_ID)?;
        let linked = self
            .links
            .get(&anonymous)
            .and_then(|links| match resolution {
                // From that point. A row before the `identify` stays anonymous,
                // which is what makes a sign-up funnel start where the person
                // started rather than where they signed in.
                Resolution::EventTime => links
                    .iter()
                    .filter(|link| link.at <= row.occurred_at)
                    .max_by_key(|link| link.at),
                Resolution::LatestKnown => links.iter().max_by_key(|link| link.at),
            });
        match linked {
            Some(link) => Some(self.canonical(&link.end_user_id)),
            // Still anonymous. The anonymous identifier is the correlation key,
            // so a funnel counts this person once rather than not at all.
            None => Some(anonymous),
        }
    }

    /// Whether one row belongs to a known end user rather than an anonymous one.
    pub fn is_known(&self, row: &EventRow, resolution: Resolution) -> bool {
        if text(row, END_USER_ID).is_some() {
            return true;
        }
        let Some(anonymous) = text(row, ANONYMOUS_ID) else {
            return false;
        };
        self.links
            .get(&anonymous)
            .is_some_and(|links| match resolution {
                Resolution::EventTime => links.iter().any(|link| link.at <= row.occurred_at),
                Resolution::LatestKnown => !links.is_empty(),
            })
    }

    /// The correlation key one operator uses for one row.
    ///
    /// `None` means this row cannot take part: a funnel by end user cannot
    /// count a row with no identity, and counting it under an empty key would
    /// merge every such row into one imaginary person.
    pub fn key_for(&self, row: &EventRow, basis: Basis, resolution: Resolution) -> Option<String> {
        match basis {
            Basis::EndUser => self.who(row, resolution),
            Basis::Session => row.session_id.clone(),
            Basis::Group => {
                let who = self.who(row, resolution)?;
                self.groups_of(&who, row.occurred_at)
                    .first()
                    .map(|m| m.group_id.clone())
            }
        }
    }

    /// The groups one end user belonged to at a moment.
    ///
    /// A membership begins when the `group` row said so and does not end: the
    /// contract has no leave payload, so claiming an end would be inventing
    /// one. `docs/DATA_MODEL.md` calls for validity time and this is the half
    /// the data supports.
    pub fn groups_of(&self, who: &str, at: i64) -> Vec<Membership> {
        let mut out: Vec<Membership> = self
            .memberships
            .get(who)
            .into_iter()
            .flatten()
            .filter(|m| m.since <= at)
            .cloned()
            .collect();
        // A membership under an anonymous identifier still counts once that
        // identifier is linked, because the same person joined the group.
        for (holder, memberships) in &self.memberships {
            if holder == who {
                continue;
            }
            let resolves_here = self
                .links
                .get(holder)
                .and_then(|links| links.iter().max_by_key(|link| link.at))
                .map(|link| self.canonical(&link.end_user_id))
                .is_some_and(|canonical| canonical == who);
            if resolves_here {
                out.extend(memberships.iter().filter(|m| m.since <= at).cloned());
            }
        }
        out.sort_by(|a, b| a.since.cmp(&b.since).then(a.group_id.cmp(&b.group_id)));
        out.dedup_by(|a, b| a.group_id == b.group_id);
        out
    }

    /// The approved traits one end user carries.
    pub fn traits_of(&self, who: &str) -> BTreeMap<String, PropertyValue> {
        let canonical = self.canonical(who);
        let mut out: BTreeMap<String, PropertyValue> = BTreeMap::new();
        for (holder, held) in &self.traits {
            if self.canonical(holder) != canonical {
                continue;
            }
            for (key, history) in held {
                let Some((value, at)) = Identity::latest_trait(history) else {
                    continue;
                };
                match out.get(key) {
                    // The latest wins, and "latest" is by the time the trait
                    // was set rather than by the order the rows were read.
                    Some(_) if !self.is_newer(&canonical, key, *at) => {}
                    _ => {
                        out.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        out
    }

    fn is_newer(&self, canonical: &str, key: &str, at: i64) -> bool {
        self.traits
            .iter()
            .filter(|(holder, _)| self.canonical(holder) == canonical)
            .filter_map(|(_, held)| held.get(key))
            .filter_map(|history| Identity::latest_trait(history))
            .all(|(_, held_at)| at >= *held_at)
    }

    /// Every identifier that is the same person as this one.
    ///
    /// An erasure needs it: removing a person means removing every timeline
    /// that resolved to them, and a person who signed in on three devices has
    /// three anonymous identifiers. See [`crate::erasure`].
    pub fn every_identifier_of(&self, who: &str) -> Vec<String> {
        let canonical = self.canonical(who);
        let mut out: BTreeSet<String> = BTreeSet::new();
        out.insert(who.to_string());
        out.insert(canonical.clone());
        for from in self.merged_into.keys() {
            if self.canonical(from) == canonical {
                out.insert(from.clone());
            }
        }
        for (anonymous, links) in &self.links {
            if links
                .iter()
                .any(|link| self.canonical(&link.end_user_id) == canonical)
            {
                out.insert(anonymous.clone());
                for link in links {
                    out.insert(link.end_user_id.clone());
                }
            }
        }
        out.into_iter().collect()
    }
}

/// A materialised identity graph, for each project.
///
/// # Why this exists
///
/// L103 built the graph from the rows and nothing else, and stated the cost it
/// paid for that: **a query that resolves identity scans its own range and
/// everything before it**, because an `identify` from last year is what makes
/// this month's anonymous events belong to somebody. The cost therefore grows
/// with the installation's age rather than with the question, which made it the
/// one deferred item that gets worse with no operator action.
///
/// # What it is, and what it is not
///
/// It is a **cache in front of the derivation**, which is exactly what L103 said
/// a materialisation would be. [`Identity::build`] is still the only thing that
/// builds a graph, so there is no second derivation to fall out of step with the
/// first — L102 is what happens when there is.
///
/// It is **not durable, on purpose**. `AGENTS.md` requires every derived
/// projection to be reproducible from retained raw data, and one that is only
/// ever derived is reproducible by construction rather than by discipline. A
/// restart costs the first identity query one rebuild, which is the cost every
/// identity query paid before this existed. L112's rule — control-plane state
/// must survive a restart — is about what a person **authored**, and a cache is
/// not that.
///
/// # An erasure empties it
///
/// The graph *is* the rows, so removing a person's rows removes them from the
/// graph. A cache in front of it would keep them, so the tombstone generation is
/// part of what a cached entry is valid for, and an erasure moves it. A cached
/// graph from before the erasure is never read again.
pub struct IdentityCache {
    held: std::sync::Mutex<BTreeMap<[u8; 16], Held>>,
    /// How long a materialised graph is used before it is built again. A rebuild
    /// is what picks up an identity row that arrived late, so this is also the
    /// longest a late `identify` can go unseen by a range that ends before it.
    refresh: std::time::Duration,
}

struct Held {
    graph: std::sync::Arc<Identity>,
    /// The erasure generation this was built under. A different one means a
    /// person has been removed and this graph still knows them.
    tombstone_generation: u64,
    built_at: i64,
    /// Whether the last read of this entry could use it.
    hits: u64,
    misses: u64,
}

/// What a caller has to fold on top of a materialised graph.
pub struct Materialised {
    pub graph: Identity,
    /// The moment the materialised part is complete up to. A caller folds the
    /// identity rows from here forwards, and that window is bounded by the
    /// refresh period rather than by the installation's age.
    pub from: i64,
    /// Whether a graph was already held. False means this call built one.
    pub reused: bool,
}

/// How long a materialised graph is used before it is built again.
///
/// It is also the longest an identity row that arrived late can go unseen by a
/// question about a range that ended before it. Five minutes is short next to
/// how long a person waits before asking the same question again, and long next
/// to a scan of the whole history.
pub const DEFAULT_IDENTITY_REFRESH: std::time::Duration = std::time::Duration::from_secs(300);

impl Default for IdentityCache {
    fn default() -> IdentityCache {
        IdentityCache::new(DEFAULT_IDENTITY_REFRESH)
    }
}

impl IdentityCache {
    pub fn new(refresh: std::time::Duration) -> IdentityCache {
        IdentityCache {
            held: std::sync::Mutex::new(BTreeMap::new()),
            refresh,
        }
    }

    /// The materialised graph for one project, bounded to `horizon`.
    ///
    /// `build` is called only when there is nothing usable held, and it is
    /// given the whole history to fold. `horizon` is the end of the caller's
    /// range: the answer must not depend on how fresh the cache is, so anything
    /// the graph knows about a later moment is removed before the caller sees
    /// it. See [`Identity::bounded_to`].
    pub fn graph<E>(
        &self,
        project_id: [u8; 16],
        tombstone_generation: u64,
        now: i64,
        horizon: i64,
        build: impl FnOnce() -> Result<Identity, E>,
    ) -> Result<Materialised, E> {
        let mut held = self.held.lock().expect("identity cache");
        let refresh_ms = self.refresh.as_millis() as i64;
        if let Some(entry) = held.get_mut(&project_id) {
            if entry.tombstone_generation == tombstone_generation
                && now.saturating_sub(entry.built_at) < refresh_ms
            {
                entry.hits += 1;
                let from = entry.graph.covered_before();
                return Ok(Materialised {
                    graph: entry.graph.bounded_to(horizon),
                    from,
                    reused: true,
                });
            }
        }
        drop(held);

        // Built outside the lock. A rebuild reads the store, and holding the
        // cache while it did would stop every other project's queries as well.
        let graph = build()?.covering_before(now);
        let graph = std::sync::Arc::new(graph);

        let mut held = self.held.lock().expect("identity cache");
        let entry = held.entry(project_id).or_insert_with(|| Held {
            graph: std::sync::Arc::clone(&graph),
            tombstone_generation,
            built_at: now,
            hits: 0,
            misses: 0,
        });
        entry.graph = std::sync::Arc::clone(&graph);
        entry.tombstone_generation = tombstone_generation;
        entry.built_at = now;
        entry.misses += 1;
        Ok(Materialised {
            graph: graph.bounded_to(horizon),
            from: graph.covered_before(),
            reused: false,
        })
    }

    /// How many reads used a held graph, and how many had to build one.
    pub fn counts(&self) -> (u64, u64) {
        let held = self.held.lock().expect("identity cache");
        held.values().fold((0, 0), |(hits, misses), entry| {
            (hits + entry.hits, misses + entry.misses)
        })
    }

    /// Forget everything. An operator command and a test both need it.
    pub fn clear(&self) {
        self.held.lock().expect("identity cache").clear();
    }
}

/// One text property of a row, if it has it.
pub fn text(row: &EventRow, key: &str) -> Option<String> {
    match row.properties.get(key) {
        Some((PropertyValue::Text(value), _)) if !value.is_empty() => Some(value.clone()),
        _ => None,
    }
}
