//! The byte layout, from `docs/SEGMENT_FORMAT.md`.
//!
//! That document is normative and this module is its implementation. Every
//! constant here has a section number beside it, so a reader can check one
//! against the other without holding both in their head.
//!
//! The seven rules of section 2 are what the rest of this crate depends on:
//!
//! 1. every multi-byte integer is little-endian;
//! 2. every offset is from the start of the segment;
//! 3. every length is a byte count;
//! 4. a reader refuses a version it does not know, and does not guess;
//! 5. a reader verifies a checksum before it trusts a byte;
//! 6. a reader allocates only after it validates a declared length;
//! 7. a segment never changes after the writer publishes it.

/// Section 4. The first eight bytes of every segment.
pub const MAGIC: &[u8; 8] = b"TOWLSEG1";
/// Section 9. The last eight bytes of every segment.
pub const TRAILER_MAGIC: &[u8; 8] = b"TOWLEND1";

/// Section 4. A major change means an incompatible layout; a reader refuses it.
pub const FORMAT_MAJOR: u16 = 1;
/// Section 4. A minor change adds an optional field an older reader skips.
pub const FORMAT_MINOR: u16 = 0;

/// Section 4. The prologue never changes size.
pub const PROLOGUE_BYTES: usize = 64;
/// Section 9. The trailer is the last 32 bytes, and a reader reads it first.
pub const TRAILER_BYTES: usize = 32;
/// Section 6. The fixed part of a page header, before the null bitmap.
pub const PAGE_HEADER_BYTES: usize = 24;

/// Section 6 and D17. The page target is 64 KiB compressed, and it binds: a
/// writer that cannot reach it writes a smaller page and never a larger one.
pub const PAGE_TARGET_BYTES: usize = 64 * 1024;

/// D49. A row group targets 8 to 16 MiB of uncompressed column data, so a
/// 256 MiB segment holds roughly 16 to 32 row groups. The row-group count sets
/// the cold read cost directly.
pub const ROW_GROUP_TARGET_BYTES: usize = 12 * 1024 * 1024;

/// Section 14. A reader never decompresses before it validates a declared
/// length.
///
/// **There is no ratio limit, and there must not be one.** Two were tried and
/// both refused real segments. The first was set for what a bomb looks like in
/// the abstract; the second raised it to 10,000 and a load run of 438,866
/// events passed that too, which made every query over the affected range
/// answer `incomplete-result` **for the life of the process**.
///
/// A ratio limit cannot work here, because a high ratio is what ordinary
/// telemetry looks like. A column where every row in a page holds the same
/// release, service name, or batch identifier compresses to almost nothing, and
/// the better TallyOwl's encoding gets the more often a real page trips the
/// limit. The guard would fire hardest on the best-compressed data.
///
/// It also protected nothing. `MAX_UNCOMPRESSED_PAGE_BYTES` below is checked
/// before any expansion and is passed to the decoder as its capacity, so a
/// hostile header can make a reader allocate that much and no more, whatever
/// ratio it claims. The ratio check only ever added false refusals, and a false
/// refusal here is the worst failure class in FAILURE_MODES.md section 2: an
/// answer smaller than the truth.
///
/// What replaces it is a fact rather than a heuristic: the decompressed length
/// must equal the length the header declared. A lying header is caught by what
/// it lied about.
///
/// The absolute bound is what protects memory.
///
/// The largest uncompressed page this reader produces, whatever a header
/// claims. A 64 KiB page target cannot honestly expand past this.
pub const MAX_UNCOMPRESSED_PAGE_BYTES: usize = 64 * 1024 * 1024;

/// Section 6. How a column's values are laid out inside a page.
///
/// A number is part of the format. Never reuse one for a different encoding;
/// section 15 requires a new number and a minor version increase instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Encoding {
    /// Boolean, float, and fixed-width identifier.
    PlainFixed = 1,
    /// Unsigned integer.
    Varint = 2,
    /// Timestamp and a sorted integer.
    VarintDelta = 3,
    /// Text and bytes, as an offset array plus one byte region.
    OffsetBytes = 4,
    /// A column where a dictionary measurably reduces the page.
    Dictionary = 5,
    /// A 16-byte identifier split into its time prefix and its random tail.
    /// BENCHMARKS.md section 6 measured this as the cheapest form for a unique
    /// identifier: the prefixes delta-encode and the tails do not compress.
    SplitPrefix = 6,
}

impl Encoding {
    pub fn from_number(number: u16) -> Option<Encoding> {
        Some(match number {
            1 => Encoding::PlainFixed,
            2 => Encoding::Varint,
            3 => Encoding::VarintDelta,
            4 => Encoding::OffsetBytes,
            5 => Encoding::Dictionary,
            6 => Encoding::SplitPrefix,
            // Section 15: a reader that meets an unknown encoding fails for
            // that column. It does not return a wrong value.
            _ => return None,
        })
    }
}

