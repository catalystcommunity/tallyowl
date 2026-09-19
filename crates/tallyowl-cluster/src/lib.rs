//! `tallyowl-cluster`: replicated storage.
//!
//! Phase 7 of `docs/PLAN.md`. A home installation never enters this phase: it
//! has one tablet, one voter, and no controller quorum, and every module here
//! is either unused or degenerate at that size. That is the point. The protocol
//! and the segment format do not change between one node and ten thousand, so
//! this crate adds placement, consensus, and fan-out and changes nothing that
//! an application or a stored byte can see.
//!
//! # What is here
//!
//! | Module | Holds |
//! | --- | --- |
//! | [`topology`] | What the cell controller quorum knows, and the only commands that change it |
//! | [`directory`] | The small global project-to-cell directory, and the cached copy that keeps a cell working when it is gone |
//! | [`routing`] | Virtual shard, tablet, leader, and the fence that refuses a stale route |
//! | [`raft`] | The consensus layer: the state machines, the durable log, and the multiplexed transport |
//! | [`groups`] | Many groups on one node, and the one place threads meet the async runtime |
//! | [`health`] | The node that is alive but slow, and the voter whose disk is full |
//! | [`controller`] | The cell controller: placement, split, merge, movement, and failover |
//! | [`replicated`] | A [`tallyowl_store::Store`] that commits through a tablet group |
//! | [`query`] | Distributed planning, partial aggregation, and partial-result semantics |
//! | [`fanout`] | Asking every readable tablet and merging, as a [`tallyowl_store::Store`] the head reads through unchanged |
//! | [`movement`] | Moving a tablet online, with parity checked before placement changes |
//! | [`transfer`] | Copying sealed segments between nodes, which is how a replica catches up behind a purged log |
//! | [`recovery`] | Snapshot, bootstrap, restore, and the two answers to a permanent quorum loss |
//! | [`plane`] | One way in for a topology change, with or without a quorum |
//! | [`service`], [`clusterservice`] | The two CSIL services this crate answers |
//! | [`simulate`] | The scale simulations, which `docs/PLAN.md` calls a milestone and not a gate |
//!
//! # The three rules this crate exists to hold
//!
//! **There is no cluster-wide consensus group.** A small controller quorum
//! holds topology, and each tablet has its own small replica group. `AGENTS.md`
//! states it and [`groups`] holds it: a node starts a group when the controller
//! places it there and not before.
//!
//! **An uncommitted entry is never acknowledged in a multi-voter group.**
//! `local-one` is legal only on a tablet with one voter, and
//! [`topology::ReceiptPolicy`] refuses the combination rather than quietly
//! choosing another policy.
//!
//! **A result that is missing a tablet says so.** A partial result is never the
//! default, it names what is missing, and it can never mark itself complete.
//! `docs/QUERY.md` section 9, held in [`query`].

pub mod clusterservice;
pub mod controller;
pub mod directory;
pub mod fanout;
pub mod groups;
pub mod health;
pub mod movement;
pub mod plane;
pub mod query;
pub mod raft;
pub mod recovery;
pub mod replicated;
pub mod routing;
pub mod service;
pub mod simulate;
pub mod topology;
pub mod transfer;

#[cfg(test)]
mod tests;

pub use groups::{GroupKey, GroupRegistry};
pub use topology::{
    ControllerCommand, Generation, Member, MemberRole, ReceiptPolicy, Tablet, TabletState, Topology,
};
