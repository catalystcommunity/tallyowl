//! The query algebra, against a real store.
//!
//! `docs/QUERY.md` sections 5 to 7 give the operators, the expressions, and the
//! measures. This file exercises each of them through the same path a dashboard
//! uses: build a typed tree, encode it, and read the answer back.
//!
//! `AGENTS.md` forbids mocking the storage interface, so every case here writes
//! rows through the real commit path and reads them back through the real
//! executor.

use std::sync::Arc;

use tallyowl_control_api::codec::{decode_query_response, encode_query_node_box};
use tallyowl_control_api::types::{
    AggregateNode, CompareOp, Dimension, LogicalExpr, LogicalOp, Measure, MeasureKind, NullExpr,
    NullExpr_null_test as NullTest, QueryNodeBox, QueryRequest, SetExpr,
    SetExpr_set_test as SetTest, TextExpr, TextExpr_text_test as TextTest, TimeBasis, TimeRange,
};
use tallyowl_head::expr::DEFAULT_MAX_DEPTH;
use tallyowl_head::query::QueryService;
use tallyowl_store::row::{EventRow, PropertyValue};
use tallyowl_store::{SegmentedStore, Store};
use tallyowl_wire::{control as wire, query, Value};

const PROJECT: [u8; 16] = [9; 16];
const BASE_TIME: i64 = 1_785_628_800_000;

fn directory(name: &str) -> std::path::PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("target"));
    let path = base
        .join("query-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// A store holding the rows a test names, and the executor over it.
///
/// The rows go through the real commit path, so what a query reads is what a
/// query would read in a running head.
fn head(name: &str, rows: Vec<EventRow>) -> QueryService {
    let store: Arc<dyn Store> = Arc::new(SegmentedStore::open(directory(name)).expect("open"));
    store
        .commit([7; 16], [1; 16], rows)
        .expect("the rows commit");
    QueryService {
        store,
        max_runtime_ms: 30_000,
        max_expression_depth: DEFAULT_MAX_DEPTH,
        guards: tallyowl_head::analysis::Guards::default(),
        attribution: Default::default(),
        policy: Default::default(),
        identity: Default::default(),
    }
}

/// One row, with the fields a test cares about and generated values elsewhere.
fn row(id: u8, name: &str, at: i64) -> EventRow {
    let mut row = EventRow::new([id; 16], "event", name, at);
    row.project_id = PROJECT;
    row.workspace_id = [8; 16];
    row.received_at = at + 5;
    row
}

fn range(start: i64, end: i64) -> TimeRange {
    TimeRange {
        range_start: start,
        range_end: end,
        basis: TimeBasis::OccurredAt,
        timezone: None,
    }
}

fn whole_range() -> TimeRange {
    range(BASE_TIME - 1, BASE_TIME + 86_400_000)
}

fn scan() -> QueryNodeBox {
    query::node::scan(query::events(&PROJECT, whole_range()))
}

fn run(service: &QueryService, node: &QueryNodeBox) -> (Vec<String>, Vec<Vec<Value>>) {
    let request = query::request(1, node);
    read(service.run(request).expect("the query runs"))
}

fn read(response: tallyowl_control_api::types::QueryResponse) -> (Vec<String>, Vec<Vec<Value>>) {
    // Through the codec, because that is the path a caller uses and a value
    // that did not survive it would be a defect nobody saw in process.
    let encoded = tallyowl_control_api::codec::encode_query_response(&response);
    let decoded = decode_query_response(&encoded).expect("the answer decodes");
    (
        decoded.columns,
        decoded
            .rows
            .iter()
            .map(|row| row.values.iter().map(|v| wire::read(v).unwrap()).collect())
            .collect(),
    )
}

fn literal(value: Value) -> tallyowl_control_api::types::ExpressionNode {
    query::expression::literal(wire::write(&value))
}

fn field(name: &str) -> tallyowl_control_api::types::ExpressionNode {
    query::expression::field(query::field(name))
}

// ---------------------------------------------------------------------------
// Breakdown
// ---------------------------------------------------------------------------

#[test]
fn a_breakdown_groups_by_a_property_and_counts_each_group() {
    let rows = vec![
        row(1, "checkout-started", BASE_TIME).with_property(
            "route",
            PropertyValue::Text("/pricing".into()),
            "client",
        ),
        row(2, "checkout-started", BASE_TIME + 1).with_property(
            "route",
            PropertyValue::Text("/pricing".into()),
            "client",
        ),
        row(3, "checkout-started", BASE_TIME + 2).with_property(
            "route",
            PropertyValue::Text("/features".into()),
            "client",
        ),
    ];
    let service = head("breakdown", rows);

    let request = query::breakdown(1, query::events(&PROJECT, whole_range()), "route", "events");
    let (columns, rows) = read(service.run(request).expect("the query runs"));

    assert_eq!(columns, vec!["route", "events"]);
    assert_eq!(
        rows,
        vec![
            vec![Value::Text("/features".into()), Value::Unsigned(1)],
            vec![Value::Text("/pricing".into()), Value::Unsigned(2)],
        ],
        "grouped, counted, and in a stable order"
    );
}

#[test]
fn a_breakdown_on_a_built_in_column_works_the_same_way() {
    let service = head(
        "breakdown-builtin",
        vec![
            row(1, "checkout-started", BASE_TIME),
            row(2, "checkout-started", BASE_TIME + 1),
            row(3, "purchase", BASE_TIME + 2),
        ],
    );
    let request = query::breakdown(1, query::events(&PROJECT, whole_range()), "name", "events");
    let (_, rows) = read(service.run(request).unwrap());
    assert_eq!(
        rows,
        vec![
            vec![Value::Text("checkout-started".into()), Value::Unsigned(2)],
            vec![Value::Text("purchase".into()), Value::Unsigned(1)],
        ]
    );
}

#[test]
fn a_row_with_no_value_for_the_dimension_becomes_its_own_group_rather_than_disappearing() {
    // A breakdown that silently dropped the rows with no value would report a
    // total smaller than the trend over the same range, and nobody would be
    // able to reconcile the two numbers.
    let service = head(
        "breakdown-absent",
        vec![
            row(1, "e", BASE_TIME).with_property(
                "plan",
                PropertyValue::Text("pro".into()),
                "client",
            ),
            row(2, "e", BASE_TIME + 1),
        ],
    );
    let request = query::breakdown(1, query::events(&PROJECT, whole_range()), "plan", "events");
    let (_, rows) = read(service.run(request).unwrap());
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|r| r[0] == Value::Null));
    let total: u64 = rows
        .iter()
        .filter_map(|r| match r[1] {
            Value::Unsigned(count) => Some(count),
            _ => None,
        })
        .sum();
    assert_eq!(total, 2, "every row is in exactly one group");
}

#[test]
fn a_dimension_that_holds_two_types_is_refused_rather_than_split_into_two_groups() {
    // D20. The same value under two types would show as two rows that look like
    // a real difference and are not.
    let service = head(
        "breakdown-two-types",
        vec![
            row(1, "e", BASE_TIME).with_property("size", PropertyValue::Integer(3), "client"),
            row(2, "e", BASE_TIME + 1).with_property(
                "size",
                PropertyValue::Text("3".into()),
                "client",
            ),
        ],
    );
    let request = query::breakdown(1, query::events(&PROJECT, whole_range()), "size", "events");
    let failure = service.run(request).unwrap_err();
    assert_eq!(failure.code, tallyowl_obs::ErrorCode::InvalidArgument);
    assert!(failure.message.contains("integer"), "{}", failure.message);
    assert!(failure.message.contains("text"), "{}", failure.message);
}