/// Section 6. The compression a page used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Codec {
    None = 0,
    Zstandard = 1,
}

impl Codec {
    pub fn from_number(number: u16) -> Option<Codec> {
        Some(match number {
            0 => Codec::None,
            1 => Codec::Zstandard,
            _ => return None,
        })
    }
}

/// D17, measured twice. Level 1 for hot and warm data; level 3 only for a float
/// column, where it gains 22.4 percent. Level 3 is 1.3 percent **larger** than
/// level 1 across the winning encodings on every other column, because an
/// encoded column has already removed the redundancy a higher level would find.
pub const ZSTD_LEVEL_DEFAULT: i32 = 1;
pub const ZSTD_LEVEL_FLOAT: i32 = 3;

/// Section 7. How an exact-indexed column is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum IndexLayout {
    /// A repeated value. A term dictionary plus compressed row-ID postings.
    TermPostings = 1,
    /// A unique value that only needs presence. One filter for each row group.
    /// **This is the default for a unique value**, and D20 gives the
    /// measurement: 2.00 bytes for each row against 12.00, which is 21 percent
    /// of a whole segment.
    BlockFilter = 2,
    /// A mostly-unique value that needs a row ID without a page read. Sorted
    /// fixed-width fingerprints plus row IDs. It needs a reason, and a
    /// measurement showing the page read costs more than 10 bytes for each row.
    UniqueLookup = 3,
}

impl IndexLayout {
    pub fn from_number(number: u16) -> Option<IndexLayout> {
        Some(match number {
            1 => IndexLayout::TermPostings,
            2 => IndexLayout::BlockFilter,
            3 => IndexLayout::UniqueLookup,
            _ => return None,
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            IndexLayout::TermPostings => "term-postings",
            IndexLayout::BlockFilter => "block-filter",
            IndexLayout::UniqueLookup => "unique-lookup",
        }
    }
}

/// D20 and HIGH_CARDINALITY.md section 3. A block filter uses 12 bits for each
/// key. Sixteen buys nothing over twelve, because both round to the same size.
pub const FILTER_BITS_EACH_KEY: usize = 12;

/// Why a segment could not be read.
///
/// Every message is written for the person who has to act on it. The byte
/// offset belongs in a log rather than in the text a person sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// The file is not a segment, or stops before it should.
    Incomplete(String),
    /// A checksum did not match. The bytes on disk are not the bytes that were
    /// written.
    Damaged(String),
    /// The file is a segment this software cannot read.
    Unsupported(String),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::Incomplete(m) | FormatError::Damaged(m) | FormatError::Unsupported(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for FormatError {}

/// Section 10 and D44. Corruption detection on the hot path. It finds a damaged
/// read; it does not defend against an attacker, and it does not need to.
pub fn page_checksum(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

/// Section 10 and D44. Identity across a backup, a restore, and an object
/// store. It needs collision resistance.
pub fn content_address(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// A 64-bit fingerprint. HIGH_CARDINALITY.md section 4 measured why it is 64
/// and not 32: a 32-bit fingerprint collides 1.2 million times at 100 million
/// distinct values, which sends every lookup to a million extra segments.
///
/// A fingerprint prunes. It never decides. The reader verifies the full typed
/// value before it returns a row, so a collision cannot produce a wrong result.
pub fn fingerprint(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

// ---------------------------------------------------------------------------
// Little-endian readers and writers, so no call site repeats the byte order.
// ---------------------------------------------------------------------------

pub fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn get_u16(bytes: &[u8], at: usize) -> Result<u16, FormatError> {
    slice(bytes, at, 2).map(|s| u16::from_le_bytes(s.try_into().expect("two bytes")))
}

pub fn get_u32(bytes: &[u8], at: usize) -> Result<u32, FormatError> {
    slice(bytes, at, 4).map(|s| u32::from_le_bytes(s.try_into().expect("four bytes")))
}

pub fn get_u64(bytes: &[u8], at: usize) -> Result<u64, FormatError> {
    slice(bytes, at, 8).map(|s| u64::from_le_bytes(s.try_into().expect("eight bytes")))
}

/// A bounded slice. Every read goes through this, so no read can run past the
/// end of a truncated file.
pub fn slice(bytes: &[u8], at: usize, length: usize) -> Result<&[u8], FormatError> {
    let end = at.saturating_add(length);
    bytes.get(at..end).ok_or_else(|| {
        FormatError::Incomplete(
            "This stored file stops before it should. It was probably not finished being written."
                .to_string(),
        )
    })
}

/// A variable-length unsigned integer, least significant group first.
pub fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Read one variable-length integer, and say how many bytes it used.
pub fn get_varint(bytes: &[u8], at: usize) -> Result<(u64, usize), FormatError> {
    let mut value = 0u64;
    let mut shift = 0;
    let mut used = at;
    loop {
        let byte = *bytes.get(used).ok_or_else(|| {
            FormatError::Incomplete("A stored number stops before it should.".to_string())
        })?;
        used += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, used));
        }
        shift += 7;
        if shift >= 64 {
            return Err(FormatError::Damaged(
                "A stored number is longer than any number can be.".to_string(),
            ));
        }
    }
}

/// Zig-zag, so a small negative difference stays small.
pub fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

pub fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_encoding_number_maps_one_way() {
        // A number is part of the format. Reusing one for a different encoding
        // would make an old reader return a wrong value for a new file.
        for encoding in [
            Encoding::PlainFixed,
            Encoding::Varint,
            Encoding::VarintDelta,
            Encoding::OffsetBytes,
            Encoding::Dictionary,
            Encoding::SplitPrefix,
        ] {
            assert_eq!(Encoding::from_number(encoding as u16), Some(encoding));
        }
        // Section 15: an unknown encoding fails for that column rather than
        // returning something.
        assert_eq!(Encoding::from_number(999), None);
        assert_eq!(Encoding::from_number(0), None);
    }

    #[test]
    fn every_index_layout_number_maps_one_way() {
        for layout in [
            IndexLayout::TermPostings,
            IndexLayout::BlockFilter,
            IndexLayout::UniqueLookup,
        ] {
            assert_eq!(IndexLayout::from_number(layout as u16), Some(layout));
        }
        assert_eq!(IndexLayout::from_number(9), None);
    }

    #[test]
    fn a_variable_length_number_round_trips_at_every_width() {
        for value in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut bytes = Vec::new();
            put_varint(&mut bytes, value);
            let (read, used) = get_varint(&bytes, 0).unwrap();
            assert_eq!(read, value);
            assert_eq!(used, bytes.len());
        }
    }

