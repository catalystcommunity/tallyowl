//! The consensus layer: one algorithm, many small groups.
//!
//! D15 says use an existing implementation and never invent the algorithm. D27
//! measured openraft 0.9 against every criterion D15 named — election,
//! partition, rejoin, membership change, snapshot, large batches, multi-group
//! overhead, API stability, and license — and selected it. `docs/BENCHMARKS.md`
//! section 10 holds the numbers.
//!
//! # What TallyOwl supplies and what the library supplies
//!
//! `docs/STORAGE.md` section 7 draws the line: "An existing consensus
//! implementation supplies the algorithm. TallyOwl supplies the tablet state
//! machine, multiplexed transport, storage, placement integration, and
//! operational limits." This module is those four things:
//!
//! | Module | Supplies |
//! | --- | --- |
//! | [`machine`] | The state machines: the controller's topology, the directory, and a tablet |
//! | [`storage`] | A durable log and a durable applied state, on the same embedded engine the catalog uses |
//! | [`network`] | The multiplexed transport: many groups over shared CSIL connections |
//! | [`super::groups`] | The registry, the runtime, and the synchronous boundary the rest of TallyOwl calls across |
//!
//! # One type configuration, many state machines
//!
//! openraft binds one application request type to one configuration. TallyOwl
//! has three kinds of group and one transport, so the request type here is an
//! opaque encoded command and the state machine behind each group decodes it.
//! Three type configurations would have meant three transports, three
//! registries, and three of every operational limit, for three types that all
//! travel the same wire.
//!
//! # Why the node ID is a number
//!
//! openraft requires a `Copy` node ID and TallyOwl names its nodes with text,
//! so [`node_id`] derives a stable number from the name. The name travels
//! beside it in the node record, so an operator never reads the number.

use std::io::Cursor;

use openraft::{BasicNode, Entry};
use serde::{Deserialize, Serialize};

pub mod machine;
pub mod network;
pub mod storage;

/// The number openraft knows a node by.
pub type NodeId = u64;

/// Encode a `Vec<u8>` as a CBOR byte string rather than an array of integers.
///
/// **serde treats `Vec<u8>` as a sequence**, so a plain derive writes one CBOR
/// integer for each byte: major type 4 with a head for every element, which
/// costs two bytes for every byte above 23. A thousand bytes encode to 2,003.
/// Told that they are bytes, the same thousand encode to 1,003.
///
/// This is not a nicety. A replicated batch is hundreds of kilobytes and it
/// travels inside two of these — the batch inside the command, and the command
/// inside the log entry — so the derive doubled it twice. See L096.
pub mod byte_string {
    use serde::de::{Error, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        struct Bytes;

        impl<'de> Visitor<'de> for Bytes {
            type Value = Vec<u8>;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a byte string")
            }

            fn visit_bytes<E: Error>(self, value: &[u8]) -> Result<Vec<u8>, E> {
                Ok(value.to_vec())
            }

            fn visit_byte_buf<E: Error>(self, value: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(value)
            }

            // A log written before L096 holds an array of integers. Reading one
            // costs nothing and means an installation that upgrades does not
            // have to discard its log.
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(byte) = seq.next_element()? {
                    out.push(byte);
                }
                Ok(out)
            }
        }

        deserializer.deserialize_byte_buf(Bytes)
    }
}

/// One replicated command, already encoded by the group that owns its meaning.
///
/// The bytes are CBOR, which is what every other durable TallyOwl structure
/// uses. A group's state machine decodes them; consensus never looks inside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRequest {
    #[serde(with = "byte_string")]
    pub payload: Vec<u8>,
}

/// What applying one command produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupResponse {
    /// The log index this command was applied at. A tablet receipt reports it
    /// as the commit watermark, so a caller can ask a replica whether it has
    /// reached the write it just made.
    pub applied_index: u64,
    /// The state machine's own answer, encoded. Empty when there is none.
    pub outcome: Vec<u8>,
}

openraft::declare_raft_types!(
    /// The one type configuration. See the module note on why there is one.
    pub TypeConfig:
        D = GroupRequest,
        R = GroupResponse,
        NodeId = NodeId,
        Node = BasicNode,
        Entry = Entry<TypeConfig>,
        SnapshotData = Cursor<Vec<u8>>,
        AsyncRuntime = openraft::TokioRuntime,
);

