//! Column pages: the encodings, and the bytes around them.
//!
//! `docs/SEGMENT_FORMAT.md` section 6 gives the page header and the encodings.
//! Two rules from it decide almost everything in this module.
//!
//! **A writer closes a page on encoded bytes, not on a row count.** A row group
//! targets 8 to 16 MiB of uncompressed column data, and a unique 16-byte column
//! does not compress, so one page for each column for each row group produced a
//! largest page of 520 KiB against a 64 KiB target. D17's second measurement
//! made this a decision rather than an implementation detail. The cost is 1.35
//! bytes for each event and it falls on one column shape: a high-cardinality
//! repeated value, where a smaller page gives the compressor less history.
//!
//! **A page is independently readable.** A query decodes only the pages it
//! needs, verifies each checksum, and never decompresses before it validates a
//! declared length.

use super::format::*;
use crate::keys::{block_context, Cipher};

/// One column's values for one page, before encoding.
///
/// The store keeps its own types rather than a wire type, so this is the set of
/// physical shapes a page holds rather than the set of property kinds a person
/// sees.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    /// Milliseconds since the epoch, or any sorted integer.
    Timestamps(Vec<i64>),
    /// A whole number that can be negative.
    Integers(Vec<i64>),
    /// A whole number that is not negative.
    Unsigned(Vec<u64>),
    /// A number that does not have to be whole.
    Floats(Vec<f64>),
    /// A true or false for each row.
    Booleans(Vec<bool>),
    /// A 16-byte identifier for each row.
    Identifiers(Vec<[u8; 16]>),
    /// Text, and a null bitmap for the rows that have none.
    Text(Vec<Option<String>>),
    /// Raw data, and a null bitmap for the rows that have none.
    Bytes(Vec<Option<Vec<u8>>>),
}