    #[test]
    fn a_number_that_never_ends_is_refused() {
        // A damaged file must not be able to spin a reader.
        let bytes = vec![0xff; 12];
        assert!(matches!(
            get_varint(&bytes, 0),
            Err(FormatError::Damaged(_))
        ));
        // And one that simply stops is incomplete rather than damaged.
        assert!(matches!(
            get_varint(&[0x80], 0),
            Err(FormatError::Incomplete(_))
        ));
    }

    #[test]
    fn a_signed_difference_round_trips_and_a_small_one_stays_small() {
        for value in [0i64, -1, 1, -1000, 1000, i64::MIN, i64::MAX] {
            assert_eq!(unzigzag(zigzag(value)), value);
        }
        let mut small = Vec::new();
        put_varint(&mut small, zigzag(-1));
        assert_eq!(small.len(), 1, "a difference of one costs one byte");
    }

    #[test]
    fn a_read_past_the_end_is_incomplete_rather_than_a_panic() {
        let bytes = [1u8, 2, 3];
        assert!(slice(&bytes, 0, 3).is_ok());
        assert!(slice(&bytes, 0, 4).is_err());
        assert!(
            slice(&bytes, 3, 0).is_ok(),
            "an empty slice at the end is fine"
        );
        assert!(slice(&bytes, 3, 1).is_err());
        assert!(slice(&bytes, 4, 0).is_err());
        assert!(get_u64(&bytes, 0).is_err());
        // An offset that would overflow the addition is refused rather than
        // wrapping into a valid range.
        assert!(slice(&bytes, usize::MAX, 1).is_err());
    }

    #[test]
    fn the_two_hash_functions_do_two_different_jobs() {
        // D44 stands on the reason rather than on speed: a content address
        // needs collision resistance and a page checksum does not.
        let bytes = b"a page of column data";
        assert_eq!(page_checksum(bytes), page_checksum(bytes));
        assert_eq!(content_address(bytes), content_address(bytes));
        assert_ne!(page_checksum(bytes), page_checksum(b"another page"));
        assert_ne!(content_address(bytes), content_address(b"another page"));
        assert_eq!(content_address(bytes).len(), 32);
    }

    #[test]
    fn a_fingerprint_is_a_function_of_the_bytes() {
        // Two values with one fingerprint would still be told apart, because
        // the reader verifies the full value. This asserts the property the
        // format depends on.
        assert_eq!(fingerprint(b"trace-1"), fingerprint(b"trace-1"));
        assert_ne!(fingerprint(b"trace-1"), fingerprint(b"trace-2"));
    }
}
