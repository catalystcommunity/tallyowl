//! Ergonomics above the generated code, and the rules an explicit discriminant
//! makes checkable.
//!
//! # Why this crate exists
//!
//! `csil/types/common.csil` gives `TypedValue` an explicit `kind` field and one
//! optional field for each type, and `csil/tallyowl-ingest.csil` gives
//! `TelemetryItem` one optional field for each payload. Those shapes are what
//! make Rust, Go, and TypeScript agree byte for byte; see
//! `docs/IMPLEMENTATION_LOG.md` L017 for the measurement that forced them.
//!
//! They are also verbose at a call site, and they permit two states that the
//! contract does not: a `kind` that names a field which is absent, and an item
//! with no payload or with two. Both are wire concerns rather than generator
//! concerns, so both are answered here, above the generated code, exactly where
//! section 10.1 of the implementation prompt says an ergonomics problem belongs.
//!
//! # What it holds
//!
//! One module for each generated package, because each package carries its own
//! copy of the shared types. The bodies are identical and come from one macro,
//! so the three cannot drift.
//!
//! | Module | Package |
//! | --- | --- |
//! | [`ingest`] | `tallyowl-ingest-api` |
//! | [`collector`] | `tallyowl-collector-api` |
//! | [`control`] | `tallyowl-control-api` |
//!
//! A caller reads a wire value into [`Value`], which is one type across all
//! three, and writes one by naming the [`Value`] rather than the field.

use std::fmt;

/// One typed value, independent of which generated package carried it.
///
/// An exact decimal keeps its integer parts. Money never becomes a float, and
/// `docs/QUERY.md` section 4 says so.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Boolean(bool),
    Integer(i64),
    Unsigned(u64),
    Float(f64),
    Decimal { exponent: i64, mantissa: i128 },
    Text(String),
    Bytes(Vec<u8>),
}

impl Value {
    /// The `kind` name this value travels under. The same word appears in the
    /// specification, so a log line and a wire dump read alike.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Boolean(_) => "bool",
            Value::Integer(_) => "int",
            Value::Unsigned(_) => "uint",
            Value::Float(_) => "float",
            Value::Decimal { .. } => "decimal",
            Value::Text(_) => "text",
            Value::Bytes(_) => "bytes",
        }
    }

    /// An exact decimal from its canonical text form, such as `19.99`.
    ///
    /// Returns `None` for text that is not a decimal number. A caller that
    /// cannot parse a value must not substitute a float.
    pub fn decimal_from_text(text: &str) -> Option<Value> {
        let text = text.trim();
        let (negative, digits) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        let (whole, fraction) = match digits.split_once('.') {
            Some((w, f)) => (w, f),
            None => (digits, ""),
        };
        if whole.is_empty() && fraction.is_empty() {
            return None;
        }
        if !whole.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        if !fraction.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let mut mantissa: i128 = 0;
        for c in whole.chars().chain(fraction.chars()) {
            mantissa = mantissa
                .checked_mul(10)?
                .checked_add((c as u8 - b'0') as i128)?;
        }
        if negative {
            mantissa = -mantissa;
        }
        Some(Value::Decimal {
            exponent: -(fraction.len() as i64),
            mantissa,
        })
    }

    /// The canonical text form of a decimal. Every other value renders the way
    /// a person expects to read it.
    pub fn to_display(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Boolean(v) => v.to_string(),
            Value::Integer(v) => v.to_string(),
            Value::Unsigned(v) => v.to_string(),
            Value::Float(v) => v.to_string(),
            Value::Decimal { exponent, mantissa } => decimal_text(*exponent, *mantissa),
            Value::Text(v) => v.clone(),
            Value::Bytes(v) => v.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }
}

fn decimal_text(exponent: i64, mantissa: i128) -> String {
    if exponent == 0 {
        return mantissa.to_string();
    }
    let negative = mantissa < 0;
    let digits = mantissa.unsigned_abs().to_string();
    let sign = if negative { "-" } else { "" };
    if exponent > 0 {
        let zeros = "0".repeat(exponent as usize);
        return format!("{sign}{digits}{zeros}");
    }
    let scale = (-exponent) as usize;
    if digits.len() <= scale {
        let pad = "0".repeat(scale - digits.len());
        format!("{sign}0.{pad}{digits}")
    } else {
        let split = digits.len() - scale;
        format!("{sign}{}.{}", &digits[..split], &digits[split..])
    }
}

/// A wire value that the contract does not permit.
///
/// Every message here is written for the person who has to act on it, per
/// `docs/CONVENTIONS.md` section 1. None of them names an internal type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError {
    pub message: String,
}

