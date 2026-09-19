//! The expression evaluator.
//!
//! `docs/QUERY.md` section 5 lists the permitted nodes and the whole list is
//! here. There is no user-defined function, no regular expression, and no
//! arbitrary code, so this evaluator has a fixed and small surface.
//!
//! # Three values, not two
//!
//! A comparison against a value that is not there is **unknown**, not false.
//! `price > 10` over a row with no price does not mean the price is ten or
//! less. A filter keeps a row only when its predicate is definitely true, so an
//! unknown drops the row, and `not(unknown)` stays unknown rather than becoming
//! true. Without that rule `filter(not(price > 10))` and `filter(price <= 10)`
//! would return different sets and both would look right.
//!
//! `docs/QUERY.md` does not say which of the two to use. `FAILURE_MODES.md`
//! section 2 ranks a silent wrong answer above a stopped request, and two-valued
//! logic over sparse columns produces exactly that.
//!
//! # A name can hold more than one type
//!
//! D20 says so, and says a reference selects one, and that a reference matching
//! several types without a selection is an error. That check happens once
//! against the rows in scope rather than once for each row, so a query that
//! cannot be answered exactly fails before it produces a number.
//!
//! # Depth
//!
//! `docs/QUERY.md` section 5 promises a configurable maximum nesting depth with
//! a default of 16, and L016 recorded that nothing enforced it. This module
//! enforces it. An unenforced limit that a document promises is worse than no
//! limit, because a reader budgets against it.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use tallyowl_control_api::codec::decode_expression_node;
use tallyowl_control_api::types::{
    ArithExpr, ArithOp, CompareExpr, CompareOp, ConvertExpr, ExpressionKind, ExpressionNode,
    FieldRef, Interval, LogicalExpr, LogicalOp, NullExpr, PropertyOrigin, SetExpr, TextExpr,
    TimeExpr,
};
use tallyowl_obs::error::TallyOwlError;
use tallyowl_store::row::{EventRow, PropertyValue};

/// The nesting depth an expression may reach. `docs/QUERY.md` section 5 gives
/// the default; the head reads its own value from configuration.
pub const DEFAULT_MAX_DEPTH: u32 = 16;

/// What one expression produced for one row.
///
/// `Unknown` is the third value. It is not a `PropertyValue`, because it is not
/// a value that could be stored: it means the question did not apply.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Known(PropertyValue),
    Unknown,
}

impl Outcome {
    fn text(value: impl Into<String>) -> Outcome {
        Outcome::Known(PropertyValue::Text(value.into()))
    }

    fn boolean(value: bool) -> Outcome {
        Outcome::Known(PropertyValue::Boolean(value))
    }

    /// The truth value, when there is one.
    pub fn truth(&self) -> Option<bool> {
        match self {
            Outcome::Known(PropertyValue::Boolean(v)) => Some(*v),
            Outcome::Known(PropertyValue::Null) | Outcome::Unknown => None,
            // A non-boolean in a boolean position is not a truth value. It is
            // refused when the expression is prepared, so reaching here means
            // an arm that produced one where a boolean was promised.
            Outcome::Known(_) => None,
        }
    }

    pub fn value(&self) -> Option<&PropertyValue> {
        match self {
            Outcome::Known(value) => Some(value),
            Outcome::Unknown => None,
        }
    }
}

/// A prepared expression. Preparing decodes the whole tree once, enforces the
/// depth limit, and resolves every field reference against the rows in scope.
#[derive(Debug, Clone)]
pub struct Prepared {
    node: Node,
}