/// A running consensus group.
pub type RaftHandle = openraft::Raft<TypeConfig>;

/// The number for one node name.
///
/// FNV-1a, so it is stable across releases and languages. A collision would put
/// two nodes under one identity, which consensus could not tell apart, so
/// [`crate::groups::GroupRegistry`] refuses a second name that lands on a
/// number it already holds rather than letting the two merge.
pub fn node_id(name: &str) -> NodeId {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in name.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    // Zero is reserved so that a default-constructed ID is never a real node.
    if hash == 0 {
        1
    } else {
        hash
    }
}

/// Encode a value as the CBOR a replicated command travels as.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes)
        .map_err(|e| format!("A replicated command could not be encoded: {e}"))?;
    Ok(bytes)
}

/// Decode a value from the CBOR a replicated command travels as.
pub fn decode<T: for<'a> Deserialize<'a>>(bytes: &[u8]) -> Result<T, String> {
    ciborium::from_reader(bytes).map_err(|e| format!("A replicated command could not be read: {e}"))
}

/// How many committed entries pass before a group takes a snapshot.
///
/// **This number is what bounds the consensus log on disk**, and until L087 it
/// could not be lowered safely: a tablet snapshot carries the marks and not the
/// rows, so purging the log took the only copy of entries a lagging replica
/// still needed. [`crate::transfer`] gives that replica the other copy, and
/// [`machine::TabletMachine::before_snapshot`] seals the store first so that
/// everything a snapshot covers is in a segment the copy can carry.
///
/// It is not lower than this because a tablet snapshot seals, and a seal is
/// what makes a segment. A snapshot every few hundred entries would make many
/// small segments, and the catalog — 3.4 times the size of the data it indexes
/// — is what pays for those. `docs/BENCHMARKS.md` section 18 measured about 173
/// events for each batch, so 4,096 entries is roughly 710,000 rows, which is
/// near the 800,000 the store's own sealing policy targets. The two policies
/// therefore agree rather than fighting.
pub const SNAPSHOT_EVERY_ENTRIES: u64 = 4096;

/// How much log stays after a snapshot.
///
/// Enough that an ordinary restart, and a follower that fell a little behind,
/// catch up from the log rather than transferring a snapshot and then segments.
/// With the number above this bounds a group's log at about 4,608 entries
/// rather than at the number of batches the installation has ever taken.
pub const KEEP_AFTER_SNAPSHOT: u64 = 512;

/// The consensus timings TallyOwl runs with.
///
/// The election window is deliberately wider than the heartbeat by a large
/// margin. D27 measured 985 ms to replace an isolated leader with these, which
/// is the number `docs/FAILURE_MODES.md` section 6 records.
pub fn config() -> std::sync::Arc<openraft::Config> {
    config_with(SNAPSHOT_EVERY_ENTRIES, KEEP_AFTER_SNAPSHOT)
}

/// The same, with the two log bounds an operator can set.
///
/// L099 asked for these to stop being constants, and the reason is that the one
/// number that fits every installation does not exist here: the constant is a
/// trade between how many small segments a snapshot makes and how much log a
/// tablet keeps on disk. The defaults above are the measured ones and stay the
/// defaults. `replication.snapshotEvery` and `replication.keepAfterSnapshot` are
/// the settings.
///
/// **Both are floored at one.** openraft refuses a zero snapshot policy, and a
/// configuration mistake must not stop a node from starting.
pub fn config_with(
    snapshot_every: u64,
    keep_after_snapshot: u64,
) -> std::sync::Arc<openraft::Config> {
    std::sync::Arc::new(
        openraft::Config {
            heartbeat_interval: 150,
            election_timeout_min: 300,
            election_timeout_max: 600,
            snapshot_policy: openraft::SnapshotPolicy::LogsSinceLast(snapshot_every.max(1)),
            max_in_snapshot_log_to_keep: keep_after_snapshot,
            ..Default::default()
        }
        .validate()
        .expect("the consensus configuration is valid"),
    )
}
