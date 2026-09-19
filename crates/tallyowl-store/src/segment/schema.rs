//! Turning rows into columns, and columns back into rows.
//!
//! `AGENTS.md` says it plainly: after a successful segment projection,
//! queryable telemetry belongs in native typed pages and indexes, not a
//! long-lived catch-all payload. This module is where that happens.
//!
//! # The envelope columns
//!
//! Every row has them, so each is a dense column with a stable name.
//!
//! # The dynamic property columns
//!
//! `docs/HIGH_CARDINALITY.md` section 3: a segment holds only the sparse
//! dynamic columns present in that segment. It does not reserve a physical
//! column for every field ever seen.
//!
//! A property key contributes a type column, an origin column, and one value
//! column for each type that key actually holds in this segment. A key that
//! only ever holds text therefore costs three columns. A key that holds two
//! types costs four, which is the honest cost of the type-conflict policy in
//! D20: TallyOwl keeps typed variants and the dashboard shows the conflict.
//!
//! A column is dense over the rows that have the property and null over the
//! rest. A column of nulls compresses to almost nothing, so a sparse property
//! costs close to nothing in the row groups that do not carry it.

use std::collections::{BTreeMap, BTreeSet};

use super::page::Column;
use crate::row::{EventRow, PropertyValue};

/// The envelope columns, in the order a segment writes them.
///
/// A column name is part of the format. Assign one time; never use a name for
/// a different field.
pub const EVENT_ID: &str = "event_id";
pub const BATCH_ID: &str = "batch_id";
pub const SOURCE_ID: &str = "source_id";
pub const KIND: &str = "kind";
pub const NAME: &str = "name";
pub const OCCURRED_AT: &str = "occurred_at";
pub const RECEIVED_AT: &str = "received_at";
pub const COMMITTED_AT: &str = "committed_at";
pub const SESSION_ID: &str = "session_id";
pub const REQUEST_ID: &str = "request_id";
pub const TRACE_ID: &str = "trace_id";
pub const SERVICE_NAME: &str = "service_name";
pub const RELEASE: &str = "release";

/// The prefix every dynamic property column carries, so a reader can tell an
/// envelope column from a property column without a table.
pub const PROPERTY_PREFIX: &str = "p:";

/// The suffix for each part of a property.
const TYPE_SUFFIX: &str = ":t";
const ORIGIN_SUFFIX: &str = ":o";

/// The value column for one property type.
fn value_suffix(type_name: &str) -> &'static str {
    match type_name {
        "boolean" => ":b",
        "integer" => ":i",
        "unsigned" => ":u",
        "float" => ":f",
        // Text, decimal, and bytes all keep their exact characters. A decimal
        // that became a float would make a revenue total disagree with the
        // customer's own records, so money never leaves its digits.
        _ => ":s",
    }
}

/// The columns one set of rows produces.
#[derive(Debug, Clone, Default)]
pub struct Columns {
    /// Column name to its values, in name order so two writers agree.
    pub columns: BTreeMap<String, Column>,
    pub rows: usize,
}