impl WireError {
    fn new(message: impl Into<String>) -> WireError {
        WireError {
            message: message.into(),
        }
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WireError {}

/// Build one package's bridge. The three bodies are identical, so they come
/// from one place and cannot drift apart.
macro_rules! shared_types_bridge {
    ($module:ident, $api:ident, $package:literal) => {
        #[doc = concat!("Bridge for the `", $package, "` package.")]
        pub mod $module {
            use $api::types::{
                CsilDecimal, Measurement, MeasurementKind, Property, PropertyOrigin, TypedValue,
                TypedValueKind,
            };

            use crate::{Value, WireError};

            /// An empty value of every field, so a constructor names only what
            /// it sets.
            fn blank(kind: TypedValueKind) -> TypedValue {
                TypedValue {
                    kind,
                    bool_value: None,
                    int_value: None,
                    uint_value: None,
                    float_value: None,
                    decimal_value: None,
                    text_value: None,
                    bytes_value: None,
                }
            }

            /// Write a [`Value`] as the wire shape, with the discriminant and
            /// the field always in agreement.
            pub fn write(value: &Value) -> TypedValue {
                match value {
                    Value::Null => blank(TypedValueKind::Null),
                    Value::Boolean(v) => TypedValue {
                        bool_value: Some(*v),
                        ..blank(TypedValueKind::Bool)
                    },
                    Value::Integer(v) => TypedValue {
                        int_value: Some(*v),
                        ..blank(TypedValueKind::Int)
                    },
                    Value::Unsigned(v) => TypedValue {
                        uint_value: Some(*v),
                        ..blank(TypedValueKind::Uint)
                    },
                    Value::Float(v) => TypedValue {
                        float_value: Some(*v),
                        ..blank(TypedValueKind::Float)
                    },
                    Value::Decimal { exponent, mantissa } => TypedValue {
                        decimal_value: Some(CsilDecimal {
                            exponent: *exponent,
                            mantissa: *mantissa,
                        }),
                        ..blank(TypedValueKind::Decimal)
                    },
                    Value::Text(v) => TypedValue {
                        text_value: Some(v.clone()),
                        ..blank(TypedValueKind::Text)
                    },
                    Value::Bytes(v) => TypedValue {
                        bytes_value: Some(v.clone()),
                        ..blank(TypedValueKind::Bytes)
                    },
                }
            }

            /// Read a wire value.
            ///
            /// A `kind` that names a field the message did not carry is a
            /// rejection, never a substituted default. A value that arrives as
            /// a number where the sender said text is the kind of fault that
            /// produces a wrong answer quietly, and this is where it stops.
            pub fn read(value: &TypedValue) -> Result<Value, WireError> {
                let missing = |named: &str| {
                    WireError::new(format!(
                        "A value says it holds {named} and carries no {named}. Send the value, or say the value is empty."
                    ))
                };
                match value.kind {
                    TypedValueKind::Null => Ok(Value::Null),
                    TypedValueKind::Bool => value
                        .bool_value
                        .map(Value::Boolean)
                        .ok_or_else(|| missing("a true or false")),
                    TypedValueKind::Int => value
                        .int_value
                        .map(Value::Integer)
                        .ok_or_else(|| missing("a whole number")),
                    TypedValueKind::Uint => value
                        .uint_value
                        .map(Value::Unsigned)
                        .ok_or_else(|| missing("a whole number that is not negative")),
                    TypedValueKind::Float => value
                        .float_value
                        .map(Value::Float)
                        .ok_or_else(|| missing("a number")),
                    TypedValueKind::Decimal => value
                        .decimal_value
                        .as_ref()
                        .map(|d| Value::Decimal {
                            exponent: d.exponent,
                            mantissa: d.mantissa,
                        })
                        .ok_or_else(|| missing("an exact number")),
                    TypedValueKind::Text => value
                        .text_value
                        .clone()
                        .map(Value::Text)
                        .ok_or_else(|| missing("text")),
                    TypedValueKind::Bytes => value
                        .bytes_value
                        .clone()
                        .map(Value::Bytes)
                        .ok_or_else(|| missing("data")),
                }
            }

            /// One property, with its origin.
            pub fn property(key: &str, value: Value, origin: PropertyOrigin) -> Property {
                Property {
                    key: key.to_string(),
                    value: write(&value),
                    origin,
                }
            }

            /// The origin name a store and a log use. `docs/DECISIONS.md` D38
            /// gives the three and what each means.
            pub fn origin_name(origin: &PropertyOrigin) -> &'static str {
                match origin {
                    PropertyOrigin::Client => "client",
                    PropertyOrigin::Driver => "driver",
                    PropertyOrigin::Collector => "collector",
                }
            }

            /// Read a measurement's number.
            pub fn read_measurement(measurement: &Measurement) -> Result<Value, WireError> {
                let missing = || {
                    WireError::new(
                        "A measurement says which kind of number it holds and carries no number. Send the number.",
                    )
                };
                match measurement.kind {
                    MeasurementKind::Float => measurement
                        .float_value
                        .map(Value::Float)
                        .ok_or_else(missing),
                    MeasurementKind::Int => measurement
                        .int_value
                        .map(Value::Integer)
                        .ok_or_else(missing),
                    MeasurementKind::Decimal => measurement
                        .decimal_value
                        .as_ref()
                        .map(|d| Value::Decimal {
                            exponent: d.exponent,
                            mantissa: d.mantissa,
                        })
                        .ok_or_else(missing),
                }
            }

            /// One measurement. A value that is not a number is refused rather
            /// than converted, because a measure that silently became something
            /// else is a wrong number on a chart.
            pub fn measurement(
                key: &str,
                value: Value,
                unit: Option<&str>,
            ) -> Result<Measurement, WireError> {
                let blank = |kind: MeasurementKind| Measurement {
                    key: key.to_string(),
                    kind,
                    float_value: None,
                    decimal_value: None,
                    int_value: None,
                    unit: unit.map(|u| u.to_string()),
                };
                match value {
                    Value::Float(v) => Ok(Measurement {
                        float_value: Some(v),
                        ..blank(MeasurementKind::Float)
                    }),
                    Value::Integer(v) => Ok(Measurement {
                        int_value: Some(v),
                        ..blank(MeasurementKind::Int)
                    }),
                    Value::Decimal { exponent, mantissa } => Ok(Measurement {
                        decimal_value: Some(CsilDecimal { exponent, mantissa }),
                        ..blank(MeasurementKind::Decimal)
                    }),
                    other => Err(WireError::new(format!(
                        "The measurement `{key}` was given {} and a measurement holds a number. Send a number, or send the value as a property instead.",
                        match other {
                            Value::Null => "nothing",
                            Value::Boolean(_) => "a true or false",
                            Value::Text(_) => "text",
                            Value::Bytes(_) => "data",
                            Value::Unsigned(_) => "a whole number that is not negative",
                            _ => "a value of another kind",
                        }
                    ))),
                }
            }
        }
    };
}

/// Build one package's telemetry-item bridge. Only the two packages that carry
/// telemetry get one; the control package does not.
macro_rules! telemetry_item_bridge {
    ($module:ident, $api:ident) => {
        impl_telemetry_item_bridge! {
            $module, $api,
            [
                (Event, event, EventPayload, "event"),
                (PageView, page_view, PageViewPayload, "page-view"),
                (SessionStart, session_start, SessionStartPayload, "session-start"),
                (SessionEnd, session_end, SessionEndPayload, "session-end"),
                (Interaction, interaction, InteractionPayload, "interaction"),
                (FeatureExposure, feature_exposure, FeatureExposurePayload, "feature-exposure"),
                (Identify, identify, IdentifyPayload, "identify"),
                (Alias, alias, AliasPayload, "alias"),
                (Group, group, GroupPayload, "group"),
                (Conversion, conversion, ConversionPayload, "conversion"),
                (Error, error, ErrorPayload, "error"),
                (Span, span, SpanPayload, "span"),
                (MetricPoint, metric_point, MetricPointPayload, "metric-point"),
                (CampaignTouch, campaign_touch, CampaignTouchPayload, "campaign-touch"),
                (CampaignCost, campaign_cost, CampaignCostPayload, "campaign-cost")
            ]
        }
    };
}

macro_rules! impl_telemetry_item_bridge {
    ($module:ident, $api:ident, [ $( ($variant:ident, $field:ident, $payload:ident, $kind_name:literal) ),+ ]) => {
        pub mod $module {
            use $api::types::{Envelope, TelemetryItem, TelemetryKind, $($payload),+};

            use crate::WireError;

            /// The payload an item carries, borrowed from the item.
            ///
            /// A heartbeat carries no payload, so it is a variant of its own
            /// rather than an absence a caller has to interpret.
            #[derive(Debug, Clone, PartialEq)]
            pub enum Payload<'a> {
                SessionHeartbeat,
                $( $variant(&'a $payload) ),+
            }

            impl Payload<'_> {
                /// The telemetry kind this payload belongs to.
                pub fn kind_name(&self) -> &'static str {
                    match self {
                        Payload::SessionHeartbeat => "session-heartbeat",
                        $( Payload::$variant(_) => $kind_name ),+
                    }
                }
            }

            /// An item with an envelope and no payload field set.
            pub fn empty_item(envelope: Envelope) -> TelemetryItem {
                TelemetryItem {
                    envelope,
                    $( $field: None ),+
                }
            }

            /// The name of the field that `kind` selects, or `None` for a kind
            /// that carries no payload.
            pub fn field_for(kind: &TelemetryKind) -> Option<&'static str> {
                match kind {
                    TelemetryKind::SessionHeartbeat => None,
                    $( TelemetryKind::$variant => Some(stringify!($field)) ),+
                }
            }

            /// Read the one payload an item carries.
            ///
            /// Three states are refused rather than guessed at: no payload for a
            /// kind that needs one, more than one payload, and a payload that
            /// does not match the kind on the envelope. Each of the three would
            /// otherwise store one thing under the name of another.
            pub fn payload(item: &TelemetryItem) -> Result<Payload<'_>, WireError> {
                let mut present: Vec<&'static str> = Vec::new();
                $( if item.$field.is_some() { present.push($kind_name); } )+

                if present.len() > 1 {
                    return Err(WireError::new(format!(
                        "One item described itself as {} at the same time. An item holds one of them.",
                        present.join(" and ")
                    )));
                }

                // The comparison is on the kind name rather than the field
                // name, so the message reads the way the specification does.
                let declared = super::kind_name_of(&item.envelope.kind);
                match present.first() {
                    None if matches!(item.envelope.kind, TelemetryKind::SessionHeartbeat) => {
                        Ok(Payload::SessionHeartbeat)
                    }
                    None => Err(WireError::new(format!(
                        "One item says it is {declared} and carries no {declared} details."
                    ))),
                    Some(found) if declared != *found => Err(WireError::new(format!(
                        "One item says it is {declared} and carries {found} details instead."
                    ))),
                    Some(_) => {
                        $( if let Some(value) = item.$field.as_ref() {
                            return Ok(Payload::$variant(value));
                        } )+
                        unreachable!("a payload was present a moment ago")
                    }
                }
            }

