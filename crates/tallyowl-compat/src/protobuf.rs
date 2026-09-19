//! Just enough of the protocol-buffer wire format to read an OpenTelemetry
//! push.
//!
//! # Why this is written rather than generated
//!
//! The OpenTelemetry payload is the only protocol buffer TallyOwl reads, it
//! reads it at one boundary, and it never writes one except for a two-field
//! acknowledgement. A code generator and its runtime would join the always-on
//! collector's dependency set for that, and the wire format itself is six wire
//! types and a varint.
//!
//! This reader is also deliberately shallow. It hands back the fields of one
//! message and lets the caller decide what a field number means, so
//! `otlp` holds every OpenTelemetry-specific decision in one readable place
//! rather than spreading it through generated code nobody reviews.
//!
//! # What it refuses
//!
//! A push arrives from outside, so this treats every length and every varint as
//! hostile. A length that runs past the end of the buffer, a varint longer than
//! ten bytes, and a group wire type all end the message rather than panicking
//! or looping. `Reader` never allocates from a length the payload chose.

/// One field of a message, still in its wire form.
#[derive(Debug, Clone, PartialEq)]
pub enum Wire<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

impl<'a> Wire<'a> {
    pub fn as_u64(&self) -> u64 {
        match self {
            Wire::Varint(v) | Wire::Fixed64(v) => *v,
            Wire::Fixed32(v) => *v as u64,
            Wire::Bytes(_) => 0,
        }
    }

    pub fn as_i64(&self) -> i64 {
        match self {
            // A protocol buffer `sfixed64` is two's complement, and an `int64`
            // varint is too. Neither is zig-zag; only `sint64` is, and no
            // OpenTelemetry field uses it.
            Wire::Varint(v) | Wire::Fixed64(v) => *v as i64,
            Wire::Fixed32(v) => *v as i32 as i64,
            Wire::Bytes(_) => 0,
        }
    }

    pub fn as_f64(&self) -> f64 {
        match self {
            Wire::Fixed64(v) => f64::from_bits(*v),
            Wire::Fixed32(v) => f32::from_bits(*v) as f64,
            Wire::Varint(v) => *v as f64,
            Wire::Bytes(_) => 0.0,
        }
    }

    pub fn as_bytes(&self) -> &'a [u8] {
        match self {
            Wire::Bytes(b) => b,
            _ => &[],
        }
    }

    pub fn as_text(&self) -> String {
        String::from_utf8_lossy(self.as_bytes()).into_owned()
    }
}

/// One field: its number and its value.
#[derive(Debug, Clone, PartialEq)]
pub struct Field<'a> {
    pub number: u32,
    pub wire: Wire<'a>,
}

/// Reads the fields of one message, in the order they arrived.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, at: 0 }
    }

    /// The next field, or `None` at the end and on anything malformed.
    ///
    /// Stopping on a malformed field rather than reporting it is deliberate:
    /// this is a compatibility edge, and half a valid message is worth more
    /// than a refusal of the whole push. What was read before the fault still
    /// reaches storage, and the receiver counts the short read.
    pub fn next_field(&mut self) -> Option<Field<'a>> {
        if self.at >= self.data.len() {
            return None;
        }
        let tag = self.varint()?;
        let number = (tag >> 3) as u32;
        if number == 0 {
            return None;
        }
        let wire = match tag & 0x7 {
            0 => Wire::Varint(self.varint()?),
            1 => Wire::Fixed64(u64::from_le_bytes(self.take(8)?.try_into().ok()?)),
            2 => {
                let length = self.varint()? as usize;
                Wire::Bytes(self.take(length)?)
            }
            5 => Wire::Fixed32(u32::from_le_bytes(self.take(4)?.try_into().ok()?)),
            // Wire types 3 and 4 are the deprecated group encoding, and 6 and 7
            // do not exist. No OpenTelemetry message uses one.
            _ => return None,
        };
        Some(Field { number, wire })
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            // Ten bytes is the widest a 64-bit varint can be. A longer one is a
            // payload trying to make this loop forever.
            if shift > 63 {
                return None;
            }
            let byte = *self.data.get(self.at)?;
            self.at += 1;
            value |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
            shift += 7;
        }
    }

    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(length)?;
        let slice = self.data.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }
}

/// Read a packed repeated field of fixed-width or varint values.
///
/// OpenTelemetry writes `bucket_counts` and `explicit_bounds` packed, and an
/// older producer may write them one at a time. A caller handles both by
/// collecting whatever arrives for that field number.
pub fn packed_fixed64(bytes: &[u8]) -> Vec<u64> {
    // `as_chunks` gives fixed-size arrays, so there is no fallible conversion
    // to explain away in the middle of a decoder.
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_le_bytes(*c))
        .collect()
}