/// Project rows into columns.
pub fn to_columns(rows: &[EventRow]) -> Columns {
    let count = rows.len();
    let mut columns: BTreeMap<String, Column> = BTreeMap::new();

    columns.insert(
        EVENT_ID.into(),
        Column::Identifiers(rows.iter().map(|r| r.event_id).collect()),
    );
    columns.insert(
        BATCH_ID.into(),
        Column::Identifiers(rows.iter().map(|r| r.batch_id).collect()),
    );
    columns.insert(
        SOURCE_ID.into(),
        Column::Identifiers(rows.iter().map(|r| r.source_id).collect()),
    );
    columns.insert(
        KIND.into(),
        Column::Text(rows.iter().map(|r| Some(r.kind.clone())).collect()),
    );
    columns.insert(
        NAME.into(),
        Column::Text(rows.iter().map(|r| Some(r.name.clone())).collect()),
    );
    columns.insert(
        OCCURRED_AT.into(),
        Column::Timestamps(rows.iter().map(|r| r.occurred_at).collect()),
    );
    columns.insert(
        RECEIVED_AT.into(),
        Column::Timestamps(rows.iter().map(|r| r.received_at).collect()),
    );
    columns.insert(
        COMMITTED_AT.into(),
        Column::Timestamps(rows.iter().map(|r| r.committed_at).collect()),
    );
    columns.insert(
        SESSION_ID.into(),
        Column::Text(rows.iter().map(|r| r.session_id.clone()).collect()),
    );
    columns.insert(
        REQUEST_ID.into(),
        Column::Text(rows.iter().map(|r| r.request_id.clone()).collect()),
    );
    columns.insert(
        SERVICE_NAME.into(),
        Column::Text(rows.iter().map(|r| r.service_name.clone()).collect()),
    );
    columns.insert(
        RELEASE.into(),
        Column::Text(rows.iter().map(|r| r.release.clone()).collect()),
    );
    // A trace ID is optional, and an identifier column has no null bitmap, so
    // an absent trace stores as all zeroes and reads back as absent. No real
    // trace ID is all zeroes, because a producer generates one.
    columns.insert(
        TRACE_ID.into(),
        Column::Identifiers(rows.iter().map(|r| r.trace_id.unwrap_or([0; 16])).collect()),
    );

    // Which property keys, and which types each holds in this segment.
    let mut keys: BTreeMap<&str, BTreeSet<&'static str>> = BTreeMap::new();
    for row in rows {
        for (key, (value, _)) in &row.properties {
            keys.entry(key.as_str())
                .or_default()
                .insert(value.type_name());
        }
    }

    for (key, types) in keys {
        let mut type_names: Vec<Option<String>> = vec![None; count];
        let mut origins: Vec<Option<String>> = vec![None; count];
        let mut values: BTreeMap<&'static str, Vec<Option<PropertyValue>>> = types
            .iter()
            .map(|type_name| (value_suffix(type_name), vec![None; count]))
            .collect();

        for (index, row) in rows.iter().enumerate() {
            if let Some((value, origin)) = row.properties.get(key) {
                type_names[index] = Some(value.type_name().to_string());
                origins[index] = Some(origin.clone());
                if let Some(column) = values.get_mut(value_suffix(value.type_name())) {
                    column[index] = Some(value.clone());
                }
            }
        }

        columns.insert(
            format!("{PROPERTY_PREFIX}{key}{TYPE_SUFFIX}"),
            Column::Text(type_names),
        );
        columns.insert(
            format!("{PROPERTY_PREFIX}{key}{ORIGIN_SUFFIX}"),
            Column::Text(origins),
        );
        for (suffix, column) in values {
            columns.insert(
                format!("{PROPERTY_PREFIX}{key}{suffix}"),
                value_column(suffix, &column),
            );
        }
    }

    Columns {
        columns,
        rows: count,
    }
}

/// One typed value column, in the physical shape its type uses.
fn value_column(suffix: &str, values: &[Option<PropertyValue>]) -> Column {
    match suffix {
        ":b" => Column::Booleans(
            values
                .iter()
                .map(|v| matches!(v, Some(PropertyValue::Boolean(true))))
                .collect(),
        ),
        ":i" => Column::Integers(
            values
                .iter()
                .map(|v| match v {
                    Some(PropertyValue::Integer(n)) => *n,
                    _ => 0,
                })
                .collect(),
        ),
        ":u" => Column::Unsigned(
            values
                .iter()
                .map(|v| match v {
                    Some(PropertyValue::Unsigned(n)) => *n,
                    _ => 0,
                })
                .collect(),
        ),
        ":f" => Column::Floats(
            values
                .iter()
                .map(|v| match v {
                    Some(PropertyValue::Float(n)) => *n,
                    _ => 0.0,
                })
                .collect(),
        ),
        // Text, decimal, and bytes keep their exact characters.
        _ => Column::Text(
            values
                .iter()
                .map(|v| v.as_ref().map(|value| value.to_display()))
                .collect(),
        ),
    }
}

