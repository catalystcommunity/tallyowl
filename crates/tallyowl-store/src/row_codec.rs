//! Rows as canonical CBOR, for the append log.
//!
//! D3: "CSIL CBOR is interchange and WAL data. It is not the final query
//! format." A frame holds the accepted rows so that a replay reproduces exactly
//! what was accepted, and the segmenter then writes native typed columns from
//! the committed range. The source CBOR expires after verified segments cover
//! its log range.
//!
//! This is the store's own encoding of its own row type rather than the wire
//! type, because D25 puts a versioned API contract at that seam and the two are
//! allowed to move independently.

use std::collections::BTreeMap;

use crate::cbor::{self, MapBuilder, Value};
use crate::row::{EventRow, PropertyValue};

/// The encoding version. A frame written by a later version is refused rather
/// than guessed at, which is the same rule the segment format uses.
const ROW_FORMAT_VERSION: u64 = 1;

/// Why a frame did not read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowCodecError {
    pub message: String,
}

impl std::fmt::Display for RowCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RowCodecError {}

fn damaged(message: impl Into<String>) -> RowCodecError {
    RowCodecError {
        message: message.into(),
    }
}

/// One batch of rows, as the bytes an append-log frame carries.
pub fn encode_rows(rows: &[EventRow]) -> Vec<u8> {
    cbor::encode(
        &MapBuilder::new()
            .put("v", Value::Unsigned(ROW_FORMAT_VERSION))
            .put("rows", Value::Array(rows.iter().map(row_to_cbor).collect()))
            .build(),
    )
}

/// Read one frame back.
pub fn decode_rows(bytes: &[u8]) -> Result<Vec<EventRow>, RowCodecError> {
    let value = cbor::decode(bytes)
        .map_err(|e| damaged(format!("A stored batch could not be read. {e}")))?;

    let version = value.field("v").and_then(|v| v.as_unsigned()).unwrap_or(0);
    if version != ROW_FORMAT_VERSION {
        return Err(damaged(format!(
            "This stored batch was written by a different version of TallyOwl \
             and this one cannot read it. It says version {version}, and this software \
             reads version {ROW_FORMAT_VERSION}."
        )));
    }

    value
        .field("rows")
        .and_then(|v| v.as_array())
        .ok_or_else(|| damaged("A stored batch holds no items."))?
        .iter()
        .map(row_from_cbor)
        .collect()
}

fn row_to_cbor(row: &EventRow) -> Value {
    let properties: BTreeMap<String, Value> = row
        .properties
        .iter()
        .map(|(key, (value, origin))| {
            (
                key.clone(),
                Value::Array(vec![
                    Value::text(value.type_name()),
                    Value::text(value.to_display()),
                    Value::text(origin),
                ]),
            )
        })
        .collect();

    MapBuilder::new()
        .put("ev", Value::Bytes(row.event_id.to_vec()))
        .put("ba", Value::Bytes(row.batch_id.to_vec()))
        .put("ws", Value::Bytes(row.workspace_id.to_vec()))
        .put("pr", Value::Bytes(row.project_id.to_vec()))
        .put("so", Value::Bytes(row.source_id.to_vec()))
        .put("k", Value::text(&row.kind))
        .put("n", Value::text(&row.name))
        // Three time facts, kept apart. CONVENTIONS.md section 7 forbids
        // collapsing them, and a replay that lost one would make every late
        // batch look punctual.
        .put("occ", Value::integer(row.occurred_at))
        .put("rec", Value::integer(row.received_at))
        .put("com", Value::integer(row.committed_at))
        .put_some("se", row.session_id.as_ref().map(Value::text))
        .put_some("rq", row.request_id.as_ref().map(Value::text))
        .put_some("tr", row.trace_id.map(|id| Value::Bytes(id.to_vec())))
        .put_some("sv", row.service_name.as_ref().map(Value::text))
        .put_some("re", row.release.as_ref().map(Value::text))
        .put("p", Value::Map(properties))
        .build()
}

fn row_from_cbor(value: &Value) -> Result<EventRow, RowCodecError> {
    let id = |name: &str| -> Result<[u8; 16], RowCodecError> {
        value
            .field(name)
            .and_then(|v| v.as_bytes())
            .and_then(|b| <[u8; 16]>::try_from(b).ok())
            .ok_or_else(|| damaged("A stored item is missing an identifier it needs."))
    };
    let text = |name: &str| -> Option<String> {
        value
            .field(name)
            .and_then(|v| v.as_text())
            .map(String::from)
    };
    let at = |name: &str| -> i64 {
        value
            .field(name)
            .and_then(|v| v.as_integer())
            .unwrap_or_default()
    };

    let mut row = EventRow::new(
        id("ev")?,
        &text("k").unwrap_or_default(),
        &text("n").unwrap_or_default(),
        at("occ"),
    );
    row.batch_id = id("ba")?;
    row.workspace_id = id("ws")?;
    row.project_id = id("pr")?;
    row.source_id = id("so")?;
    row.received_at = at("rec");
    row.committed_at = at("com");
    row.session_id = text("se");
    row.request_id = text("rq");
    row.trace_id = value
        .field("tr")
        .and_then(|v| v.as_bytes())
        .and_then(|b| <[u8; 16]>::try_from(b).ok());
    row.service_name = text("sv");
    row.release = text("re");

    if let Some(properties) = value.field("p").and_then(|v| v.as_map()) {
        for (key, entry) in properties {
            let parts = entry
                .as_array()
                .ok_or_else(|| damaged("A stored property could not be read."))?;
            let type_name = parts.first().and_then(|v| v.as_text()).unwrap_or("text");
            let rendered = parts.get(1).and_then(|v| v.as_text()).unwrap_or("");
            let origin = parts.get(2).and_then(|v| v.as_text()).unwrap_or("client");
            row.properties.insert(
                key.clone(),
                (parse_value(type_name, rendered)?, origin.to_string()),
            );
        }
    }
    Ok(row)
}

