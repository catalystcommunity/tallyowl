//! `tallyowl-store`: the native store, behind its own API contract.
//!
//! D25 makes this a separate module with a versioned API contract, and says why:
//! the reason for the boundary is discipline, not reuse. Telemetry code is a
//! consumer of this contract and does not reach around it.
//!
//! # What this holds
//!
//! | Module | Holds |
//! | --- | --- |
//! | [`store`] | The contract. Phase 3 replaced the implementation behind it and did not move it, which is what Phase 1 said would happen |
//! | [`segmented`] | The implementation: the append log, the catalog, and immutable segments |
//! | [`wal`] | The checksummed framed append log, with bounded group commit at the linger D47 measured |
//! | [`catalog`] | The embedded transactional catalog, and the erasure ledger that is durable independently of it |
//! | [`segment`] | The on-disk format `docs/SEGMENT_FORMAT.md` makes normative |
//! | [`cbor`] | Canonical CBOR, for the parts of a segment that describe themselves |
//! | [`row`], [`row_codec`] | The stored shape of one item, and its append-log encoding |
//!
//! Two properties held in Phase 1 and still hold, because the tests assert
//! them against this implementation rather than against the one they were
//! written for:
//!
//! - a commit that returned survives an abrupt process kill;
//! - a repeated commit of one batch ID gives one logical commit.
//!
//! # What is not built yet
//!
//! The tablet locator, compaction, the cold tier, and the Parquet exporter.
//! `docs/IMPLEMENTATION_LOG.md` L022 says where each one goes.

pub mod catalog;
pub mod cbor;
pub mod certificates;
pub mod compact;
pub mod control;
pub mod identity;
pub mod keys;
pub mod locator;
pub mod metrics;
pub mod row;
pub mod row_codec;
pub mod segment;
pub mod segmented;
pub mod snapshot;
pub mod space;
pub mod store;
pub mod tier;
pub mod wal;

pub use row::{EventRow, PropertyValue};
pub use segmented::{Sealing, SegmentedStore};
pub use store::{CommitOutcome, Receipt, Scanned, Store, StoreError, TimeBasis, Trend};