            $(
                #[doc = concat!("An item carrying a ", $kind_name, " payload, with the envelope kind set to match.")]
                pub fn $field(mut envelope: Envelope, value: $payload) -> TelemetryItem {
                    envelope.kind = TelemetryKind::$variant;
                    TelemetryItem {
                        $field: Some(value),
                        ..empty_item(envelope)
                    }
                }
            )+
        }
    };
}

/// The kind name shared by every generated package, from the one enum shape
/// that every package copies.
macro_rules! kind_name_fn {
    ($api:ident) => {
        pub(crate) fn kind_name_of(kind: &$api::types::TelemetryKind) -> &'static str {
            use $api::types::TelemetryKind as K;
            match kind {
                K::Event => "event",
                K::PageView => "page-view",
                K::SessionStart => "session-start",
                K::SessionEnd => "session-end",
                K::SessionHeartbeat => "session-heartbeat",
                K::Interaction => "interaction",
                K::FeatureExposure => "feature-exposure",
                K::Identify => "identify",
                K::Alias => "alias",
                K::Group => "group",
                K::Conversion => "conversion",
                K::Error => "error",
                K::Span => "span",
                K::MetricPoint => "metric-point",
                K::CampaignTouch => "campaign-touch",
                K::CampaignCost => "campaign-cost",
            }
        }
    };
}