#[test]
fn a_dimension_that_selects_a_type_answers_over_that_type_only() {
    let service = head(
        "breakdown-typed",
        vec![
            row(1, "e", BASE_TIME).with_property("size", PropertyValue::Integer(3), "client"),
            row(2, "e", BASE_TIME + 1).with_property(
                "size",
                PropertyValue::Text("3".into()),
                "client",
            ),
        ],
    );
    let node = query::node::aggregate(AggregateNode {
        dimensions: vec![Dimension {
            field: query::typed_field("size", "integer"),
            alias: "size".into(),
        }],
        measures: vec![Measure {
            kind: MeasureKind::Count,
            field: None,
            quantile: None,
            k: None,
            alias: "events".into(),
        }],
        interval: None,
        input: query::node_ref(&scan()),
    });
    let (_, rows) = run(&service, &node);
    // The integer row groups under 3; the text row has no integer value, so it
    // groups under nothing. Both rows are still counted.
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|r| r[0] == Value::Integer(3)));
    assert!(rows.iter().any(|r| r[0] == Value::Null));
}

// ---------------------------------------------------------------------------
// Filter
// ---------------------------------------------------------------------------

#[test]
fn a_filter_keeps_only_the_rows_that_satisfy_it() {
    let service = head(
        "filter",
        vec![
            row(1, "checkout-started", BASE_TIME),
            row(2, "purchase", BASE_TIME + 1),
            row(3, "purchase", BASE_TIME + 2),
        ],
    );
    let node = query::filtered(
        &scan(),
        &query::compare(
            CompareOp::Eq,
            &field("name"),
            &literal(Value::Text("purchase".into())),
        ),
    );
    let (_, rows) = run(&service, &node);
    assert_eq!(rows.len(), 2);
}

#[test]
fn a_comparison_against_a_value_that_is_not_there_drops_the_row_rather_than_keeping_it() {
    // Three-valued logic. `price > 10` over a row with no price is unknown, not
    // false and not true.
    let service = head(
        "filter-absent",
        vec![
            row(1, "e", BASE_TIME).with_property("price", PropertyValue::Integer(30), "client"),
            row(2, "e", BASE_TIME + 1),
        ],
    );
    let node = query::filtered(
        &scan(),
        &query::compare(CompareOp::Gt, &field("price"), &literal(Value::Integer(10))),
    );
    let (_, rows) = run(&service, &node);
    assert_eq!(rows.len(), 1);
}

#[test]
fn negating_a_comparison_over_an_absent_value_does_not_bring_the_row_back() {
    // The property that makes three-valued logic consistent. Under two-valued
    // logic `not(price > 10)` and `price <= 10` would return different sets,
    // and both would look right.
    let rows = vec![
        row(1, "e", BASE_TIME).with_property("price", PropertyValue::Integer(30), "client"),
        row(2, "e", BASE_TIME + 1).with_property("price", PropertyValue::Integer(5), "client"),
        row(3, "e", BASE_TIME + 2),
    ];
    let service = head("filter-negated", rows);

    let greater = query::compare(CompareOp::Gt, &field("price"), &literal(Value::Integer(10)));
    let negated = query::expression::logical(LogicalExpr {
        logical: LogicalOp::Not,
        operands: vec![query::expression_ref(&greater)],
    });
    let (_, from_not) = run(&service, &query::filtered(&scan(), &negated));

    let at_most = query::compare(CompareOp::Le, &field("price"), &literal(Value::Integer(10)));
    let (_, from_le) = run(&service, &query::filtered(&scan(), &at_most));

    assert_eq!(from_not.len(), 1, "the row with no price is in neither");
    assert_eq!(from_not, from_le);
}

#[test]
fn a_null_test_answers_over_an_absent_value_because_that_is_what_it_is_for() {
    let service = head(
        "filter-null",
        vec![
            row(1, "e", BASE_TIME).with_property(
                "plan",
                PropertyValue::Text("pro".into()),
                "client",
            ),
            row(2, "e", BASE_TIME + 1),
        ],
    );
    let is_null = query::expression::null_test(NullExpr {
        null_test: NullTest::IsNull,
        operand: query::expression_ref(&field("plan")),
    });
    let (_, rows) = run(&service, &query::filtered(&scan(), &is_null));
    assert_eq!(rows.len(), 1);

    let is_not_null = query::expression::null_test(NullExpr {
        null_test: NullTest::IsNotNull,
        operand: query::expression_ref(&field("plan")),
    });
    let (_, rows) = run(&service, &query::filtered(&scan(), &is_not_null));
    assert_eq!(rows.len(), 1);
}

#[test]
fn a_text_test_and_a_set_test_each_answer_what_they_promise() {
    let service = head(
        "filter-text",
        vec![
            row(1, "checkout-started", BASE_TIME),
            row(2, "checkout-completed", BASE_TIME + 1),
            row(3, "purchase", BASE_TIME + 2),
        ],
    );
    let starts = query::expression::text(TextExpr {
        text_test: TextTest::StartsWith,
        left: query::expression_ref(&field("name")),
        pattern: "checkout".into(),
    });
    let (_, rows) = run(&service, &query::filtered(&scan(), &starts));
    assert_eq!(rows.len(), 2);

    let in_set = query::expression::set(SetExpr {
        set_test: SetTest::In,
        left: query::expression_ref(&field("name")),
        values: vec![
            wire::write(&Value::Text("purchase".into())),
            wire::write(&Value::Text("refund".into())),
        ],
    });
    let (_, rows) = run(&service, &query::filtered(&scan(), &in_set));
    assert_eq!(rows.len(), 1);
}

#[test]
fn an_and_of_two_conditions_keeps_only_the_rows_that_satisfy_both() {
    let service = head(
        "filter-and",
        vec![
            row(1, "purchase", BASE_TIME).with_property(
                "plan",
                PropertyValue::Text("pro".into()),
                "client",
            ),
            row(2, "purchase", BASE_TIME + 1).with_property(
                "plan",
                PropertyValue::Text("free".into()),
                "client",
            ),
            row(3, "refund", BASE_TIME + 2).with_property(
                "plan",
                PropertyValue::Text("pro".into()),
                "client",
            ),
        ],
    );
    let both = query::expression::logical(LogicalExpr {
        logical: LogicalOp::And,
        operands: vec![
            query::expression_ref(&query::compare(
                CompareOp::Eq,
                &field("name"),
                &literal(Value::Text("purchase".into())),
            )),
            query::expression_ref(&query::compare(
                CompareOp::Eq,
                &field("plan"),
                &literal(Value::Text("pro".into())),
            )),
        ],
    });
    let (_, rows) = run(&service, &query::filtered(&scan(), &both));
    assert_eq!(rows.len(), 1);
}

#[test]
fn an_expression_nested_deeper_than_the_limit_is_refused_before_it_runs() {
    // QUERY.md section 5 promises a maximum depth. L016 recorded that nothing
    // enforced it, and an unenforced limit a document promises is worse than
    // none, because a reader budgets against it.
    let service = head("filter-depth", vec![row(1, "e", BASE_TIME)]);
    let mut deep = query::compare(
        CompareOp::Eq,
        &field("name"),
        &literal(Value::Text("e".into())),
    );
    for _ in 0..DEFAULT_MAX_DEPTH + 4 {
        deep = query::expression::logical(LogicalExpr {
            logical: LogicalOp::Not,
            operands: vec![query::expression_ref(&deep)],
        });
    }
    let failure = service
        .run(query::request(1, &query::filtered(&scan(), &deep)))
        .unwrap_err();
    assert_eq!(failure.code, tallyowl_obs::ErrorCode::BudgetExceeded);
    assert!(failure.message.contains("deep"), "{}", failure.message);
}

// ---------------------------------------------------------------------------
// Measures
// ---------------------------------------------------------------------------

fn measure(kind: MeasureKind, field_name: Option<&str>, alias: &str) -> Measure {
    Measure {
        kind,
        field: field_name.map(query::field),
        quantile: None,
        k: None,
        alias: alias.to_string(),
    }
}

