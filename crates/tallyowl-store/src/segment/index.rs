//! The index region: exact lookup on high-cardinality values.
//!
//! `docs/SEGMENT_FORMAT.md` section 7 gives the layouts and
//! `docs/HIGH_CARDINALITY.md` gives the reasoning. Three rules decide this
//! module, and each came from a measurement.
//!
//! **A unique value gets a block filter, not a list.** The whole-segment
//! measurement showed the `event_id` unique-lookup index costing 25 percent of
//! the segment, which is more than the column it indexes. At 12 bits for each
//! key a filter costs 2.00 bytes for each row against 12.00, saves 21 percent
//! of the whole segment, and answers "no" exactly. See D20.
//!
//! **A repeated value gets postings.** A filter cannot give a row list, and a
//! query for a whole trace wants one.
//!
//! **A fingerprint prunes. It never decides.** The reader verifies the full
//! typed value before it returns a row, so a collision cannot produce an
//! incorrect result. That is what makes a 64-bit fingerprint safe and a 32-bit
//! one merely expensive.

use std::collections::BTreeMap;

use super::format::*;
use crate::keys::{block_context, Cipher};

/// A blocked filter over the values of one row group.
///
/// One 64-bit word for each key and four bits inside it, so every probe touches
/// one cache line. D20 measured 21 million lookups each second and a false
/// positive rate of 0.53 percent for each row group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockFilter {
    words: Vec<u64>,
}

impl BlockFilter {
    /// A filter sized for `rows` keys. The word count is a power of two so the
    /// index is a mask rather than a division.
    pub fn new(rows: usize) -> BlockFilter {
        let words = (rows * FILTER_BITS_EACH_KEY / 64)
            .max(8)
            .next_power_of_two();
        BlockFilter {
            words: vec![0u64; words],
        }
    }

    fn positions(&self, hash: u64) -> [(usize, u64); 4] {
        let word = (hash as usize >> 32) & (self.words.len() - 1);
        let mut out = [(0usize, 0u64); 4];
        let mut mixed = hash;
        for slot in out.iter_mut() {
            mixed = mixed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (mixed >> 29);
            *slot = (word, 1u64 << (mixed & 63));
        }
        out
    }

    pub fn insert(&mut self, value: &[u8]) {
        let hash = fingerprint(value);
        for (word, bit) in self.positions(hash) {
            self.words[word] |= bit;
        }
    }

    /// `false` means the value is definitely absent. `true` means it may be
    /// present, and the reader then decodes one column page for an exact
    /// answer. Correctness holds either way, because the reader verifies the
    /// full value.
    pub fn maybe(&self, value: &[u8]) -> bool {
        let hash = fingerprint(value);
        self.positions(hash)
            .iter()
            .all(|(word, bit)| self.words[*word] & bit != 0)
    }

    pub fn byte_len(&self) -> usize {
        self.words.len() * 8
    }

    fn write(&self, out: &mut Vec<u8>) {
        put_u32(out, self.words.len() as u32);
        for word in &self.words {
            put_u64(out, *word);
        }
    }

    fn read(bytes: &[u8], at: usize) -> Result<(BlockFilter, usize), FormatError> {
        let count = get_u32(bytes, at)? as usize;
        // Validate the declared length before allocating for it.
        slice(bytes, at + 4, count * 8)?;
        let mut words = Vec::with_capacity(count);
        for index in 0..count {
            words.push(get_u64(bytes, at + 4 + index * 8)?);
        }
        Ok((BlockFilter { words }, at + 4 + count * 8))
    }
}

/// One column's index inside one segment.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnIndex {
    /// One filter for each row group, in row-group order.
    BlockFilter(Vec<BlockFilter>),
    /// A sorted term dictionary plus delta-encoded row IDs.
    TermPostings(TermPostings),
    /// Sorted fingerprints paired with row IDs, twelve bytes for each row.
    UniqueLookup(UniqueLookup),
}

impl ColumnIndex {
    pub fn layout(&self) -> IndexLayout {
        match self {
            ColumnIndex::BlockFilter(_) => IndexLayout::BlockFilter,
            ColumnIndex::TermPostings(_) => IndexLayout::TermPostings,
            ColumnIndex::UniqueLookup(_) => IndexLayout::UniqueLookup,
        }
    }
}