/// Building a query tree, without naming the nine fields a caller leaves empty.
///
/// `QueryNodeBox`, `ExpressionNode`, and `QueryRequest` each carry a
/// discriminant and one optional field for each shape, for the reason the
/// module documentation gives. That is right on the wire and unreadable at a
/// call site, so the call site uses these.
pub mod query {
    use tallyowl_control_api::codec::{encode_expression_node, encode_query_node_box};
    use tallyowl_control_api::types::{
        AggregateNode, ArithExpr, CompareExpr, Consistency, ConvertExpr, ExpressionKind,
        ExpressionNode, FieldRef, FilterNode, JoinNode, LimitNode, LogicalExpr, NullExpr,
        ProjectNode, QueryBudget, QueryForm, QueryNodeBox, QueryNodeKind, QueryRequest, ScanNode,
        SetExpr, SortNode, TextExpr, TimeExpr, TimeRange, TypedValue, UnionNode,
    };

    fn blank_node(kind: QueryNodeKind) -> QueryNodeBox {
        QueryNodeBox {
            node: kind,
            scan: None,
            filter: None,
            project: None,
            aggregate: None,
            sort: None,
            limit: None,
            join: None,
            union: None,
        }
    }

    /// One node, with its discriminant and its field in agreement.
    pub mod node {
        use super::*;

