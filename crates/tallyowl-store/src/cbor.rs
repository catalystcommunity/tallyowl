//! Canonical CBOR, for the parts of a segment that are self-describing.
//!
//! `docs/SEGMENT_FORMAT.md` sections 5, 8, and 12 say the header, the footer,
//! and the manifest are canonical CBOR. The format is the contract that
//! outlives every implementation, so this module writes bytes that a reader in
//! any language can decode from the specification alone, rather than a
//! structure dump from whichever library happened to be linked.
//!
//! Canonical means what RFC 8949 section 4.2.1 means:
//!
//! - the shortest head for every length and integer;
//! - map keys sorted by their encoded bytes;
//! - no indefinite-length item.
//!
//! Two segments written from the same values therefore hold the same bytes, and
//! the content address over them is stable. That property is what makes a
//! backup verifiable and a cold object addressable.
//!
//! This is deliberately small. It covers the value kinds a header, a footer, and
//! a manifest use, and nothing else.

use std::collections::BTreeMap;

/// A CBOR value, in the shape this format uses.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Unsigned(u64),
    Negative(i64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Value>),
    /// Keys are text, which is what every map in this format uses. The order on
    /// the wire is canonical rather than insertion order.
    Map(BTreeMap<String, Value>),
    Bool(bool),
    Null,
    Float(f64),
}

impl Value {
    pub fn integer(value: i64) -> Value {
        if value < 0 {
            Value::Negative(value)
        } else {
            Value::Unsigned(value as u64)
        }
    }

    pub fn text(value: impl Into<String>) -> Value {
        Value::Text(value.into())
    }

    pub fn as_unsigned(&self) -> Option<u64> {
        match self {
            Value::Unsigned(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Unsigned(v) => i64::try_from(*v).ok(),
            Value::Negative(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Value::Map(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(v) => Some(*v),
            Value::Unsigned(v) => Some(*v as f64),
            Value::Negative(v) => Some(*v as f64),
            _ => None,
        }
    }

    /// One field of a map, by name.
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.as_map()?.get(name)
    }
}

/// Build a map without repeating the boilerplate at each call site.
#[derive(Debug, Default)]
pub struct MapBuilder {
    entries: BTreeMap<String, Value>,
}

impl MapBuilder {
    pub fn new() -> MapBuilder {
        MapBuilder::default()
    }

    pub fn put(mut self, key: &str, value: Value) -> MapBuilder {
        self.entries.insert(key.to_string(), value);
        self
    }

    /// Add a field only when it has a value. An absent optional field costs no
    /// bytes, which is what keeps a small segment small.
    pub fn put_some(self, key: &str, value: Option<Value>) -> MapBuilder {
        match value {
            Some(value) => self.put(key, value),
            None => self,
        }
    }

    pub fn build(self) -> Value {
        Value::Map(self.entries)
    }
}

/// Why a value did not decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    pub message: String,
    /// Where in the bytes the reader stopped, so a damaged file can be
    /// described rather than only refused.
    pub at: usize,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.message, self.at)
    }
}

impl std::error::Error for DecodeError {}

fn error(message: impl Into<String>, at: usize) -> DecodeError {
    DecodeError {
        message: message.into(),
        at,
    }
}

/// The bytes for one value.
pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write(value, &mut out);
    out
}