pub fn packed_double(bytes: &[u8]) -> Vec<f64> {
    packed_fixed64(bytes)
        .into_iter()
        .map(f64::from_bits)
        .collect()
}

pub fn packed_varint(bytes: &[u8]) -> Vec<u64> {
    let mut reader = Reader::new(bytes);
    let mut out = Vec::new();
    while let Some(value) = reader.varint() {
        out.push(value);
    }
    out
}

// ---------------------------------------------------------------------------
// Writing, for the acknowledgement only
// ---------------------------------------------------------------------------

/// Write one length-delimited field.
pub fn write_bytes_field(out: &mut Vec<u8>, number: u32, value: &[u8]) {
    write_varint(out, ((number as u64) << 3) | 2);
    write_varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

/// Write one varint field. A zero is written, because a partial-success message
/// that omitted a zero would be an empty message, and an empty message is what
/// full success looks like.
pub fn write_varint_field(out: &mut Vec<u8>, number: u32, value: u64) {
    write_varint(out, (number as u64) << 3);
    write_varint(out, value);
}

pub fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_varint_field_reads_back() {
        let mut out = Vec::new();
        write_varint_field(&mut out, 3, 300);
        let field = Reader::new(&out).next_field().unwrap();
        assert_eq!(field.number, 3);
        assert_eq!(field.wire.as_u64(), 300);
    }

    #[test]
    fn a_length_delimited_field_reads_back() {
        let mut out = Vec::new();
        write_bytes_field(&mut out, 1, b"hello");
        let field = Reader::new(&out).next_field().unwrap();
        assert_eq!(field.number, 1);
        assert_eq!(field.wire.as_text(), "hello");
    }

    #[test]
    fn a_double_reads_back_through_fixed64() {
        let mut out = Vec::new();
        write_varint(&mut out, (4 << 3) | 1);
        out.extend_from_slice(&2.5f64.to_bits().to_le_bytes());
        let field = Reader::new(&out).next_field().unwrap();
        assert_eq!(field.wire.as_f64(), 2.5);
    }

    #[test]
    fn a_negative_sfixed64_survives() {
        let mut out = Vec::new();
        write_varint(&mut out, (6 << 3) | 1);
        out.extend_from_slice(&(-42i64).to_le_bytes());
        let field = Reader::new(&out).next_field().unwrap();
        assert_eq!(field.wire.as_i64(), -42);
    }

    #[test]
    fn every_field_arrives_in_order() {
        let mut out = Vec::new();
        write_varint_field(&mut out, 1, 7);
        write_bytes_field(&mut out, 2, b"x");
        write_varint_field(&mut out, 3, 9);
        let mut reader = Reader::new(&out);
        let numbers: Vec<u32> = std::iter::from_fn(|| reader.next_field())
            .map(|f| f.number)
            .collect();
        assert_eq!(numbers, vec![1, 2, 3]);
    }

    #[test]
    fn a_length_past_the_end_stops_rather_than_reading_past_the_buffer() {
        // A push arrives from outside. A length it chose must never index past
        // what it sent.
        let mut out = Vec::new();
        write_varint(&mut out, (1 << 3) | 2);
        write_varint(&mut out, 4_000_000);
        out.extend_from_slice(b"short");
        assert!(Reader::new(&out).next_field().is_none());
    }

    #[test]
    fn a_varint_that_never_ends_stops_rather_than_looping() {
        let forever = vec![0xffu8; 64];
        assert!(Reader::new(&forever).next_field().is_none());
    }

    #[test]
    fn a_group_wire_type_ends_the_message() {
        let out = vec![(1u8 << 3) | 3];
        assert!(Reader::new(&out).next_field().is_none());
    }

    #[test]
    fn a_field_number_of_zero_ends_the_message() {
        let out = vec![0u8, 1, 2, 3];
        assert!(Reader::new(&out).next_field().is_none());
    }

    #[test]
    fn an_empty_message_holds_no_fields() {
        assert!(Reader::new(&[]).next_field().is_none());
    }

    #[test]
    fn packed_values_read_back() {
        let mut bytes = Vec::new();
        for value in [1u64, 2, 3] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        assert_eq!(packed_fixed64(&bytes), vec![1, 2, 3]);

        let mut doubles = Vec::new();
        for value in [0.5f64, 1.5] {
            doubles.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        assert_eq!(packed_double(&doubles), vec![0.5, 1.5]);

        let mut varints = Vec::new();
        for value in [1u64, 300, 5] {
            write_varint(&mut varints, value);
        }
        assert_eq!(packed_varint(&varints), vec![1, 300, 5]);
    }

    #[test]
    fn a_packed_run_with_a_trailing_partial_value_keeps_the_whole_ones() {
        let mut bytes = vec![0u8; 8];
        bytes.extend_from_slice(&[1, 2, 3]);
        assert_eq!(packed_fixed64(&bytes).len(), 1);
    }
}