        pub fn scan(value: ScanNode) -> QueryNodeBox {
            QueryNodeBox {
                scan: Some(value),
                ..blank_node(QueryNodeKind::Scan)
            }
        }
        pub fn filter(value: FilterNode) -> QueryNodeBox {
            QueryNodeBox {
                filter: Some(value),
                ..blank_node(QueryNodeKind::Filter)
            }
        }
        pub fn project(value: ProjectNode) -> QueryNodeBox {
            QueryNodeBox {
                project: Some(value),
                ..blank_node(QueryNodeKind::Project)
            }
        }
        pub fn aggregate(value: AggregateNode) -> QueryNodeBox {
            QueryNodeBox {
                aggregate: Some(value),
                ..blank_node(QueryNodeKind::Aggregate)
            }
        }
        pub fn sort(value: SortNode) -> QueryNodeBox {
            QueryNodeBox {
                sort: Some(value),
                ..blank_node(QueryNodeKind::Sort)
            }
        }
        pub fn limit(value: LimitNode) -> QueryNodeBox {
            QueryNodeBox {
                limit: Some(value),
                ..blank_node(QueryNodeKind::Limit)
            }
        }
        pub fn join(value: JoinNode) -> QueryNodeBox {
            QueryNodeBox {
                join: Some(value),
                ..blank_node(QueryNodeKind::Join)
            }
        }
        pub fn union(value: UnionNode) -> QueryNodeBox {
            QueryNodeBox {
                union: Some(value),
                ..blank_node(QueryNodeKind::Union)
            }
        }
    }

    /// A node reference: the encoded box that every nesting point carries.
    pub fn node_ref(node: &QueryNodeBox) -> Vec<u8> {
        encode_query_node_box(node)
    }

    fn blank_expression(kind: ExpressionKind) -> ExpressionNode {
        ExpressionNode {
            expression: kind,
            literal: None,
            field: None,
            compare: None,
            set_expr: None,
            text_expr: None,
            null_expr: None,
            logical: None,
            arith: None,
            time_expr: None,
            convert: None,
        }
    }

    /// One expression node, with its discriminant and its field in agreement.
    pub mod expression {
        use super::*;

        pub fn literal(value: TypedValue) -> ExpressionNode {
            ExpressionNode {
                literal: Some(value),
                ..blank_expression(ExpressionKind::Literal)
            }
        }
        pub fn field(value: FieldRef) -> ExpressionNode {
            ExpressionNode {
                field: Some(value),
                ..blank_expression(ExpressionKind::Field)
            }
        }
        pub fn compare(value: CompareExpr) -> ExpressionNode {
            ExpressionNode {
                compare: Some(value),
                ..blank_expression(ExpressionKind::Compare)
            }
        }
        pub fn set(value: SetExpr) -> ExpressionNode {
            ExpressionNode {
                set_expr: Some(value),
                ..blank_expression(ExpressionKind::Set)
            }
        }
        pub fn text(value: TextExpr) -> ExpressionNode {
            ExpressionNode {
                text_expr: Some(value),
                ..blank_expression(ExpressionKind::Text)
            }
        }
        pub fn null_test(value: NullExpr) -> ExpressionNode {
            ExpressionNode {
                null_expr: Some(value),
                ..blank_expression(ExpressionKind::Null)
            }
        }
        pub fn logical(value: LogicalExpr) -> ExpressionNode {
            ExpressionNode {
                logical: Some(value),
                ..blank_expression(ExpressionKind::Logical)
            }
        }
        pub fn arith(value: ArithExpr) -> ExpressionNode {
            ExpressionNode {
                arith: Some(value),
                ..blank_expression(ExpressionKind::Arith)
            }
        }
        pub fn time(value: TimeExpr) -> ExpressionNode {
            ExpressionNode {
                time_expr: Some(value),
                ..blank_expression(ExpressionKind::Time)
            }
        }
        pub fn convert(value: ConvertExpr) -> ExpressionNode {
            ExpressionNode {
                convert: Some(value),
                ..blank_expression(ExpressionKind::Convert)
            }
        }
    }

    /// An expression reference: the encoded node that every nesting point
    /// carries.
    pub fn expression_ref(node: &ExpressionNode) -> Vec<u8> {
        encode_expression_node(node)
    }