/// A term dictionary plus its postings.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TermPostings {
    /// The full term, so the reader verifies the value rather than a hash of
    /// it, paired with the rows that hold it.
    terms: BTreeMap<Vec<u8>, Vec<u32>>,
}

impl TermPostings {
    pub fn new() -> TermPostings {
        TermPostings::default()
    }

    pub fn add(&mut self, value: &[u8], row: u32) {
        self.terms.entry(value.to_vec()).or_default().push(row);
    }

    /// Every row that holds this value, or nothing when the segment has none.
    pub fn rows(&self, value: &[u8]) -> Option<&[u32]> {
        self.terms.get(value).map(|rows| rows.as_slice())
    }

    pub fn term_count(&self) -> usize {
        self.terms.len()
    }

    fn write(&self, out: &mut Vec<u8>) {
        put_u32(out, self.terms.len() as u32);
        for (term, rows) in &self.terms {
            put_varint(out, term.len() as u64);
            out.extend_from_slice(term);
            put_varint(out, rows.len() as u64);
            // Row IDs rise, so a difference is small and a varint is cheap.
            let mut previous = 0u32;
            for row in rows {
                put_varint(out, (row - previous) as u64);
                previous = *row;
            }
        }
    }

    fn read(bytes: &[u8], at: usize) -> Result<(TermPostings, usize), FormatError> {
        let count = get_u32(bytes, at)? as usize;
        let mut used = at + 4;
        // A term needs at least one byte, so a count past what remains is a
        // damaged file rather than a large allocation.
        if count > bytes.len().saturating_sub(used) {
            return Err(FormatError::Damaged(
                "A stored index claims more values than it holds.".to_string(),
            ));
        }
        let mut terms = BTreeMap::new();
        for _ in 0..count {
            let (length, next) = get_varint(bytes, used)?;
            let length = usize::try_from(length).map_err(|_| {
                FormatError::Damaged("A stored index value is longer than any value.".to_string())
            })?;
            let term = slice(bytes, next, length)?.to_vec();
            used = next + length;

            let (rows, next) = get_varint(bytes, used)?;
            used = next;
            let rows = usize::try_from(rows).map_err(|_| {
                FormatError::Damaged("A stored index names more rows than any segment.".to_string())
            })?;
            if rows > bytes.len().saturating_sub(used) {
                return Err(FormatError::Damaged(
                    "A stored index names more rows than it holds.".to_string(),
                ));
            }
            let mut list = Vec::with_capacity(rows);
            let mut previous = 0u32;
            for _ in 0..rows {
                let (step, next) = get_varint(bytes, used)?;
                used = next;
                previous = previous.wrapping_add(step as u32);
                list.push(previous);
            }
            terms.insert(term, list);
        }
        Ok((TermPostings { terms }, used))
    }
}

/// Sorted fingerprints paired with row IDs.
///
/// Twelve bytes for each row, whatever the value. It stays in the format for a
/// query that must locate a row without decoding a page, and D20 now requires a
/// measurement to justify choosing it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UniqueLookup {
    entries: Vec<(u64, u32)>,
}

impl UniqueLookup {
    pub fn new() -> UniqueLookup {
        UniqueLookup::default()
    }

    pub fn add(&mut self, value: &[u8], row: u32) {
        self.entries.push((fingerprint(value), row));
    }

    pub fn seal(&mut self) {
        self.entries.sort_unstable();
    }

    /// Candidate rows for a value. A collision returns extra rows, and the
    /// caller verifies the full value, so the answer is never wrong.
    pub fn candidates(&self, value: &[u8]) -> Vec<u32> {
        let target = fingerprint(value);
        let start = self.entries.partition_point(|(hash, _)| *hash < target);
        self.entries[start..]
            .iter()
            .take_while(|(hash, _)| *hash == target)
            .map(|(_, row)| *row)
            .collect()
    }

    pub fn byte_len(&self) -> usize {
        self.entries.len() * 12
    }

    fn write(&self, out: &mut Vec<u8>) {
        put_u32(out, self.entries.len() as u32);
        for (hash, row) in &self.entries {
            put_u64(out, *hash);
            put_u32(out, *row);
        }
    }

    fn read(bytes: &[u8], at: usize) -> Result<(UniqueLookup, usize), FormatError> {
        let count = get_u32(bytes, at)? as usize;
        slice(bytes, at + 4, count * 12)?;
        let mut entries = Vec::with_capacity(count);
        for index in 0..count {
            let base = at + 4 + index * 12;
            entries.push((get_u64(bytes, base)?, get_u32(bytes, base + 8)?));
        }
        Ok((UniqueLookup { entries }, at + 4 + count * 12))
    }
}

