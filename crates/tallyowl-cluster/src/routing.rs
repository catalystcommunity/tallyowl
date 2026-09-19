//! Where a write belongs, and what happens when the answer is out of date.
//!
//! `docs/CELLS.md` section 5 gives the whole chain:
//!
//! ```text
//! project -> cell -> virtual shard -> tablet -> leader
//! ```
//!
//! The first hop is the global directory and the rest is one cell's controller
//! quorum. This module owns the middle: the shard a value lands in, the tablet
//! that owns the shard, and the cache a gateway keeps so it does not ask again
//! for every batch.
//!
//! # The shard is derived, never assigned
//!
//! A virtual shard comes from tenancy and an affinity key, and nothing else.
//! The same project and the same trace ID always land in the same shard, on
//! every node, for the life of the installation. That is what lets a tablet
//! move without moving a shard, and it is why [`VIRTUAL_SHARDS`] is fixed.
//!
//! The affinity key depends on the telemetry type, and `docs/STORAGE.md`
//! section 6 states which: a span uses its trace ID, a behavior event uses its
//! session or end-user ID, a metric point uses its series ID, and everything
//! else uses a stable event key. [`affinity_key`] is that rule in one place, so
//! the ingest gateway and the query planner cannot disagree about it.
//!
//! # The generation is a fence
//!
//! A cached route carries the generation it was learned at. A leader refuses a
//! write that carries an older generation and says what the current one is, so
//! a gateway that missed a movement is redirected on the next attempt instead
//! of writing into a tablet that no longer owns the data. A cache with no
//! generation would be a correctness problem rather than a latency one.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::topology::{
    Epoch, Generation, NodeName, ReceiptPolicy, TabletName, Topology, VirtualShard, VIRTUAL_SHARDS,
};

/// Which fact decides where a row goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    /// A span, keyed by its trace. One tablet therefore holds a whole trace, so
    /// assembling one never fans out. See D16.
    Trace,
    /// A behavior event, keyed by session or end user.
    Session,
    /// A metric point, keyed by its series.
    Series,
    /// Anything else, keyed by a stable event key.
    Event,
}

/// The affinity key for one item, chosen by the rule in `docs/STORAGE.md`
/// section 6.
///
/// Each argument is the value if the item has one. The order is the order the
/// rule states, so a span with a trace never falls through to its event ID.
pub fn affinity_key<'a>(
    affinity: Affinity,
    trace_id: Option<&'a [u8]>,
    session_or_user: Option<&'a [u8]>,
    series_id: Option<&'a [u8]>,
    event_id: &'a [u8],
) -> &'a [u8] {
    match affinity {
        Affinity::Trace => trace_id.unwrap_or(event_id),
        Affinity::Session => session_or_user.unwrap_or(event_id),
        Affinity::Series => series_id.unwrap_or(event_id),
        Affinity::Event => event_id,
    }
}

/// The virtual shard for one project and affinity key.
///
/// FNV-1a over the project and then the key. The hash needs to spread and to be
/// stable across languages and releases; it does not need to resist an
/// adversary, because a client never chooses its own shard and a collision only
/// puts two keys in one bucket that already holds many.
pub fn shard_of(project_id: &[u8], affinity_key: &[u8]) -> VirtualShard {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in project_id.iter().chain(affinity_key.iter()) {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash % VIRTUAL_SHARDS
}

/// One answer to "where does this write go".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub cell: String,
    pub shard: VirtualShard,
    pub tablet: TabletName,
    pub leader: Option<NodeName>,
    pub leader_address: Option<String>,
    pub generation: Generation,
    pub epoch: Epoch,
    pub receipt_policy: ReceiptPolicy,
    pub write_region: String,
}

/// Why a route could not be given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    /// No tablet owns the shard. A cluster mid-bootstrap looks like this.
    NoOwner(String),
    /// A tablet owns it and is not accepting writes.
    NotWritable(String),
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::NoOwner(m) | RouteError::NotWritable(m) => f.write_str(m),
        }
    }
}