    /// A request over the general algebra.
    ///
    /// `allow_partial` is false, because a caller must ask for a partial result
    /// and it is never the default. See D18.
    pub fn request(algebra_version: u64, node: &QueryNodeBox) -> QueryRequest {
        QueryRequest {
            algebra_version,
            consistency: Consistency::Committed,
            max_staleness_ms: None,
            budget: None,
            allow_partial: false,
            comparison_range: None,
            form: QueryForm::Node,
            node: Some(node_ref(node)),
            funnel: None,
            retention: None,
            path: None,
            trace: None,
            timeline: None,
            attribution: None,
            campaign_summary: None,
        }
    }

    /// A request with no query in it, for a caller that fills one of the domain
    /// operator fields.
    pub fn empty_request(algebra_version: u64, form: QueryForm) -> QueryRequest {
        QueryRequest {
            form,
            node: None,
            ..request(algebra_version, &blank_node(QueryNodeKind::Scan))
        }
    }

    /// The whole of a trend query: count the rows of one project's events in
    /// one time range, by one interval.
    pub fn trend(
        algebra_version: u64,
        scan: ScanNode,
        interval_ms: i64,
        alias: &str,
    ) -> QueryRequest {
        use tallyowl_control_api::types::{Interval, Measure, MeasureKind};
        let input = node_ref(&node::scan(scan));
        request(
            algebra_version,
            &node::aggregate(AggregateNode {
                dimensions: Vec::new(),
                measures: vec![Measure {
                    kind: MeasureKind::Count,
                    field: None,
                    quantile: None,
                    k: None,
                    alias: alias.to_string(),
                }],
                interval: Some(Interval {
                    fixed_ms: Some(interval_ms),
                    calendar: None,
                }),
                input,
            }),
        )
    }

    /// A budget a caller attaches to a request.
    pub fn with_budget(mut request: QueryRequest, budget: QueryBudget) -> QueryRequest {
        request.budget = Some(budget);
        request
    }

    /// A breakdown: group one project's events by a property and count them.
    ///
    /// This is the second chart every dashboard draws, after the trend. The
    /// dimension is a field reference, so it reads a built-in column or a
    /// property with the same shape.
    pub fn breakdown(algebra_version: u64, scan: ScanNode, by: &str, alias: &str) -> QueryRequest {
        use tallyowl_control_api::types::{Dimension, Measure, MeasureKind};
        let input = node_ref(&node::scan(scan));
        request(
            algebra_version,
            &node::aggregate(AggregateNode {
                dimensions: vec![Dimension {
                    field: field(by),
                    alias: by.to_string(),
                }],
                measures: vec![Measure {
                    kind: MeasureKind::Count,
                    field: None,
                    quantile: None,
                    k: None,
                    alias: alias.to_string(),
                }],
                interval: None,
                input,
            }),
        )
    }

    /// A field reference by name, with no type selection.
    pub fn field(name: &str) -> FieldRef {
        FieldRef {
            name: name.to_string(),
            value_type: None,
            origin: None,
        }
    }

    /// A field reference that selects one type, for a name that holds several.
    pub fn typed_field(name: &str, value_type: &str) -> FieldRef {
        FieldRef {
            name: name.to_string(),
            value_type: Some(value_type.to_string()),
            origin: None,
        }
    }

    /// Wrap a node in a filter.
    pub fn filtered(input: &QueryNodeBox, predicate: &ExpressionNode) -> QueryNodeBox {
        use tallyowl_control_api::types::FilterNode;
        node::filter(FilterNode {
            filter: expression_ref(predicate),
            input: node_ref(input),
        })
    }

    /// Wrap a node in a sort.
    pub fn sorted(input: &QueryNodeBox, alias: &str, descending: bool) -> QueryNodeBox {
        use tallyowl_control_api::types::{SortKey, SortKey_direction as Direction, SortNode};
        node::sort(SortNode {
            sort: vec![SortKey {
                alias: alias.to_string(),
                direction: if descending {
                    Direction::Desc
                } else {
                    Direction::Asc
                },
            }],
            input: node_ref(input),
        })
    }

    /// Wrap a node in a limit.
    pub fn limited(input: &QueryNodeBox, limit: u64, offset: Option<u64>) -> QueryNodeBox {
        use tallyowl_control_api::types::LimitNode;
        node::limit(LimitNode {
            limit,
            offset,
            cursor: None,
            input: node_ref(input),
        })
    }

    /// Combine several nodes that produce the same columns.
    pub fn unioned(inputs: &[QueryNodeBox]) -> QueryNodeBox {
        use tallyowl_control_api::types::UnionNode;
        node::union(UnionNode {
            union: inputs.iter().map(node_ref).collect(),
        })
    }