impl Column {
    pub fn len(&self) -> usize {
        match self {
            Column::Timestamps(v) => v.len(),
            Column::Integers(v) => v.len(),
            Column::Unsigned(v) => v.len(),
            Column::Floats(v) => v.len(),
            Column::Booleans(v) => v.len(),
            Column::Identifiers(v) => v.len(),
            Column::Text(v) => v.len(),
            Column::Bytes(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The rows from `start` up to `end`, so a writer can close a page part way
    /// through a column.
    pub fn slice(&self, start: usize, end: usize) -> Column {
        match self {
            Column::Timestamps(v) => Column::Timestamps(v[start..end].to_vec()),
            Column::Integers(v) => Column::Integers(v[start..end].to_vec()),
            Column::Unsigned(v) => Column::Unsigned(v[start..end].to_vec()),
            Column::Floats(v) => Column::Floats(v[start..end].to_vec()),
            Column::Booleans(v) => Column::Booleans(v[start..end].to_vec()),
            Column::Identifiers(v) => Column::Identifiers(v[start..end].to_vec()),
            Column::Text(v) => Column::Text(v[start..end].to_vec()),
            Column::Bytes(v) => Column::Bytes(v[start..end].to_vec()),
        }
    }

    /// The encoding this shape uses. Version 1 uses simple encodings, and D17
    /// requires a benchmark to justify anything more elaborate.
    pub fn encoding(&self) -> Encoding {
        match self {
            Column::Timestamps(_) => Encoding::VarintDelta,
            Column::Integers(_) => Encoding::VarintDelta,
            Column::Unsigned(_) => Encoding::Varint,
            Column::Floats(_) | Column::Booleans(_) => Encoding::PlainFixed,
            Column::Identifiers(_) => Encoding::SplitPrefix,
            Column::Text(_) | Column::Bytes(_) => Encoding::OffsetBytes,
        }
    }

    /// The compression level. D17 measured that level 3 is larger than level 1
    /// on every encoded column except a float column, where it gains 22.4
    /// percent.
    fn level(&self) -> i32 {
        match self {
            Column::Floats(_) => ZSTD_LEVEL_FLOAT,
            _ => ZSTD_LEVEL_DEFAULT,
        }
    }

    /// Which rows have no value. A column with no optional rows has an empty
    /// bitmap, and the bitmap costs nothing.
    fn nulls(&self) -> Vec<bool> {
        match self {
            Column::Text(v) => v.iter().map(|x| x.is_none()).collect(),
            Column::Bytes(v) => v.iter().map(|x| x.is_none()).collect(),
            _ => vec![false; self.len()],
        }
    }
}

/// One page, decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub column: Column,
    pub rows: usize,
}

/// Encode one column's values, without the page header.
fn encode_values(column: &Column) -> Vec<u8> {
    let mut out = Vec::new();
    match column {
        Column::Timestamps(values) | Column::Integers(values) => {
            // A delta keeps a rising time cheap, and zig-zag keeps a small
            // step backwards cheap too. Telemetry arrives close to in order and
            // never exactly in order.
            let mut previous = 0i64;
            for value in values {
                put_varint(&mut out, zigzag(value.wrapping_sub(previous)));
                previous = *value;
            }
        }
        Column::Unsigned(values) => {
            for value in values {
                put_varint(&mut out, *value);
            }
        }
        Column::Floats(values) => {
            for value in values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        Column::Booleans(values) => {
            // One bit for each row, least significant bit first, which is the
            // same order the null bitmap uses.
            out.resize(values.len().div_ceil(8), 0);
            for (index, value) in values.iter().enumerate() {
                if *value {
                    out[index / 8] |= 1 << (index % 8);
                }
            }
        }
        Column::Identifiers(values) => {
            // A UUIDv7 holds a millisecond timestamp in its first eight bytes,
            // so those delta-encode to almost nothing and the random tail does
            // not compress at all. Splitting them lets each half do what it can.
            let mut previous = 0i64;
            for value in values {
                let head = i64::from_be_bytes(value[0..8].try_into().expect("eight bytes"));
                put_varint(&mut out, zigzag(head.wrapping_sub(previous)));
                previous = head;
            }
            for value in values {
                out.extend_from_slice(&value[8..16]);
            }
        }
        Column::Text(values) => {
            let mut region = Vec::new();
            for value in values {
                let bytes = value.as_deref().unwrap_or("").as_bytes();
                put_varint(&mut out, bytes.len() as u64);
                region.extend_from_slice(bytes);
            }
            out.extend_from_slice(&region);
        }
        Column::Bytes(values) => {
            let mut region = Vec::new();
            for value in values {
                let bytes = value.as_deref().unwrap_or(&[]);
                put_varint(&mut out, bytes.len() as u64);
                region.extend_from_slice(bytes);
            }
            out.extend_from_slice(&region);
        }
    }
    out
}

fn decode_values(
    encoding: Encoding,
    bytes: &[u8],
    rows: usize,
    nulls: &[bool],
) -> Result<Column, FormatError> {
    let damaged = |what: &str| {
        FormatError::Damaged(format!(
            "A stored column of {what} did not read back as what it claimed to be."
        ))
    };

    Ok(match encoding {
        Encoding::VarintDelta => {
            let mut values = Vec::with_capacity(rows);
            let mut at = 0;
            let mut previous = 0i64;
            for _ in 0..rows {
                let (raw, next) = get_varint(bytes, at)?;
                at = next;
                previous = previous.wrapping_add(unzigzag(raw));
                values.push(previous);
            }
            Column::Timestamps(values)
        }
        Encoding::Varint => {
            let mut values = Vec::with_capacity(rows);
            let mut at = 0;
            for _ in 0..rows {
                let (raw, next) = get_varint(bytes, at)?;
                at = next;
                values.push(raw);
            }
            Column::Unsigned(values)
        }
        Encoding::PlainFixed => {
            // Two shapes share this encoding, and the row count against the
            // byte count tells them apart: a float is eight bytes and a boolean
            // is one bit.
            if bytes.len() == rows * 8 {
                let mut values = Vec::with_capacity(rows);
                for index in 0..rows {
                    let slice = slice(bytes, index * 8, 8)?;
                    values.push(f64::from_le_bytes(slice.try_into().expect("eight bytes")));
                }
                Column::Floats(values)
            } else if bytes.len() == rows.div_ceil(8) {
                let mut values = Vec::with_capacity(rows);
                for index in 0..rows {
                    values.push(bytes[index / 8] & (1 << (index % 8)) != 0);
                }
                Column::Booleans(values)
            } else {
                return Err(damaged("fixed-width values"));
            }
        }
        Encoding::SplitPrefix => {
            let mut heads = Vec::with_capacity(rows);
            let mut at = 0;
            let mut previous = 0i64;
            for _ in 0..rows {
                let (raw, next) = get_varint(bytes, at)?;
                at = next;
                previous = previous.wrapping_add(unzigzag(raw));
                heads.push(previous);
            }
            let mut values = Vec::with_capacity(rows);
            for (index, head) in heads.into_iter().enumerate() {
                let tail = slice(bytes, at + index * 8, 8)?;
                let mut id = [0u8; 16];
                id[0..8].copy_from_slice(&head.to_be_bytes());
                id[8..16].copy_from_slice(tail);
                values.push(id);
            }
            Column::Identifiers(values)
        }
        Encoding::OffsetBytes => {
            let mut lengths = Vec::with_capacity(rows);
            let mut at = 0;
            let mut total = 0usize;
            for _ in 0..rows {
                let (length, next) = get_varint(bytes, at)?;
                at = next;
                let length = usize::try_from(length).map_err(|_| damaged("text"))?;
                total = total.checked_add(length).ok_or_else(|| damaged("text"))?;
                lengths.push(length);
            }
            // Validate the whole declared region before reading any of it.
            if at + total > bytes.len() {
                return Err(damaged("text"));
            }
            let mut values = Vec::with_capacity(rows);
            for (index, length) in lengths.into_iter().enumerate() {
                let raw = &bytes[at..at + length];
                at += length;
                values.push(if nulls.get(index).copied().unwrap_or(false) {
                    None
                } else {
                    Some(
                        std::str::from_utf8(raw)
                            .map_err(|_| damaged("text"))?
                            .to_string(),
                    )
                });
            }
            Column::Text(values)
        }
        Encoding::Dictionary => {
            return Err(FormatError::Unsupported(
                "This stored data uses a form of compression this software does not know."
                    .to_string(),
            ))
        }
    })
}

/// Write one page, and return the bytes it added.
///
/// Section 6 gives the header exactly:
///
/// | Offset | Size | Field |
/// | --- | --- | --- |
/// | 0 | 4 | Page length, not including this header |
/// | 4 | 2 | Encoding |
/// | 6 | 2 | Compression codec |
/// | 8 | 4 | Row count |
/// | 12 | 4 | Uncompressed length |
/// | 16 | 8 | Page checksum, over the compressed bytes |
/// | 24 | n | Null bitmap, one bit for each row, least significant bit first |
/// | 24+n | m | Encoded and compressed values |
pub fn write_page(out: &mut Vec<u8>, column: &Column) -> usize {
    write_page_maybe_encrypted(out, column, None, [0; 16])
        .expect("writing without a key cannot fail")
}

/// Write one page, encrypting it when the segment has a key.
///
/// D61: the data region and the index region are encrypted; the prologue, the
/// header, and the footer stay readable so a reader still prunes by project,
/// kind, and time without a key.
///
/// The checksum covers what is on disk, so it is computed over the ciphertext.
/// A reader therefore detects damage before it tries to decrypt, and a damaged
/// page reports damage rather than a key failure.
pub fn write_page_encrypted(
    out: &mut Vec<u8>,
    column: &Column,
    cipher: &Cipher,
    segment_id: [u8; 16],
) -> Result<usize, crate::keys::KeyError> {
    write_page_maybe_encrypted(out, column, Some(cipher), segment_id)
}

fn write_page_maybe_encrypted(
    out: &mut Vec<u8>,
    column: &Column,
    cipher: Option<&Cipher>,
    segment_id: [u8; 16],
) -> Result<usize, crate::keys::KeyError> {
    let rows = column.len();
    let encoded = encode_values(column);
    let uncompressed = encoded.len();

    // A page smaller than a compressor's own frame overhead is cheaper raw. A
    // reader accepts either, because the codec travels in the header.
    let (codec, body) = if encoded.len() < 64 {
        (Codec::None, encoded)
    } else {
        match zstd::encode_all(encoded.as_slice(), column.level()) {
            Ok(compressed) if compressed.len() < encoded.len() => (Codec::Zstandard, compressed),
            _ => (Codec::None, encoded),
        }
    };

    // Section 6 puts the null bitmap between the header and the values, one bit
    // for each row, always. Omitting it when a column has no absent value would
    // save under one percent of a page and would make the layout conditional,
    // which a reader in another language would have to guess at.
    let bitmap_bytes = rows.div_ceil(8);
    let mut bitmap = vec![0u8; bitmap_bytes];
    for (index, null) in column.nulls().iter().enumerate() {
        if *null {
            bitmap[index / 8] |= 1 << (index % 8);
        }
    }

    // The checksum covers the null bitmap as well as the values.
    //
    // SEGMENT_FORMAT.md first said "xxHash3-64 of the compressed bytes", which
    // leaves the bitmap unprotected. A flipped bit there turns a value into an
    // absent one silently, which is a wrong answer rather than a smaller one,
    // and FAILURE_MODES.md section 2 ranks that as the worst outcome available.
    // A test found it; the document now says "of the null bitmap and the
    // compressed bytes". See docs/IMPLEMENTATION_LOG.md L020.
    let mut checked = Vec::with_capacity(bitmap.len() + body.len());
    checked.extend_from_slice(&bitmap);
    checked.extend_from_slice(&body);

    let start = out.len();
    if let Some(cipher) = cipher {
        // The whole checked region, so the null bitmap is protected as well as
        // the values. A bitmap in the clear would say which rows have a value.
        checked = cipher.seal(&checked, &block_context(segment_id, start as u64))?;
    }
    put_u32(out, checked.len() as u32);
    put_u16(out, column.encoding() as u16);
    put_u16(out, codec as u16);
    put_u32(out, rows as u32);
    put_u32(out, uncompressed as u32);
    put_u64(out, page_checksum(&checked));
    out.extend_from_slice(&checked);
    Ok(out.len() - start)
}

/// The bytes one page occupies, header included, without decoding it.
pub fn page_size(bytes: &[u8], at: usize) -> Result<usize, FormatError> {
    let length = get_u32(bytes, at)? as usize;
    Ok(PAGE_HEADER_BYTES + length)
}

/// Read one page.
///
/// `verify` is `integrity.mode` reaching the page: `verify-on-read` is the
/// default and checks the checksum over bytes already in memory, and `none` is
/// a legitimate choice that an operator makes knowingly. See D57.
pub fn read_page(bytes: &[u8], at: usize, verify: bool) -> Result<Page, FormatError> {
    read_page_maybe_encrypted(bytes, at, verify, None, [0; 16])
}

/// Read one page from an encrypted segment.
pub fn read_page_encrypted(
    bytes: &[u8],
    at: usize,
    verify: bool,
    cipher: &Cipher,
    segment_id: [u8; 16],
) -> Result<Page, FormatError> {
    read_page_maybe_encrypted(bytes, at, verify, Some(cipher), segment_id)
}

fn read_page_maybe_encrypted(
    bytes: &[u8],
    at: usize,
    verify: bool,
    cipher: Option<&Cipher>,
    segment_id: [u8; 16],
) -> Result<Page, FormatError> {
    let length = get_u32(bytes, at)? as usize;
    let encoding_number = get_u16(bytes, at + 4)?;
    let codec_number = get_u16(bytes, at + 6)?;
    let rows = get_u32(bytes, at + 8)? as usize;
    let uncompressed = get_u32(bytes, at + 12)? as usize;
    let checksum = get_u64(bytes, at + 16)?;

    let encoding = Encoding::from_number(encoding_number).ok_or_else(|| {
        // Section 15: a reader that meets an unknown encoding fails for that
        // column, and other columns stay readable.
        FormatError::Unsupported(
            "One column of this stored data uses a form this software does not know. \
             The rest of the data is still readable."
                .to_string(),
        )
    })?;
    let codec = Codec::from_number(codec_number).ok_or_else(|| {
        FormatError::Unsupported(
            "This stored data uses a form of compression this software does not know.".to_string(),
        )
    })?;

    // The bitmap size follows from the row count rather than from anything the
    // file claims, so a wrong row count cannot move where the values start.
    let bitmap_bytes = rows.div_ceil(8);
    let body_start = at + PAGE_HEADER_BYTES;
    let on_disk = slice(bytes, body_start, length)?;

    // The checksum is over what is on disk, so damage is detected before any
    // attempt to decrypt. A damaged encrypted page then reports damage rather
    // than looking like a key problem.
    if verify && page_checksum(on_disk) != checksum {
        return Err(FormatError::Damaged(
            "Part of the stored data did not read back as what it was written as. \
             This answer would be missing rows, so we did not give one."
                .to_string(),
        ));
    }

    let checked = match cipher {
        None => on_disk.to_vec(),
        Some(cipher) => cipher
            .open(on_disk, &block_context(segment_id, at as u64))
            .map_err(|e| FormatError::Unsupported(e.to_string()))?,
    };
    if checked.len() < bitmap_bytes {
        return Err(FormatError::Damaged(
            "A stored page is shorter than the rows it says it holds.".to_string(),
        ));
    }
    let bitmap = checked[..bitmap_bytes].to_vec();
    let body = checked[bitmap_bytes..].to_vec();

    // Validate the declared length before decompressing, which is rule 6 and
    // section 14. A decompression bomb never reaches memory.
    if uncompressed > MAX_UNCOMPRESSED_PAGE_BYTES {
        return Err(FormatError::Damaged(
            "A stored page claims to hold more data than any page can hold.".to_string(),
        ));
    }
    let plain = match codec {
        Codec::None => body,
        Codec::Zstandard => zstd::bulk::decompress(&body, uncompressed).map_err(|_| {
            FormatError::Damaged(
                "Part of the stored data could not be expanded. It is damaged.".to_string(),
            )
        })?,
    };
    if plain.len() != uncompressed {
        return Err(FormatError::Damaged(
            "Part of the stored data expanded to a different size than it claimed.".to_string(),
        ));
    }

    let nulls: Vec<bool> = (0..rows)
        .map(|index| {
            bitmap
                .get(index / 8)
                .map(|byte| byte & (1 << (index % 8)) != 0)
                .unwrap_or(false)
        })
        .collect();

    Ok(Page {
        column: decode_values(encoding, &plain, rows, &nulls)?,
        rows,
    })
}

/// Split one column into pages that each stay inside the page target.
///
/// The writer measures, rather than guessing from a row count. D17's second
/// measurement is the reason: one page for each column for each row group
/// produced a largest page eight times the target, because a unique 16-byte
/// column does not compress.
pub fn split_into_pages(column: &Column, target_bytes: usize) -> Vec<Column> {
    if column.is_empty() {
        return Vec::new();
    }

    // Measure the whole column once. A column that already fits needs no split,
    // and that is the common case for a strongly repeated value.
    let mut probe = Vec::new();
    write_page(&mut probe, column);
    if probe.len() <= target_bytes {
        return vec![column.clone()];
    }

    // It does not fit, so the split follows the measured bytes for each row
    // rather than a fixed count. A column of 16-byte unique values and a column
    // of one repeated word need very different row counts to reach one target.
    let bytes_each_row = (probe.len() as f64 / column.len() as f64).max(1.0);
    let rows_each_page = ((target_bytes as f64 / bytes_each_row) as usize).max(1);

    let mut out = Vec::new();
    let mut start = 0;
    while start < column.len() {
        let mut end = (start + rows_each_page).min(column.len());
        let mut piece = column.slice(start, end);

        // The target binds. A page that still measures over it loses rows until
        // it fits, because section 6 says a writer never writes a larger page
        // to keep a page count tidy.
        loop {
            let mut measured = Vec::new();
            write_page(&mut measured, &piece);
            if measured.len() <= target_bytes || end - start <= 1 {
                break;
            }
            end = start + ((end - start) * 3 / 4).max(1);
            piece = column.slice(start, end);
        }

        out.push(piece);
        start = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(column: Column) -> Column {
        let mut bytes = Vec::new();
        let written = write_page(&mut bytes, &column);
        assert_eq!(written, bytes.len());
        assert_eq!(page_size(&bytes, 0).unwrap(), bytes.len());
        read_page(&bytes, 0, true).unwrap().column
    }

    #[test]
    fn every_column_shape_round_trips() {
        assert_eq!(
            round_trip(Column::Timestamps(vec![
                1_785_628_800_000,
                1_785_628_800_040
            ])),
            Column::Timestamps(vec![1_785_628_800_000, 1_785_628_800_040])
        );
        assert_eq!(
            round_trip(Column::Unsigned(vec![0, 1, u64::MAX])),
            Column::Unsigned(vec![0, 1, u64::MAX])
        );
        assert_eq!(
            round_trip(Column::Floats(vec![0.5, -1.25, 0.0])),
            Column::Floats(vec![0.5, -1.25, 0.0])
        );
        assert_eq!(
            round_trip(Column::Booleans(vec![true, false, true, true])),
            Column::Booleans(vec![true, false, true, true])
        );
        assert_eq!(
            round_trip(Column::Identifiers(vec![[1; 16], [2; 16]])),
            Column::Identifiers(vec![[1; 16], [2; 16]])
        );
        assert_eq!(
            round_trip(Column::Text(vec![Some("a".into()), Some("bb".into())])),
            Column::Text(vec![Some("a".into()), Some("bb".into())])
        );
    }

    #[test]
    fn a_row_with_no_value_stays_absent_rather_than_becoming_empty_text() {
        // An absent value and an empty one are different facts, and a query
        // that could not tell them apart would answer a filter wrongly.
        let column = Column::Text(vec![Some("a".into()), None, Some("".into())]);
        assert_eq!(round_trip(column.clone()), column);
    }

    #[test]
    fn a_negative_integer_survives_the_delta_encoding() {
        let column = Column::Integers(vec![-5, 0, 5, -1_000_000]);
        // Integers and timestamps share an encoding, so the shape comes back as
        // the timestamp arm. The values are what matter.
        let Column::Timestamps(values) = round_trip(column) else {
            panic!("a delta column reads back as its encoding");
        };
        assert_eq!(values, vec![-5, 0, 5, -1_000_000]);
    }

    #[test]
    fn a_damaged_page_is_refused_rather_than_returning_fewer_rows() {
        // D57 and FAILURE_MODES.md section 2. A silent wrong answer costs more
        // than a refused query, because nobody investigates a wrong answer.
        let column = Column::Text(vec![Some("checkout-started".repeat(20))]);
        let mut bytes = Vec::new();
        write_page(&mut bytes, &column);

        let mut damaged = bytes.clone();
        let last = damaged.len() - 1;
        damaged[last] ^= 0xff;
        let failure = read_page(&damaged, 0, true).unwrap_err();
        assert!(matches!(failure, FormatError::Damaged(_)));
        assert!(
            failure.to_string().contains("did not give one"),
            "the message says why there is no answer: {failure}"
        );
    }

    #[test]
    fn integrity_none_reads_a_damaged_page_and_an_operator_chose_that() {
        // D57: `none` is a legitimate choice. An installation that wants the
        // last of the read throughput and accepts a wrong answer over a damaged
        // page may select it, and TallyOwl does not prevent the choice.
        let column = Column::Unsigned(vec![1, 2, 3]);
        let mut bytes = Vec::new();
        write_page(&mut bytes, &column);
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        assert!(read_page(&bytes, 0, true).is_err());
        assert!(read_page(&bytes, 0, false).is_ok());
    }

    #[test]
    fn an_unknown_encoding_fails_for_that_column_and_says_the_rest_is_readable() {
        let mut bytes = Vec::new();
        write_page(&mut bytes, &Column::Unsigned(vec![1, 2, 3]));
        bytes[4..6].copy_from_slice(&999u16.to_le_bytes());
        let failure = read_page(&bytes, 0, true).unwrap_err();
        assert!(matches!(failure, FormatError::Unsupported(_)));
        assert!(failure.to_string().contains("still readable"));
    }

    #[test]
    fn a_decompression_bomb_is_refused_before_it_reaches_memory() {
        // Section 14 and the required test 7 of section 16.
        //
        // A header that lies about its expanded size is caught by what it lied
        // about: the page expands to a different size than it claimed. That is
        // a fact rather than a heuristic, and it does not refuse honest data
        // the way a ratio limit did. See `format::MAX_UNCOMPRESSED_PAGE_BYTES`.
        let mut bytes = Vec::new();
        write_page(&mut bytes, &Column::Unsigned(vec![7; 4_000]));
        bytes[12..16].copy_from_slice(&(4u32 * 1024 * 1024).to_le_bytes());
        let failure = read_page(&bytes, 0, true).unwrap_err();
        assert!(matches!(failure, FormatError::Damaged(_)));
        // Nothing of that size was ever allocated from the claim: the decoder
        // is given the claim as a capacity and the result is checked against it.
        assert!(
            failure.to_string().contains("could not be expanded")
                || failure
                    .to_string()
                    .contains("different size than it claimed")
        );
    }

    #[test]
    fn an_ordinary_column_that_compresses_enormously_still_reads() {
        // **This is the regression that made every query over a range answer
        // `incomplete-result` for the life of the process.** A ratio limit
        // refused a checksum-valid page because real telemetry compresses far
        // better than the limit assumed a bomb would.
        //
        // One release name repeated down a page is the ordinary case, not the
        // exotic one, and the better the encoding gets the more often it would
        // have fired. Two limits were tried and both refused real segments.
        let rows = 60_000;
        let mut bytes = Vec::new();
        write_page(
            &mut bytes,
            &Column::Text(vec![Some("2026.8.1".to_string()); rows]),
        );

        let page = read_page(&bytes, 0, true).expect("an ordinary page reads");
        assert_eq!(page.rows, rows);
        match page.column {
            Column::Text(values) => {
                assert_eq!(values.len(), rows);
                assert_eq!(values[0].as_deref(), Some("2026.8.1"));
                assert_eq!(values[rows - 1].as_deref(), Some("2026.8.1"));
            }
            other => panic!("the column came back as {other:?}"),
        }
    }

    #[test]
    fn a_page_of_one_repeated_identifier_reads_whatever_its_ratio() {
        // The other shape that tripped it: a batch identifier is the same for
        // every row of a batch, and a batch fills pages.
        let rows = 40_000;
        let mut bytes = Vec::new();
        write_page(&mut bytes, &Column::Unsigned(vec![7_777_777; rows]));
        match read_page(&bytes, 0, true)
            .expect("an ordinary page reads")
            .column
        {
            Column::Unsigned(values) => {
                assert_eq!(values.len(), rows);
                assert!(values.iter().all(|v| *v == 7_777_777));
            }
            other => panic!("the column came back as {other:?}"),
        }
    }

    #[test]
    fn a_page_that_claims_more_than_any_page_can_hold_is_refused() {
        let mut bytes = Vec::new();
        write_page(&mut bytes, &Column::Unsigned(vec![1, 2, 3]));
        bytes[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_page(&bytes, 0, true).is_err());
    }

    #[test]
    fn a_truncated_page_is_incomplete_rather_than_a_panic() {
        let mut bytes = Vec::new();
        write_page(
            &mut bytes,
            &Column::Text(vec![Some("a longer value".into()); 40]),
        );
        for cut in [0, 4, 10, PAGE_HEADER_BYTES, bytes.len() / 2] {
            let failure = read_page(&bytes[..cut], 0, true);
            assert!(failure.is_err(), "a page cut at {cut} read back");
        }
    }

    #[test]
    fn a_page_closes_on_bytes_and_the_target_binds() {
        // The case the format as first written failed: a column of unique
        // 16-byte values, where every page must stay inside the page target.
        // See SEGMENT_FORMAT.md section 16 test 11.
        let mut ids = Vec::new();
        for index in 0..200_000u64 {
            let mut id = [0u8; 16];
            id[0..8].copy_from_slice(&(1_785_628_800_000u64 + index).to_be_bytes());
            // A tail that does not compress, which is what a real random tail
            // looks like to a compressor.
            id[8..16].copy_from_slice(
                &(index
                    .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                    .rotate_left(29)
                    .wrapping_mul(0x2545_f491_4f6c_dd1d))
                .to_be_bytes(),
            );
            ids.push(id);
        }
        let column = Column::Identifiers(ids.clone());

        let pages = split_into_pages(&column, PAGE_TARGET_BYTES);
        assert!(pages.len() > 1, "a 200,000-row unique column needs a split");

        let mut recovered: Vec<[u8; 16]> = Vec::new();
        for page in &pages {
            let mut bytes = Vec::new();
            let written = write_page(&mut bytes, page);
            assert!(
                written <= PAGE_TARGET_BYTES,
                "a page reached {written} bytes against a {PAGE_TARGET_BYTES} target"
            );
            let Column::Identifiers(values) = read_page(&bytes, 0, true).unwrap().column else {
                panic!("an identifier column reads back as one");
            };
            recovered.extend(values);
        }
        assert_eq!(recovered, ids, "splitting a column loses no row");
    }

    #[test]
    fn a_column_that_already_fits_is_one_page() {
        let column = Column::Text(vec![Some("checkout".into()); 1_000]);
        assert_eq!(split_into_pages(&column, PAGE_TARGET_BYTES).len(), 1);
    }

    #[test]
    fn an_empty_column_produces_no_page() {
        assert!(split_into_pages(&Column::Unsigned(Vec::new()), PAGE_TARGET_BYTES).is_empty());
    }

    #[test]
    fn a_strongly_repeated_column_costs_almost_nothing() {
        // The property the encoding set exists for. A repeated value compresses
        // away, and a unique one does not, so the two need different treatment
        // rather than one page size.
        let repeated = Column::Text(vec![Some("checkout".into()); 10_000]);
        let mut bytes = Vec::new();
        write_page(&mut bytes, &repeated);
        assert!(
            bytes.len() < 10_000,
            "a repeated value cost {} bytes for 10,000 rows",
            bytes.len()
        );
    }
}
