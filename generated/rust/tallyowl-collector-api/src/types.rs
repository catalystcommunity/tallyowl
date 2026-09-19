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

/// Compression variants
#[derive(Debug, Clone, PartialEq)]
pub enum Compression {
    None,
    Zstd,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub batch_id: BatchId,
    pub items: Vec<TelemetryItem>,
    pub common_properties: Option<PropertyList>,
    pub sealed_at: Timestamp,
    pub compression: Option<Compression>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubmitBatchRequest {
    pub batch: Batch,
    pub policy_version: Option<u64>,
    pub protocol_version: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubmitBatchResponse {
    pub batch_id: BatchId,
    pub accepted: u64,
    pub durable_copies: u64,
    pub queued_at: Timestamp,
    pub rejected: Option<Vec<RejectedItem>>,
    pub policy_version: Option<u64>,
}

/// ReceiptPolicy variants
#[derive(Debug, Clone, PartialEq)]
pub enum ReceiptPolicy {
    LocalOne,
    LocalQuorum,
    RemoteOne,
    Custom,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommitBatchRequest {
    pub batch: Batch,
    pub source_id: SourceId,
    pub attempt: Option<u64>,
    pub protocol_version: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommitBatchResponse {
    pub batch_id: BatchId,
    pub accepted: u64,
    pub committed_at: Timestamp,
    pub satisfied_policy: ReceiptPolicy,
    pub commit_watermark: u64,
    pub protocol_version: u64,
    pub projector_version: u64,
    pub rejected: Option<Vec<RejectedItem>>,
    pub deduplicated: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryTask {
    pub task_version: u64,
    pub batch_id: BatchId,
    pub source_id: SourceId,
    pub batch: Vec<u8>,
    pub compression: Compression,
    pub uncompressed_bytes: u64,
    pub attempts: u64,
    pub accepted_at: Timestamp,
    pub last_attempt_at: Option<Timestamp>,
    pub next_attempt_at: Option<Timestamp>,
    /// constraint: size in 1..=512
    pub last_failure: Option<String>,
}

impl DeliveryTask {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.last_failure {
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "last_failure".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolveKeyRequest {
    /// constraint: size in 1..=512
    pub credential: String,
}

impl ResolveKeyRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.credential;
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "credential".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolveKeyResponse {
    /// constraint: size in 1..=64
    pub key_id: String,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub source_id: SourceId,
    pub cache_ttl_ms: DurationMs,
    pub expires_at: Option<Timestamp>,
}

impl ResolveKeyResponse {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.key_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "key_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// RetentionClass variants
#[derive(Debug, Clone, PartialEq)]
pub enum RetentionClass {
    Provisional,
    Raw,
    Detailed,
    Rollup,
    Audit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetentionRule {
    pub class: RetentionClass,
    pub duration_ms: DurationMs,
    pub kind: Option<TelemetryKind>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SamplingClause {
    pub mode: SamplingClause_mode,
    pub percent: Option<f64>,
    pub count: Option<u64>,
    pub interval_ms: Option<DurationMs>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TailRule {
    /// constraint: size in 1..=128
    pub name: String,
    pub expression: Vec<u8>,
    pub sampling: SamplingClause,
}

impl TailRule {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.name;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "name".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProtectedKey {
    /// constraint: size in 1..=64
    pub key: String,
    pub origin: PropertyOrigin,
}

impl ProtectedKey {
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

/// CampaignLinking variants
#[derive(Debug, Clone, PartialEq)]
pub enum CampaignLinking {
    Linked,
    Unlinked,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CollectionPolicy {
    pub policy_version: u64,
    pub enabled_kinds: Vec<TelemetryKind>,
    pub head_sample_rate: f64,
    pub tail_rules: Option<Vec<TailRule>>,
    pub tail_decision_window_ms: Option<DurationMs>,
    pub late_span_grace_ms: Option<DurationMs>,
    pub always_keep_expressions: Option<Vec<Vec<u8>>>,
    pub retention: Vec<RetentionRule>,
    pub protected_keys: Vec<ProtectedKey>,
    pub stamped_properties: PropertyList,
    pub max_event_bytes: u64,
    pub max_batch_bytes: u64,
    pub max_properties: u64,
    pub session_max_lifetime_ms: DurationMs,
    pub redact_keys: Option<Vec<String>>,
    pub blocked_event_names: Option<Vec<String>>,
    pub blocked_property_keys: Option<Vec<String>>,
    pub campaign_linking: Option<CampaignLinking>,
    pub kill_switch: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FetchPolicyRequest {
    pub source_id: SourceId,
    pub known_version: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FetchPolicyResponse {
    pub policy: Option<CollectionPolicy>,
    pub unchanged: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CollectorHealth {
    pub role: CollectorHealth_role,
    pub ready: bool,
    pub queue_depth: u64,
    pub oldest_task_age_ms: DurationMs,
    pub quarantine_count: u64,
    pub applied_policy_version: u64,
    pub policy_age_ms: Option<DurationMs>,
    pub last_sweep_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HealthRequest {}

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

#[derive(Debug, Clone, PartialEq)]
pub struct EventPayload {
    /// constraint: size in 1..=128
    pub name: String,
    /// constraint: size in 1..=512
    pub route: Option<String>,
    /// constraint: size in 1..=512
    pub page_title: Option<String>,
}

impl EventPayload {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.name;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "name".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.route {
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "route".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        if let Some(v) = &self.page_title {
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "page_title".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PageViewPayload {
    /// constraint: size in 1..=512
    pub route: String,
    /// constraint: size in 1..=512
    pub page_title: Option<String>,
    /// constraint: size in 1..=1024
    pub referrer: Option<String>,
    pub campaign: Option<CampaignParameters>,
}

impl PageViewPayload {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.route;
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "route".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        if let Some(v) = &self.page_title {
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "page_title".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        if let Some(v) = &self.referrer {
            if v.is_empty() || v.len() > 1024usize {
                return Err(ValidationError {
                    field: "referrer".to_string(),
                    message: "length must be in 1..=1024".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CampaignParameters {
    /// constraint: size in 1..=128
    pub source: Option<String>,
    /// constraint: size in 1..=128
    pub medium: Option<String>,
    /// constraint: size in 1..=128
    pub campaign: Option<String>,
    /// constraint: size in 1..=128
    pub term: Option<String>,
    /// constraint: size in 1..=128
    pub content: Option<String>,
    /// constraint: size in 1..=256
    pub click_id: Option<String>,
}

impl CampaignParameters {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.source {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "source".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.medium {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "medium".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.campaign {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "campaign".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.term {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "term".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.content {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "content".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.click_id {
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "click_id".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionStartPayload {
    /// constraint: size in 1..=512
    pub entry_route: Option<String>,
}

impl SessionStartPayload {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.entry_route {
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "entry_route".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionEndPayload {
    pub reason: SessionEndPayload_reason,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InteractionPayload {
    /// constraint: size in 1..=256
    pub target: String,
    /// constraint: size in 1..=64
    pub action: String,
}

impl InteractionPayload {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.target;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "target".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        {
            let v = &self.action;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "action".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FeatureExposurePayload {
    /// constraint: size in 1..=128
    pub feature: String,
    /// constraint: size in 1..=128
    pub variant: String,
}

impl FeatureExposurePayload {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.feature;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "feature".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        {
            let v = &self.variant;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "variant".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IdentifyPayload {
    /// constraint: size in 1..=256
    pub end_user_id: String,
}

impl IdentifyPayload {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.end_user_id;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "end_user_id".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AliasPayload {
    /// constraint: size in 1..=256
    pub from_id: String,
    /// constraint: size in 1..=256
    pub to_id: String,
}

impl AliasPayload {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.from_id;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "from_id".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        {
            let v = &self.to_id;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "to_id".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupPayload {
    /// constraint: size in 1..=256
    pub group_id: String,
    /// constraint: size in 1..=64
    pub group_kind: Option<String>,
}

impl GroupPayload {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.group_id;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "group_id".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        if let Some(v) = &self.group_kind {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "group_kind".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConversionPayload {
    /// constraint: size in 1..=128
    pub goal: String,
    pub value: Option<CsilDecimal>,
    /// constraint: size in 3..=3
    pub currency: Option<String>,
    /// constraint: size in 1..=128
    pub order_id: Option<String>,
    pub campaign: Option<CampaignParameters>,
    pub touch_event_id: Option<EventId>,
}

impl ConversionPayload {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.goal;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "goal".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.currency {
            if v.len() < 3usize || v.len() > 3usize {
                return Err(ValidationError {
                    field: "currency".to_string(),
                    message: "length must be in 3..=3".to_string(),
                });
            }
        }
        if let Some(v) = &self.order_id {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "order_id".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StackFrame {
    /// constraint: size in 1..=256
    pub module: Option<String>,
    /// constraint: size in 1..=256
    pub function: Option<String>,
    /// constraint: size in 1..=512
    pub file: Option<String>,
    pub line: Option<u64>,
    pub in_app: bool,
}

impl StackFrame {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.module {
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "module".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        if let Some(v) = &self.function {
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "function".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        if let Some(v) = &self.file {
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "file".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ErrorPayload {
    /// constraint: size in 1..=256
    pub error_type: String,
    /// constraint: size in 1..=2048
    pub message: String,
    pub handled: bool,
    pub severity: ErrorPayload_severity,
    /// constraint: size in 1..=64
    pub mechanism: Option<String>,
    /// constraint: size in 1..=128
    pub runtime: Option<String>,
    pub frames: Option<Vec<StackFrame>>,
    pub breadcrumbs: Option<Vec<EventId>>,
}

impl ErrorPayload {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.error_type;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "error_type".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        {
            let v = &self.message;
            if v.is_empty() || v.len() > 2048usize {
                return Err(ValidationError {
                    field: "message".to_string(),
                    message: "length must be in 1..=2048".to_string(),
                });
            }
        }
        if let Some(v) = &self.mechanism {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "mechanism".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        if let Some(v) = &self.runtime {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "runtime".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// SpanKind variants
#[derive(Debug, Clone, PartialEq)]
pub enum SpanKind {
    Internal,
    Server,
    Client,
    Producer,
    Consumer,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpanLink {
    pub trace_id: TraceId,
    pub span_id: SpanId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpanPayload {
    /// constraint: size in 1..=256
    pub operation: String,
    pub kind: SpanKind,
    pub start_at: Timestamp,
    pub duration_ms: DurationMs,
    pub status: SpanPayload_status,
    /// constraint: size in 1..=256
    pub resource: Option<String>,
    pub parent_span_id: Option<SpanId>,
    pub links: Option<Vec<SpanLink>>,
    pub error_event_id: Option<EventId>,
    /// constraint: size in 1..=64
    pub sampling_reason: Option<String>,
}

impl SpanPayload {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.operation;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "operation".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        if let Some(v) = &self.resource {
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "resource".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        if let Some(v) = &self.sampling_reason {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "sampling_reason".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// MetricKind variants
#[derive(Debug, Clone, PartialEq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistogramValue {
    pub count: u64,
    pub sum: f64,
    pub bounds: Vec<f64>,
    pub counts: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MetricPointPayload {
    /// constraint: size in 1..=256
    pub metric_name: String,
    pub metric_kind: MetricKind,
    /// constraint: size in 1..=32
    pub unit: Option<String>,
    /// constraint: size in 1..=512
    pub description: Option<String>,
    pub monotonic: bool,
    pub temporality: MetricPointPayload_temporality,
    pub start_at: Timestamp,
    pub end_at: Timestamp,
    pub labels: PropertyList,
    pub number_value: Option<f64>,
    pub histogram_value: Option<HistogramValue>,
    pub exemplar_trace_id: Option<TraceId>,
}

impl MetricPointPayload {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.metric_name;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "metric_name".to_string(),
                    message: "length must be in 1..=256".to_string(),
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
        if let Some(v) = &self.description {
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "description".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CampaignTouchPayload {
    pub campaign: CampaignParameters,
    /// constraint: size in 1..=1024
    pub referrer: Option<String>,
    /// constraint: size in 1..=256
    pub referrer_domain: Option<String>,
    /// constraint: size in 1..=512
    pub landing_route: Option<String>,
}

impl CampaignTouchPayload {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.referrer {
            if v.is_empty() || v.len() > 1024usize {
                return Err(ValidationError {
                    field: "referrer".to_string(),
                    message: "length must be in 1..=1024".to_string(),
                });
            }
        }
        if let Some(v) = &self.referrer_domain {
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "referrer_domain".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        if let Some(v) = &self.landing_route {
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "landing_route".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CampaignCostPayload {
    /// constraint: size in 1..=128
    pub campaign: String,
    /// constraint: size in 1..=128
    pub platform: Option<String>,
    pub cost: CsilDecimal,
    /// constraint: size in 3..=3
    pub currency: String,
    pub period_start: Timestamp,
    pub period_end: Timestamp,
}

impl CampaignCostPayload {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.campaign;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "campaign".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.platform {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "platform".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        {
            let v = &self.currency;
            if v.len() < 3usize || v.len() > 3usize {
                return Err(ValidationError {
                    field: "currency".to_string(),
                    message: "length must be in 3..=3".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryItem {
    pub envelope: Envelope,
    pub event: Option<EventPayload>,
    pub page_view: Option<PageViewPayload>,
    pub session_start: Option<SessionStartPayload>,
    pub session_end: Option<SessionEndPayload>,
    pub interaction: Option<InteractionPayload>,
    pub feature_exposure: Option<FeatureExposurePayload>,
    pub identify: Option<IdentifyPayload>,
    pub alias: Option<AliasPayload>,
    pub group: Option<GroupPayload>,
    pub conversion: Option<ConversionPayload>,
    pub error: Option<ErrorPayload>,
    pub span: Option<SpanPayload>,
    pub metric_point: Option<MetricPointPayload>,
    pub campaign_touch: Option<CampaignTouchPayload>,
    pub campaign_cost: Option<CampaignCostPayload>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CaptureRequest {
    pub items: Vec<TelemetryItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CaptureResponse {
    pub accepted: u64,
    pub rejected: Option<Vec<RejectedItem>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RejectedItem {
    pub event_id: EventId,
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CaptureCriticalRequest {
    pub items: Vec<TelemetryItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CaptureCriticalResponse {
    pub accepted: u64,
    pub durable: bool,
    pub batch_id: Option<BatchId>,
    pub rejected: Option<Vec<RejectedItem>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PolicyVersionRequest {}

#[derive(Debug, Clone, PartialEq)]
pub struct PolicyVersionResponse {
    pub policy_version: u64,
    pub sampling_rate: f64,
    pub enabled_kinds: Vec<TelemetryKind>,
}

/// SamplingClause_mode variants
#[derive(Debug, Clone, PartialEq)]
pub enum SamplingClause_mode {
    KeepAll,
    KeepPercent,
    KeepFirstN,
}

/// CollectorHealth_role variants
#[derive(Debug, Clone, PartialEq)]
pub enum CollectorHealth_role {
    Intake,
    Forwarder,
    CompatibilityReceiver,
}

/// SessionEndPayload_reason variants
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEndPayload_reason {
    Explicit,
    Timeout,
    MaximumLifetime,
}

/// ErrorPayload_severity variants
#[derive(Debug, Clone, PartialEq)]
pub enum ErrorPayload_severity {
    Fatal,
    Error,
    Warning,
    Info,
}

/// SpanPayload_status variants
#[derive(Debug, Clone, PartialEq)]
pub enum SpanPayload_status {
    Ok,
    Error,
    Unset,
}

/// MetricPointPayload_temporality variants
#[derive(Debug, Clone, PartialEq)]
pub enum MetricPointPayload_temporality {
    Delta,
    Cumulative,
}