    /// `left <op> right`.
    pub fn compare(
        op: tallyowl_control_api::types::CompareOp,
        left: &ExpressionNode,
        right: &ExpressionNode,
    ) -> ExpressionNode {
        expression::compare(CompareExpr {
            compare: op,
            left: expression_ref(left),
            right: expression_ref(right),
        })
    }

    /// A scan of one project's events over one range.
    pub fn events(project_id: &[u8], range: TimeRange) -> ScanNode {
        use tallyowl_control_api::types::Dataset;
        ScanNode {
            scan: Dataset::Events,
            project_id: project_id.to_vec(),
            range,
        }
    }
}

pub mod protocol;
pub mod scrub;

shared_types_bridge!(ingest, tallyowl_ingest_api, "tallyowl-ingest-api");
shared_types_bridge!(collector, tallyowl_collector_api, "tallyowl-collector-api");
shared_types_bridge!(control, tallyowl_control_api, "tallyowl-control-api");

mod ingest_items {
    kind_name_fn!(tallyowl_ingest_api);
    telemetry_item_bridge!(items, tallyowl_ingest_api);
}
mod collector_items {
    kind_name_fn!(tallyowl_collector_api);
    telemetry_item_bridge!(items, tallyowl_collector_api);
}

pub use collector_items::items as collector_items_bridge;
pub use ingest_items::items as ingest_items_bridge;

#[cfg(test)]
mod tests {
    use super::*;
    use tallyowl_collector_api::types::{
        Envelope, EventPayload, PageViewPayload, PropertyOrigin, TelemetryKind, TypedValueKind,
    };

    fn envelope() -> Envelope {
        Envelope {
            event_id: vec![1; 16],
            kind: TelemetryKind::Event,
            schema_version: 1,
            occurred_at: 1_000,
            observed_at: None,
            received_at: None,
            workspace_id: None,
            project_id: None,
            source_id: None,
            sequence: None,
            release: None,
            service_name: None,
            request_id: None,
            session_id: None,
            end_user_id: None,
            anonymous_id: None,
            trace_id: None,
            span_id: None,
            consent: None,
            sdk_name: "test".into(),
            sdk_version: "0".into(),
            properties: Vec::new(),
            measurements: None,
        }
    }

    #[test]
    fn every_value_kind_survives_a_write_and_a_read() {
        let values = [
            Value::Null,
            Value::Boolean(true),
            Value::Integer(-3),
            Value::Unsigned(9),
            Value::Float(0.5),
            Value::Decimal {
                exponent: -2,
                mantissa: 1999,
            },
            Value::Text("us-west2".into()),
            Value::Bytes(vec![1, 2, 3]),
        ];
        for value in values {
            let wire = collector::write(&value);
            assert_eq!(collector::read(&wire).unwrap(), value, "{value:?}");
        }
    }

    #[test]
    fn an_unsigned_value_stays_unsigned_rather_than_becoming_an_integer() {
        // This is the distinction the old bare choice could not carry in a
        // dynamically typed language. It is the reason the shape changed.
        let wire = collector::write(&Value::Unsigned(9));
        assert_eq!(wire.kind, TypedValueKind::Uint);
        assert_eq!(wire.int_value, None);
        assert_eq!(wire.uint_value, Some(9));
    }

    #[test]
    fn a_kind_that_names_an_absent_field_is_refused() {
        let mut wire = collector::write(&Value::Text("x".into()));
        wire.text_value = None;
        let failure = collector::read(&wire).unwrap_err();
        assert!(failure.message.contains("text"), "{}", failure.message);
    }

    #[test]
    fn a_null_value_differs_from_an_absent_one() {
        let wire = collector::write(&Value::Null);
        assert_eq!(wire.kind, TypedValueKind::Null);
        assert_eq!(collector::read(&wire).unwrap(), Value::Null);
    }

    #[test]
    fn money_keeps_its_exact_digits_through_text_and_back() {
        for text in ["19.99", "-0.01", "0.1", "1000", "-12345.6789"] {
            let value = Value::decimal_from_text(text).expect(text);
            assert_eq!(value.to_display(), text);
        }
        assert_eq!(Value::decimal_from_text("not a number"), None);
        assert_eq!(Value::decimal_from_text(""), None);
    }

    #[test]
    fn a_measurement_takes_a_number_and_refuses_anything_else() {
        assert!(collector::measurement("duration", Value::Float(1.5), Some("ms")).is_ok());
        assert!(collector::measurement("count", Value::Integer(3), None).is_ok());
        let failure = collector::measurement("name", Value::Text("x".into()), None).unwrap_err();
        assert!(failure.message.contains("holds a number"), "{failure}");
    }