fn aggregate_over(measures: Vec<Measure>) -> QueryNodeBox {
    query::node::aggregate(AggregateNode {
        dimensions: Vec::new(),
        measures,
        interval: None,
        input: query::node_ref(&scan()),
    })
}

#[test]
fn a_sum_of_exact_decimals_stays_exact() {
    // The whole reason money is a decimal. Three payments of 19.99 through a
    // float total 59.97000000000001, and a customer's own records say 59.97.
    let service = head(
        "sum-decimal",
        (1..=3)
            .map(|n| {
                row(n, "purchase", BASE_TIME + n as i64).with_property(
                    "value",
                    PropertyValue::Decimal("19.99".into()),
                    "client",
                )
            })
            .collect(),
    );
    let (_, rows) = run(
        &service,
        &aggregate_over(vec![measure(MeasureKind::Sum, Some("value"), "revenue")]),
    );
    assert_eq!(rows[0][0].to_display(), "59.97");
}

#[test]
fn min_max_and_average_read_the_field_they_name() {
    let service = head(
        "measures",
        vec![
            row(1, "e", BASE_TIME).with_property("score", PropertyValue::Integer(10), "client"),
            row(2, "e", BASE_TIME + 1).with_property("score", PropertyValue::Integer(30), "client"),
            row(3, "e", BASE_TIME + 2).with_property("score", PropertyValue::Integer(20), "client"),
        ],
    );
    let (columns, rows) = run(
        &service,
        &aggregate_over(vec![
            measure(MeasureKind::Min, Some("score"), "lowest"),
            measure(MeasureKind::Max, Some("score"), "highest"),
            measure(MeasureKind::Avg, Some("score"), "mean"),
            measure(MeasureKind::Count, None, "events"),
        ]),
    );
    assert_eq!(columns, vec!["lowest", "highest", "mean", "events"]);
    assert_eq!(rows[0][0], Value::Integer(10));
    assert_eq!(rows[0][1], Value::Integer(30));
    assert_eq!(rows[0][2], Value::Float(20.0));
    assert_eq!(rows[0][3], Value::Unsigned(3));
}

#[test]
fn a_row_with_no_value_contributes_nothing_to_an_average_rather_than_a_zero() {
    let service = head(
        "average-absent",
        vec![
            row(1, "e", BASE_TIME).with_property("score", PropertyValue::Integer(10), "client"),
            row(2, "e", BASE_TIME + 1),
        ],
    );
    let (_, rows) = run(
        &service,
        &aggregate_over(vec![measure(MeasureKind::Avg, Some("score"), "mean")]),
    );
    assert_eq!(rows[0][0], Value::Float(10.0), "not five");
}

#[test]
fn count_distinct_counts_values_and_not_rows() {
    let service = head(
        "distinct",
        vec![
            row(1, "e", BASE_TIME).with_property(
                "plan",
                PropertyValue::Text("pro".into()),
                "client",
            ),
            row(2, "e", BASE_TIME + 1).with_property(
                "plan",
                PropertyValue::Text("pro".into()),
                "client",
            ),
            row(3, "e", BASE_TIME + 2).with_property(
                "plan",
                PropertyValue::Text("free".into()),
                "client",
            ),
        ],
    );
    let (_, rows) = run(
        &service,
        &aggregate_over(vec![measure(
            MeasureKind::CountDistinct,
            Some("plan"),
            "plans",
        )]),
    );
    assert_eq!(rows[0][0], Value::Unsigned(2));
}

#[test]
fn a_measure_this_release_does_not_answer_is_refused_by_name() {
    let service = head("unsupported-measure", vec![row(1, "e", BASE_TIME)]);
    let failure = service
        .run(query::request(
            1,
            &aggregate_over(vec![measure(MeasureKind::QuantileApprox, Some("x"), "p95")]),
        ))
        .unwrap_err();
    assert!(!failure.retryable);
    assert!(
        failure.message.contains("quantile_approx"),
        "{}",
        failure.message
    );
}

#[test]
fn every_measure_this_release_answers_says_it_is_exact() {
    // D21: an exact measure never becomes approximate on its own, and every
    // result says which it is.
    let service = head(
        "exactness",
        vec![row(1, "e", BASE_TIME).with_property("score", PropertyValue::Integer(1), "client")],
    );
    let response = service
        .run(query::request(
            1,
            &aggregate_over(vec![
                measure(MeasureKind::Count, None, "events"),
                measure(MeasureKind::Sum, Some("score"), "total"),
            ]),
        ))
        .unwrap();
    assert_eq!(response.metadata.exactness.len(), 2);
    assert!(response.metadata.exactness.iter().all(|e| e.exact));
}

// ---------------------------------------------------------------------------
// Sort, limit, and union
// ---------------------------------------------------------------------------

fn counted_by_name() -> QueryNodeBox {
    query::node::aggregate(AggregateNode {
        dimensions: vec![Dimension {
            field: query::field("name"),
            alias: "name".into(),
        }],
        measures: vec![measure(MeasureKind::Count, None, "events")],
        interval: None,
        input: query::node_ref(&scan()),
    })
}

fn sorted_rows() -> Vec<EventRow> {
    let mut rows = Vec::new();
    let mut id = 0u8;
    for (name, count) in [("a", 1), ("b", 3), ("c", 2)] {
        for _ in 0..count {
            id += 1;
            rows.push(row(id, name, BASE_TIME + id as i64));
        }
    }
    rows
}

#[test]
fn a_sort_orders_a_breakdown_by_its_measure() {
    // A dashboard that cannot order a breakdown is not a dashboard.
    let service = head("sort", sorted_rows());
    let node = query::sorted(&counted_by_name(), "events", true);
    let (_, rows) = run(&service, &node);
    assert_eq!(
        rows.iter().map(|r| r[0].to_display()).collect::<Vec<_>>(),
        vec!["b", "c", "a"]
    );
}

#[test]
fn a_sort_by_a_column_that_is_not_there_names_the_columns_that_are() {
    let service = head("sort-missing", sorted_rows());
    let failure = service
        .run(query::request(
            1,
            &query::sorted(&counted_by_name(), "revenue", true),
        ))
        .unwrap_err();
    assert!(failure.message.contains("revenue"));
    assert!(failure.message.contains("events"), "{}", failure.message);
}

#[test]
fn a_limit_takes_the_first_rows_and_an_offset_skips_them() {
    let service = head("limit", sorted_rows());
    let ordered = query::sorted(&counted_by_name(), "events", true);

    let (_, top) = run(&service, &query::limited(&ordered, 2, None));
    assert_eq!(
        top.iter().map(|r| r[0].to_display()).collect::<Vec<_>>(),
        vec!["b", "c"]
    );

    let (_, skipped) = run(&service, &query::limited(&ordered, 2, Some(1)));
    assert_eq!(
        skipped
            .iter()
            .map(|r| r[0].to_display())
            .collect::<Vec<_>>(),
        vec!["c", "a"]
    );
}

#[test]
fn an_offset_past_the_end_gives_no_rows_rather_than_an_error() {
    let service = head("limit-past-end", sorted_rows());
    let (_, rows) = run(
        &service,
        &query::limited(&counted_by_name(), 10, Some(1_000)),
    );
    assert!(rows.is_empty());
}

#[test]
fn a_union_combines_two_results_with_the_same_columns() {
    let service = head(
        "union",
        vec![
            row(1, "purchase", BASE_TIME),
            row(2, "refund", BASE_TIME + 1),
            row(3, "page-view", BASE_TIME + 2),
        ],
    );
    let one = |name: &str| {
        query::node::aggregate(AggregateNode {
            dimensions: vec![Dimension {
                field: query::field("name"),
                alias: "name".into(),
            }],
            measures: vec![measure(MeasureKind::Count, None, "events")],
            interval: None,
            input: query::node_ref(&query::filtered(
                &scan(),
                &query::compare(
                    CompareOp::Eq,
                    &field("name"),
                    &literal(Value::Text(name.into())),
                ),
            )),
        })
    };
    let (columns, rows) = run(&service, &query::unioned(&[one("purchase"), one("refund")]));
    assert_eq!(columns, vec!["name", "events"]);
    assert_eq!(rows.len(), 2);
}

