//! Generated types from CSIL specification

#![allow(non_camel_case_types, clippy::large_enum_variant)]

/// Exact base-10 decimal carried on the wire as a CBOR tag-4 decimal fraction:
/// the two-element array `[exponent, mantissa]`, value = mantissa * 10^exponent.
/// Stored as that exact pair so no precision is lost. Convert to/from a decimal
/// library (e.g. `rust_decimal::Decimal`) through `as_str` / `from_str`, which is
/// why this type needs no such dependency of its own.
///
/// Equality and ordering are by *value*, not by representation: `(-2, 0)` ("0.00")
/// and `(-1, 0)` ("0.0") are equal, and the comparison honours the exact base-10
/// magnitude after normalizing differing exponents. This keeps `.eq`/`.ne` and the
/// `.ge/.le/.gt/.lt` validation bounds correct regardless of how a value was scaled.
///
/// On the wire it encodes as a CBOR **tag 4** decimal fraction — the two-element
/// array `[exponent, mantissa]` — so it interoperates byte-for-byte with the Go,
/// TypeScript, and Python generators. The generated `codec.gen.rs` owns that wire
/// form directly (reading/writing `exponent` and `mantissa` against its own value
/// model), so this type carries no serde impl and the crate needs no CBOR library.
#[derive(Debug, Clone, Copy)]
pub struct CsilDecimal {
    pub exponent: i64,
    pub mantissa: i128,
}

impl PartialEq for CsilDecimal {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for CsilDecimal {}

impl PartialOrd for CsilDecimal {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CsilDecimal {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let sign_a = self.mantissa.signum();
        let sign_b = other.mantissa.signum();
        if sign_a != sign_b {
            return sign_a.cmp(&sign_b);
        }
        if sign_a == 0 {
            return Ordering::Equal;
        }
        // Same nonzero sign: compare absolute magnitudes, then flip for negatives.
        let magnitude = Self::cmp_magnitude(
            self.mantissa.unsigned_abs(),
            self.exponent,
            other.mantissa.unsigned_abs(),
            other.exponent,
        );
        if sign_a < 0 {
            magnitude.reverse()
        } else {
            magnitude
        }
    }
}

impl CsilDecimal {
    /// Order two positive magnitudes `ma * 10^ea` and `mb * 10^eb` without ever
    /// scaling a mantissa (so no overflow): after stripping trailing zeros each
    /// value lands in the decade `[10^(w-1), 10^w)` for `w = digits + exponent`, so
    /// differing weights settle the order outright and equal weights reduce to a
    /// trailing-zero-padded digit-string compare.
    fn cmp_magnitude(mut ma: u128, mut ea: i64, mut mb: u128, mut eb: i64) -> std::cmp::Ordering {
        while ma.is_multiple_of(10) {
            ma /= 10;
            ea += 1;
        }
        while mb.is_multiple_of(10) {
            mb /= 10;
            eb += 1;
        }
        let digits_a = ma.to_string();
        let digits_b = mb.to_string();
        let weight_a = digits_a.len() as i64 + ea;
        let weight_b = digits_b.len() as i64 + eb;
        if weight_a != weight_b {
            return weight_a.cmp(&weight_b);
        }
        let width = digits_a.len().max(digits_b.len());
        let padded_a = format!("{digits_a:0<width$}");
        let padded_b = format!("{digits_b:0<width$}");
        padded_a.cmp(&padded_b)
    }
}

impl From<CsilDecimal> for (i64, i128) {
    fn from(d: CsilDecimal) -> Self {
        (d.exponent, d.mantissa)
    }
}

impl From<(i64, i128)> for CsilDecimal {
    fn from((exponent, mantissa): (i64, i128)) -> Self {
        Self { exponent, mantissa }
    }
}

#[allow(clippy::should_implement_trait, clippy::wrong_self_convention)]
impl CsilDecimal {
    pub fn new(exponent: i64, mantissa: i128) -> Self {
        Self { exponent, mantissa }
    }

    /// Canonical decimal string for the exact value: `(-2, 12345)` renders as
    /// `123.45`. Round-trips through `from_str` by value.
    pub fn as_str(&self) -> String {
        let digits = self.mantissa.unsigned_abs().to_string();
        let body = if self.exponent >= 0 {
            let mut out = digits;
            out.push_str(&"0".repeat(self.exponent as usize));
            out
        } else {
            let scale = (-self.exponent) as usize;
            if digits.len() > scale {
                let point = digits.len() - scale;
                format!("{}.{}", &digits[..point], &digits[point..])
            } else {
                let pad = "0".repeat(scale - digits.len());
                format!("0.{pad}{digits}")
            }
        };
        if self.mantissa < 0 {
            format!("-{body}")
        } else {
            body
        }
    }