/// Rebuild rows from columns.
///
/// A column this reader cannot find is an absent value rather than a failure,
/// which is what makes a minor format addition readable by an older reader.
pub fn to_rows(
    columns: &BTreeMap<String, Column>,
    rows: usize,
    workspace_id: [u8; 16],
    project_id: [u8; 16],
) -> Vec<EventRow> {
    let identifiers = |name: &str| -> Option<&Vec<[u8; 16]>> {
        match columns.get(name) {
            Some(Column::Identifiers(v)) => Some(v),
            _ => None,
        }
    };
    let text = |name: &str| -> Option<&Vec<Option<String>>> {
        match columns.get(name) {
            Some(Column::Text(v)) => Some(v),
            _ => None,
        }
    };
    let times = |name: &str| -> Option<&Vec<i64>> {
        match columns.get(name) {
            Some(Column::Timestamps(v)) => Some(v),
            _ => None,
        }
    };

    let mut out = Vec::with_capacity(rows);
    for index in 0..rows {
        let at = |name: &str| times(name).and_then(|v| v.get(index)).copied().unwrap_or(0);
        let id = |name: &str| {
            identifiers(name)
                .and_then(|v| v.get(index))
                .copied()
                .unwrap_or([0; 16])
        };
        let word = |name: &str| text(name).and_then(|v| v.get(index)).cloned().flatten();

        let mut row = EventRow::new(
            id(EVENT_ID),
            &word(KIND).unwrap_or_default(),
            &word(NAME).unwrap_or_default(),
            at(OCCURRED_AT),
        );
        row.batch_id = id(BATCH_ID);
        row.source_id = id(SOURCE_ID);
        row.workspace_id = workspace_id;
        row.project_id = project_id;
        row.received_at = at(RECEIVED_AT);
        row.committed_at = at(COMMITTED_AT);
        row.session_id = word(SESSION_ID);
        row.request_id = word(REQUEST_ID);
        row.service_name = word(SERVICE_NAME);
        row.release = word(RELEASE);
        let trace = id(TRACE_ID);
        row.trace_id = (trace != [0; 16]).then_some(trace);
        out.push(row);
    }

    // Put each property back on the rows that had one. The type column says
    // which value column to read, so a key that holds two types comes back with
    // each row's own type rather than one of them for all.
    for (name, column) in columns {
        let Some(rest) = name.strip_prefix(PROPERTY_PREFIX) else {
            continue;
        };
        let Some(key) = rest.strip_suffix(TYPE_SUFFIX) else {
            continue;
        };
        let Column::Text(type_names) = column else {
            continue;
        };
        let origins = text(&format!("{PROPERTY_PREFIX}{key}{ORIGIN_SUFFIX}"));

        for (index, type_name) in type_names.iter().enumerate() {
            let Some(type_name) = type_name else { continue };
            let suffix = value_suffix(type_name);
            let Some(value_column) = columns.get(&format!("{PROPERTY_PREFIX}{key}{suffix}")) else {
                continue;
            };
            let Some(value) = read_value(type_name, value_column, index) else {
                continue;
            };
            let origin = origins
                .and_then(|v| v.get(index))
                .cloned()
                .flatten()
                .unwrap_or_else(|| "client".to_string());
            if let Some(row) = out.get_mut(index) {
                row.properties.insert(key.to_string(), (value, origin));
            }
        }
    }

    out
}