#[test]
fn a_union_of_two_different_shapes_is_refused_rather_than_stacked() {
    let service = head("union-mismatch", vec![row(1, "e", BASE_TIME)]);
    let by_name = counted_by_name();
    let counted = aggregate_over(vec![measure(MeasureKind::Count, None, "events")]);
    let failure = service
        .run(query::request(1, &query::unioned(&[by_name, counted])))
        .unwrap_err();
    assert!(
        failure.message.contains("same columns"),
        "{}",
        failure.message
    );
}

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

#[test]
fn a_result_larger_than_the_row_budget_is_refused_rather_than_returned() {
    let service = head(
        "budget",
        (1..=5).map(|n| row(n, "e", BASE_TIME + n as i64)).collect(),
    );
    let mut request: QueryRequest = query::request(1, &scan());
    request.budget = Some(tallyowl_control_api::types::QueryBudget {
        deadline_ms: None,
        max_scanned_bytes: None,
        max_scanned_segments: None,
        max_rows: Some(2),
    });
    let failure = service.run(request).unwrap_err();
    assert_eq!(failure.code, tallyowl_obs::ErrorCode::BudgetExceeded);
    assert!(failure.message.contains("Add a limit"));
}

#[test]
fn a_join_is_refused_by_name_rather_than_answered_wrongly() {
    let service = head("join", vec![row(1, "e", BASE_TIME)]);
    let node = query::node::join(tallyowl_control_api::types::JoinNode {
        join_key: query::field("trace_id"),
        max_rows_each_side: 100,
        left: query::node_ref(&scan()),
        right: query::node_ref(&scan()),
    });
    let failure = service.run(query::request(1, &node)).unwrap_err();
    assert!(!failure.retryable);
    assert!(failure.message.contains("join"), "{}", failure.message);
}

#[test]
fn a_projection_selects_the_columns_it_names() {
    let service = head(
        "project",
        vec![row(1, "checkout-started", BASE_TIME).with_property(
            "route",
            PropertyValue::Text("/pricing".into()),
            "client",
        )],
    );
    let node = query::node::project(tallyowl_control_api::types::ProjectNode {
        project_fields: vec![
            Dimension {
                field: query::field("name"),
                alias: "name".into(),
            },
            Dimension {
                field: query::field("route"),
                alias: "route".into(),
            },
        ],
        input: query::node_ref(&scan()),
    });
    let (columns, rows) = run(&service, &node);
    assert_eq!(columns, vec!["name", "route"]);
    assert_eq!(
        rows[0],
        vec![
            Value::Text("checkout-started".into()),
            Value::Text("/pricing".into())
        ]
    );
}

#[test]
fn a_duplicate_delivery_counts_once_through_every_operator() {
    // DELIVERY.md section 6: a primary query counts logical events. The
    // deduplication is above the scan, so a filter and a breakdown see the same
    // rows a count would.
    let mut rows = vec![
        row(1, "purchase", BASE_TIME),
        row(2, "purchase", BASE_TIME + 1),
    ];
    // The same identifier again, as a pathological retry would leave it.
    rows.push(row(1, "purchase", BASE_TIME));
    let service = head("duplicate", rows);

    let (_, counted) = run(
        &service,
        &aggregate_over(vec![measure(MeasureKind::Count, None, "events")]),
    );
    assert_eq!(counted[0][0], Value::Unsigned(2));

    let (_, listed) = run(&service, &scan());
    assert_eq!(listed.len(), 2);
}

/// The encoded form a dashboard sends, so the tree survives the wire.
#[test]
fn a_whole_tree_survives_encoding_and_decoding() {
    let node = query::limited(&query::sorted(&counted_by_name(), "events", true), 1, None);
    let encoded = encode_query_node_box(&node);
    let decoded = tallyowl_control_api::codec::decode_query_node_box(&encoded).unwrap();
    assert_eq!(decoded, node);
}

// ---------------------------------------------------------------------------
// Metric measures
//
// QUERY.md section 12.8: `rate` and `increase` handle a counter reset, and
// `quantile` applies to a histogram through `histogram_merge`. Section 7 says
// `histogram_merge` fails when bucket boundaries do not align, and that
// TallyOwl does not silently rebucket.
// ---------------------------------------------------------------------------

/// One stored metric point, in the shape the projector writes.
fn counter_point(id: u8, series: &str, start_at: i64, end_at: i64, value: f64) -> EventRow {
    let mut row = EventRow::new([id; 16], "metric-point", "requests_total", end_at);
    row.project_id = PROJECT;
    row.workspace_id = [8; 16];
    row.received_at = end_at + 5;
    row = row
        .with_property(
            "metric_name",
            PropertyValue::Text("requests_total".into()),
            "client",
        )
        .with_property(
            "metric_kind",
            PropertyValue::Text("counter".into()),
            "client",
        )
        .with_property(
            "temporality",
            PropertyValue::Text("cumulative".into()),
            "client",
        )
        .with_property("series_key", PropertyValue::Text(series.into()), "client")
        .with_property("start_at", PropertyValue::Integer(start_at), "client")
        .with_property("end_at", PropertyValue::Integer(end_at), "client")
        .with_property("value", PropertyValue::Float(value), "client");
    row
}

fn delta_point(id: u8, series: &str, start_at: i64, end_at: i64, value: f64) -> EventRow {
    let mut row = counter_point(id, series, start_at, end_at, value);
    row.properties.insert(
        "temporality".to_string(),
        (PropertyValue::Text("delta".into()), "client".to_string()),
    );
    row
}

fn histogram_point(id: u8, at: i64, bounds: &str, counts: &str, total: u64, sum: f64) -> EventRow {
    let mut row = EventRow::new([id; 16], "metric-point", "latency_seconds", at);
    row.project_id = PROJECT;
    row.workspace_id = [8; 16];
    row.received_at = at + 5;
    row.with_property(
        "metric_kind",
        PropertyValue::Text("histogram".into()),
        "client",
    )
    .with_property("start_at", PropertyValue::Integer(at), "client")
    .with_property("end_at", PropertyValue::Integer(at), "client")
    .with_property(
        "histogram_bounds",
        PropertyValue::Text(bounds.into()),
        "client",
    )
    .with_property(
        "histogram_counts",
        PropertyValue::Text(counts.into()),
        "client",
    )
    .with_property("histogram_count", PropertyValue::Unsigned(total), "client")
    .with_property("histogram_sum", PropertyValue::Float(sum), "client")
}

fn metric_scan() -> QueryNodeBox {
    query::node::scan(tallyowl_control_api::types::ScanNode {
        scan: tallyowl_control_api::types::Dataset::MetricPoints,
        project_id: PROJECT.to_vec(),
        range: whole_range(),
    })
}

fn metric_aggregate(measures: Vec<Measure>) -> QueryNodeBox {
    query::node::aggregate(AggregateNode {
        dimensions: Vec::new(),
        measures,
        interval: None,
        input: query::node_ref(&metric_scan()),
    })
}

#[test]
fn increase_reads_the_growth_of_a_cumulative_counter_and_not_its_level() {
    // Three readings of one series: 10, 30, 45. The counter grew by 35 inside
    // the range. Reporting 85 would count the level it started at.
    let service = head(
        "increase",
        vec![
            counter_point(1, "s", 1_000, BASE_TIME, 10.0),
            counter_point(2, "s", 1_000, BASE_TIME + 60_000, 30.0),
            counter_point(3, "s", 1_000, BASE_TIME + 120_000, 45.0),
        ],
    );
    let (_, rows) = run(
        &service,
        &metric_aggregate(vec![measure(MeasureKind::Increase, None, "growth")]),
    );
    assert_eq!(rows[0][0].to_display(), "35");
}