    /// Parse a decimal string into the exact `(exponent, mantissa)` pair.
    pub fn from_str(s: &str) -> Result<Self, String> {
        let t = s.trim();
        let (negative, rest) = match t.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, t.strip_prefix('+').unwrap_or(t)),
        };
        let (int_part, frac_part) = match rest.split_once('.') {
            Some((i, f)) => (i, f),
            None => (rest, ""),
        };
        if frac_part.contains('.') || (int_part.is_empty() && frac_part.is_empty()) {
            return Err(format!("invalid decimal: {s}"));
        }
        let mut digits = String::with_capacity(int_part.len() + frac_part.len());
        digits.push_str(int_part);
        digits.push_str(frac_part);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("invalid decimal: {s}"));
        }
        let magnitude: i128 = digits
            .parse()
            .map_err(|e| format!("decimal out of range: {e}"))?;
        let mantissa = if negative { -magnitude } else { magnitude };
        Ok(Self {
            exponent: -(frac_part.len() as i64),
            mantissa,
        })
    }
}

impl std::fmt::Display for CsilDecimal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_str())
    }
}

/// Returned by a generated `validate` method when a field violates one of its
/// CSIL constraints. `field` names the offending field; `message` explains.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "validation failed for `{}`: {}",
            self.field, self.message
        )
    }
}

impl std::error::Error for ValidationError {}

pub type NodeName = String;

pub type CellName = String;

pub type RegionName = String;

pub type TabletName = String;

pub type DomainName = String;

pub type VirtualShard = u64;

pub type Generation = u64;

pub type Epoch = u64;