/// One property value from its type name and its exact rendered form.
///
/// A decimal keeps its digits as text through the whole path, so money never
/// becomes a float on the way to storage and back.
fn parse_value(type_name: &str, rendered: &str) -> Result<PropertyValue, RowCodecError> {
    let number = |what: &str| damaged(format!("A stored {what} could not be read."));
    Ok(match type_name {
        "null" => PropertyValue::Null,
        "boolean" => PropertyValue::Boolean(rendered == "true"),
        "integer" => PropertyValue::Integer(rendered.parse().map_err(|_| number("number"))?),
        "unsigned" => PropertyValue::Unsigned(rendered.parse().map_err(|_| number("number"))?),
        "float" => PropertyValue::Float(rendered.parse().map_err(|_| number("number"))?),
        "decimal" => PropertyValue::Decimal(rendered.to_string()),
        "bytes" => {
            PropertyValue::Bytes(crate::row::from_hex(rendered).ok_or_else(|| number("value"))?)
        }
        _ => PropertyValue::Text(rendered.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(n: u8) -> EventRow {
        let mut row = EventRow::new([n; 16], "event", "checkout-started", 1_785_628_800_000);
        row.batch_id = [1; 16];
        row.workspace_id = [8; 16];
        row.project_id = [9; 16];
        row.source_id = [7; 16];
        row.received_at = 1_785_628_800_005;
        row.committed_at = 1_785_628_800_009;
        row
    }

    #[test]
    fn a_batch_round_trips_with_every_field() {
        let mut original = row(1);
        original.session_id = Some("s-1".into());
        original.request_id = Some("r-1".into());
        original.trace_id = Some([3; 16]);
        original.service_name = Some("checkout".into());
        original.release = Some("2026.8.1".into());

        let back = decode_rows(&encode_rows(std::slice::from_ref(&original))).unwrap();
        assert_eq!(back, vec![original]);
    }

    #[test]
    fn every_property_type_survives_a_replay() {
        let original = row(1)
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

        let back = decode_rows(&encode_rows(std::slice::from_ref(&original))).unwrap();
        assert_eq!(back[0].properties, original.properties);
    }

    #[test]
    fn money_keeps_its_exact_digits_through_a_replay() {
        let original =
            row(1).with_property("value", PropertyValue::Decimal("0.1".into()), "client");
        let back = decode_rows(&encode_rows(std::slice::from_ref(&original))).unwrap();
        assert_eq!(
            back[0].properties["value"].0,
            PropertyValue::Decimal("0.1".into())
        );
    }

    #[test]
    fn the_three_time_facts_stay_separate_through_a_replay() {
        let back = decode_rows(&encode_rows(&[row(1)])).unwrap();
        assert_eq!(back[0].occurred_at, 1_785_628_800_000);
        assert_eq!(back[0].received_at, 1_785_628_800_005);
        assert_eq!(back[0].committed_at, 1_785_628_800_009);
    }

    #[test]
    fn an_absent_field_stays_absent() {
        let back = decode_rows(&encode_rows(&[row(1)])).unwrap();
        assert_eq!(back[0].session_id, None);
        assert_eq!(back[0].trace_id, None);
        assert!(back[0].properties.is_empty());
    }

    #[test]
    fn many_rows_round_trip_in_order() {
        let rows: Vec<EventRow> = (0..200u8).map(row).collect();
        let back = decode_rows(&encode_rows(&rows)).unwrap();
        assert_eq!(back, rows);
    }

    #[test]
    fn an_empty_batch_round_trips() {
        // A batch whose every item was rejected still commits, so it still has
        // to encode.
        assert_eq!(decode_rows(&encode_rows(&[])).unwrap(), Vec::new());
    }

    #[test]
    fn a_frame_from_a_later_version_is_refused_rather_than_guessed_at() {
        let mut bytes = encode_rows(&[row(1)]);
        // Move the version to something this reader does not know.
        let at = bytes
            .windows(2)
            .position(|w| w == [0x61, b'v'])
            .expect("the version field");
        bytes[at + 2] = 0x09;
        let failure = decode_rows(&bytes).unwrap_err();
        assert!(failure.message.contains("cannot read it"));
    }

    #[test]
    fn damaged_bytes_are_refused_rather_than_partly_read() {
        assert!(decode_rows(&[]).is_err());
        assert!(decode_rows(b"not a batch").is_err());
        let mut bytes = encode_rows(&[row(1)]);
        bytes.truncate(bytes.len() / 2);
        assert!(decode_rows(&bytes).is_err());
    }

    #[test]
    fn one_batch_encodes_the_same_bytes_every_time() {
        // A frame is checksummed, so the encoding has to be a function of the
        // rows rather than of when they were written.
        let rows: Vec<EventRow> = (0..20u8).map(row).collect();
        assert_eq!(encode_rows(&rows), encode_rows(&rows));
    }
}