#[test]
fn a_counter_reset_counts_the_new_value_rather_than_a_negative_step() {
    // The producer restarted: 100, then 5 with a later start. The increase over
    // the range is 5, and a subtraction would report -95.
    let service = head(
        "reset",
        vec![
            counter_point(1, "s", 1_000, BASE_TIME, 100.0),
            counter_point(2, "s", BASE_TIME + 30_000, BASE_TIME + 60_000, 5.0),
            counter_point(3, "s", BASE_TIME + 30_000, BASE_TIME + 120_000, 12.0),
        ],
    );
    let (_, rows) = run(
        &service,
        &metric_aggregate(vec![measure(MeasureKind::Increase, None, "growth")]),
    );
    // 5 from the restart, then 7 more.
    assert_eq!(rows[0][0].to_display(), "12");
}

#[test]
fn a_value_that_falls_with_the_same_start_is_still_read_as_a_reset() {
    // QUERY.md section 12.8 defines a reset as a decrease in a cumulative
    // series. A producer that restarted without moving its start still gets a
    // correct answer rather than a negative one.
    let service = head(
        "reset-by-decrease",
        vec![
            counter_point(1, "s", 1_000, BASE_TIME, 100.0),
            counter_point(2, "s", 1_000, BASE_TIME + 60_000, 5.0),
        ],
    );
    let (_, rows) = run(
        &service,
        &metric_aggregate(vec![measure(MeasureKind::Increase, None, "growth")]),
    );
    assert_eq!(rows[0][0].to_display(), "5");
}

#[test]
fn two_series_in_one_group_never_read_as_one_that_jumped() {
    // Without per-series state, series `a` at 100 followed by series `b` at 5
    // would look like a reset and report 5 instead of 20.
    let service = head(
        "two-series",
        vec![
            counter_point(1, "a", 1_000, BASE_TIME, 100.0),
            counter_point(2, "b", 1_000, BASE_TIME + 1_000, 5.0),
            counter_point(3, "a", 1_000, BASE_TIME + 60_000, 110.0),
            counter_point(4, "b", 1_000, BASE_TIME + 61_000, 15.0),
        ],
    );
    let (_, rows) = run(
        &service,
        &metric_aggregate(vec![measure(MeasureKind::Increase, None, "growth")]),
    );
    // 10 from `a` and 10 from `b`.
    assert_eq!(rows[0][0].to_display(), "20");
}

#[test]
fn a_delta_series_adds_its_periods_and_looks_for_no_reset() {
    let service = head(
        "delta",
        vec![
            delta_point(1, "s", BASE_TIME - 60_000, BASE_TIME, 5.0),
            delta_point(2, "s", BASE_TIME, BASE_TIME + 60_000, 3.0),
            delta_point(3, "s", BASE_TIME + 60_000, BASE_TIME + 120_000, 4.0),
        ],
    );
    let (_, rows) = run(
        &service,
        &metric_aggregate(vec![measure(MeasureKind::Increase, None, "growth")]),
    );
    assert_eq!(rows[0][0].to_display(), "12");
}

#[test]
fn rate_divides_the_increase_by_the_period_it_covered() {
    // 60 more requests across 120 seconds is half a request each second.
    let service = head(
        "rate",
        vec![
            counter_point(1, "s", 1_000, BASE_TIME, 10.0),
            counter_point(2, "s", 1_000, BASE_TIME + 120_000, 70.0),
        ],
    );
    let (_, rows) = run(
        &service,
        &metric_aggregate(vec![measure(MeasureKind::Rate, None, "each_second")]),
    );
    assert_eq!(rows[0][0].to_display(), "0.5");
}

#[test]
fn a_rate_from_one_reading_is_absent_rather_than_invented() {
    let service = head(
        "rate-one",
        vec![counter_point(1, "s", 1_000, BASE_TIME, 10.0)],
    );
    let (_, rows) = run(
        &service,
        &metric_aggregate(vec![measure(MeasureKind::Rate, None, "each_second")]),
    );
    assert_eq!(rows[0][0], Value::Null);
}

#[test]
fn histogram_merge_adds_bucket_counts_when_the_layouts_agree() {
    let service = head(
        "merge",
        vec![
            histogram_point(1, BASE_TIME, "0.1,0.5,1", "2,5,7", 8, 3.0),
            histogram_point(2, BASE_TIME + 1_000, "0.1,0.5,1", "1,3,4", 5, 2.0),
        ],
    );
    let (_, rows) = run(
        &service,
        &metric_aggregate(vec![measure(MeasureKind::HistogramMerge, None, "merged")]),
    );
    let text = rows[0][0].to_display();
    assert!(text.contains("counts=[3,8,11]"), "{text}");
    assert!(text.contains("count=13"), "{text}");
    assert!(text.contains("sum=5"), "{text}");
}

#[test]
fn histogram_merge_over_two_bucket_layouts_fails_and_names_both() {
    // QUERY.md section 7: it fails when the boundaries do not align, and
    // TallyOwl does not silently rebucket. A merged shape nobody observed would
    // be indistinguishable from a real one.
    let service = head(
        "merge-misaligned",
        vec![
            histogram_point(1, BASE_TIME, "0.1,0.5,1", "2,5,7", 8, 3.0),
            histogram_point(2, BASE_TIME + 1_000, "0.2,0.8", "1,3", 4, 2.0),
        ],
    );
    let request = query::request(
        1,
        &metric_aggregate(vec![measure(MeasureKind::HistogramMerge, None, "merged")]),
    );
    let refused = service.run(request).expect_err("two layouts cannot merge");
    assert_eq!(
        refused.code,
        tallyowl_obs::error::ErrorCode::FailedPrecondition
    );
    assert!(refused.message.contains("0.1,0.5,1"), "{}", refused.message);
    assert!(refused.message.contains("0.2,0.8"), "{}", refused.message);
    assert!(
        refused.message.contains("rebucket"),
        "the refusal says what TallyOwl will not do: {}",
        refused.message
    );
}

#[test]
fn a_quantile_reads_the_merged_buckets() {
    // Ten observations, cumulative counts 2, 5, 10 over bounds 1, 2, 4. The
    // median is the fifth, which lands at the top of the second bucket.
    let service = head(
        "quantile",
        vec![histogram_point(1, BASE_TIME, "1,2,4", "2,5,10", 10, 15.0)],
    );
    let mut measure = measure(MeasureKind::Quantile, None, "p50");
    measure.quantile = Some(0.5);
    let (_, rows) = run(&service, &metric_aggregate(vec![measure]));
    assert_eq!(rows[0][0].to_display(), "2");
}

#[test]
fn a_quantile_interpolates_inside_the_bucket_it_falls_in() {
    // Cumulative 0, 10 over bounds 0, 10: the 90th is nine tenths of the way
    // through the only bucket that holds anything.
    let service = head(
        "quantile-inside",
        vec![histogram_point(1, BASE_TIME, "0,10", "0,10", 10, 50.0)],
    );
    let mut measure = measure(MeasureKind::Quantile, None, "p90");
    measure.quantile = Some(0.9);
    let (_, rows) = run(&service, &metric_aggregate(vec![measure]));
    assert_eq!(rows[0][0].to_display(), "9");
}

#[test]
fn a_quantile_over_two_bucket_layouts_fails_for_the_same_reason_a_merge_does() {
    // A quantile applies to a histogram through `histogram_merge`, so it
    // inherits the alignment rule rather than quietly reading one of the two.
    let service = head(
        "quantile-misaligned",
        vec![
            histogram_point(1, BASE_TIME, "1,2,4", "2,5,10", 10, 15.0),
            histogram_point(2, BASE_TIME + 1_000, "1,3,9", "1,2,3", 3, 5.0),
        ],
    );
    let mut measure = measure(MeasureKind::Quantile, None, "p50");
    measure.quantile = Some(0.5);
    let request = query::request(1, &metric_aggregate(vec![measure]));
    let refused = service.run(request).expect_err("two layouts cannot merge");
    assert_eq!(
        refused.code,
        tallyowl_obs::error::ErrorCode::FailedPrecondition
    );
}