/// Write one column's index as a self-describing block.
///
/// Section 7: each index block carries its own length, encoding, and checksum.
pub fn write_index(out: &mut Vec<u8>, index: &ColumnIndex) -> usize {
    write_index_maybe_encrypted(out, index, None, [0; 16])
        .expect("writing without a key cannot fail")
}

/// Write one index block, encrypting it when the segment has a key.
///
/// D61 encrypts the index region as well as the data region, because the index
/// holds fingerprints of end-user, session, request, and trace identifiers. D9
/// makes the end-user identifier an erasure key, so a readable fingerprint index
/// in an object store would let anyone with bucket access enumerate and
/// correlate exactly the values erasure exists to make unreadable.
pub fn write_index_encrypted(
    out: &mut Vec<u8>,
    index: &ColumnIndex,
    cipher: &Cipher,
    segment_id: [u8; 16],
) -> Result<usize, crate::keys::KeyError> {
    write_index_maybe_encrypted(out, index, Some(cipher), segment_id)
}

fn write_index_maybe_encrypted(
    out: &mut Vec<u8>,
    index: &ColumnIndex,
    cipher: Option<&Cipher>,
    segment_id: [u8; 16],
) -> Result<usize, crate::keys::KeyError> {
    let mut body = Vec::new();
    match index {
        ColumnIndex::BlockFilter(filters) => {
            put_u32(&mut body, filters.len() as u32);
            for filter in filters {
                filter.write(&mut body);
            }
        }
        ColumnIndex::TermPostings(postings) => postings.write(&mut body),
        ColumnIndex::UniqueLookup(lookup) => lookup.write(&mut body),
    }

    let start = out.len();
    if let Some(cipher) = cipher {
        body = cipher.seal(&body, &block_context(segment_id, start as u64))?;
    }
    put_u32(out, body.len() as u32);
    put_u16(out, index.layout() as u16);
    put_u64(out, page_checksum(&body));
    out.extend_from_slice(&body);
    Ok(out.len() - start)
}

/// The bytes one index block occupies, without decoding it.
pub const INDEX_HEADER_BYTES: usize = 14;

pub fn index_size(bytes: &[u8], at: usize) -> Result<usize, FormatError> {
    Ok(INDEX_HEADER_BYTES + get_u32(bytes, at)? as usize)
}

/// Read one index block.
pub fn read_index(bytes: &[u8], at: usize, verify: bool) -> Result<ColumnIndex, FormatError> {
    read_index_maybe_encrypted(bytes, at, verify, None, [0; 16])
}

/// Read one index block from an encrypted segment.
pub fn read_index_encrypted(
    bytes: &[u8],
    at: usize,
    verify: bool,
    cipher: &Cipher,
    segment_id: [u8; 16],
) -> Result<ColumnIndex, FormatError> {
    read_index_maybe_encrypted(bytes, at, verify, Some(cipher), segment_id)
}