/// The decoded tree. A child is a real child here; only the wire form is
/// encoded bytes, so a query walks a tree rather than decoding at each step.
#[derive(Debug, Clone)]
enum Node {
    Literal(PropertyValue),
    Field(Field),
    Compare(CompareOp, Box<Node>, Box<Node>),
    Set {
        negated: bool,
        left: Box<Node>,
        values: Vec<PropertyValue>,
    },
    Text {
        test: TextTest,
        left: Box<Node>,
        pattern: String,
    },
    Null {
        negated: bool,
        operand: Box<Node>,
    },
    Logical(LogicalOp, Vec<Node>),
    Arith(ArithOp, Box<Node>, Box<Node>),
    Time {
        function: TimeFunction,
        operand: Box<Node>,
        interval_ms: Option<i64>,
        part: Option<String>,
        shift_ms: Option<i64>,
    },
    Convert {
        to: String,
        operand: Box<Node>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextTest {
    StartsWith,
    EndsWith,
    Contains,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeFunction {
    Truncate,
    Extract,
    Shift,
}

/// Where a field reference reads from.
#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    /// The type this reference selected, when a name holds more than one.
    pub value_type: Option<String>,
    pub origin: Option<PropertyOrigin>,
}

/// The built-in column names, which are the correlation fields plus the three
/// time facts. Everything else is a property.
///
/// Keeping this list explicit means a property called `kind` cannot shadow the
/// column called `kind`, in either direction, silently.
pub const BUILT_IN: &[&str] = &[
    "event_id",
    "batch_id",
    "workspace_id",
    "project_id",
    "source_id",
    "kind",
    "name",
    "occurred_at",
    "received_at",
    "committed_at",
    "session_id",
    "request_id",
    "trace_id",
    "service_name",
    "release",
];

impl Field {
    /// Read this field from one row.
    pub fn read(&self, row: &EventRow) -> Outcome {
        if let Some(built_in) = read_built_in(&self.name, row) {
            return built_in;
        }
        let Some((value, origin)) = row.properties.get(&self.name) else {
            return Outcome::Unknown;
        };
        if let Some(wanted) = &self.origin {
            if origin != origin_name(wanted) {
                return Outcome::Unknown;
            }
        }
        if let Some(wanted) = &self.value_type {
            if value.type_name() != wanted {
                // A selection that this row does not match is not this row's
                // value. It is not an error either: a name holding two types is
                // the case D20 exists for.
                return Outcome::Unknown;
            }
        }
        Outcome::Known(value.clone())
    }
}

fn read_built_in(name: &str, row: &EventRow) -> Option<Outcome> {
    let bytes = |value: [u8; 16]| Some(Outcome::Known(PropertyValue::Bytes(value.to_vec())));
    let optional_text = |value: &Option<String>| {
        Some(match value {
            Some(text) => Outcome::text(text.clone()),
            None => Outcome::Unknown,
        })
    };
    match name {
        "event_id" => bytes(row.event_id),
        "batch_id" => bytes(row.batch_id),
        "workspace_id" => bytes(row.workspace_id),
        "project_id" => bytes(row.project_id),
        "source_id" => bytes(row.source_id),
        "kind" => Some(Outcome::text(row.kind.clone())),
        "name" => Some(Outcome::text(row.name.clone())),
        "occurred_at" => Some(Outcome::Known(PropertyValue::Integer(row.occurred_at))),
        "received_at" => Some(Outcome::Known(PropertyValue::Integer(row.received_at))),
        "committed_at" => Some(Outcome::Known(PropertyValue::Integer(row.committed_at))),
        "session_id" => optional_text(&row.session_id),
        "request_id" => optional_text(&row.request_id),
        "service_name" => optional_text(&row.service_name),
        "release" => optional_text(&row.release),
        "trace_id" => Some(match row.trace_id {
            Some(id) => Outcome::Known(PropertyValue::Bytes(id.to_vec())),
            None => Outcome::Unknown,
        }),
        _ => None,
    }
}

pub fn origin_name(origin: &PropertyOrigin) -> &'static str {
    match origin {
        PropertyOrigin::Client => "client",
        PropertyOrigin::Driver => "driver",
        PropertyOrigin::Collector => "collector",
    }
}

// ---------------------------------------------------------------------------
// Preparing
// ---------------------------------------------------------------------------

/// The one field a node pins to one exact value, when it does.
fn exact_match(node: &Node) -> Option<(Field, PropertyValue)> {
    match node {
        Node::Compare(CompareOp::Eq, left, right) => match (left.as_ref(), right.as_ref()) {
            (Node::Field(field), Node::Literal(value))
            | (Node::Literal(value), Node::Field(field)) => Some((field.clone(), value.clone())),
            _ => None,
        },
        // Every branch of an `and` must hold, so pruning on one keeps every row
        // the whole predicate would keep. `or` and `not` are deliberately
        // absent: either can match a row that carries a different value.
        Node::Logical(LogicalOp::And, parts) => parts.iter().find_map(exact_match),
        _ => None,
    }
}

/// Decode an encoded expression and check it against the rows it will run over.
pub fn prepare(
    encoded: &[u8],
    rows: &[EventRow],
    max_depth: u32,
) -> Result<Prepared, TallyOwlError> {
    let node = decode(encoded, 0, max_depth)?;
    check_types(&node, rows)?;
    Ok(Prepared { node })
}

/// Prepare without a type check, for a place that has no rows in scope yet.
pub fn prepare_unchecked(encoded: &[u8], max_depth: u32) -> Result<Prepared, TallyOwlError> {
    Ok(Prepared {
        node: decode(encoded, 0, max_depth)?,
    })
}

impl Prepared {
    /// The one field this predicate pins to one exact value, when it does.
    ///
    /// **This is what turns a point lookup from a scan into a lookup.** The
    /// store's locator can name the few segments that hold a value, and it can
    /// only be asked when the query says which value. A predicate of the shape
    /// `field == literal`, or an `and` with one of those in it, says so.
    ///
    /// An `and` qualifies because every branch of an `and` must hold, so
    /// pruning on one of them keeps every row the whole predicate would keep.
    /// An `or` does not, and neither does a `not`: either can match a row that
    /// carries a different value entirely.
    ///
    /// The caller still evaluates the whole predicate on what comes back. This
    /// only decides which rows are read.
    pub fn exact_match(&self) -> Option<(Field, PropertyValue)> {
        exact_match(&self.node)
    }