#[test]
fn a_quantile_without_a_number_between_zero_and_one_is_refused_before_any_row_is_read() {
    let service = head("quantile-missing", vec![]);
    let request = query::request(
        1,
        &metric_aggregate(vec![measure(MeasureKind::Quantile, None, "p50")]),
    );
    let refused = service.run(request).expect_err("a quantile needs a number");
    assert!(refused.message.contains("0.99"), "{}", refused.message);

    let mut out_of_range = measure(MeasureKind::Quantile, None, "p50");
    out_of_range.quantile = Some(1.5);
    let request = query::request(1, &metric_aggregate(vec![out_of_range]));
    assert!(service.run(request).is_err());
}

#[test]
fn a_metric_measure_over_a_range_that_holds_nothing_answers_absent() {
    let service = head("metric-empty", vec![]);
    let (_, rows) = run(
        &service,
        &metric_aggregate(vec![measure(MeasureKind::Increase, None, "growth")]),
    );
    assert!(rows.is_empty(), "no rows, no groups");
}

#[test]
fn increase_grouped_by_a_label_reports_each_label_on_its_own() {
    let mut a = counter_point(1, "a", 1_000, BASE_TIME, 10.0);
    a.properties.insert(
        "route".to_string(),
        (PropertyValue::Text("/a".into()), "client".to_string()),
    );
    let mut a2 = counter_point(2, "a", 1_000, BASE_TIME + 60_000, 40.0);
    a2.properties.insert(
        "route".to_string(),
        (PropertyValue::Text("/a".into()), "client".to_string()),
    );
    let mut b = counter_point(3, "b", 1_000, BASE_TIME, 100.0);
    b.properties.insert(
        "route".to_string(),
        (PropertyValue::Text("/b".into()), "client".to_string()),
    );
    let mut b2 = counter_point(4, "b", 1_000, BASE_TIME + 60_000, 105.0);
    b2.properties.insert(
        "route".to_string(),
        (PropertyValue::Text("/b".into()), "client".to_string()),
    );

    let service = head("increase-by-label", vec![a, a2, b, b2]);
    let node = query::node::aggregate(AggregateNode {
        dimensions: vec![Dimension {
            field: query::field("route"),
            alias: "route".into(),
        }],
        measures: vec![measure(MeasureKind::Increase, None, "growth")],
        interval: None,
        input: query::node_ref(&metric_scan()),
    });
    let (_, rows) = run(&service, &node);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0].to_display(), "/a");
    assert_eq!(rows[0][1].to_display(), "30");
    assert_eq!(rows[1][1].to_display(), "5");
}

// ---------------------------------------------------------------------------
// The locator pushdown
//
// A filter over a scan that pins one exact value is answered from the locator
// rather than by materialising the range. The risk of getting this wrong is
// returning **fewer** rows than the truth, which FAILURE_MODES.md section 2
// ranks worst, so every case below asserts the whole answer.
// ---------------------------------------------------------------------------

fn eq(field_name: &str, value: Value) -> tallyowl_control_api::types::ExpressionNode {
    query::compare(CompareOp::Eq, &field(field_name), &literal(value))
}

/// Rows spread over enough distinct values that a locator has something to
/// prune with.
fn correlated_rows() -> Vec<EventRow> {
    (0..40u8)
        .map(|n| {
            let mut r = row(n, "checkout-started", BASE_TIME + n as i64 * 1_000);
            r.request_id = Some(format!("r-{n}"));
            r.session_id = Some(format!("s-{}", n % 4));
            r.with_property("route", PropertyValue::Text(format!("/p/{n}")), "client")
        })
        .collect()
}

#[test]
fn a_point_lookup_on_a_request_id_finds_exactly_the_one_row() {
    let service = head("lookup-request", correlated_rows());
    let node = query::filtered(&scan(), &eq("request_id", Value::Text("r-17".into())));
    let (_, rows) = run(&service, &node);
    assert_eq!(rows.len(), 1);
}

#[test]
fn a_point_lookup_on_a_property_finds_exactly_the_one_row() {
    let service = head("lookup-property", correlated_rows());
    let node = query::filtered(&scan(), &eq("route", Value::Text("/p/23".into())));
    let (_, rows) = run(&service, &node);
    assert_eq!(rows.len(), 1);
}

#[test]
fn a_lookup_that_matches_several_rows_returns_every_one() {
    // A session identifier is not unique. Returning one row would be the
    // failure this whole change has to avoid.
    let service = head("lookup-many", correlated_rows());
    let node = query::filtered(&scan(), &eq("session_id", Value::Text("s-2".into())));
    let (_, rows) = run(&service, &node);
    assert_eq!(rows.len(), 10, "40 rows over 4 sessions is 10 each");
}

#[test]
fn a_value_nothing_carries_returns_nothing_rather_than_everything() {
    let service = head("lookup-absent", correlated_rows());
    let node = query::filtered(&scan(), &eq("request_id", Value::Text("r-none".into())));
    let (_, rows) = run(&service, &node);
    assert!(rows.is_empty());
}

#[test]
fn a_lookup_still_honours_the_time_range() {
    // The store's correlated lookup reads the whole store. The range is applied
    // on the way back, and a row outside it must not appear because the locator
    // found it.
    let service = head("lookup-range", correlated_rows());
    let narrow = query::node::scan(tallyowl_control_api::types::ScanNode {
        scan: tallyowl_control_api::types::Dataset::Events,
        project_id: PROJECT.to_vec(),
        range: range(BASE_TIME - 1, BASE_TIME + 5_000),
    });
    let node = query::filtered(&narrow, &eq("request_id", Value::Text("r-30".into())));
    let (_, rows) = run(&service, &node);
    assert!(
        rows.is_empty(),
        "r-30 occurred at +30 seconds, outside the requested range"
    );
}

#[test]
fn a_lookup_never_reaches_another_projects_rows() {
    // A correlated lookup reads across the whole store, so tenancy is applied
    // on the way back. One project guessing another's request identifier must
    // find nothing of theirs.
    let mut all = correlated_rows();
    let mut theirs = correlated_rows();
    for (index, row) in theirs.iter_mut().enumerate() {
        row.event_id = [200u8.wrapping_add(index as u8); 16];
        row.project_id = [77; 16];
        row.workspace_id = [78; 16];
    }
    all.append(&mut theirs);
    let service = head("lookup-tenancy", all);

    let node = query::filtered(&scan(), &eq("request_id", Value::Text("r-5".into())));
    let (_, rows) = run(&service, &node);
    assert_eq!(rows.len(), 1, "only this project's row comes back");
}

#[test]
fn an_or_is_never_pushed_down() {
    // Only an `and` may prune on one branch. An `or` matches rows carrying a
    // different value entirely, so pruning on one side would lose the other.
    use tallyowl_control_api::types::LogicalExpr;
    let service = head("lookup-or", correlated_rows());
    let either = query::expression::logical(LogicalExpr {
        logical: LogicalOp::Or,
        operands: vec![
            query::expression_ref(&eq("request_id", Value::Text("r-1".into()))),
            query::expression_ref(&eq("request_id", Value::Text("r-2".into()))),
        ],
    });
    let (_, rows) = run(&service, &query::filtered(&scan(), &either));
    assert_eq!(rows.len(), 2, "both sides of the `or` are answered");
}