fn head(major: u8, argument: u64, out: &mut Vec<u8>) {
    let major = major << 5;
    match argument {
        0..=23 => out.push(major | argument as u8),
        24..=0xff => {
            out.push(major | 24);
            out.push(argument as u8);
        }
        0x100..=0xffff => {
            out.push(major | 25);
            out.extend_from_slice(&(argument as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(major | 26);
            out.extend_from_slice(&(argument as u32).to_be_bytes());
        }
        _ => {
            out.push(major | 27);
            out.extend_from_slice(&argument.to_be_bytes());
        }
    }
}

fn write(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Unsigned(v) => head(0, *v, out),
        Value::Negative(v) => head(1, (-1 - *v) as u64, out),
        Value::Bytes(v) => {
            head(2, v.len() as u64, out);
            out.extend_from_slice(v);
        }
        Value::Text(v) => {
            head(3, v.len() as u64, out);
            out.extend_from_slice(v.as_bytes());
        }
        Value::Array(items) => {
            head(4, items.len() as u64, out);
            for item in items {
                write(item, out);
            }
        }
        Value::Map(entries) => {
            head(5, entries.len() as u64, out);
            // Canonical order is by encoded key bytes, which for text keys of
            // this format means shorter first and then bytewise. A `BTreeMap`
            // orders by the string, so the two agree only when every key has
            // the same length class; sorting here makes it exact.
            let mut keys: Vec<&String> = entries.keys().collect();
            keys.sort_by(|a, b| {
                a.len()
                    .cmp(&b.len())
                    .then_with(|| a.as_bytes().cmp(b.as_bytes()))
            });
            for key in keys {
                write(&Value::Text(key.clone()), out);
                write(&entries[key], out);
            }
        }
        Value::Bool(v) => out.push(if *v { 0xf5 } else { 0xf4 }),
        Value::Null => out.push(0xf6),
        Value::Float(v) => {
            out.push(0xfb);
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
}

/// Read one value, and say how many bytes it used.
///
/// A declared length is validated against what is left before anything is
/// allocated, which is rule 6 of `docs/SEGMENT_FORMAT.md` section 2.
pub fn decode(bytes: &[u8]) -> Result<Value, DecodeError> {
    let (value, used) = read(bytes, 0, 0)?;
    if used != bytes.len() {
        return Err(error("more bytes follow the value", used));
    }
    Ok(value)
}

/// The deepest nesting a reader follows. A hostile file cannot exhaust the
/// stack, because the depth is a count rather than a type.
const MAX_DEPTH: usize = 32;

fn read(bytes: &[u8], at: usize, depth: usize) -> Result<(Value, usize), DecodeError> {
    if depth > MAX_DEPTH {
        return Err(error("the value nests deeper than this reader follows", at));
    }
    let initial = *bytes.get(at).ok_or_else(|| error("the value stops", at))?;
    let major = initial >> 5;
    let short = initial & 0x1f;

    let (argument, mut used) = match short {
        0..=23 => (short as u64, at + 1),
        24 => (read_be(bytes, at + 1, 1)?, at + 2),
        25 => (read_be(bytes, at + 1, 2)?, at + 3),
        26 => (read_be(bytes, at + 1, 4)?, at + 5),
        27 => (read_be(bytes, at + 1, 8)?, at + 9),
        _ => return Err(error("the value uses a form this reader refuses", at)),
    };

    let value = match major {
        0 => Value::Unsigned(argument),
        1 => Value::Negative(
            -1 - i64::try_from(argument)
                .map_err(|_| error("a negative number is larger than this reader holds", at))?,
        ),
        2 | 3 => {
            let length = usize::try_from(argument)
                .map_err(|_| error("a length is larger than this reader holds", at))?;
            // Validate the declared length before allocating for it.
            if used + length > bytes.len() {
                return Err(error("a value claims more bytes than remain", at));
            }
            let slice = &bytes[used..used + length];
            used += length;
            if major == 2 {
                Value::Bytes(slice.to_vec())
            } else {
                Value::Text(
                    std::str::from_utf8(slice)
                        .map_err(|_| error("a text value is not valid text", at))?
                        .to_string(),
                )
            }
        }
        4 => {
            let count = usize::try_from(argument)
                .map_err(|_| error("an array is longer than this reader holds", at))?;
            // One byte is the least any item can take, so a count larger than
            // what remains is a damaged file rather than a large allocation.
            if count > bytes.len() - used {
                return Err(error("an array claims more items than remain", at));
            }
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                let (item, next) = read(bytes, used, depth + 1)?;
                items.push(item);
                used = next;
            }
            Value::Array(items)
        }
        5 => {
            let count = usize::try_from(argument)
                .map_err(|_| error("a map is larger than this reader holds", at))?;
            if count > bytes.len() - used {
                return Err(error("a map claims more entries than remain", at));
            }
            let mut entries = BTreeMap::new();
            for _ in 0..count {
                let (key, next) = read(bytes, used, depth + 1)?;
                let Value::Text(key) = key else {
                    return Err(error("a map key is not text", used));
                };
                let (item, next) = read(bytes, next, depth + 1)?;
                entries.insert(key, item);
                used = next;
            }
            Value::Map(entries)
        }
        7 => match short {
            20 => Value::Bool(false),
            21 => Value::Bool(true),
            22 => Value::Null,
            27 => Value::Float(f64::from_be_bytes(
                bytes[at + 1..at + 9]
                    .try_into()
                    .map_err(|_| error("a number stops early", at))?,
            )),
            _ => return Err(error("a simple value this reader does not know", at)),
        },
        _ => return Err(error("a tag this format does not use", at)),
    };

    Ok((value, used))
}

fn read_be(bytes: &[u8], at: usize, width: usize) -> Result<u64, DecodeError> {
    if at + width > bytes.len() {
        return Err(error("a number stops early", at));
    }
    let mut out = 0u64;
    for byte in &bytes[at..at + width] {
        out = (out << 8) | *byte as u64;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(value: Value) {
        let bytes = encode(&value);
        assert_eq!(decode(&bytes).unwrap(), value, "{value:?}");
    }

    #[test]
    fn every_value_kind_round_trips() {
        round_trip(Value::Unsigned(0));
        round_trip(Value::Unsigned(23));
        round_trip(Value::Unsigned(24));
        round_trip(Value::Unsigned(u64::MAX));
        round_trip(Value::Negative(-1));
        round_trip(Value::Negative(-1_000_000));
        round_trip(Value::Bytes(vec![0, 1, 2, 255]));
        round_trip(Value::Text("segment".into()));
        round_trip(Value::Bool(true));
        round_trip(Value::Bool(false));
        round_trip(Value::Null);
        round_trip(Value::Float(0.5));
        round_trip(Value::Array(vec![
            Value::Unsigned(1),
            Value::Text("a".into()),
        ]));
        round_trip(
            MapBuilder::new()
                .put("rows", Value::Unsigned(7))
                .put("kind", Value::text("event"))
                .build(),
        );
    }

    #[test]
    fn a_head_uses_the_shortest_form() {
        // Canonical CBOR is a byte-level promise, and a content address over
        // the bytes depends on it.
        assert_eq!(encode(&Value::Unsigned(0)), vec![0x00]);
        assert_eq!(encode(&Value::Unsigned(23)), vec![0x17]);
        assert_eq!(encode(&Value::Unsigned(24)), vec![0x18, 0x18]);
        assert_eq!(encode(&Value::Unsigned(256)), vec![0x19, 0x01, 0x00]);
        assert_eq!(encode(&Value::Negative(-1)), vec![0x20]);
    }

    #[test]
    fn map_keys_are_written_in_canonical_order() {
        // Shorter first, then bytewise. Two writers that used insertion order
        // would produce two content addresses for one segment.
        let value = MapBuilder::new()
            .put("zz", Value::Unsigned(1))
            .put("a", Value::Unsigned(2))
            .put("mmm", Value::Unsigned(3))
            .build();
        let bytes = encode(&value);
        let text: Vec<u8> = bytes.clone();
        let a = text.iter().position(|b| *b == b'a').unwrap();
        let z = text.windows(2).position(|w| w == b"zz").unwrap();
        let m = text.windows(3).position(|w| w == b"mmm").unwrap();
        assert!(a < z, "the shortest key comes first");
        assert!(z < m, "then the next shortest");
    }

    #[test]
    fn one_value_encodes_the_same_way_every_time() {
        let build = || {
            MapBuilder::new()
                .put("rows", Value::Unsigned(1_000_000))
                .put("kind", Value::text("event"))
                .put(
                    "time",
                    Value::Array(vec![Value::Unsigned(1), Value::Unsigned(2)]),
                )
                .build()
        };
        assert_eq!(encode(&build()), encode(&build()));
    }

    #[test]
    fn an_absent_optional_field_costs_nothing() {
        let with = MapBuilder::new()
            .put("a", Value::Unsigned(1))
            .put_some("b", Some(Value::Unsigned(2)))
            .build();
        let without = MapBuilder::new()
            .put("a", Value::Unsigned(1))
            .put_some("b", None)
            .build();
        assert!(encode(&without).len() < encode(&with).len());
        assert!(without.field("b").is_none());
    }

    #[test]
    fn a_declared_length_larger_than_the_bytes_is_refused_before_it_allocates() {
        // Rule 6 of SEGMENT_FORMAT.md section 2. A damaged file must not be
        // able to ask for a large allocation.
        let mut bytes = encode(&Value::Bytes(vec![1, 2, 3]));
        bytes[0] = 0x5a; // a byte string with a four-byte length
        assert!(decode(&bytes).is_err());

        // A truncated head.
        assert!(decode(&[0x19, 0x01]).is_err());
        // An array that claims more items than can possibly follow.
        assert!(decode(&[0x9a, 0xff, 0xff, 0xff, 0xff]).is_err());
    }

    #[test]
    fn a_value_that_nests_too_deep_is_refused_rather_than_exhausting_the_stack() {
        // A hostile tree cannot exhaust a stack during parsing, because the
        // depth is a count rather than a type.
        let mut bytes = vec![0x00];
        for _ in 0..(MAX_DEPTH + 5) {
            let mut wrapped = vec![0x81];
            wrapped.extend_from_slice(&bytes);
            bytes = wrapped;
        }
        let failure = decode(&bytes).unwrap_err();
        assert!(failure.message.contains("nests deeper"));
    }

    #[test]
    fn trailing_bytes_after_a_value_are_refused() {
        let mut bytes = encode(&Value::Unsigned(1));
        bytes.push(0x00);
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn a_map_key_that_is_not_text_is_refused() {
        // Every map in this format uses text keys, so a numeric key is a file
        // this reader does not understand rather than one it guesses at.
        let bytes = vec![0xa1, 0x01, 0x02];
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn a_failure_says_where_it_stopped() {
        let failure = decode(&[0x19, 0x01]).unwrap_err();
        assert!(failure.to_string().contains("byte"));
    }
}