    /// Evaluate against one row.
    pub fn evaluate(&self, row: &EventRow) -> Outcome {
        evaluate(&self.node, row)
    }

    /// Whether this row satisfies the expression. Unknown is not satisfied.
    pub fn keeps(&self, row: &EventRow) -> bool {
        self.evaluate(row).truth().unwrap_or(false)
    }

    /// Every field this expression reads. A caller uses it to say which columns
    /// a query touched.
    pub fn fields(&self) -> Vec<Field> {
        let mut out = Vec::new();
        collect_fields(&self.node, &mut out);
        out
    }
}

fn decode(encoded: &[u8], depth: u32, max_depth: u32) -> Result<Node, TallyOwlError> {
    if depth >= max_depth {
        return Err(TallyOwlError::new(
            tallyowl_obs::ErrorCode::BudgetExceeded,
            format!(
                "This query nests expressions more than {max_depth} deep. Simplify it, or raise \
                 `query.maxExpressionDepth`."
            ),
        )
        .retryable(false));
    }
    let node = decode_expression_node(encoded).map_err(|e| {
        TallyOwlError::invalid_argument(format!("Part of this query could not be read. {e}"))
    })?;
    build(&node, depth, max_depth)
}

fn build(node: &ExpressionNode, depth: u32, max_depth: u32) -> Result<Node, TallyOwlError> {
    let child = |encoded: &[u8]| decode(encoded, depth + 1, max_depth);
    match node.expression {
        ExpressionKind::Literal => {
            let literal = node.literal.as_ref().ok_or_else(|| missing("a literal"))?;
            Ok(Node::Literal(crate::project::to_store_value(
                tallyowl_wire::control::read(literal)
                    .map_err(|e| TallyOwlError::invalid_argument(e.message))?,
            )))
        }
        ExpressionKind::Field => {
            let field: &FieldRef = node.field.as_ref().ok_or_else(|| missing("a field"))?;
            Ok(Node::Field(Field {
                name: field.name.clone(),
                value_type: field.value_type.clone(),
                origin: field.origin.clone(),
            }))
        }
        ExpressionKind::Compare => {
            let compare: &CompareExpr = node
                .compare
                .as_ref()
                .ok_or_else(|| missing("a comparison"))?;
            Ok(Node::Compare(
                compare.compare.clone(),
                Box::new(child(&compare.left)?),
                Box::new(child(&compare.right)?),
            ))
        }
        ExpressionKind::Set => {
            let set: &SetExpr = node
                .set_expr
                .as_ref()
                .ok_or_else(|| missing("a set test"))?;
            use tallyowl_control_api::types::SetExpr_set_test as Test;
            let values = set
                .values
                .iter()
                .map(|value| {
                    tallyowl_wire::control::read(value)
                        .map(crate::project::to_store_value)
                        .map_err(|e| TallyOwlError::invalid_argument(e.message))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Node::Set {
                negated: set.set_test == Test::NotIn,
                left: Box::new(child(&set.left)?),
                values,
            })
        }
        ExpressionKind::Text => {
            let text: &TextExpr = node
                .text_expr
                .as_ref()
                .ok_or_else(|| missing("a text test"))?;
            use tallyowl_control_api::types::TextExpr_text_test as Test;
            Ok(Node::Text {
                test: match text.text_test {
                    Test::StartsWith => TextTest::StartsWith,
                    Test::EndsWith => TextTest::EndsWith,
                    Test::Contains => TextTest::Contains,
                },
                left: Box::new(child(&text.left)?),
                pattern: text.pattern.clone(),
            })
        }
        ExpressionKind::Null => {
            let null: &NullExpr = node
                .null_expr
                .as_ref()
                .ok_or_else(|| missing("a null test"))?;
            use tallyowl_control_api::types::NullExpr_null_test as Test;
            Ok(Node::Null {
                negated: null.null_test == Test::IsNotNull,
                operand: Box::new(child(&null.operand)?),
            })
        }
        ExpressionKind::Logical => {
            let logical: &LogicalExpr = node
                .logical
                .as_ref()
                .ok_or_else(|| missing("a logical operator"))?;
            if logical.operands.is_empty() {
                return Err(TallyOwlError::invalid_argument(
                    "This query has an `and`, an `or`, or a `not` with nothing in it.",
                ));
            }
            if logical.logical == LogicalOp::Not && logical.operands.len() != 1 {
                return Err(TallyOwlError::invalid_argument(
                    "A `not` applies to one thing. This one names more than one.",
                ));
            }
            let operands = logical
                .operands
                .iter()
                .map(|operand| child(operand))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Node::Logical(logical.logical.clone(), operands))
        }
        ExpressionKind::Arith => {
            let arith: &ArithExpr = node
                .arith
                .as_ref()
                .ok_or_else(|| missing("an arithmetic"))?;
            Ok(Node::Arith(
                arith.arith.clone(),
                Box::new(child(&arith.left)?),
                Box::new(child(&arith.right)?),
            ))
        }
        ExpressionKind::Time => {
            let time: &TimeExpr = node
                .time_expr
                .as_ref()
                .ok_or_else(|| missing("a time function"))?;
            use tallyowl_control_api::types::TimeExpr_time_fn as Function;
            Ok(Node::Time {
                function: match time.time_fn {
                    Function::Truncate => TimeFunction::Truncate,
                    Function::Extract => TimeFunction::Extract,
                    Function::Shift => TimeFunction::Shift,
                },
                operand: Box::new(child(&time.operand)?),
                interval_ms: time.interval.as_ref().and_then(interval_ms),
                part: time.part.clone(),
                shift_ms: time.shift_ms,
            })
        }
        ExpressionKind::Convert => {
            let convert: &ConvertExpr = node
                .convert
                .as_ref()
                .ok_or_else(|| missing("a conversion"))?;
            Ok(Node::Convert {
                to: convert.convert_to.clone(),
                operand: Box::new(child(&convert.operand)?),
            })
        }
    }
}

fn interval_ms(interval: &Interval) -> Option<i64> {
    if let Some(fixed) = interval.fixed_ms {
        return Some(fixed);
    }
    use tallyowl_control_api::types::Interval_calendar as Calendar;
    interval
        .calendar
        .as_ref()
        .and_then(|calendar| match calendar {
            Calendar::Hour => Some(3_600_000),
            Calendar::Day => Some(86_400_000),
            Calendar::Week => Some(604_800_000),
            // A calendar month is not a fixed span, so it is not a millisecond
            // count. The executor refuses it by name rather than using thirty days,
            // which is wrong for eleven months of the year.
            Calendar::Month => None,
        })
}

fn collect_fields(node: &Node, out: &mut Vec<Field>) {
    match node {
        Node::Literal(_) => {}
        Node::Field(field) => out.push(field.clone()),
        Node::Compare(_, left, right) | Node::Arith(_, left, right) => {
            collect_fields(left, out);
            collect_fields(right, out);
        }
        Node::Set { left, .. } | Node::Text { left, .. } => collect_fields(left, out),
        Node::Null { operand, .. } | Node::Time { operand, .. } | Node::Convert { operand, .. } => {
            collect_fields(operand, out)
        }
        Node::Logical(_, operands) => {
            for operand in operands {
                collect_fields(operand, out);
            }
        }
    }
}

/// D20: a reference that matches several types without a selection is an error.
///
/// The check runs once against the rows in scope, not once for each row, so a
/// query that cannot be answered exactly fails before it produces a number
/// rather than after.
fn check_types(node: &Node, rows: &[EventRow]) -> Result<(), TallyOwlError> {
    let mut fields = Vec::new();
    collect_fields(node, &mut fields);
    for field in fields {
        if field.value_type.is_some() || BUILT_IN.contains(&field.name.as_str()) {
            continue;
        }
        let mut kinds: BTreeSet<&'static str> = BTreeSet::new();
        for row in rows {
            if let Some((value, _)) = row.properties.get(&field.name) {
                kinds.insert(value.type_name());
            }
        }
        if kinds.len() > 1 {
            let named: Vec<&str> = kinds.into_iter().collect();
            return Err(TallyOwlError::new(
                tallyowl_obs::ErrorCode::InvalidArgument,
                format!(
                    "The property `{}` holds more than one type here: {}. Say which one this \
                     query means.",
                    field.name,
                    named.join(", ")
                ),
            )
            .retryable(false));
        }
    }
    Ok(())
}

fn missing(named: &str) -> TallyOwlError {
    TallyOwlError::invalid_argument(format!(
        "Part of this query says it is {named} and carries no {named}. Send the whole query."
    ))
}

// ---------------------------------------------------------------------------
// Evaluating
// ---------------------------------------------------------------------------

fn evaluate(node: &Node, row: &EventRow) -> Outcome {
    match node {
        Node::Literal(value) => Outcome::Known(value.clone()),
        Node::Field(field) => field.read(row),
        Node::Compare(op, left, right) => {
            let (Some(left), Some(right)) = (
                evaluate(left, row).value().cloned(),
                evaluate(right, row).value().cloned(),
            ) else {
                return Outcome::Unknown;
            };
            match compare(&left, &right) {
                Some(ordering) => Outcome::boolean(match op {
                    CompareOp::Eq => ordering == Ordering::Equal,
                    CompareOp::Ne => ordering != Ordering::Equal,
                    CompareOp::Lt => ordering == Ordering::Less,
                    CompareOp::Le => ordering != Ordering::Greater,
                    CompareOp::Gt => ordering == Ordering::Greater,
                    CompareOp::Ge => ordering != Ordering::Less,
                }),
                // Two values that cannot be ordered against each other. Not
                // false: a text and a number are not "not equal", the question
                // does not apply.
                None => Outcome::Unknown,
            }
        }
        Node::Set {
            negated,
            left,
            values,
        } => {
            let Some(left) = evaluate(left, row).value().cloned() else {
                return Outcome::Unknown;
            };
            let found = values
                .iter()
                .any(|value| compare(&left, value) == Some(Ordering::Equal));
            Outcome::boolean(found != *negated)
        }
        Node::Text {
            test,
            left,
            pattern,
        } => {
            let Some(PropertyValue::Text(text)) = evaluate(left, row).value().cloned() else {
                return Outcome::Unknown;
            };
            Outcome::boolean(match test {
                TextTest::StartsWith => text.starts_with(pattern.as_str()),
                TextTest::EndsWith => text.ends_with(pattern.as_str()),
                TextTest::Contains => text.contains(pattern.as_str()),
            })
        }
        Node::Null { negated, operand } => {
            // A null test is the one test that answers over an absent value.
            // That is what it is for, so it is never unknown.
            let outcome = evaluate(operand, row);
            let is_null = matches!(
                outcome,
                Outcome::Unknown | Outcome::Known(PropertyValue::Null)
            );
            Outcome::boolean(is_null != *negated)
        }
        Node::Logical(op, operands) => {
            let truths: Vec<Option<bool>> =
                operands.iter().map(|o| evaluate(o, row).truth()).collect();
            match op {
                LogicalOp::And => {
                    if truths.contains(&Some(false)) {
                        Outcome::boolean(false)
                    } else if truths.iter().all(|t| *t == Some(true)) {
                        Outcome::boolean(true)
                    } else {
                        Outcome::Unknown
                    }
                }
                LogicalOp::Or => {
                    if truths.contains(&Some(true)) {
                        Outcome::boolean(true)
                    } else if truths.iter().all(|t| *t == Some(false)) {
                        Outcome::boolean(false)
                    } else {
                        Outcome::Unknown
                    }
                }
                // The rule that makes the whole thing consistent: negating an
                // unknown gives an unknown, so `not(a > b)` and `a <= b` keep
                // the same rows.
                LogicalOp::Not => match truths[0] {
                    Some(value) => Outcome::boolean(!value),
                    None => Outcome::Unknown,
                },
            }
        }
        Node::Arith(op, left, right) => {
            let (Some(left), Some(right)) = (
                evaluate(left, row).value().cloned(),
                evaluate(right, row).value().cloned(),
            ) else {
                return Outcome::Unknown;
            };
            arithmetic(op, &left, &right)
        }
        Node::Time {
            function,
            operand,
            interval_ms,
            part,
            shift_ms,
        } => {
            let Some(at) = evaluate(operand, row).value().and_then(as_integer) else {
                return Outcome::Unknown;
            };
            match function {
                TimeFunction::Truncate => match interval_ms {
                    Some(interval) if *interval > 0 => {
                        Outcome::Known(PropertyValue::Integer(at - at.rem_euclid(*interval)))
                    }
                    _ => Outcome::Unknown,
                },
                TimeFunction::Shift => {
                    Outcome::Known(PropertyValue::Integer(at + shift_ms.unwrap_or(0)))
                }
                TimeFunction::Extract => match part.as_deref() {
                    Some(part) => extract(at, part),
                    None => Outcome::Unknown,
                },
            }
        }
        Node::Convert { to, operand } => {
            let Some(value) = evaluate(operand, row).value().cloned() else {
                return Outcome::Unknown;
            };
            convert(&value, to)
        }
    }
}

/// Order two values, when they can be ordered.
///
/// Numbers order against numbers whatever their storage type, because a query
/// that asked `attempts > 2` should not depend on whether the driver sent 2 as
/// signed or unsigned. Anything else orders only against its own kind.
pub fn compare(left: &PropertyValue, right: &PropertyValue) -> Option<Ordering> {
    use PropertyValue as P;
    match (left, right) {
        (P::Null, P::Null) => Some(Ordering::Equal),
        (P::Boolean(a), P::Boolean(b)) => Some(a.cmp(b)),
        (P::Text(a), P::Text(b)) => Some(a.cmp(b)),
        (P::Bytes(a), P::Bytes(b)) => Some(a.cmp(b)),
        // A decimal keeps its exact digits, so two decimals compare exactly
        // rather than through a float. Money that compared through a float
        // would make 19.99 and 19.990000000000002 different amounts.
        (P::Decimal(a), P::Decimal(b)) => decimal_compare(a, b),
        (P::Decimal(a), other) => other
            .clone()
            .pipe_decimal()
            .and_then(|b| decimal_compare(a, &b)),
        (other, P::Decimal(b)) => other
            .clone()
            .pipe_decimal()
            .and_then(|a| decimal_compare(&a, b)),
        _ => {
            let a = as_float(left)?;
            let b = as_float(right)?;
            a.partial_cmp(&b)
        }
    }
}

trait PipeDecimal {
    fn pipe_decimal(self) -> Option<String>;
}

impl PipeDecimal for PropertyValue {
    /// The decimal text of a number, so a decimal can be compared against an
    /// integer without either of them passing through a float.
    fn pipe_decimal(self) -> Option<String> {
        match self {
            PropertyValue::Integer(v) => Some(v.to_string()),
            PropertyValue::Unsigned(v) => Some(v.to_string()),
            // A float has no exact decimal form, so this is where the exactness
            // stops. Comparing a decimal against a float is a question the
            // caller should not be asking, and it answers unknown.
            _ => None,
        }
    }
}

/// Compare two exact decimals without a float anywhere.
fn decimal_compare(left: &str, right: &str) -> Option<Ordering> {
    let (left_negative, left_whole, left_fraction) = split_decimal(left)?;
    let (right_negative, right_whole, right_fraction) = split_decimal(right)?;

    if left_negative != right_negative {
        // Zero is not negative, whichever way it was written.
        let left_zero = left_whole.iter().all(|d| *d == 0) && left_fraction.iter().all(|d| *d == 0);
        let right_zero =
            right_whole.iter().all(|d| *d == 0) && right_fraction.iter().all(|d| *d == 0);
        if left_zero && right_zero {
            return Some(Ordering::Equal);
        }
        return Some(if left_negative {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }

    let magnitude = compare_digits(&left_whole, &left_fraction, &right_whole, &right_fraction);
    Some(if left_negative {
        magnitude.reverse()
    } else {
        magnitude
    })
}

fn compare_digits(
    left_whole: &[u8],
    left_fraction: &[u8],
    right_whole: &[u8],
    right_fraction: &[u8],
) -> Ordering {
    let left_significant = trim_leading(left_whole);
    let right_significant = trim_leading(right_whole);
    match left_significant.len().cmp(&right_significant.len()) {
        Ordering::Equal => {}
        other => return other,
    }
    match left_significant.cmp(right_significant) {
        Ordering::Equal => {}
        other => return other,
    }
    // The whole parts are equal, so the fractions decide, digit by digit.
    let most = left_fraction.len().max(right_fraction.len());
    for index in 0..most {
        let a = left_fraction.get(index).copied().unwrap_or(0);
        let b = right_fraction.get(index).copied().unwrap_or(0);
        match a.cmp(&b) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

fn trim_leading(digits: &[u8]) -> &[u8] {
    let start = digits.iter().position(|d| *d != 0).unwrap_or(digits.len());
    &digits[start..]
}

/// Split decimal text into a sign, whole digits, and fraction digits.
fn split_decimal(text: &str) -> Option<(bool, Vec<u8>, Vec<u8>)> {
    let text = text.trim();
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if rest.is_empty() {
        return None;
    }
    let (whole, fraction) = match rest.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (rest, ""),
    };
    let digits = |part: &str| -> Option<Vec<u8>> {
        part.chars()
            .map(|c| c.to_digit(10).map(|d| d as u8))
            .collect()
    };
    let whole = if whole.is_empty() {
        Vec::new()
    } else {
        digits(whole)?
    };
    let fraction = if fraction.is_empty() {
        Vec::new()
    } else {
        digits(fraction)?
    };
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    Some((negative, whole, fraction))
}

pub fn as_float(value: &PropertyValue) -> Option<f64> {
    match value {
        PropertyValue::Integer(v) => Some(*v as f64),
        PropertyValue::Unsigned(v) => Some(*v as f64),
        PropertyValue::Float(v) => Some(*v),
        PropertyValue::Decimal(text) => text.parse().ok(),
        _ => None,
    }
}

pub fn as_integer(value: &PropertyValue) -> Option<i64> {
    match value {
        PropertyValue::Integer(v) => Some(*v),
        PropertyValue::Unsigned(v) => i64::try_from(*v).ok(),
        _ => None,
    }
}

fn arithmetic(op: &ArithOp, left: &PropertyValue, right: &PropertyValue) -> Outcome {
    // Money never becomes a float. An exact decimal on either side keeps the
    // result exact for the three operations that can stay exact, and a division
    // of exact decimals answers unknown rather than producing digits that are
    // not there.
    if matches!(left, PropertyValue::Decimal(_)) || matches!(right, PropertyValue::Decimal(_)) {
        return decimal_arithmetic(op, left, right);
    }
    let (Some(a), Some(b)) = (as_float(left), as_float(right)) else {
        return Outcome::Unknown;
    };
    // Two integers stay an integer, so a count arithmetic does not come back as
    // a float and print as `4.0`.
    if let (Some(a), Some(b)) = (as_integer(left), as_integer(right)) {
        let value = match op {
            ArithOp::Add => a.checked_add(b),
            ArithOp::Sub => a.checked_sub(b),
            ArithOp::Mul => a.checked_mul(b),
            ArithOp::Div => {
                if b == 0 {
                    None
                } else if a % b == 0 {
                    Some(a / b)
                } else {
                    // Not an exact integer. Fall through to the float below
                    // rather than truncating, which would be a wrong answer
                    // that looks right.
                    None
                }
            }
        };
        if let Some(value) = value {
            return Outcome::Known(PropertyValue::Integer(value));
        }
        if *op != ArithOp::Div {
            // An overflow, rather than an inexact division.
            return Outcome::Unknown;
        }
    }
    if *op == ArithOp::Div && b == 0.0 {
        return Outcome::Unknown;
    }
    Outcome::Known(PropertyValue::Float(match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div => a / b,
    }))
}

fn decimal_arithmetic(op: &ArithOp, left: &PropertyValue, right: &PropertyValue) -> Outcome {
    let (Some(a), Some(b)) = (scaled(left), scaled(right)) else {
        return Outcome::Unknown;
    };
    let scale = a.1.max(b.1);
    let lift = |value: (i128, u32)| -> Option<i128> {
        10i128
            .checked_pow(scale - value.1)
            .and_then(|factor| value.0.checked_mul(factor))
    };
    let (Some(a_lifted), Some(b_lifted)) = (lift(a), lift(b)) else {
        return Outcome::Unknown;
    };
    let (mantissa, result_scale) = match op {
        ArithOp::Add => (a_lifted.checked_add(b_lifted), scale),
        ArithOp::Sub => (a_lifted.checked_sub(b_lifted), scale),
        ArithOp::Mul => (a.0.checked_mul(b.0), a.1 + b.1),
        // An exact division of two decimals is not generally a decimal. One
        // third of a penny has no exact form, so this refuses rather than
        // rounding silently.
        ArithOp::Div => (None, 0),
    };
    match mantissa {
        Some(mantissa) => Outcome::Known(PropertyValue::Decimal(render(mantissa, result_scale))),
        None => Outcome::Unknown,
    }
}

/// A decimal as a mantissa and a scale, so exact arithmetic is integer
/// arithmetic.
fn scaled(value: &PropertyValue) -> Option<(i128, u32)> {
    match value {
        PropertyValue::Integer(v) => Some((*v as i128, 0)),
        PropertyValue::Unsigned(v) => Some((*v as i128, 0)),
        PropertyValue::Decimal(text) => {
            let (negative, whole, fraction) = split_decimal(text)?;
            let mut mantissa: i128 = 0;
            for digit in whole.iter().chain(fraction.iter()) {
                mantissa = mantissa.checked_mul(10)?.checked_add(*digit as i128)?;
            }
            Some((
                if negative { -mantissa } else { mantissa },
                fraction.len() as u32,
            ))
        }
        _ => None,
    }
}

pub fn render(mantissa: i128, scale: u32) -> String {
    if scale == 0 {
        return mantissa.to_string();
    }
    let negative = mantissa < 0;
    let digits = mantissa.unsigned_abs().to_string();
    let padded = if digits.len() <= scale as usize {
        format!(
            "{}{}",
            "0".repeat(scale as usize + 1 - digits.len()),
            digits
        )
    } else {
        digits
    };
    let split = padded.len() - scale as usize;
    let text = format!("{}.{}", &padded[..split], &padded[split..]);
    if negative {
        format!("-{text}")
    } else {
        text
    }
}

/// A part of a timestamp, from `extract`.
///
/// Every part is derived from the millisecond count directly, in UTC. A
/// timezone belongs to the time range rather than to this function, and the
/// range carries one.
fn extract(at: i64, part: &str) -> Outcome {
    let days = at.div_euclid(86_400_000);
    let in_day = at.rem_euclid(86_400_000);
    let value = match part {
        "millisecond" => in_day % 1_000,
        "second" => (in_day / 1_000) % 60,
        "minute" => (in_day / 60_000) % 60,
        "hour" => in_day / 3_600_000,
        // 1970-01-01 was a Thursday, so day 0 is weekday 4 with Sunday as 0.
        "weekday" => (days + 4).rem_euclid(7),
        "year" => civil_from_days(days).0,
        "month" => civil_from_days(days).1,
        "day" => civil_from_days(days).2,
        _ => return Outcome::Unknown,
    };
    Outcome::Known(PropertyValue::Integer(value))
}

/// Days since 1970-01-01 to a civil year, month, and day.
///
/// Howard Hinnant's algorithm, which is exact for every day this system can
/// hold and needs no calendar table.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// A named and explicit conversion. There is no implicit one anywhere.
fn convert(value: &PropertyValue, to: &str) -> Outcome {
    match to {
        "text" => Outcome::text(value.to_display()),
        "int" => match value {
            PropertyValue::Text(text) => match text.parse::<i64>() {
                Ok(number) => Outcome::Known(PropertyValue::Integer(number)),
                Err(_) => Outcome::Unknown,
            },
            PropertyValue::Float(v) => Outcome::Known(PropertyValue::Integer(*v as i64)),
            other => match as_integer(other) {
                Some(number) => Outcome::Known(PropertyValue::Integer(number)),
                None => Outcome::Unknown,
            },
        },
        "float" => match as_float(value) {
            Some(number) => Outcome::Known(PropertyValue::Float(number)),
            None => Outcome::Unknown,
        },
        "decimal" => match value {
            PropertyValue::Decimal(text) => Outcome::Known(PropertyValue::Decimal(text.clone())),
            PropertyValue::Text(text) if split_decimal(text).is_some() => {
                Outcome::Known(PropertyValue::Decimal(text.clone()))
            }
            PropertyValue::Integer(v) => Outcome::Known(PropertyValue::Decimal(v.to_string())),
            PropertyValue::Unsigned(v) => Outcome::Known(PropertyValue::Decimal(v.to_string())),
            // A float has no exact decimal form. Converting one would invent
            // digits, which is exactly what the decimal type exists to prevent.
            _ => Outcome::Unknown,
        },
        "bool" => match value {
            PropertyValue::Boolean(v) => Outcome::boolean(*v),
            PropertyValue::Text(text) => match text.as_str() {
                "true" => Outcome::boolean(true),
                "false" => Outcome::boolean(false),
                _ => Outcome::Unknown,
            },
            _ => Outcome::Unknown,
        },
        _ => Outcome::Unknown,
    }
}