#[test]
fn an_and_prunes_on_one_branch_and_still_applies_the_other() {
    use tallyowl_control_api::types::LogicalExpr;
    let service = head("lookup-and", correlated_rows());
    let both = query::expression::logical(LogicalExpr {
        logical: LogicalOp::And,
        operands: vec![
            query::expression_ref(&eq("session_id", Value::Text("s-1".into()))),
            query::expression_ref(&eq("route", Value::Text("/p/5".into()))),
        ],
    });
    let (_, rows) = run(&service, &query::filtered(&scan(), &both));
    assert_eq!(rows.len(), 1, "row 5 is in session s-1 and on /p/5");
}

#[test]
fn a_predicate_the_locator_cannot_answer_still_reads_the_range() {
    // `service_name` is deliberately not pushed down: it holds few distinct
    // values, so pruning on it would name every segment and buy nothing.
    let mut rows = correlated_rows();
    for (index, row) in rows.iter_mut().enumerate() {
        row.service_name = Some(if index % 2 == 0 { "a" } else { "b" }.to_string());
    }
    let service = head("lookup-not-pushed", rows);
    let node = query::filtered(&scan(), &eq("service_name", Value::Text("a".into())));
    let (_, found) = run(&service, &node);
    assert_eq!(found.len(), 20);
}

#[test]
fn a_trend_over_events_does_not_count_what_tallyowl_derived() {
    // L070. `events` means every kind a producer sent. A golden signal is
    // TallyOwl's own output, and counting it in a trend made a chart report
    // work nobody did. The reference application's ledger had to filter them by
    // hand, which is how the defect showed.
    let mut rows = vec![
        row(1, "checkout-started", BASE_TIME),
        row(2, "checkout-started", BASE_TIME + 1),
    ];
    let mut derived = row(
        3,
        "tallyowl_service_operation_requests_total",
        BASE_TIME + 2,
    );
    derived.kind = "metric-point".to_string();
    derived = derived.with_property(
        "derived",
        PropertyValue::Text("tallyowl-rollup".into()),
        "collector",
    );
    rows.push(derived);

    let service = head("derived-events", rows);
    let (_, counted) = run(
        &service,
        &aggregate_over(vec![measure(MeasureKind::Count, None, "events")]),
    );
    assert_eq!(
        counted[0][0].to_display(),
        "2",
        "the rollup is not an event"
    );
}

#[test]
fn the_metric_points_dataset_still_holds_what_tallyowl_derived() {
    // The other half of the rule. An operator asking for metric points wants
    // the golden signals: a derived point **is** a metric point.
    let mut derived = row(1, "tallyowl_service_operation_requests_total", BASE_TIME);
    derived.kind = "metric-point".to_string();
    derived = derived.with_property(
        "derived",
        PropertyValue::Text("tallyowl-rollup".into()),
        "collector",
    );
    let service = head("derived-metrics", vec![derived]);

    let node = query::node::aggregate(AggregateNode {
        dimensions: Vec::new(),
        measures: vec![measure(MeasureKind::Count, None, "points")],
        interval: None,
        input: query::node_ref(&query::node::scan(tallyowl_control_api::types::ScanNode {
            scan: tallyowl_control_api::types::Dataset::MetricPoints,
            project_id: PROJECT.to_vec(),
            range: whole_range(),
        })),
    });
    let (_, rows) = run(&service, &node);
    assert_eq!(rows[0][0].to_display(), "1");
}

// ---------------------------------------------------------------------------
// The pushed-down aggregate. L101 recorded it as not built; L136 built it.
// ---------------------------------------------------------------------------

/// Two stores standing in for two tablets, each computing its own partial
/// state.
///
/// **`scan` refuses.** That is what makes this a test of the push-down rather
/// than of the answer: if the coordinator asked for rows, this would fail
/// instead of quietly proving the arithmetic a second way.
struct TwoTablets {
    parts: Vec<(Arc<dyn Store>, QueryService)>,
}

impl TwoTablets {
    fn new(name: &str, rows: Vec<EventRow>) -> TwoTablets {
        let mut parts = Vec::new();
        for (index, half) in rows.chunks(rows.len().div_ceil(2)).enumerate() {
            let store: Arc<dyn Store> = Arc::new(
                SegmentedStore::open(directory(&format!("{name}-{index}"))).expect("open"),
            );
            store
                .commit([7; 16], [index as u8 + 1; 16], half.to_vec())
                .expect("the rows commit");
            let query = QueryService {
                store: Arc::clone(&store),
                max_runtime_ms: 30_000,
                max_expression_depth: DEFAULT_MAX_DEPTH,
                guards: tallyowl_head::analysis::Guards::default(),
                attribution: Default::default(),
                policy: Default::default(),
                identity: Default::default(),
            };
            parts.push((store, query));
        }
        TwoTablets { parts }
    }
}

impl Store for TwoTablets {
    fn commit(
        &self,
        _source_id: [u8; 16],
        _batch_id: [u8; 16],
        _rows: Vec<EventRow>,
    ) -> Result<tallyowl_store::store::CommitOutcome, tallyowl_store::StoreError> {
        unreachable!("this fixture never takes a write")
    }

    fn receipt(
        &self,
        _source_id: [u8; 16],
        _batch_id: [u8; 16],
    ) -> Option<tallyowl_store::store::Receipt> {
        None
    }

    fn scan(
        &self,
        _project_id: [u8; 16],
        _range_start: i64,
        _range_end: i64,
        _basis: tallyowl_store::TimeBasis,
    ) -> Result<tallyowl_store::Scanned, tallyowl_store::StoreError> {
        Err(tallyowl_store::StoreError::Unavailable(
            "a pushed-down aggregate must not move rows".to_string(),
        ))
    }

    fn lookup_event(
        &self,
        _event_id: [u8; 16],
    ) -> Result<Option<EventRow>, tallyowl_store::StoreError> {
        Ok(None)
    }

