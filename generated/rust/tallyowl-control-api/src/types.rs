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

pub type ExpressionRef = Vec<u8>;

pub type QueryNodeRef = Vec<u8>;

/// ExpressionKind variants
#[derive(Debug, Clone, PartialEq)]
pub enum ExpressionKind {
    Literal,
    Field,
    Compare,
    Set,
    Text,
    Null,
    Logical,
    Arith,
    Time,
    Convert,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpressionNode {
    pub expression: ExpressionKind,
    pub literal: Option<TypedValue>,
    pub field: Option<FieldRef>,
    pub compare: Option<CompareExpr>,
    pub set_expr: Option<SetExpr>,
    pub text_expr: Option<TextExpr>,
    pub null_expr: Option<NullExpr>,
    pub logical: Option<LogicalExpr>,
    pub arith: Option<ArithExpr>,
    pub time_expr: Option<TimeExpr>,
    pub convert: Option<ConvertExpr>,
}

/// QueryNodeKind variants
#[derive(Debug, Clone, PartialEq)]
pub enum QueryNodeKind {
    Scan,
    Filter,
    Project,
    Aggregate,
    Sort,
    Limit,
    Join,
    Union,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryNodeBox {
    pub node: QueryNodeKind,
    pub scan: Option<ScanNode>,
    pub filter: Option<FilterNode>,
    pub project: Option<ProjectNode>,
    pub aggregate: Option<AggregateNode>,
    pub sort: Option<SortNode>,
    pub limit: Option<LimitNode>,
    pub join: Option<JoinNode>,
    pub union: Option<UnionNode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldRef {
    /// constraint: size in 1..=128
    pub name: String,
    /// constraint: size in 1..=32
    pub value_type: Option<String>,
    pub origin: Option<PropertyOrigin>,
}

impl FieldRef {
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
        if let Some(v) = &self.value_type {
            if v.is_empty() || v.len() > 32usize {
                return Err(ValidationError {
                    field: "value_type".to_string(),
                    message: "length must be in 1..=32".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// CompareOp variants
#[derive(Debug, Clone, PartialEq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// LogicalOp variants
#[derive(Debug, Clone, PartialEq)]
pub enum LogicalOp {
    And,
    Or,
    Not,
}

/// ArithOp variants
#[derive(Debug, Clone, PartialEq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompareExpr {
    pub compare: CompareOp,
    pub left: ExpressionRef,
    pub right: ExpressionRef,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetExpr {
    pub set_test: SetExpr_set_test,
    pub left: ExpressionRef,
    pub values: Vec<TypedValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextExpr {
    pub text_test: TextExpr_text_test,
    pub left: ExpressionRef,
    /// constraint: size in 1..=256
    pub pattern: String,
}

impl TextExpr {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.pattern;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "pattern".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NullExpr {
    pub null_test: NullExpr_null_test,
    pub operand: ExpressionRef,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogicalExpr {
    pub logical: LogicalOp,
    pub operands: Vec<ExpressionRef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArithExpr {
    pub arith: ArithOp,
    pub left: ExpressionRef,
    pub right: ExpressionRef,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimeExpr {
    pub time_fn: TimeExpr_time_fn,
    pub operand: ExpressionRef,
    pub interval: Option<Interval>,
    /// constraint: size in 1..=32
    pub part: Option<String>,
    pub shift_ms: Option<DurationMs>,
}

impl TimeExpr {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.part {
            if v.is_empty() || v.len() > 32usize {
                return Err(ValidationError {
                    field: "part".to_string(),
                    message: "length must be in 1..=32".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConvertExpr {
    /// constraint: size in 1..=32
    pub convert_to: String,
    pub operand: ExpressionRef,
}

impl ConvertExpr {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.convert_to;
            if v.is_empty() || v.len() > 32usize {
                return Err(ValidationError {
                    field: "convert_to".to_string(),
                    message: "length must be in 1..=32".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// TimeBasis variants
#[derive(Debug, Clone, PartialEq)]
pub enum TimeBasis {
    OccurredAt,
    ReceivedAt,
    CommittedAt,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Interval {
    pub fixed_ms: Option<DurationMs>,
    pub calendar: Option<Interval_calendar>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimeRange {
    pub range_start: Timestamp,
    pub range_end: Timestamp,
    pub basis: TimeBasis,
    /// constraint: size in 1..=64
    pub timezone: Option<String>,
}

impl TimeRange {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.timezone {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "timezone".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// Dataset variants
#[derive(Debug, Clone, PartialEq)]
pub enum Dataset {
    Events,
    ErrorOccurrences,
    ErrorGroups,
    Spans,
    MetricPoints,
    IdentityEdges,
    CampaignTouches,
    Conversions,
    CampaignCosts,
}

/// MeasureKind variants
#[derive(Debug, Clone, PartialEq)]
pub enum MeasureKind {
    Count,
    Sum,
    Min,
    Max,
    Avg,
    CountDistinct,
    CountDistinctApprox,
    Quantile,
    QuantileApprox,
    HistogramMerge,
    TopK,
    TopKApprox,
    Rate,
    Increase,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Measure {
    pub kind: MeasureKind,
    pub field: Option<FieldRef>,
    pub quantile: Option<f64>,
    pub k: Option<u64>,
    /// constraint: size in 1..=64
    pub alias: String,
}

impl Measure {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.alias;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "alias".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Dimension {
    pub field: FieldRef,
    /// constraint: size in 1..=64
    pub alias: String,
}

impl Dimension {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.alias;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "alias".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    /// constraint: size in 1..=64
    pub alias: String,
    pub direction: SortKey_direction,
}

impl SortKey {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.alias;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "alias".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScanNode {
    pub scan: Dataset,
    pub project_id: ProjectId,
    pub range: TimeRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FilterNode {
    pub filter: ExpressionRef,
    pub input: QueryNodeRef,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectNode {
    pub project_fields: Vec<Dimension>,
    pub input: QueryNodeRef,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AggregateNode {
    pub dimensions: Vec<Dimension>,
    pub measures: Vec<Measure>,
    pub interval: Option<Interval>,
    pub input: QueryNodeRef,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortNode {
    pub sort: Vec<SortKey>,
    pub input: QueryNodeRef,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LimitNode {
    pub limit: u64,
    pub offset: Option<u64>,
    pub cursor: Option<Vec<u8>>,
    pub input: QueryNodeRef,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JoinNode {
    pub join_key: FieldRef,
    pub max_rows_each_side: u64,
    pub left: QueryNodeRef,
    pub right: QueryNodeRef,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnionNode {
    pub union: Vec<QueryNodeRef>,
}

/// CorrelationBasis variants
#[derive(Debug, Clone, PartialEq)]
pub enum CorrelationBasis {
    EndUser,
    Session,
    Group,
}

/// IdentityResolution variants
#[derive(Debug, Clone, PartialEq)]
pub enum IdentityResolution {
    EventTime,
    LatestKnown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunnelStep {
    /// constraint: size in 1..=128
    pub name: String,
    pub r#match: ExpressionRef,
    pub exclusion: Option<bool>,
}

impl FunnelStep {
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
pub struct FunnelQuery {
    pub project_id: ProjectId,
    pub range: TimeRange,
    pub steps: Vec<FunnelStep>,
    pub window_ms: DurationMs,
    pub basis: CorrelationBasis,
    pub ordered: bool,
    pub breakdown: Option<Dimension>,
    pub resolution: Option<IdentityResolution>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetentionQuery {
    pub project_id: ProjectId,
    pub range: TimeRange,
    pub initial: ExpressionRef,
    pub returning: ExpressionRef,
    pub period: RetentionQuery_period,
    pub periods: u64,
    pub first_time_only: bool,
    pub resolution: Option<IdentityResolution>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathQuery {
    pub project_id: ProjectId,
    pub range: TimeRange,
    pub anchor: ExpressionRef,
    pub direction: PathQuery_direction,
    pub depth: u64,
    pub min_frequency: u64,
    pub collapse_loops: bool,
    pub resolution: Option<IdentityResolution>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TraceQuery {
    pub project_id: ProjectId,
    pub trace_id: TraceId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimelineQuery {
    pub project_id: ProjectId,
    pub range: TimeRange,
    /// constraint: size in 1..=256
    pub end_user_id: Option<String>,
    pub session_id: Option<SessionId>,
    pub kinds: Vec<TelemetryKind>,
    pub limit: u64,
    pub cursor: Option<Vec<u8>>,
    pub resolution: Option<IdentityResolution>,
}

impl TimelineQuery {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.end_user_id {
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

/// AttributionModel variants
#[derive(Debug, Clone, PartialEq)]
pub enum AttributionModel {
    FirstTouch,
    LastTouch,
    LastNonDirect,
    Linear,
    Position,
    Decay,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttributionQuery {
    pub project_id: ProjectId,
    pub range: TimeRange,
    /// constraint: size in 1..=128
    pub conversion_goal: String,
    pub model: AttributionModel,
    pub lookback_ms: DurationMs,
    pub touch_filter: Option<ExpressionRef>,
    pub breakdown: Option<Dimension>,
    pub resolution: Option<IdentityResolution>,
}

impl AttributionQuery {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.conversion_goal;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "conversion_goal".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CampaignSummaryQuery {
    pub project_id: ProjectId,
    pub range: TimeRange,
    /// constraint: size in 1..=128
    pub conversion_goal: String,
    pub model: AttributionModel,
    pub lookback_ms: DurationMs,
    pub dimension: Option<CampaignSummaryQuery_dimension>,
    pub touch_filter: Option<ExpressionRef>,
    pub resolution: Option<IdentityResolution>,
}

impl CampaignSummaryQuery {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.conversion_goal;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "conversion_goal".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// Consistency variants
#[derive(Debug, Clone, PartialEq)]
pub enum Consistency {
    Committed,
    BoundedStale,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryBudget {
    pub deadline_ms: Option<DurationMs>,
    pub max_scanned_bytes: Option<u64>,
    pub max_scanned_segments: Option<u64>,
    pub max_rows: Option<u64>,
}

/// QueryForm variants
#[derive(Debug, Clone, PartialEq)]
pub enum QueryForm {
    Node,
    Funnel,
    Retention,
    Path,
    Trace,
    Timeline,
    Attribution,
    CampaignSummary,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryRequest {
    pub algebra_version: u64,
    pub consistency: Consistency,
    pub max_staleness_ms: Option<DurationMs>,
    pub budget: Option<QueryBudget>,
    pub allow_partial: bool,
    pub comparison_range: Option<TimeRange>,
    pub form: QueryForm,
    pub node: Option<QueryNodeRef>,
    pub funnel: Option<FunnelQuery>,
    pub retention: Option<RetentionQuery>,
    pub path: Option<PathQuery>,
    pub trace: Option<TraceQuery>,
    pub timeline: Option<TimelineQuery>,
    pub attribution: Option<AttributionQuery>,
    pub campaign_summary: Option<CampaignSummaryQuery>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Exactness {
    /// constraint: size in 1..=64
    pub alias: String,
    pub exact: bool,
    /// constraint: size in 1..=64
    pub method: Option<String>,
    pub error_bound: Option<f64>,
}

impl Exactness {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.alias;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "alias".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        if let Some(v) = &self.method {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "method".to_string(),
                    message: "length must be in 1..=64".to_string(),
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

#[derive(Debug, Clone, PartialEq)]
pub struct ResultMetadata {
    pub algebra_version: u64,
    pub commit_watermark: u64,
    pub freshness_ms: DurationMs,
    pub complete: bool,
    pub missing: Option<Vec<MissingRange>>,
    pub exactness: Vec<Exactness>,
    pub scanned_bytes: u64,
    pub scanned_segments: u64,
    pub cold_bytes: Option<u64>,
    pub tombstone_generation: u64,
    pub applied_retention_class: Option<RetentionClass>,
    pub warnings: Option<Vec<String>>,
    pub next_cursor: Option<Vec<u8>>,
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
pub struct ResultRow {
    pub values: Vec<TypedValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryResponse {
    pub columns: Vec<String>,
    pub rows: Vec<ResultRow>,
    pub metadata: ResultMetadata,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThresholdCondition {
    /// constraint: size in 1..=64
    pub alias: String,
    pub compare: CompareOp,
    pub value: f64,
    pub sustained_ms: Option<DurationMs>,
}

impl ThresholdCondition {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.alias;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "alias".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AbsenceCondition {
    pub for_ms: DurationMs,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlertRule {
    /// constraint: size in 1..=64
    pub rule_id: String,
    /// constraint: size in 1..=128
    pub name: String,
    pub project_id: ProjectId,
    pub query: QueryRequest,
    pub interval_ms: DurationMs,
    pub threshold: Option<ThresholdCondition>,
    pub absence: Option<AbsenceCondition>,
    pub notify: Vec<NotificationTarget>,
    pub enabled: bool,
    pub escalate_after_ms: Option<DurationMs>,
    pub silenced_until: Option<Timestamp>,
    /// constraint: size in 1..=512
    pub silence_reason: Option<String>,
    /// constraint: size in 1..=512
    pub disabled_reason: Option<String>,
    pub updated_at: Option<Timestamp>,
    /// constraint: size in 1..=128
    pub updated_by: Option<String>,
}

impl AlertRule {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.rule_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "rule_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        {
            let v = &self.name;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "name".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.silence_reason {
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "silence_reason".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        if let Some(v) = &self.disabled_reason {
            if v.is_empty() || v.len() > 512usize {
                return Err(ValidationError {
                    field: "disabled_reason".to_string(),
                    message: "length must be in 1..=512".to_string(),
                });
            }
        }
        if let Some(v) = &self.updated_by {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "updated_by".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NotificationTarget {
    pub kind: NotificationTarget_kind,
    /// constraint: size in 1..=1024
    pub url: Option<String>,
    /// constraint: size in 1..=256
    pub secret_ref: Option<String>,
}

impl NotificationTarget {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.url {
            if v.is_empty() || v.len() > 1024usize {
                return Err(ValidationError {
                    field: "url".to_string(),
                    message: "length must be in 1..=1024".to_string(),
                });
            }
        }
        if let Some(v) = &self.secret_ref {
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "secret_ref".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// AlertState variants
#[derive(Debug, Clone, PartialEq)]
pub enum AlertState {
    Ok,
    Firing,
    NoData,
    Unknown,
    Silenced,
}

/// AlertOutcome variants
#[derive(Debug, Clone, PartialEq)]
pub enum AlertOutcome {
    Value,
    NoData,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlertInstance {
    /// constraint: size in 1..=64
    pub rule_id: String,
    pub state: AlertState,
    pub since: Timestamp,
    pub observed_value: Option<f64>,
    /// constraint: size in 1..=512
    pub reason: Option<String>,
    pub outcome: Option<AlertOutcome>,
    pub last_evaluated_at: Option<Timestamp>,
    pub notifications_sent: Option<u64>,
    pub last_notified_at: Option<Timestamp>,
    pub holding_ms: Option<DurationMs>,
    pub commit_watermark: Option<u64>,
}

impl AlertInstance {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.rule_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "rule_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        if let Some(v) = &self.reason {
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
pub struct SilenceRequest {
    /// constraint: size in 1..=64
    pub rule_id: String,
    pub project_id: ProjectId,
    pub until: Timestamp,
    /// constraint: size in 1..=512
    pub reason: Option<String>,
}

impl SilenceRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.rule_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "rule_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        if let Some(v) = &self.reason {
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
pub struct ResolveRequest {
    /// constraint: size in 1..=64
    pub rule_id: String,
    pub project_id: ProjectId,
    /// constraint: size in 1..=512
    pub reason: Option<String>,
}

impl ResolveRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.rule_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "rule_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        if let Some(v) = &self.reason {
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
pub struct DeleteAlertRuleRequest {
    /// constraint: size in 1..=64
    pub rule_id: String,
    pub project_id: ProjectId,
}

impl DeleteAlertRuleRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.rule_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "rule_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NotificationDelivery {
    /// constraint: size in 1..=64
    pub rule_id: String,
    /// constraint: size in 1..=1024
    pub target: String,
    pub state: AlertState,
    pub attempts: u64,
    pub delivered: bool,
    /// constraint: size in 1..=512
    pub last_failure: Option<String>,
    pub next_attempt_at: Option<Timestamp>,
    pub at: Timestamp,
}

impl NotificationDelivery {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.rule_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "rule_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        {
            let v = &self.target;
            if v.is_empty() || v.len() > 1024usize {
                return Err(ValidationError {
                    field: "target".to_string(),
                    message: "length must be in 1..=1024".to_string(),
                });
            }
        }
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
pub struct NotificationDeliveryList {
    pub deliveries: Vec<NotificationDelivery>,
    pub next_cursor: Option<Vec<u8>>,
}

/// WorkflowKind variants
#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowKind {
    AlertEvaluation,
    Notification,
    ProjectorRebuild,
    Retention,
    Deletion,
    Export,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowStatus {
    pub kind: WorkflowKind,
    /// constraint: size in 1..=128
    pub queue: String,
    pub pending: u64,
    pub in_flight: u64,
    pub quarantined: u64,
    pub oldest_pending_age_ms: DurationMs,
    pub failures: u64,
    pub last_success_at: Option<Timestamp>,
    /// constraint: size in 1..=512
    pub last_failure: Option<String>,
}

impl WorkflowStatus {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.queue;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "queue".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
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
pub struct WorkflowList {
    pub workflows: Vec<WorkflowStatus>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunWorkflowRequest {
    pub kind: WorkflowKind,
    pub project_id: ProjectId,
    pub range: Option<TimeRange>,
    /// constraint: size in 1..=1024
    pub destination: Option<String>,
}

impl RunWorkflowRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.destination {
            if v.is_empty() || v.len() > 1024usize {
                return Err(ValidationError {
                    field: "destination".to_string(),
                    message: "length must be in 1..=1024".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunWorkflowResponse {
    /// constraint: size in 1..=128
    pub task_id: String,
    pub kind: WorkflowKind,
    pub accepted: bool,
    /// constraint: size in 1..=512
    pub reason: Option<String>,
}

impl RunWorkflowResponse {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.task_id;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "task_id".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.reason {
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
pub struct BeginLoginRequest {
    /// constraint: size in 1..=253
    pub user_domain: String,
    /// constraint: size in 1..=1024
    pub callback_url: String,
}

impl BeginLoginRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.user_domain;
            if v.is_empty() || v.len() > 253usize {
                return Err(ValidationError {
                    field: "user_domain".to_string(),
                    message: "length must be in 1..=253".to_string(),
                });
            }
        }
        {
            let v = &self.callback_url;
            if v.is_empty() || v.len() > 1024usize {
                return Err(ValidationError {
                    field: "callback_url".to_string(),
                    message: "length must be in 1..=1024".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BeginLoginResponse {
    /// constraint: size in 1..=4096
    pub redirect_url: String,
    /// constraint: size in 1..=64
    pub login_id: String,
    pub expires_at: Timestamp,
}

impl BeginLoginResponse {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.redirect_url;
            if v.is_empty() || v.len() > 4096usize {
                return Err(ValidationError {
                    field: "redirect_url".to_string(),
                    message: "length must be in 1..=4096".to_string(),
                });
            }
        }
        {
            let v = &self.login_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "login_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompleteLoginRequest {
    /// constraint: size in 1..=64
    pub login_id: String,
    /// constraint: size in 1..=65536
    pub encrypted_token: String,
    /// constraint: size in 1..=4096
    pub arrived_url: String,
}

impl CompleteLoginRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.login_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "login_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        {
            let v = &self.encrypted_token;
            if v.is_empty() || v.len() > 65536usize {
                return Err(ValidationError {
                    field: "encrypted_token".to_string(),
                    message: "length must be in 1..=65536".to_string(),
                });
            }
        }
        {
            let v = &self.arrived_url;
            if v.is_empty() || v.len() > 4096usize {
                return Err(ValidationError {
                    field: "arrived_url".to_string(),
                    message: "length must be in 1..=4096".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompleteLoginResponse {
    /// constraint: size in 1..=256
    pub session_token: String,
    /// constraint: size in 1..=320
    pub subject: String,
    pub expires_at: Timestamp,
    pub memberships: Vec<Membership>,
}

impl CompleteLoginResponse {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.session_token;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "session_token".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        {
            let v = &self.subject;
            if v.is_empty() || v.len() > 320usize {
                return Err(ValidationError {
                    field: "subject".to_string(),
                    message: "length must be in 1..=320".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Membership {
    pub workspace_id: WorkspaceId,
    pub role: Membership_role,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub project_id: ProjectId,
    pub workspace_id: WorkspaceId,
    /// constraint: size in 1..=128
    pub name: String,
    /// constraint: size in 1..=1024
    pub description: Option<String>,
}

impl Project {
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
        if let Some(v) = &self.description {
            if v.is_empty() || v.len() > 1024usize {
                return Err(ValidationError {
                    field: "description".to_string(),
                    message: "length must be in 1..=1024".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    pub workspace_id: WorkspaceId,
    /// constraint: size in 1..=128
    pub name: String,
}

impl Workspace {
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
pub struct ApiKeySummary {
    /// constraint: size in 1..=64
    pub key_id: String,
    pub project_id: ProjectId,
    pub created_at: Timestamp,
    pub expires_at: Option<Timestamp>,
    pub last_used_at: Option<Timestamp>,
    pub revoked: bool,
}

impl ApiKeySummary {
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

#[derive(Debug, Clone, PartialEq)]
pub struct DeletionTarget {
    pub project_id: ProjectId,
    /// constraint: size in 1..=256
    pub end_user_id: Option<String>,
    pub event_ids: Option<Vec<EventId>>,
    pub range: Option<TimeRange>,
}

impl DeletionTarget {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.end_user_id {
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
pub struct DeletionRequest {
    pub target: DeletionTarget,
    /// constraint: size in 1..=512
    pub reason: String,
}

impl DeletionRequest {
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
pub struct DeletionResponse {
    /// constraint: size in 1..=64
    pub request_id: String,
    pub tombstone_generation: u64,
    pub accepted_at: Timestamp,
    pub predicates: Option<u64>,
    pub identifiers: Option<Vec<String>>,
}

impl DeletionResponse {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.request_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "request_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// PolicyScope variants
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyScope {
    Installation,
    Workspace,
    Project,
    Environment,
    Source,
}

/// CampaignLinking variants
#[derive(Debug, Clone, PartialEq)]
pub enum CampaignLinking {
    Linked,
    Unlinked,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PolicyDocument {
    pub scope: PolicyScope,
    /// constraint: size in 1..=64
    pub scope_id: Option<String>,
    pub enabled_kinds: Option<Vec<TelemetryKind>>,
    pub head_sample_rate: Option<f64>,
    pub session_max_lifetime_ms: Option<DurationMs>,
    pub max_event_bytes: Option<u64>,
    pub max_properties: Option<u64>,
    pub blocked_event_names: Option<Vec<String>>,
    pub blocked_property_keys: Option<Vec<String>>,
    pub redact_property_keys: Option<Vec<String>>,
    pub campaign_linking: Option<CampaignLinking>,
    pub attribution_needs_consent: Option<bool>,
    pub kill_switch: Option<bool>,
}

impl PolicyDocument {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.scope_id {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "scope_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledPolicy {
    pub policy_version: u64,
    pub enabled_kinds: Vec<TelemetryKind>,
    pub head_sample_rate: f64,
    pub session_max_lifetime_ms: DurationMs,
    pub max_event_bytes: u64,
    pub max_properties: u64,
    pub blocked_event_names: Vec<String>,
    pub blocked_property_keys: Vec<String>,
    pub redact_property_keys: Vec<String>,
    pub campaign_linking: CampaignLinking,
    pub attribution_needs_consent: bool,
    pub kill_switch: bool,
    pub from_levels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PolicyRequest {
    pub workspace_id: Option<WorkspaceId>,
    pub project_id: Option<ProjectId>,
    /// constraint: size in 1..=64
    pub environment: Option<String>,
    pub source_id: Option<SourceId>,
}

impl PolicyRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.environment {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "environment".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttributionSettings {
    pub project_id: ProjectId,
    pub position_first_weight: f64,
    pub position_last_weight: f64,
    pub decay_half_life_ms: DurationMs,
    pub lookback_ms: DurationMs,
    pub enabled_models: Vec<AttributionModel>,
    pub touch_retention_ms: DurationMs,
    pub settings_version: Option<u64>,
    pub updated_at: Option<Timestamp>,
    /// constraint: size in 1..=128
    pub updated_by: Option<String>,
}

impl AttributionSettings {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.updated_by {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "updated_by".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedAnalysis {
    /// constraint: size in 1..=64
    pub analysis_id: String,
    pub project_id: ProjectId,
    /// constraint: size in 1..=128
    pub name: String,
    pub form: QueryForm,
    /// constraint: size in 1..=1048576
    pub request: Vec<u8>,
    pub algebra_version: u64,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
    /// constraint: size in 1..=128
    pub updated_by: Option<String>,
}

impl SavedAnalysis {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.analysis_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "analysis_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        {
            let v = &self.name;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "name".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        {
            let v = &self.request;
            if v.is_empty() || v.len() > 1048576usize {
                return Err(ValidationError {
                    field: "request".to_string(),
                    message: "length must be in 1..=1048576".to_string(),
                });
            }
        }
        if let Some(v) = &self.updated_by {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "updated_by".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedAnalysisList {
    pub analyses: Vec<SavedAnalysis>,
    pub next_cursor: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DashboardPanel {
    /// constraint: size in 1..=64
    pub analysis_id: String,
    /// constraint: size in 1..=128
    pub title: Option<String>,
    pub column: u64,
    pub row: u64,
    pub width: u64,
    pub height: u64,
}

impl DashboardPanel {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.analysis_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "analysis_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        if let Some(v) = &self.title {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "title".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedDashboard {
    /// constraint: size in 1..=64
    pub dashboard_id: String,
    pub project_id: ProjectId,
    /// constraint: size in 1..=128
    pub name: String,
    pub panels: Vec<DashboardPanel>,
    pub updated_at: Option<Timestamp>,
    /// constraint: size in 1..=128
    pub updated_by: Option<String>,
}

impl SavedDashboard {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.dashboard_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "dashboard_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        {
            let v = &self.name;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "name".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        if let Some(v) = &self.updated_by {
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "updated_by".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedDashboardList {
    pub dashboards: Vec<SavedDashboard>,
    pub next_cursor: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedRequest {
    pub project_id: ProjectId,
    /// constraint: size in 1..=64
    pub id: Option<String>,
    pub cursor: Option<Vec<u8>>,
    pub limit: Option<u64>,
}

impl SavedRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.id {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// NodeRole variants
#[derive(Debug, Clone, PartialEq)]
pub enum NodeRole {
    CollectorIntake,
    CollectorForwarder,
    CompatibilityReceiver,
    IngestGateway,
    QueryCoordinator,
    Projector,
    WorkflowWorker,
    ReadReplica,
    ExportReplica,
    StorageProcess,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoleTokenPolicy {
    pub roles: Vec<NodeRole>,
    pub cells: Option<Vec<String>>,
    pub regions: Option<Vec<String>>,
    pub workspaces: Option<Vec<WorkspaceId>>,
    pub projects: Option<Vec<ProjectId>>,
    pub expires_at: Option<Timestamp>,
    pub max_uses: Option<u64>,
    pub max_active_nodes: Option<u64>,
    pub certificate_lifetime_ms: Option<u64>,
    pub enrollments_each_hour: Option<u64>,
    pub audit_labels: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateRoleTokenRequest {
    /// constraint: size in 1..=128
    pub label: String,
    pub policy: RoleTokenPolicy,
}

impl CreateRoleTokenRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.label;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "label".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateRoleTokenResponse {
    /// constraint: size in 1..=64
    pub token_id: String,
    /// constraint: size in 1..=256
    pub token: String,
    pub policy: RoleTokenPolicy,
    pub created_at: Timestamp,
}

impl CreateRoleTokenResponse {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.token_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "token_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        {
            let v = &self.token;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "token".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoleTokenSummary {
    /// constraint: size in 1..=64
    pub token_id: String,
    /// constraint: size in 1..=128
    pub label: String,
    pub policy: RoleTokenPolicy,
    pub created_at: Timestamp,
    pub uses: u64,
    pub active_nodes: u64,
    pub last_used_at: Option<Timestamp>,
    pub revoked: bool,
}

impl RoleTokenSummary {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.token_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "token_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        {
            let v = &self.label;
            if v.is_empty() || v.len() > 128usize {
                return Err(ValidationError {
                    field: "label".to_string(),
                    message: "length must be in 1..=128".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoleTokenList {
    pub tokens: Vec<RoleTokenSummary>,
    pub next_cursor: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RevokeRoleTokenRequest {
    /// constraint: size in 1..=64
    pub token_id: String,
    pub cascade: Option<bool>,
}

impl RevokeRoleTokenRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.token_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "token_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeCapabilities {
    /// constraint: size in 1..=64
    pub software_version: String,
    pub protocol_versions: Option<Vec<u64>>,
    pub segment_versions: Option<Vec<u64>>,
    pub compression_codecs: Option<Vec<String>>,
    pub storage_bytes: Option<u64>,
    pub policy_generation: Option<u64>,
}

impl NodeCapabilities {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.software_version;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "software_version".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnrollNodeRequest {
    /// constraint: size in 1..=256
    pub token: String,
    /// constraint: size in 1..=65536
    pub certificate_request: Vec<u8>,
    pub requested_role: NodeRole,
    /// constraint: size in 1..=64
    pub cell: Option<String>,
    /// constraint: size in 1..=64
    pub region: Option<String>,
    /// constraint: size in 1..=64
    pub node_id: Option<String>,
    pub capabilities: Option<NodeCapabilities>,
}

impl EnrollNodeRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.token;
            if v.is_empty() || v.len() > 256usize {
                return Err(ValidationError {
                    field: "token".to_string(),
                    message: "length must be in 1..=256".to_string(),
                });
            }
        }
        {
            let v = &self.certificate_request;
            if v.is_empty() || v.len() > 65536usize {
                return Err(ValidationError {
                    field: "certificate_request".to_string(),
                    message: "length must be in 1..=65536".to_string(),
                });
            }
        }
        if let Some(v) = &self.cell {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "cell".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        if let Some(v) = &self.region {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "region".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        if let Some(v) = &self.node_id {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "node_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnrollNodeResponse {
    /// constraint: size in 1..=64
    pub node_id: String,
    pub certificate_chain: Vec<Vec<u8>>,
    /// constraint: size in 1..=64
    pub certificate_serial: String,
    pub effective_role: NodeRole,
    /// constraint: size in 1..=64
    pub cell: Option<String>,
    /// constraint: size in 1..=64
    pub region: Option<String>,
    pub issued_at: Timestamp,
    pub expires_at: Timestamp,
    pub renew_after: Timestamp,
    pub permitted_capabilities: Option<NodeCapabilities>,
}

impl EnrollNodeResponse {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.node_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "node_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        {
            let v = &self.certificate_serial;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "certificate_serial".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        if let Some(v) = &self.cell {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "cell".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        if let Some(v) = &self.region {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "region".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenewNodeCertificateRequest {
    /// constraint: size in 1..=64
    pub node_id: String,
    /// constraint: size in 1..=65536
    pub certificate_request: Vec<u8>,
}

impl RenewNodeCertificateRequest {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.node_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "node_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        {
            let v = &self.certificate_request;
            if v.is_empty() || v.len() > 65536usize {
                return Err(ValidationError {
                    field: "certificate_request".to_string(),
                    message: "length must be in 1..=65536".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeSummary {
    /// constraint: size in 1..=64
    pub node_id: String,
    /// constraint: size in 1..=64
    pub token_id: String,
    pub role: NodeRole,
    /// constraint: size in 1..=64
    pub cell: Option<String>,
    /// constraint: size in 1..=64
    pub region: Option<String>,
    /// constraint: size in 1..=64
    pub certificate_serial: String,
    pub enrolled_at: Timestamp,
    pub expires_at: Timestamp,
    pub revoked: bool,
}

impl NodeSummary {
    /// Validate this value against the constraints declared in the CSIL spec.
    pub fn validate(&self) -> Result<(), ValidationError> {
        {
            let v = &self.node_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "node_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        {
            let v = &self.token_id;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "token_id".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        if let Some(v) = &self.cell {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "cell".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        if let Some(v) = &self.region {
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "region".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        {
            let v = &self.certificate_serial;
            if v.is_empty() || v.len() > 64usize {
                return Err(ValidationError {
                    field: "certificate_serial".to_string(),
                    message: "length must be in 1..=64".to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeList {
    pub nodes: Vec<NodeSummary>,
    pub next_cursor: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListRequest {
    pub cursor: Option<Vec<u8>>,
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceList {
    pub workspaces: Vec<Workspace>,
    pub next_cursor: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectList {
    pub projects: Vec<Project>,
    pub next_cursor: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApiKeyList {
    pub keys: Vec<ApiKeySummary>,
    pub next_cursor: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlertListRequest {
    pub project_id: ProjectId,
    pub cursor: Option<Vec<u8>>,
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlertRuleList {
    pub rules: Vec<AlertRule>,
    pub next_cursor: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlertInstanceList {
    pub instances: Vec<AlertInstance>,
    pub next_cursor: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Empty {}

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

/// SetExpr_set_test variants
#[derive(Debug, Clone, PartialEq)]
pub enum SetExpr_set_test {
    In,
    NotIn,
}

/// TextExpr_text_test variants
#[derive(Debug, Clone, PartialEq)]
pub enum TextExpr_text_test {
    StartsWith,
    EndsWith,
    Contains,
}

/// NullExpr_null_test variants
#[derive(Debug, Clone, PartialEq)]
pub enum NullExpr_null_test {
    IsNull,
    IsNotNull,
}

/// TimeExpr_time_fn variants
#[derive(Debug, Clone, PartialEq)]
pub enum TimeExpr_time_fn {
    Truncate,
    Extract,
    Shift,
}

/// Interval_calendar variants
#[derive(Debug, Clone, PartialEq)]
pub enum Interval_calendar {
    Hour,
    Day,
    Week,
    Month,
}

/// SortKey_direction variants
#[derive(Debug, Clone, PartialEq)]
pub enum SortKey_direction {
    Asc,
    Desc,
}

/// RetentionQuery_period variants
#[derive(Debug, Clone, PartialEq)]
pub enum RetentionQuery_period {
    Day,
    Week,
    Month,
}

/// PathQuery_direction variants
#[derive(Debug, Clone, PartialEq)]
pub enum PathQuery_direction {
    Previous,
    Next,
}

/// CampaignSummaryQuery_dimension variants
#[derive(Debug, Clone, PartialEq)]
pub enum CampaignSummaryQuery_dimension {
    Campaign,
    Channel,
    Source,
    Medium,
    Content,
}

/// NotificationTarget_kind variants
#[derive(Debug, Clone, PartialEq)]
pub enum NotificationTarget_kind {
    Webhook,
    CsilCallback,
}

/// Membership_role variants
#[derive(Debug, Clone, PartialEq)]
pub enum Membership_role {
    Viewer,
    Admin,
    Owner,
}
