//! The stored shape of one telemetry item.
//!
//! This is the store's own type, not a generated one. D25 puts a versioned API
//! contract at this seam, so the wire type and the stored type are allowed to
//! move independently. The collector and the head translate at the boundary.
//!
//! Three time facts stay separate here, and CONVENTIONS.md section 7 forbids
//! collapsing them: `occurred_at` from the producer, `received_at` from the
//! collector, and `committed_at` from the store.

use std::collections::BTreeMap;

/// A typed property value. One typed property namespace carries every
/// descriptive value, and each property records its origin. See D38.
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyValue {
    Null,
    Boolean(bool),
    Integer(i64),
    Unsigned(u64),
    Float(f64),
    /// An exact decimal, kept as its text form. Money never uses a float.
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
}

impl PropertyValue {
    pub fn type_name(&self) -> &'static str {
        match self {
            PropertyValue::Null => "null",
            PropertyValue::Boolean(_) => "boolean",
            PropertyValue::Integer(_) => "integer",
            PropertyValue::Unsigned(_) => "unsigned",
            PropertyValue::Float(_) => "float",
            PropertyValue::Decimal(_) => "decimal",
            PropertyValue::Text(_) => "text",
            PropertyValue::Bytes(_) => "bytes",
        }
    }

    pub fn to_display(&self) -> String {
        match self {
            PropertyValue::Null => String::new(),
            PropertyValue::Boolean(v) => v.to_string(),
            PropertyValue::Integer(v) => v.to_string(),
            PropertyValue::Unsigned(v) => v.to_string(),
            PropertyValue::Float(v) => v.to_string(),
            PropertyValue::Decimal(v) => v.clone(),
            PropertyValue::Text(v) => v.clone(),
            PropertyValue::Bytes(v) => hex(v),
        }
    }
}

/// One stored telemetry item, projected to the canonical generic event.
#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    /// UUIDv7 from the original producer, as 16 bytes. An ID travels as raw
    /// bytes, never as hexadecimal text; a measurement showed 16 bytes costs 53
    /// percent less than 36 characters. See BENCHMARKS.md section 6.
    pub event_id: [u8; 16],
    pub batch_id: [u8; 16],
    pub workspace_id: [u8; 16],
    pub project_id: [u8; 16],
    pub source_id: [u8; 16],
    pub kind: String,
    pub name: String,
    /// Producer time.
    pub occurred_at: i64,
    /// Collector time.
    pub received_at: i64,
    /// Store time. The store sets this; a caller never supplies it.
    pub committed_at: i64,
    pub session_id: Option<String>,
    pub request_id: Option<String>,
    pub trace_id: Option<[u8; 16]>,
    pub service_name: Option<String>,
    pub release: Option<String>,
    /// Property key to value and origin.
    pub properties: BTreeMap<String, (PropertyValue, String)>,
}

impl EventRow {
    /// A row with every required field set and nothing else. A test names only
    /// what it cares about, which is the `DataUtils` pattern D56 requires.
    pub fn new(event_id: [u8; 16], kind: &str, name: &str, occurred_at: i64) -> EventRow {
        EventRow {
            event_id,
            batch_id: [0; 16],
            workspace_id: [0; 16],
            project_id: [0; 16],
            source_id: [0; 16],
            kind: kind.to_string(),
            name: name.to_string(),
            occurred_at,
            received_at: occurred_at,
            committed_at: 0,
            session_id: None,
            request_id: None,
            trace_id: None,
            service_name: None,
            release: None,
            properties: BTreeMap::new(),
        }
    }

    pub fn with_property(mut self, key: &str, value: PropertyValue, origin: &str) -> EventRow {
        self.properties
            .insert(key.to_string(), (value, origin.to_string()));
        self
    }

    pub fn event_id_text(&self) -> String {
        hex(&self.event_id)
    }

    pub fn batch_id_text(&self) -> String {
        hex(&self.batch_id)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

pub fn id_from_hex(text: &str) -> Option<[u8; 16]> {
    let bytes = from_hex(text)?;
    bytes.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identifier_round_trips_through_its_text_form() {
        let id = [
            0x01, 0x8f, 0x2a, 0xbc, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99,
            0xaa, 0xff,
        ];
        let text = hex(&id);
        assert_eq!(text.len(), 32);
        assert_eq!(id_from_hex(&text), Some(id));
    }

    #[test]
    fn a_text_that_is_not_an_identifier_does_not_parse() {
        assert_eq!(id_from_hex("abc"), None);
        assert_eq!(id_from_hex("zz"), None);
        assert_eq!(id_from_hex(""), None, "an empty text is not 16 bytes");
    }

    #[test]
    fn the_three_time_facts_stay_separate() {
        let row = EventRow::new([1; 16], "event", "checkout-started", 100);
        assert_eq!(row.occurred_at, 100);
        assert_eq!(row.received_at, 100);
        // The store sets the commit time. A caller never supplies it.
        assert_eq!(row.committed_at, 0);
    }

    #[test]
    fn a_property_keeps_its_type_and_its_origin() {
        let row = EventRow::new([1; 16], "event", "purchase", 1)
            .with_property("value", PropertyValue::Decimal("19.99".into()), "client")
            .with_property(
                "region",
                PropertyValue::Text("us-west2".into()),
                "collector",
            );
        assert_eq!(row.properties["value"].0.type_name(), "decimal");
        assert_eq!(row.properties["value"].1, "client");
        // An operator can trust a `collector` origin, because the collector
        // stamps it from its own configuration. See D38.
        assert_eq!(row.properties["region"].1, "collector");
    }

    #[test]
    fn money_keeps_its_exact_text_and_never_becomes_a_float() {
        let value = PropertyValue::Decimal("0.1".into());
        assert_eq!(value.to_display(), "0.1");
    }
}