fn read_value(type_name: &str, column: &Column, index: usize) -> Option<PropertyValue> {
    Some(match (type_name, column) {
        ("null", _) => PropertyValue::Null,
        ("boolean", Column::Booleans(v)) => PropertyValue::Boolean(*v.get(index)?),
        ("integer", Column::Timestamps(v)) | ("integer", Column::Integers(v)) => {
            PropertyValue::Integer(*v.get(index)?)
        }
        ("unsigned", Column::Unsigned(v)) => PropertyValue::Unsigned(*v.get(index)?),
        ("float", Column::Floats(v)) => PropertyValue::Float(*v.get(index)?),
        ("decimal", Column::Text(v)) => PropertyValue::Decimal(v.get(index)?.clone()?),
        ("bytes", Column::Text(v)) => {
            PropertyValue::Bytes(crate::row::from_hex(v.get(index)?.as_deref()?)?)
        }
        ("text", Column::Text(v)) => PropertyValue::Text(v.get(index)?.clone()?),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(n: u8, name: &str, at: i64) -> EventRow {
        let mut row = EventRow::new([n; 16], "event", name, at);
        row.batch_id = [7; 16];
        row.source_id = [6; 16];
        row.received_at = at + 5;
        row.committed_at = at + 9;
        row
    }

    fn round_trip(rows: Vec<EventRow>) -> Vec<EventRow> {
        let columns = to_columns(&rows);
        to_rows(&columns.columns, columns.rows, [8; 16], [9; 16])
    }

    #[test]
    fn every_envelope_field_survives_the_projection() {
        let mut original = row(1, "checkout-started", 1_785_628_800_000);
        original.session_id = Some("s-1".into());
        original.request_id = Some("r-1".into());
        original.trace_id = Some([3; 16]);
        original.service_name = Some("checkout".into());
        original.release = Some("2026.8.1".into());

        let back = round_trip(vec![original.clone()]);
        assert_eq!(back.len(), 1);
        let back = &back[0];
        assert_eq!(back.event_id, original.event_id);
        assert_eq!(back.batch_id, original.batch_id);
        assert_eq!(back.source_id, original.source_id);
        assert_eq!(back.kind, original.kind);
        assert_eq!(back.name, original.name);
        assert_eq!(back.occurred_at, original.occurred_at);
        assert_eq!(back.received_at, original.received_at);
        assert_eq!(back.committed_at, original.committed_at);
        assert_eq!(back.session_id, original.session_id);
        assert_eq!(back.request_id, original.request_id);
        assert_eq!(back.trace_id, original.trace_id);
        assert_eq!(back.service_name, original.service_name);
        assert_eq!(back.release, original.release);
        // Tenancy comes from the header rather than from a column, because it
        // is constant across a segment.
        assert_eq!(back.workspace_id, [8; 16]);
        assert_eq!(back.project_id, [9; 16]);
    }

    #[test]
    fn the_three_time_facts_stay_separate() {
        let back = round_trip(vec![row(1, "a", 1_000)]);
        assert_eq!(back[0].occurred_at, 1_000);
        assert_eq!(back[0].received_at, 1_005);
        assert_eq!(back[0].committed_at, 1_009);
    }

    #[test]
    fn every_property_type_survives_with_its_origin() {
        let original = row(1, "purchase", 1_000)
            .with_property("count", PropertyValue::Integer(-3), "client")
            .with_property("attempts", PropertyValue::Unsigned(4), "driver")
            .with_property("ratio", PropertyValue::Float(0.5), "client")
            .with_property("enabled", PropertyValue::Boolean(true), "client")
            .with_property(
                "region",
                PropertyValue::Text("us-west2".into()),
                "collector",
            )
            .with_property("value", PropertyValue::Decimal("19.99".into()), "client")
            .with_property("raw", PropertyValue::Bytes(vec![1, 2, 255]), "client")
            .with_property("nothing", PropertyValue::Null, "client");

        let back = round_trip(vec![original.clone()]);
        assert_eq!(back[0].properties, original.properties);
    }

    #[test]
    fn money_keeps_its_exact_digits_through_a_column() {
        // A decimal that became a float would make a revenue total disagree
        // with the customer's own records.
        let back = round_trip(vec![row(1, "purchase", 1_000).with_property(
            "value",
            PropertyValue::Decimal("0.1".into()),
            "client",
        )]);
        assert_eq!(
            back[0].properties["value"].0,
            PropertyValue::Decimal("0.1".into())
        );
    }

    #[test]
    fn a_property_only_some_rows_have_stays_absent_on_the_rest() {
        // HIGH_CARDINALITY.md section 3: a segment holds only the sparse
        // columns present in it, and a row without the property has none.
        let rows = vec![
            row(1, "a", 1_000).with_property("plan", PropertyValue::Text("pro".into()), "client"),
            row(2, "b", 2_000),
        ];
        let back = round_trip(rows);
        assert!(back[0].properties.contains_key("plan"));
        assert!(back[1].properties.is_empty(), "a row without it has none");
    }

    #[test]
    fn one_name_that_holds_two_types_keeps_both() {
        // D20: if one field name has different types, TallyOwl stores typed
        // variants. It never silently converts one to the other.
        let rows = vec![
            row(1, "a", 1_000).with_property("value", PropertyValue::Integer(3), "client"),
            row(2, "b", 2_000).with_property(
                "value",
                PropertyValue::Text("three".into()),
                "client",
            ),
        ];
        let columns = to_columns(&rows);
        assert!(columns.columns.contains_key("p:value:i"));
        assert!(columns.columns.contains_key("p:value:s"));

        let back = round_trip(rows);
        assert_eq!(back[0].properties["value"].0, PropertyValue::Integer(3));
        assert_eq!(
            back[1].properties["value"].0,
            PropertyValue::Text("three".into())
        );
    }

    #[test]
    fn a_segment_reserves_no_column_for_a_field_it_does_not_hold() {
        let columns = to_columns(&[row(1, "a", 1_000)]);
        assert!(
            !columns
                .columns
                .keys()
                .any(|k| k.starts_with(PROPERTY_PREFIX)),
            "a segment with no properties holds no property column"
        );
    }

    #[test]
    fn an_absent_trace_reads_back_as_absent() {
        let back = round_trip(vec![row(1, "a", 1_000)]);
        assert_eq!(back[0].trace_id, None);
    }

    #[test]
    fn many_rows_round_trip_in_order() {
        let rows: Vec<EventRow> = (0..500u16)
            .map(|n| {
                let mut r = row((n % 251) as u8, "a", 1_000 + i64::from(n));
                r.event_id[14..16].copy_from_slice(&n.to_be_bytes());
                r
            })
            .collect();
        let back = round_trip(rows.clone());
        assert_eq!(back.len(), rows.len());
        for (a, b) in rows.iter().zip(&back) {
            assert_eq!(a.event_id, b.event_id);
            assert_eq!(a.occurred_at, b.occurred_at);
        }
    }
}