    #[test]
    fn an_item_carries_the_payload_its_kind_names() {
        let item = collector_items_bridge::event(
            envelope(),
            EventPayload {
                name: "checkout-started".into(),
                route: None,
                page_title: None,
            },
        );
        assert_eq!(item.envelope.kind, TelemetryKind::Event);
        let payload = collector_items_bridge::payload(&item).unwrap();
        assert_eq!(payload.kind_name(), "event");
    }

    #[test]
    fn a_constructor_sets_the_kind_so_the_two_cannot_disagree() {
        // The envelope arrives saying `event` and comes back saying `page-view`,
        // because the payload decides.
        let item = collector_items_bridge::page_view(
            envelope(),
            PageViewPayload {
                route: "/pricing".into(),
                page_title: None,
                referrer: None,
                campaign: None,
            },
        );
        assert_eq!(item.envelope.kind, TelemetryKind::PageView);
    }

    #[test]
    fn an_item_whose_kind_and_payload_disagree_is_refused() {
        let mut item = collector_items_bridge::page_view(
            envelope(),
            PageViewPayload {
                route: "/pricing".into(),
                page_title: None,
                referrer: None,
                campaign: None,
            },
        );
        item.envelope.kind = TelemetryKind::Conversion;
        let failure = collector_items_bridge::payload(&item).unwrap_err();
        assert!(failure.message.contains("conversion"), "{failure}");
        assert!(failure.message.contains("page-view"), "{failure}");
    }

    #[test]
    fn an_item_with_two_payloads_is_refused() {
        let mut item = collector_items_bridge::event(
            envelope(),
            EventPayload {
                name: "a".into(),
                route: None,
                page_title: None,
            },
        );
        item.page_view = Some(PageViewPayload {
            route: "/x".into(),
            page_title: None,
            referrer: None,
            campaign: None,
        });
        let failure = collector_items_bridge::payload(&item).unwrap_err();
        assert!(failure.message.contains("one of them"), "{failure}");
    }

    #[test]
    fn an_item_with_no_payload_is_refused_unless_it_is_a_heartbeat() {
        let item = collector_items_bridge::empty_item(envelope());
        assert!(collector_items_bridge::payload(&item).is_err());

        let mut heartbeat = collector_items_bridge::empty_item(envelope());
        heartbeat.envelope.kind = TelemetryKind::SessionHeartbeat;
        assert_eq!(
            collector_items_bridge::payload(&heartbeat).unwrap(),
            collector_items_bridge::Payload::SessionHeartbeat
        );
    }

    #[test]
    fn a_heartbeat_that_carries_a_payload_is_refused() {
        let mut item = collector_items_bridge::event(
            envelope(),
            EventPayload {
                name: "a".into(),
                route: None,
                page_title: None,
            },
        );
        item.envelope.kind = TelemetryKind::SessionHeartbeat;
        let failure = collector_items_bridge::payload(&item).unwrap_err();
        assert!(failure.message.contains("heartbeat"), "{failure}");
    }

    #[test]
    fn every_kind_names_its_field_or_says_it_has_none() {
        use tallyowl_collector_api::types::TelemetryKind as K;
        let kinds = [
            K::Event,
            K::PageView,
            K::SessionStart,
            K::SessionEnd,
            K::SessionHeartbeat,
            K::Interaction,
            K::FeatureExposure,
            K::Identify,
            K::Alias,
            K::Group,
            K::Conversion,
            K::Error,
            K::Span,
            K::MetricPoint,
            K::CampaignTouch,
            K::CampaignCost,
        ];
        for kind in kinds {
            let named = collector_items_bridge::field_for(&kind);
            if matches!(kind, K::SessionHeartbeat) {
                assert_eq!(named, None);
            } else {
                assert!(named.is_some(), "{kind:?} names no payload field");
            }
        }
    }

    #[test]
    fn the_property_origin_names_match_the_specification() {
        assert_eq!(collector::origin_name(&PropertyOrigin::Client), "client");
        assert_eq!(collector::origin_name(&PropertyOrigin::Driver), "driver");
        assert_eq!(
            collector::origin_name(&PropertyOrigin::Collector),
            "collector"
        );
    }

    #[test]
    fn the_three_packages_write_one_value_the_same_way() {
        // Each package carries its own copy of the shared types, so this is the
        // check that the copies still agree.
        let value = Value::Text("us-west2".into());
        let a = tallyowl_ingest_api::codec::encode_typed_value(&ingest::write(&value));
        let b = tallyowl_collector_api::codec::encode_typed_value(&collector::write(&value));
        let c = tallyowl_control_api::codec::encode_typed_value(&control::write(&value));
        assert_eq!(a, b);
        assert_eq!(b, c);
    }
}