/// Resolve a write route from a topology.
pub fn route_write(
    topology: &Topology,
    project_id: &[u8],
    affinity_key: &[u8],
    leader_of: impl Fn(&str) -> Option<NodeName>,
) -> Result<Route, RouteError> {
    let shard = shard_of(project_id, affinity_key);
    let tablet = topology.owner_of(shard).ok_or_else(|| {
        RouteError::NoOwner(format!(
            "No tablet owns virtual shard {shard} yet. The cell has no placement for it."
        ))
    })?;
    if !tablet.writable() {
        return Err(RouteError::NotWritable(format!(
            "`{}` owns virtual shard {shard} and is not accepting writes right now.",
            tablet.name
        )));
    }
    let leader = leader_of(&tablet.name);
    let leader_address = leader
        .as_ref()
        .and_then(|node| tablet.member(node))
        .map(|m| m.address.clone());
    Ok(Route {
        cell: tablet.cell.clone(),
        shard,
        tablet: tablet.name.clone(),
        leader,
        leader_address,
        generation: topology.generation,
        epoch: tablet.epoch,
        receipt_policy: tablet.receipt_policy,
        write_region: tablet.write_region.clone(),
    })
}

/// What a leader answers a write that carried a stale route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fence {
    /// The route is current. Proceed.
    Current,
    /// The placement moved. Ask again and retry; the answer names the current
    /// generation so the caller does not have to guess how far behind it is.
    Redirect {
        current_generation: Generation,
        reason: String,
    },
    /// The write region moved. This one is not a retry against the same tablet:
    /// another region owns the write now.
    Fenced {
        current_epoch: Epoch,
        write_region: String,
        reason: String,
    },
}

/// Check one incoming write against the placement the receiver believes.
///
/// Two different refusals, because they need two different answers. A stale
/// generation means "you have an old map"; a stale epoch means "this is not
/// your region any more", and a caller that treated the second as the first
/// would retry into a fence forever.
pub fn check_fence(
    topology: &Topology,
    tablet: &str,
    carried_generation: Generation,
    carried_epoch: Epoch,
    writer_region: Option<&str>,
) -> Fence {
    let Some(found) = topology.tablet(tablet) else {
        return Fence::Redirect {
            current_generation: topology.generation,
            reason: format!("`{tablet}` is not a tablet this controller knows."),
        };
    };
    if carried_epoch < found.epoch {
        return Fence::Fenced {
            current_epoch: found.epoch,
            write_region: found.write_region.clone(),
            reason: format!(
                "`{tablet}` moved its write region to `{}` at epoch {}. A write from epoch {carried_epoch} is refused rather than accepted beside the new region.",
                found.write_region, found.epoch
            ),
        };
    }
    if let Some(region) = writer_region {
        if region != found.write_region {
            return Fence::Fenced {
                current_epoch: found.epoch,
                write_region: found.write_region.clone(),
                reason: format!(
                    "`{tablet}` accepts writes in `{}` and this write came from `{region}`.",
                    found.write_region
                ),
            };
        }
    }
    if carried_generation < topology.generation && carried_generation != 0 {
        // A generation behind is only a redirect when the placement of *this*
        // tablet actually moved. The generation rises for every change in the
        // cell, so treating any older number as a redirect would bounce every
        // gateway on every unrelated change.
        if !found.writable() {
            return Fence::Redirect {
                current_generation: topology.generation,
                reason: format!("`{tablet}` no longer accepts writes."),
            };
        }
    }
    Fence::Current
}

/// A gateway's cached view of routing.
///
/// It holds one entry for each shard it has used, and it drops the whole cache
/// when the generation moves. Dropping everything is deliberate: a change that
/// moved one tablet may have moved several, and a cache that expired only what
/// it was told about would keep a stale entry for a shard nobody happened to
/// mention.
pub struct RouteCache {
    generation: Mutex<Generation>,
    entries: Mutex<HashMap<VirtualShard, Route>>,
}

impl Default for RouteCache {
    fn default() -> RouteCache {
        RouteCache::new()
    }
}

impl RouteCache {
    pub fn new() -> RouteCache {
        RouteCache {
            generation: Mutex::new(0),
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, shard: VirtualShard) -> Option<Route> {
        self.entries
            .lock()
            .expect("route cache")
            .get(&shard)
            .cloned()
    }

    pub fn put(&self, route: Route) {
        let mut generation = self.generation.lock().expect("route cache");
        let mut entries = self.entries.lock().expect("route cache");
        if route.generation > *generation {
            entries.clear();
            *generation = route.generation;
        }
        if route.generation >= *generation {
            entries.insert(route.shard, route);
        }
    }

    /// Forget everything learned before `generation`.
    pub fn invalidate_before(&self, generation: Generation) {
        let mut held = self.generation.lock().expect("route cache");
        if generation > *held {
            *held = generation;
            self.entries.lock().expect("route cache").clear();
        }
    }

    pub fn generation(&self) -> Generation {
        *self.generation.lock().expect("route cache")
    }

    pub fn len(&self) -> usize {
        self.entries.lock().expect("route cache").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