/// GroupKind variants
#[derive(Debug, Clone, PartialEq)]
pub enum GroupKind {
    GlobalDirectory,
    CellController,
    Tablet,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupRef {
    pub kind: GroupKind,
    /// constraint: size in 1..=64
    pub name: Option<String>,
}

impl GroupRef {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.name {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "name".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// MemberRole variants
#[derive(Debug, Clone, PartialEq)]
pub enum MemberRole {
    Voter,
    Learner,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupMember {
    pub node: NodeName,
    pub role: MemberRole,
    /// constraint: size in 1..=256
    pub address: String,
    pub region: Option<RegionName>,
    pub domain: Option<DomainName>,
}

impl GroupMember {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.address;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "address".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// ConsensusKind variants
#[derive(Debug, Clone, PartialEq)]
pub enum ConsensusKind {
    AppendEntries,
    Vote,
    InstallSnapshot,
    Proposal,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConsensusMessage {
    pub group: GroupRef,
    pub kind: ConsensusKind,
    pub sender: NodeName,
    pub generation: Generation,
    /// constraint: size in 1..=67108864
    pub payload: Vec<u8>,
}

impl ConsensusMessage {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.payload;
            if v.is_empty() || v.len() > 67108864usize {
                return Err(ValidationError {
                    field: "payload".to_string(),
                    message: "length must be in 1..=67108864".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConsensusReply {
    pub accepted: bool,
    /// constraint: size in 1..=67108864
    pub payload: Option<Vec<u8>>,
    pub current_generation: Option<Generation>,
    /// constraint: size in 1..=512
    pub refusal: Option<String>,
}

impl ConsensusReply {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.payload {
            if v.is_empty() || v.len() > 67108864usize {
                return Err(ValidationError {
                    field: "payload".to_string(),
                    message: "length must be in 1..=67108864".to_string(),
                });
            }
        }
        if let Some(v) = &self.refusal {
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "refusal".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotChunkRequest {
    pub group: GroupRef,
    /// constraint: size in 1..=128
    pub snapshot_id: Option<String>,
    pub offset: u64,
    pub max_bytes: u64,
}

impl SnapshotChunkRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.snapshot_id {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "snapshot_id".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotChunk {
    /// constraint: size in 1..=128
    pub snapshot_id: String,
    pub offset: u64,
    /// constraint: size in 0..=67108864
    pub data: Vec<u8>,
    pub total_bytes: u64,
    pub last: bool,
    /// constraint: size in 32..=32
    pub whole_digest: Option<Vec<u8>>,
}

impl SnapshotChunk {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.snapshot_id;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "snapshot_id".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        {
            let v = &self.data;
            if v.len() > 67108864usize {
                return Err(ValidationError {
                    field: "data".to_string(),
                    message: "length must be in 0..=67108864".to_string(),
                });
            }
        }
        if let Some(v) = &self.whole_digest {
            if v.len() < 32usize || v.len() > 32usize {
                return Err(ValidationError {
                    field: "whole_digest".to_string(),
                    message: "length must be in 32..=32".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentListRequest {
    pub tablet: TabletName,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentSummary {
    /// constraint: size in 1..=128
    pub segment_id: String,
    pub total_bytes: u64,
    /// constraint: size in 32..=32
    pub digest: Vec<u8>,
    pub row_count: u64,
    pub occurred_start: Timestamp,
    pub occurred_end: Timestamp,
}

impl SegmentSummary {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.segment_id;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "segment_id".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        {
            let v = &self.digest;
            if v.len() < 32usize || v.len() > 32usize {
                return Err(ValidationError {
                    field: "digest".to_string(),
                    message: "length must be in 32..=32".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentList {
    pub tablet: TabletName,
    pub segments: Vec<SegmentSummary>,
    pub generation: Generation,
    pub applied_index: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentTransferRequest {
    pub tablet: TabletName,
    /// constraint: size in 1..=128
    pub segment_id: String,
    pub offset: u64,
    pub max_bytes: u64,
}

impl SegmentTransferRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.segment_id;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "segment_id".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentTransfer {
    /// constraint: size in 1..=128
    pub segment_id: String,
    pub offset: u64,
    /// constraint: size in 0..=67108864
    pub data: Vec<u8>,
    pub total_bytes: u64,
    pub last: bool,
    /// constraint: size in 32..=32
    pub whole_digest: Option<Vec<u8>>,
}

impl SegmentTransfer {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.segment_id;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "segment_id".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        {
            let v = &self.data;
            if v.len() > 67108864usize {
                return Err(ValidationError {
                    field: "data".to_string(),
                    message: "length must be in 0..=67108864".to_string(),
                });
            }
        }
        if let Some(v) = &self.whole_digest {
            if v.len() < 32usize || v.len() > 32usize {
                return Err(ValidationError {
                    field: "whole_digest".to_string(),
                    message: "length must be in 32..=32".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// PartialKind variants
#[derive(Debug, Clone, PartialEq)]
pub enum PartialKind {
    Count,
    Trend,
    Rows,
    Lookup,
    Aggregate,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PartialQueryRequest {
    pub tablet: TabletName,
    pub generation: Generation,
    pub kind: PartialKind,
    pub project_id: ProjectId,
    pub range_start: Timestamp,
    pub range_end: Timestamp,
    /// constraint: size in 1..=32
    pub basis: String,
    pub bucket_ms: Option<DurationMs>,
    /// constraint: size in 1..=128
    pub event_name: Option<String>,
    /// constraint: size in 1..=128
    pub column: Option<String>,
    /// constraint: size in 1..=1024
    pub value: Option<Vec<u8>>,
    pub max_rows: Option<u64>,
    /// constraint: size in 1..=1048576
    pub aggregate_plan: Option<Vec<u8>>,
    pub require_watermark: Option<u64>,
    pub max_staleness_ms: Option<DurationMs>,
}

impl PartialQueryRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.basis;
            if v.is_empty() || v.len() > 32usize {
                return Err(ValidationError {
                    field: "basis".to_string(),
                    message: "length must be in 1..=32".to_string(),
                });
            }
        }
        if let Some(v) = &self.event_name {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "event_name".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.column {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "column".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.value {
            if v.is_empty() || v.len() > 1024usize {
                return Err(ValidationError {
                    field: "value".to_string(),
                    message: "length must be in 1..=1024".to_string(),
                });
            }
        }
        if let Some(v) = &self.aggregate_plan {
            if v.is_empty() || v.len() > 1048576usize {
                return Err(ValidationError {
                    field: "aggregate_plan".to_string(),
                    message: "length must be in 1..=1048576".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrendBucket {
    pub bucket_start: Timestamp,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PartialQueryResponse {
    pub tablet: TabletName,
    pub commit_watermark: u64,
    pub freshness_ms: DurationMs,
    pub complete: bool,
    pub missing: Option<Vec<MissingRange>>,
    pub count: u64,
    pub buckets: Option<Vec<TrendBucket>>,
    pub rows: Option<Vec<Vec<u8>>>,
    pub scanned_segments: u64,
    pub scanned_bytes: u64,
    /// constraint: size in 1..=16777216
    pub aggregate_state: Option<Vec<u8>>,
    pub degraded: Option<bool>,
}

impl PartialQueryResponse {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.aggregate_state {
            if v.is_empty() || v.len() > 16777216usize {
                return Err(ValidationError {
                    field: "aggregate_state".to_string(),
                    message: "length must be in 1..=16777216".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MissingRange {
    /// constraint: size in 1..=64
    pub tablet_id: String,
    pub range_start: Timestamp,
    pub range_end: Timestamp,
}

impl MissingRange {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.tablet_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "tablet_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// SlowCause variants
#[derive(Debug, Clone, PartialEq)]
pub enum SlowCause {
    StorageErrors,
    StorageSaturated,
    StorageSlow,
    WriteVolume,
    CompactionPressure,
    MemoryPressure,
    NetworkLatency,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeHealthReport {
    pub node: NodeName,
    pub reported_at: Timestamp,
    pub append_latency_us: u64,
    pub fsync_latency_us: u64,
    pub queue_depth: u64,
    pub accepted_bytes_each_second: u64,
    pub device_errors: u64,
    pub device_service_time_us: u64,
    pub compaction_backlog_bytes: u64,
    pub memory_reclaim_events: u64,
    pub peer_round_trip_us: u64,
    pub self_cause: Option<SlowCause>,
    pub writable: bool,
    pub free_bytes: u64,
}

/// NodeState variants
#[derive(Debug, Clone, PartialEq)]
pub enum NodeState {
    Healthy,
    Slow,
    Unreachable,
    ReadOnly,
    Draining,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeCondition {
    pub node: NodeName,
    pub state: NodeState,
    pub cause: Option<SlowCause>,
    pub append_latency_us: u64,
    pub group_median_append_latency_us: u64,
    pub fsync_latency_us: u64,
    pub group_median_fsync_latency_us: u64,
    pub queue_depth: u64,
    pub accepted_bytes_each_second: u64,
    pub since: Timestamp,
    /// constraint: size in 1..=256
    pub action_taken: Option<String>,
}

impl NodeCondition {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.action_taken {
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "action_taken".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// TabletState variants
#[derive(Debug, Clone, PartialEq)]
pub enum TabletState {
    Active,
    Splitting,
    Merging,
    Moving,
    Draining,
    Retired,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TabletInfo {
    pub tablet: TabletName,
    pub cell: CellName,
    pub write_region: RegionName,
    pub epoch: Epoch,
    pub generation: Generation,
    pub state: TabletState,
    pub shard_start: VirtualShard,
    pub shard_end: VirtualShard,
    pub members: Vec<GroupMember>,
    /// constraint: size in 1..=32
    pub receipt_policy: String,
    pub leader: Option<NodeName>,
    pub commit_watermark: u64,
    pub stored_bytes: u64,
    pub degraded_since: Option<Timestamp>,
    pub degraded_range_start: Option<Timestamp>,
    pub degraded_range_end: Option<Timestamp>,
}

impl TabletInfo {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.receipt_policy;
            if v.is_empty() || v.len() > 32usize {
                return Err(ValidationError {
                    field: "receipt_policy".to_string(),
                    message: "length must be in 1..=32".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CellInfo {
    pub cell: CellName,
    pub region: RegionName,
    pub controllers: Vec<GroupMember>,
    pub generation: Generation,
    pub quorum_available: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectPlacement {
    pub project_id: ProjectId,
    pub cells: Vec<CellName>,
    pub moving_to: Option<CellName>,
    pub policy_generation: Generation,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DescribeTopologyRequest {
    pub cell: Option<CellName>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TopologyResponse {
    pub cells: Vec<CellInfo>,
    pub tablets: Vec<TabletInfo>,
    pub nodes: Vec<NodeCondition>,
    pub generation: Generation,
    pub directory_available: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SplitTabletRequest {
    pub tablet: TabletName,
    pub at_shard: Option<VirtualShard>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MergeTabletsRequest {
    pub left: TabletName,
    pub right: TabletName,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MoveTabletRequest {
    pub tablet: TabletName,
    pub away_from: NodeName,
    pub onto: NodeName,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeReplicaRequest {
    pub tablet: TabletName,
    pub node: NodeName,
    /// constraint: size in 1..=256
    pub address: String,
    pub role: MemberRole,
    pub region: Option<RegionName>,
    pub domain: Option<DomainName>,
}

impl ChangeReplicaRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.address;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "address".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RemoveReplicaRequest {
    pub tablet: TabletName,
    pub node: NodeName,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetReceiptPolicyRequest {
    pub tablet: TabletName,
    /// constraint: size in 1..=32
    pub policy: String,
}

impl SetReceiptPolicyRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.policy;
            if v.is_empty() || v.len() > 32usize {
                return Err(ValidationError {
                    field: "policy".to_string(),
                    message: "length must be in 1..=32".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FailOverRegionRequest {
    pub tablet: TabletName,
    pub onto_region: RegionName,
    pub accept_data_loss: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnsafeRecoverRequest {
    pub tablet: TabletName,
    pub confirm_tablet: TabletName,
    pub survivor: NodeName,
    /// constraint: size in 1..=512
    pub reason: String,
}

impl UnsafeRecoverRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.reason;
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "reason".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnsafeRecoverResponse {
    pub tablet: TabletName,
    /// constraint: size in 1..=64
    pub audit_id: String,
    pub degraded_range_start: Timestamp,
    pub degraded_range_end: Timestamp,
    pub survivor_watermark: u64,
    pub performed_at: Timestamp,
}

impl UnsafeRecoverResponse {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.audit_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "audit_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClearDegradedRequest {
    pub tablet: TabletName,
    /// constraint: size in 1..=128
    pub accepted_by: String,
    /// constraint: size in 1..=512
    pub reason: String,
}

impl ClearDegradedRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.accepted_by;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "accepted_by".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        {
            let v = &self.reason;
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "reason".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClusterSnapshotRequest {
    pub tablet: Option<TabletName>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClusterSnapshotResponse {
    /// constraint: size in 1..=128
    pub snapshot_id: String,
    pub tablets: Vec<TabletName>,
    pub taken_at: Timestamp,
    pub total_bytes: u64,
    /// constraint: size in 32..=32
    pub whole_digest: Vec<u8>,
}

impl ClusterSnapshotResponse {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.snapshot_id;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "snapshot_id".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        {
            let v = &self.whole_digest;
            if v.len() < 32usize || v.len() > 32usize {
                return Err(ValidationError {
                    field: "whole_digest".to_string(),
                    message: "length must be in 32..=32".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RestoreRequest {
    /// constraint: size in 1..=128
    pub snapshot_id: String,
    pub onto_tablet: Option<TabletName>,
}

impl RestoreRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.snapshot_id;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "snapshot_id".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BootstrapGroupRequest {
    pub group: GroupRef,
    pub members: Vec<GroupMember>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClusterAck {
    pub accepted: bool,
    pub generation: Generation,
    /// constraint: size in 1..=512
    pub message: Option<String>,
}

impl ClusterAck {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.message {
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "message".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssignProjectRequest {
    pub project_id: ProjectId,
    pub cells: Vec<CellName>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DirectoryRequest {
    pub project_id: Option<ProjectId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DirectoryResponse {
    pub placements: Vec<ProjectPlacement>,
    pub generation: Generation,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteRequest {
    pub project_id: ProjectId,
    /// constraint: size in 1..=256
    pub affinity_key: Vec<u8>,
}

impl RouteRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.affinity_key;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "affinity_key".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteResponse {
    pub cell: CellName,
    pub shard: VirtualShard,
    pub tablet: TabletName,
    pub leader: Option<NodeName>,
    /// constraint: size in 1..=256
    pub leader_address: Option<String>,
    pub generation: Generation,
    pub epoch: Epoch,
    /// constraint: size in 1..=32
    pub receipt_policy: String,
}

impl RouteResponse {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.leader_address {
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "leader_address".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        {
            let v = &self.receipt_policy;
            if v.is_empty() || v.len() > 32usize {
                return Err(ValidationError {
                    field: "receipt_policy".to_string(),
                    message: "length must be in 1..=32".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplicaStatusRequest {
    pub tablet: Option<TabletName>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplicaStatus {
    pub node: NodeName,
    pub tablets: Vec<TabletInfo>,
    pub applied_watermark: u64,
    pub lag_ms: Option<DurationMs>,
    pub writable: bool,
}

pub type EventId = Vec<u8>;

pub type BatchId = Vec<u8>;

pub type WorkspaceId = Vec<u8>;

pub type ProjectId = Vec<u8>;

pub type SourceId = Vec<u8>;

pub type SessionId = String;

pub type TraceId = Vec<u8>;

pub type SpanId = Vec<u8>;

pub type Timestamp = i64;

pub type DurationMs = i64;

/// TypedValueKind variants
#[derive(Debug, Clone, PartialEq)]
pub enum TypedValueKind {
    Null,
    Bool,
    Int,
    Uint,
    Float,
    Decimal,
    Text,
    Bytes,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypedValue {
    pub kind: TypedValueKind,
    pub bool_value: Option<bool>,
    pub int_value: Option<i64>,
    pub uint_value: Option<u64>,
    pub float_value: Option<f64>,
    pub decimal_value: Option<CsilDecimal>,
    pub text_value: Option<String>,
    pub bytes_value: Option<Vec<u8>>,
}

/// PropertyOrigin variants
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyOrigin {
    Client,
    Driver,
    Collector,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    /// constraint: size in 1..=64
    pub key: String,
    pub value: TypedValue,
    pub origin: PropertyOrigin,
}

impl Property {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.key;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "key".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

pub type PropertyList = Vec<Property>;

/// MeasurementKind variants
#[derive(Debug, Clone, PartialEq)]
pub enum MeasurementKind {
    Float,
    Decimal,
    Int,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// constraint: size in 1..=64
    pub key: String,
    pub kind: MeasurementKind,
    pub float_value: Option<f64>,
    pub decimal_value: Option<CsilDecimal>,
    pub int_value: Option<i64>,
    /// constraint: size in 1..=32
    pub unit: Option<String>,
}

impl Measurement {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.key;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "key".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        if let Some(v) = &self.unit {
            if v.is_empty() || v.len() > 32usize {
                return Err(ValidationError {
                    field: "unit".to_string(),
                    message: "length must be in 1..=32".to_string(),
                });
            }
        }
        Ok(())
    }
}

pub type MeasurementList = Vec<Measurement>;

/// ConsentState variants
#[derive(Debug, Clone, PartialEq)]
pub enum ConsentState {
    Granted,
    Denied,
    Absent,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Consent {
    pub marketing: ConsentState,
    pub analytics: ConsentState,
    /// constraint: size in 1..=64
    pub policy_version: Option<String>,
}

impl Consent {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.policy_version {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "policy_version".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// TelemetryKind variants
#[derive(Debug, Clone, PartialEq)]
pub enum TelemetryKind {
    Event,
    PageView,
    SessionStart,
    SessionEnd,
    SessionHeartbeat,
    Interaction,
    FeatureExposure,
    Identify,
    Alias,
    Group,
    Conversion,
    Error,
    Span,
    MetricPoint,
    CampaignTouch,
    CampaignCost,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub event_id: EventId,
    pub kind: TelemetryKind,
    pub schema_version: u64,
    pub occurred_at: Timestamp,
    pub observed_at: Option<Timestamp>,
    pub received_at: Option<Timestamp>,
    pub workspace_id: Option<WorkspaceId>,
    pub project_id: Option<ProjectId>,
    pub source_id: Option<SourceId>,
    pub sequence: Option<u64>,
    /// constraint: size in 1..=128
    pub release: Option<String>,
    /// constraint: size in 1..=128
    pub service_name: Option<String>,
    /// constraint: size in 1..=128
    pub request_id: Option<String>,
    pub session_id: Option<SessionId>,
    /// constraint: size in 1..=256
    pub end_user_id: Option<String>,
    /// constraint: size in 1..=256
    pub anonymous_id: Option<String>,
    pub trace_id: Option<TraceId>,
    pub span_id: Option<SpanId>,
    pub consent: Option<Consent>,
    /// constraint: size in 1..=64
    pub sdk_name: String,
    /// constraint: size in 1..=32
    pub sdk_version: String,
    pub properties: PropertyList,
    pub measurements: Option<MeasurementList>,
}

impl Envelope {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.release {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "release".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.service_name {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "service_name".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.request_id {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "request_id".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.end_user_id {
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "end_user_id".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        if let Some(v) = &self.anonymous_id {
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "anonymous_id".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        {
            let v = &self.sdk_name;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "sdk_name".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        {
            let v = &self.sdk_version;
            if v.is_empty() || v.len() > 32usize {
                return Err(ValidationError {
                    field: "sdk_version".to_string(),
                    message: "length must be in 1..=32".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// ErrorCode variants
#[derive(Debug, Clone, PartialEq)]
pub enum ErrorCode {
    InvalidArgument,
    Unauthenticated,
    PermissionDenied,
    NotFound,
    AlreadyExists,
    ResourceExhausted,
    FailedPrecondition,
    Unavailable,
    SchemaUnsupported,
    BudgetExceeded,
    IncompleteResult,
    Internal,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    pub detail: Option<PropertyList>,
}