    fn lookup_correlated(
        &self,
        _column: &str,
        _value: &[u8],
    ) -> Result<tallyowl_store::Scanned, tallyowl_store::StoreError> {
        Ok(tallyowl_store::Scanned {
            rows: Vec::new(),
            incomplete: false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn trend(
        &self,
        _project_id: [u8; 16],
        _range_start: i64,
        _range_end: i64,
        _basis: tallyowl_store::TimeBasis,
        _bucket_ms: i64,
        _name: Option<&str>,
    ) -> Result<tallyowl_store::Trend, tallyowl_store::StoreError> {
        unreachable!("this fixture answers aggregates only")
    }

    fn partial_aggregates(
        &self,
        plan: &[u8],
        _project_id: [u8; 16],
        _range_start: i64,
        _range_end: i64,
        _basis: tallyowl_store::TimeBasis,
    ) -> Result<Option<tallyowl_store::store::PartialAggregates>, tallyowl_store::StoreError> {
        let mut states = Vec::new();
        for (_, query) in &self.parts {
            match query.partial_aggregate(plan) {
                Ok(Some(state)) => states.push(state),
                // A measure with no partial state. The coordinator falls back,
                // and `scan` refusing is what proves it did.
                Ok(None) => return Ok(None),
                Err(e) => return Err(tallyowl_store::StoreError::Unavailable(e.message)),
            }
        }
        Ok(Some(tallyowl_store::store::PartialAggregates {
            states,
            complete: true,
            unreadable: Vec::new(),
        }))
    }

    fn commit_watermark(&self) -> u64 {
        self.parts
            .iter()
            .map(|(store, _)| store.commit_watermark())
            .sum()
    }

    fn row_count(&self) -> usize {
        self.parts.iter().map(|(store, _)| store.row_count()).sum()
    }

    fn is_writable(&self) -> bool {
        false
    }

    fn seal_now(&self) -> Result<bool, tallyowl_store::StoreError> {
        Ok(false)
    }

    fn erase(
        &self,
        _tombstone: &tallyowl_store::catalog::Tombstone,
    ) -> Result<u64, tallyowl_store::StoreError> {
        Ok(0)
    }

    fn tombstone_generation(&self) -> Result<u64, tallyowl_store::StoreError> {
        Ok(0)
    }
}

fn across_two_tablets(name: &str, rows: Vec<EventRow>) -> QueryService {
    QueryService {
        store: Arc::new(TwoTablets::new(name, rows)),
        max_runtime_ms: 30_000,
        max_expression_depth: DEFAULT_MAX_DEPTH,
        guards: tallyowl_head::analysis::Guards::default(),
        attribution: Default::default(),
        policy: Default::default(),
        identity: Default::default(),
    }
}

/// Six purchases across two tablets, with a size that groups them three ways.
fn split_rows() -> Vec<EventRow> {
    (1..=6u8)
        .map(|n| {
            row(n, "purchase", BASE_TIME + n as i64)
                .with_property("size", PropertyValue::Integer(i64::from(n % 3)), "client")
                .with_property("value", PropertyValue::Decimal(format!("{n}.50")), "client")
        })
        .collect()
}

fn grouped_measures(measures: Vec<Measure>) -> QueryNodeBox {
    query::node::aggregate(AggregateNode {
        dimensions: vec![Dimension {
            field: query::typed_field("size", "integer"),
            alias: "size".into(),
        }],
        measures,
        interval: None,
        input: query::node_ref(&scan()),
    })
}

#[test]
fn an_aggregate_over_two_tablets_is_merged_from_partial_states() {
    // `docs/QUERY.md` section 5: the coordinator merges partial states and
    // never pulls raw rows to aggregate. The fixture's `scan` refuses, so a
    // coordinator that asked for rows fails here rather than answering right
    // for the wrong reason.
    let across = across_two_tablets("pushdown-count", split_rows());
    let whole = head("pushdown-whole", split_rows());
    let node = grouped_measures(vec![
        Measure {
            kind: MeasureKind::Count,
            field: None,
            quantile: None,
            k: None,
            alias: "events".into(),
        },
        Measure {
            kind: MeasureKind::Sum,
            field: Some(query::field("value")),
            quantile: None,
            k: None,
            alias: "total".into(),
        },
        Measure {
            kind: MeasureKind::Min,
            field: Some(query::field("value")),
            quantile: None,
            k: None,
            alias: "smallest".into(),
        },
        Measure {
            kind: MeasureKind::Max,
            field: Some(query::field("value")),
            quantile: None,
            k: None,
            alias: "largest".into(),
        },
        Measure {
            kind: MeasureKind::Avg,
            field: Some(query::field("size")),
            quantile: None,
            k: None,
            alias: "mean_size".into(),
        },
        Measure {
            kind: MeasureKind::CountDistinct,
            field: Some(query::field("value")),
            quantile: None,
            k: None,
            alias: "distinct_values".into(),
        },
    ]);

    let (_, merged) = run(&across, &node);
    let (_, single) = run(&whole, &node);
    assert_eq!(
        merged, single,
        "merging partial states answered differently from folding the rows"
    );
    assert_eq!(merged.len(), 3, "three sizes, so three groups");
}

#[test]
fn an_aggregate_over_a_filter_is_pushed_down() {
    // The owner lifted L136's filter exclusion at the Phase 11 review. The
    // predicate travels inside the plan bytes the contract already carries
    // opaquely, and each tablet evaluates it with the coordinator's own
    // expression code — one implementation, wherever it runs. The fixture's
    // `scan` refuses, so this answers only if the filter went down with the
    // aggregate.
    let across = across_two_tablets("pushdown-filter", split_rows());
    let whole = head("pushdown-filter-whole", split_rows());
    let node = query::node::aggregate(AggregateNode {
        dimensions: Vec::new(),
        measures: vec![
            Measure {
                kind: MeasureKind::Count,
                field: None,
                quantile: None,
                k: None,
                alias: "events".into(),
            },
            Measure {
                kind: MeasureKind::Sum,
                field: Some(query::field("value")),
                quantile: None,
                k: None,
                alias: "total".into(),
            },
        ],
        interval: None,
        input: query::node_ref(&query::filtered(&scan(), &eq("size", Value::Integer(1)))),
    });

    let (_, merged) = run(&across, &node);
    let (_, single) = run(&whole, &node);
    assert_eq!(
        merged, single,
        "a filtered aggregate merged from partial states answered differently \
         from folding the rows"
    );
    // Sizes cycle 1, 2, 0, so rows 1 and 4 carry size 1: two events, and
    // 1.50 + 4.50 is a sum a person can check by reading it.
    assert_eq!(merged[0][0].to_display(), "2");
    assert_eq!(merged[0][1].to_display(), "6.00");
}

#[test]
fn stacked_filters_are_pushed_down_in_the_order_the_executor_applies_them() {
    // Two filters between the aggregate and the scan, because one filter
    // proves the walk and two prove the chain.
    let across = across_two_tablets("pushdown-filter-stack", split_rows());
    let whole = head("pushdown-filter-stack-whole", split_rows());
    let inner = query::filtered(&scan(), &eq("kind", Value::Text("event".into())));
    let node = query::node::aggregate(AggregateNode {
        dimensions: Vec::new(),
        measures: vec![Measure {
            kind: MeasureKind::Count,
            field: None,
            quantile: None,
            k: None,
            alias: "events".into(),
        }],
        interval: None,
        input: query::node_ref(&query::filtered(&inner, &eq("size", Value::Integer(2)))),
    });

    let (_, merged) = run(&across, &node);
    let (_, single) = run(&whole, &node);
    assert_eq!(merged, single);
    assert_eq!(merged[0][0].to_display(), "2", "rows 2 and 5 carry size 2");
}

#[test]
fn a_sum_of_decimals_stays_exact_across_a_merge() {
    // The property `a_sum_of_exact_decimals_stays_exact` proves on one tablet.
    // A merge that added two floats would lose it, and money is the reason the
    // exact sum exists at all.
    let across = across_two_tablets(
        "pushdown-decimal",
        (1..=6u8)
            .map(|n| {
                row(n, "purchase", BASE_TIME + n as i64).with_property(
                    "value",
                    PropertyValue::Decimal("19.99".into()),
                    "client",
                )
            })
            .collect(),
    );
    let node = query::node::aggregate(AggregateNode {
        dimensions: Vec::new(),
        measures: vec![Measure {
            kind: MeasureKind::Sum,
            field: Some(query::field("value")),
            quantile: None,
            k: None,
            alias: "total".into(),
        }],
        interval: None,
        input: query::node_ref(&scan()),
    });
    let (_, rows) = run(&across, &node);
    assert_eq!(
        rows[0][0].to_display(),
        "119.94",
        "six payments of 19.99, added exactly across two tablets"
    );
}

#[test]
fn two_exact_sums_at_different_scales_add_at_the_wider_one() {
    // 1.5 and 1.50 are the same number written two ways, and an exact sum keeps
    // the scale it saw. A merge that added the units without aligning the
    // scales would answer 16.5 for a total of 3.00.
    let across = across_two_tablets(
        "pushdown-scale",
        vec![
            row(1, "purchase", BASE_TIME + 1).with_property(
                "value",
                PropertyValue::Decimal("1.5".into()),
                "client",
            ),
            row(2, "purchase", BASE_TIME + 2).with_property(
                "value",
                PropertyValue::Decimal("1.50".into()),
                "client",
            ),
        ],
    );
    let node = query::node::aggregate(AggregateNode {
        dimensions: Vec::new(),
        measures: vec![Measure {
            kind: MeasureKind::Sum,
            field: Some(query::field("value")),
            quantile: None,
            k: None,
            alias: "total".into(),
        }],
        interval: None,
        input: query::node_ref(&scan()),
    });
    let (_, rows) = run(&across, &node);
    assert_eq!(rows[0][0].to_display(), "3.00");
}