fn read_index_maybe_encrypted(
    bytes: &[u8],
    at: usize,
    verify: bool,
    cipher: Option<&Cipher>,
    segment_id: [u8; 16],
) -> Result<ColumnIndex, FormatError> {
    let length = get_u32(bytes, at)? as usize;
    let layout_number = get_u16(bytes, at + 4)?;
    let checksum = get_u64(bytes, at + 6)?;
    let on_disk = slice(bytes, at + INDEX_HEADER_BYTES, length)?;

    if verify && page_checksum(on_disk) != checksum {
        return Err(FormatError::Damaged(
            "Part of the stored index did not read back as what it was written as. \
             A lookup here would miss rows, so we did not answer."
                .to_string(),
        ));
    }

    let opened;
    let body: &[u8] = match cipher {
        None => on_disk,
        Some(cipher) => {
            opened = cipher
                .open(on_disk, &block_context(segment_id, at as u64))
                .map_err(|e| FormatError::Unsupported(e.to_string()))?;
            &opened
        }
    };

    let layout = IndexLayout::from_number(layout_number).ok_or_else(|| {
        FormatError::Unsupported(
            "One index in this stored data uses a form this software does not know.".to_string(),
        )
    })?;

    Ok(match layout {
        IndexLayout::BlockFilter => {
            let count = get_u32(body, 0)? as usize;
            if count > body.len() {
                return Err(FormatError::Damaged(
                    "A stored index claims more parts than it holds.".to_string(),
                ));
            }
            let mut filters = Vec::with_capacity(count);
            let mut used = 4;
            for _ in 0..count {
                let (filter, next) = BlockFilter::read(body, used)?;
                filters.push(filter);
                used = next;
            }
            ColumnIndex::BlockFilter(filters)
        }
        IndexLayout::TermPostings => ColumnIndex::TermPostings(TermPostings::read(body, 0)?.0),
        IndexLayout::UniqueLookup => ColumnIndex::UniqueLookup(UniqueLookup::read(body, 0)?.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identifier(n: u64) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[0..8].copy_from_slice(&(1_785_628_800_000u64 + n).to_be_bytes());
        out[8..16].copy_from_slice(&n.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
        out
    }

    fn round_trip(index: &ColumnIndex) -> ColumnIndex {
        let mut bytes = Vec::new();
        let written = write_index(&mut bytes, index);
        assert_eq!(written, bytes.len());
        assert_eq!(index_size(&bytes, 0).unwrap(), bytes.len());
        read_index(&bytes, 0, true).unwrap()
    }

    #[test]
    fn a_filter_answers_no_exactly() {
        // The property that makes a filter safe: a value it rejects is
        // definitely absent, so a query never has to open the segment.
        let mut filter = BlockFilter::new(10_000);
        for n in 0..10_000u64 {
            filter.insert(&identifier(n));
        }
        for n in 0..10_000u64 {
            assert!(filter.maybe(&identifier(n)), "a present value was rejected");
        }
    }

    #[test]
    fn a_filter_costs_about_two_bytes_for_each_row() {
        // D20: 12 bits for each key is 2.00 bytes for each row against 12.00
        // for a unique lookup, and it saved 21 percent of a whole segment.
        let filter = BlockFilter::new(100_000);
        let each_row = filter.byte_len() as f64 / 100_000.0;
        assert!(
            (1.0..=3.0).contains(&each_row),
            "a filter cost {each_row} bytes for each row"
        );
    }

    #[test]
    fn a_filter_says_maybe_rarely_and_a_page_read_then_decides() {
        // The measured rate is 0.53 percent for each row group. This asserts
        // the order of magnitude rather than the exact figure, because the
        // exact figure belongs to a benchmark and not to a unit test.
        let mut filter = BlockFilter::new(20_000);
        for n in 0..20_000u64 {
            filter.insert(&identifier(n));
        }
        let mut maybes = 0;
        for n in 1_000_000..1_020_000u64 {
            if filter.maybe(&identifier(n)) {
                maybes += 1;
            }
        }
        let rate = maybes as f64 / 20_000.0;
        assert!(
            rate < 0.05,
            "a filter said maybe {} percent of the time",
            rate * 100.0
        );
    }

    #[test]
    fn postings_give_the_row_list_a_filter_cannot() {
        // A query for a whole trace wants the rows, which is why a repeated
        // value uses postings rather than a filter.
        let mut postings = TermPostings::new();
        for row in 0..30u32 {
            postings.add(&identifier(u64::from(row) / 10), row);
        }
        assert_eq!(postings.term_count(), 3);
        assert_eq!(
            postings.rows(&identifier(1)).unwrap(),
            &[10, 11, 12, 13, 14, 15, 16, 17, 18, 19]
        );
        assert!(postings.rows(&identifier(99)).is_none());
    }

    #[test]
    fn postings_round_trip_through_the_bytes() {
        let mut postings = TermPostings::new();
        for row in 0..100u32 {
            postings.add(format!("service-{}", row % 7).as_bytes(), row);
        }
        let ColumnIndex::TermPostings(back) =
            round_trip(&ColumnIndex::TermPostings(postings.clone()))
        else {
            panic!("a postings index reads back as one");
        };
        assert_eq!(back, postings);
        // Rows 3, 10, 17, ... 94: fourteen of the hundred.
        assert_eq!(back.rows(b"service-3").unwrap().len(), 14);
    }

    #[test]
    fn a_unique_lookup_finds_a_row_without_a_page_read() {
        let mut lookup = UniqueLookup::new();
        for row in 0..5_000u32 {
            lookup.add(&identifier(u64::from(row)), row);
        }
        lookup.seal();
        for row in [0u32, 1, 2_500, 4_999] {
            assert_eq!(lookup.candidates(&identifier(u64::from(row))), vec![row]);
        }
        assert!(lookup.candidates(&identifier(999_999)).is_empty());
    }

    #[test]
    fn a_unique_lookup_costs_twelve_bytes_for_each_row() {
        // The measurement that made a filter the default: this is the cost a
        // filter avoids, and it was the largest single line item in the format.
        let mut lookup = UniqueLookup::new();
        for row in 0..1_000u32 {
            lookup.add(&identifier(u64::from(row)), row);
        }
        assert_eq!(lookup.byte_len(), 12_000);
    }

    #[test]
    fn every_layout_round_trips_through_the_bytes() {
        let mut filter = BlockFilter::new(1_000);
        for n in 0..1_000u64 {
            filter.insert(&identifier(n));
        }
        let ColumnIndex::BlockFilter(back) = round_trip(&ColumnIndex::BlockFilter(vec![
            filter.clone(),
            BlockFilter::new(10),
        ])) else {
            panic!("a filter index reads back as one");
        };
        assert_eq!(back.len(), 2);
        assert!(back[0].maybe(&identifier(7)));

        let mut lookup = UniqueLookup::new();
        lookup.add(&identifier(1), 0);
        lookup.seal();
        let ColumnIndex::UniqueLookup(back) = round_trip(&ColumnIndex::UniqueLookup(lookup)) else {
            panic!("a unique lookup reads back as one");
        };
        assert_eq!(back.candidates(&identifier(1)), vec![0]);
    }

    #[test]
    fn a_damaged_index_block_is_refused_rather_than_answered_wrongly() {
        // A lookup over a damaged index would miss rows, which is a wrong
        // answer rather than a smaller one.
        let mut postings = TermPostings::new();
        postings.add(b"checkout", 0);
        postings.add(b"pricing", 1);
        let mut bytes = Vec::new();
        write_index(&mut bytes, &ColumnIndex::TermPostings(postings));

        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        let failure = read_index(&bytes, 0, true).unwrap_err();
        assert!(matches!(failure, FormatError::Damaged(_)));
        assert!(failure.to_string().contains("did not answer"));
    }

    #[test]
    fn integrity_none_reads_a_damaged_index_and_an_operator_chose_that() {
        // D57: `none` is a legitimate choice, and TallyOwl does not prevent it.
        // A filter is the shape that shows this plainly, because a flipped bit
        // still decodes and simply answers differently.
        let mut filter = BlockFilter::new(1_000);
        filter.insert(b"trace-1");
        let mut bytes = Vec::new();
        write_index(&mut bytes, &ColumnIndex::BlockFilter(vec![filter]));

        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        assert!(
            read_index(&bytes, 0, true).is_err(),
            "the default refuses it"
        );
        assert!(
            read_index(&bytes, 0, false).is_ok(),
            "`none` reads it anyway"
        );
    }

    #[test]
    fn an_unknown_index_layout_is_refused() {
        let mut bytes = Vec::new();
        write_index(&mut bytes, &ColumnIndex::UniqueLookup(UniqueLookup::new()));
        bytes[4..6].copy_from_slice(&77u16.to_le_bytes());
        assert!(matches!(
            read_index(&bytes, 0, true),
            Err(FormatError::Unsupported(_))
        ));
    }

    #[test]
    fn an_index_that_claims_more_than_it_holds_is_refused_before_it_allocates() {
        let mut bytes = Vec::new();
        write_index(&mut bytes, &ColumnIndex::UniqueLookup(UniqueLookup::new()));
        // Claim a huge entry count inside the body, and turn off verification
        // so the length check is what has to catch it.
        let body = INDEX_HEADER_BYTES;
        bytes[body..body + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_index(&bytes, 0, false).is_err());
    }

    #[test]
    fn a_truncated_index_block_is_incomplete_rather_than_a_panic() {
        let mut postings = TermPostings::new();
        for row in 0..50u32 {
            postings.add(format!("route-{row}").as_bytes(), row);
        }
        let mut bytes = Vec::new();
        write_index(&mut bytes, &ColumnIndex::TermPostings(postings));
        for cut in [0, 5, INDEX_HEADER_BYTES, bytes.len() / 2, bytes.len() - 1] {
            assert!(read_index(&bytes[..cut], 0, false).is_err(), "cut at {cut}");
        }
    }
}
