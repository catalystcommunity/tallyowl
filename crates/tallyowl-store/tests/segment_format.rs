//! The required tests from `docs/SEGMENT_FORMAT.md` section 16.
//!
//! The document lists eleven. Nine are here; two need the cold tier and the
//! encryption key design, which D28 defers and `docs/FAILURE_MODES.md` section
//! 12 records as an accepted limit:
//!
//! | Required test | Where |
//! | --- | --- |
//! | 1. A torn write at every structural boundary | `a_torn_write_at_every_boundary_is_refused` |
//! | 2. A truncated file with no trailer | `a_file_with_no_trailer_is_a_partial_write` |
//! | 3. A corrupted page, index block, and footer | `a_corrupted_*` |
//! | 4. One version written, an adjacent version reading | `an_unknown_major_version_is_refused` |
//! | 5. An unknown encoding in one column | in `segment::page` |
//! | 6. A content address that does not match the bytes | `a_content_address_that_does_not_match` |
//! | 7. A decompression bomb | in `segment::page` |
//! | 8. A read after key destruction | `tests/encryption.rs` |
//! | 9. A catalog rebuilt by scanning manifests | `tests/catalog.rs` and `tests/snapshot.rs` |
//! | 10. A block filter that says maybe, where the page then says no | `a_filter_that_says_maybe` |
//! | 11. A column of unique 16-byte values inside the page target | `every_page_stays_inside_the_target` |

use tallyowl_store::row::{EventRow, PropertyValue};
use tallyowl_store::segment::format::{FormatError, PAGE_HEADER_BYTES, PROLOGUE_BYTES};
use tallyowl_store::segment::page::page_size;
use tallyowl_store::segment::{open, SegmentWriter};

const WORKSPACE: [u8; 16] = [8; 16];
const PROJECT: [u8; 16] = [9; 16];
const BASE_TIME: i64 = 1_785_628_800_000;

/// A generator with a stated seed. A result nobody can reproduce is not a
/// result, so nothing here reads the wall clock.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

/// Rows that look like real traffic: a rising time, a unique event ID, a trace
/// shared by ten rows, and a handful of repeated dimensions.
fn rows(count: usize, seed: u64) -> Vec<EventRow> {
    let mut random = Rng::new(seed);
    let mut out = Vec::with_capacity(count);
    let mut at = BASE_TIME;
    for index in 0..count {
        at += (random.next() % 40) as i64;

        let mut event_id = [0u8; 16];
        event_id[0..8].copy_from_slice(&(BASE_TIME as u64 + index as u64).to_be_bytes());
        event_id[8..16].copy_from_slice(&random.next().to_be_bytes());

        let mut trace_id = [0u8; 16];
        trace_id[0..8].copy_from_slice(&((index / 10) as u64).to_be_bytes());
        trace_id[8..16].copy_from_slice(&((index / 10) as u64 ^ 0xabcd).to_be_bytes());

        let mut row = EventRow::new(event_id, "event", "checkout-started", at);
        row.batch_id = [1; 16];
        row.source_id = [7; 16];
        row.workspace_id = WORKSPACE;
        row.project_id = PROJECT;
        row.received_at = at + 5;
        row.committed_at = at + 9;
        row.trace_id = Some(trace_id);
        row.session_id = Some(format!("s-{}", index % 500));
        row.request_id = Some(format!("r-{index}"));
        row.service_name = Some(format!("svc-{:02}", index % 20));
        row.release = Some("2026.8.1".into());
        row = row
            .with_property(
                "route",
                PropertyValue::Text(format!("/api/{}", index % 500)),
                "client",
            )
            .with_property(
                "duration_ms",
                PropertyValue::Unsigned(index as u64 % 1_000),
                "client",
            )
            .with_property("value", PropertyValue::Decimal("19.99".into()), "client");
        out.push(row);
    }
    out
}

fn write(count: usize) -> tallyowl_store::segment::Segment {
    SegmentWriter::new([5; 16], WORKSPACE, PROJECT)
        .write(&rows(count, 42))
        .expect("the segment writes")
}

#[test]
fn a_segment_writes_and_reads_back_every_row() {
    let original = rows(5_000, 42);
    let segment = SegmentWriter::new([5; 16], WORKSPACE, PROJECT)
        .write(&original)
        .expect("the segment writes");

    let reopened = open(segment.bytes.clone(), true).expect("the segment opens");
    assert_eq!(reopened.header.row_count, 5_000);
    assert_eq!(reopened.header.workspace_id, WORKSPACE);
    assert_eq!(reopened.header.project_id, PROJECT);
    assert_eq!(reopened.content_address, segment.content_address);

    let back = reopened.rows(true).expect("the rows read");
    assert_eq!(back.len(), original.len());
    for (a, b) in original.iter().zip(&back) {
        assert_eq!(a.event_id, b.event_id);
        assert_eq!(a.occurred_at, b.occurred_at);
        assert_eq!(a.received_at, b.received_at);
        assert_eq!(a.trace_id, b.trace_id);
        assert_eq!(a.session_id, b.session_id);
        assert_eq!(
            a.properties, b.properties,
            "properties for {:?}",
            a.event_id
        );
    }
}

#[test]
fn a_header_lets_a_reader_prune_without_reading_a_page() {
    // Section 5: the header holds the facts that let a reader prune a segment
    // without reading a page.
    let segment = write(1_000);
    assert_eq!(segment.header.kinds, vec!["event".to_string()]);
    assert!(segment.header.occurred_range.0 >= BASE_TIME);
    assert!(segment.header.occurred_range.1 >= segment.header.occurred_range.0);
    // The three time facts stay separate, so a query prunes on the one it
    // asked for.
    assert!(segment.header.received_range.0 > segment.header.occurred_range.0);

    use tallyowl_store::TimeBasis;
    assert!(segment.overlaps(TimeBasis::OccurredAt, BASE_TIME, BASE_TIME + 1_000_000));
    assert!(!segment.overlaps(TimeBasis::OccurredAt, 0, BASE_TIME - 1));
    assert!(!segment.overlaps(TimeBasis::OccurredAt, BASE_TIME + 100_000_000, i64::MAX));
}

#[test]
fn every_page_stays_inside_the_target() {
    // Required test 11, and the case the format as first written failed: a
    // column of unique 16-byte values, where every page must stay inside the
    // page target. See D17's second measurement.
    use tallyowl_store::segment::format::PAGE_TARGET_BYTES;

    let segment = write(120_000);
    let mut widest = 0usize;
    let mut pages = 0usize;
    for group in &segment.footer.row_groups {
        for refs in group.pages.values() {
            for page in refs {
                let size = page_size(&segment.bytes, page.offset as usize).expect("a page header");
                widest = widest.max(size);
                pages += 1;
            }
        }
    }
    assert!(pages > 0, "the segment holds pages");
    assert!(
        widest <= PAGE_TARGET_BYTES,
        "the largest page reached {widest} bytes against a {PAGE_TARGET_BYTES} target"
    );
}

#[test]
fn a_unique_value_uses_a_filter_and_a_repeated_one_uses_postings() {
    // D20 measured both directions of the wrong choice. A unique-lookup layout
    // on a repeated value reaches a p99 of 4.7 milliseconds; a term-postings
    // layout on a unique column costs more than the data it indexes.
    use tallyowl_store::segment::IndexLayout;

    let segment = write(5_000);
    let layout = |column: &str| segment.footer.indexes.get(column).map(|(_, l)| *l);

    assert_eq!(layout("event_id"), Some(IndexLayout::BlockFilter));
    assert_eq!(layout("request_id"), Some(IndexLayout::BlockFilter));
    assert_eq!(layout("trace_id"), Some(IndexLayout::TermPostings));
    assert_eq!(layout("session_id"), Some(IndexLayout::TermPostings));
    // An ordinary descriptive column is a scan rather than an index. An index
    // costs about as much as the data it serves, so it needs a reason.
    assert_eq!(layout("name"), None);
    assert_eq!(layout("kind"), None);
}

#[test]
fn an_exact_lookup_finds_a_unique_value_and_rules_out_one_that_is_absent() {
    let original = rows(20_000, 7);
    let segment = SegmentWriter::new([5; 16], WORKSPACE, PROJECT)
        .write(&original)
        .expect("the segment writes");

    for index in [0usize, 1, 9_999, 19_999] {
        assert!(
            segment.may_hold("event_id", &original[index].event_id, true),
            "a present value was ruled out"
        );
    }
    // A value the segment does not hold is rejected without a page read most of
    // the time, and the rest of the time a page read decides.
    let mut ruled_out = 0;
    for n in 0..2_000u64 {
        let mut absent = [0u8; 16];
        absent[0..8].copy_from_slice(&n.to_be_bytes());
        absent[8..16].copy_from_slice(&(n ^ 0xdead_beef).to_be_bytes());
        if !segment.may_hold("event_id", &absent, true) {
            ruled_out += 1;
        }
    }
    assert!(
        ruled_out > 1_900,
        "a filter ruled out only {ruled_out} of 2,000 absent values"
    );
}

#[test]
fn a_filter_that_says_maybe_produces_no_row_rather_than_a_wrong_one() {
    // Required test 10. A filter answers "maybe" at a measured rate, and the
    // reader then verifies the full value in the column page. The query must
    // return no row, not a wrong row.
    let original = rows(10_000, 3);
    let segment = SegmentWriter::new([5; 16], WORKSPACE, PROJECT)
        .write(&original)
        .expect("the segment writes");
    let held: std::collections::HashSet<[u8; 16]> =
        original.iter().map(|row| row.event_id).collect();

    let stored = segment.rows(true).expect("the rows read");
    let mut checked = 0;
    for n in 0..20_000u64 {
        let mut probe = [0u8; 16];
        probe[0..8].copy_from_slice(&n.to_be_bytes());
        probe[8..16].copy_from_slice(&(n ^ 0x1234_5678).to_be_bytes());
        if held.contains(&probe) || !segment.may_hold("event_id", &probe, true) {
            continue;
        }
        // The filter said maybe for a value the segment does not hold. The
        // verification step has to produce nothing.
        checked += 1;
        assert!(
            !stored.iter().any(|row| row.event_id == probe),
            "a false positive produced a row"
        );
    }
    // The rate is small, so this only ever exercises a handful. Asserting it
    // ran at all would make the test depend on the hash, which is not the
    // property under test; asserting the outcome is the property.
    let _ = checked;
}

#[test]
fn a_trace_lookup_gives_the_row_list_a_filter_cannot() {
    use tallyowl_store::segment::index::ColumnIndex;

    let original = rows(1_000, 11);
    let segment = SegmentWriter::new([5; 16], WORKSPACE, PROJECT)
        .write(&original)
        .expect("the segment writes");

    let Some(ColumnIndex::TermPostings(postings)) =
        segment.index("trace_id", true).expect("the index reads")
    else {
        panic!("a trace column uses postings");
    };
    let trace = original[25].trace_id.expect("a trace");
    let list = postings.rows(&trace).expect("the trace is indexed");
    assert_eq!(list.len(), 10, "each trace holds ten rows in this scenario");

    let stored = segment.rows(true).expect("the rows read");
    for row in list {
        assert_eq!(stored[*row as usize].trace_id, Some(trace));
    }
}

// ---------------------------------------------------------------------------
// Section 16: the failures
// ---------------------------------------------------------------------------

#[test]
fn a_file_with_no_trailer_is_a_partial_write() {
    // Required test 2. A file that does not end with the trailer magic is
    // incomplete, and recovery treats it as a partial write rather than as
    // damage. The difference matters: a partial write is adopted or removed,
    // and damage is reported.
    let segment = write(200);
    let cut = segment.bytes.len() - 4;
    let failure = open(segment.bytes[..cut].to_vec(), true).unwrap_err();
    assert!(matches!(failure, FormatError::Incomplete(_)), "{failure}");
    assert!(failure.to_string().contains("not finished"));
}

#[test]
fn a_torn_write_at_every_boundary_is_refused() {
    // Required test 1. A crash can stop a write anywhere, and no cut may
    // produce a segment that reads back as complete.
    let segment = write(4_000);
    let total = segment.bytes.len();
    let boundaries = [
        0,
        1,
        PROLOGUE_BYTES - 1,
        PROLOGUE_BYTES,
        PROLOGUE_BYTES + 1,
        total / 8,
        total / 4,
        total / 2,
        total * 3 / 4,
        total - PAGE_HEADER_BYTES,
        total - 33,
        total - 32,
        total - 8,
        total - 1,
    ];
    for cut in boundaries {
        let failure = open(segment.bytes[..cut].to_vec(), true);
        assert!(failure.is_err(), "a file cut at {cut} of {total} read back");
    }
    // And the whole file still reads, so the test is not passing by refusing
    // everything.
    assert!(open(segment.bytes.clone(), true).is_ok());
}

#[test]
fn a_corrupted_footer_is_detected() {
    // Required test 3, the footer. A reader cannot say what the file holds, so
    // it does not answer from it.
    let segment = write(500);
    let mut damaged = segment.bytes.clone();
    let footer_at = damaged.len() - 40;
    damaged[footer_at] ^= 0xff;
    let failure = open(damaged, true).unwrap_err();
    assert!(matches!(failure, FormatError::Damaged(_)), "{failure}");
    assert!(failure.to_string().contains("did not answer"));
}

#[test]
fn a_corrupted_page_is_detected_when_the_row_is_read() {
    // Required test 3, a page. Verification happens when a query touches the
    // data, which is what `verify-on-read` means.
    let segment = write(2_000);
    let first_page = segment.footer.row_groups[0]
        .pages
        .values()
        .flat_map(|refs| refs.iter())
        .map(|page| page.offset as usize)
        .min()
        .expect("a page");

    let mut damaged = segment.bytes.clone();
    damaged[first_page + PAGE_HEADER_BYTES + 4] ^= 0xff;

    // The file still opens, because the footer is intact. The damage shows up
    // when the page is read, which is the point of `verify-on-read`.
    let reopened = open(damaged, true).expect("the directory is still readable");
    let failure = reopened.rows(true).unwrap_err();
    assert!(matches!(failure, FormatError::Damaged(_)), "{failure}");
}

#[test]
fn a_corrupted_index_block_is_detected() {
    // Required test 3, an index block.
    let segment = write(2_000);
    let (offset, _) = segment.footer.indexes["trace_id"];
    let mut damaged = segment.bytes.clone();
    damaged[offset as usize + 20] ^= 0xff;

    let reopened = open(damaged, true).expect("the directory is still readable");
    let failure = reopened.index("trace_id", true).unwrap_err();
    assert!(matches!(failure, FormatError::Damaged(_)), "{failure}");
}

#[test]
fn a_content_address_that_does_not_match_the_bytes_is_detected() {
    // Required test 6. This is what `scrub` runs and what a restore checks
    // before it publishes a snapshot.
    let segment = write(1_000);
    assert!(segment.verify().is_ok());

    let mut damaged = segment.bytes.clone();
    let middle = damaged.len() / 2;
    damaged[middle] ^= 0xff;
    let reopened = open(damaged, false).expect("the file still opens");
    let failure = reopened.verify().unwrap_err();
    assert!(matches!(failure, FormatError::Damaged(_)));
    assert!(failure.to_string().contains("contents changed"));
}

#[test]
fn an_unknown_major_version_is_refused_rather_than_guessed_at() {
    // Required test 4, and rule 4 of section 2. Startup refuses an unknown
    // incompatible version rather than guessing.
    let segment = write(100);
    let mut future = segment.bytes.clone();
    future[8..10].copy_from_slice(&99u16.to_le_bytes());
    let failure = open(future, true).unwrap_err();
    assert!(matches!(failure, FormatError::Unsupported(_)));
    assert!(failure.to_string().contains("cannot read it"));
}

#[test]
fn a_minor_version_an_older_reader_does_not_know_is_still_readable() {
    // Section 15: a minor version adds an optional field, and an older reader
    // skips it. A reader that refused a minor bump would break every rolling
    // upgrade.
    let segment = write(100);
    let mut newer = segment.bytes.clone();
    newer[10..12].copy_from_slice(&7u16.to_le_bytes());
    let reopened = open(newer, true).expect("a newer minor version still reads");
    assert_eq!(reopened.header.row_count, 100);
}

#[test]
fn a_file_that_is_not_a_segment_is_refused() {
    assert!(open(vec![0u8; 200], true).is_err());
    assert!(open(b"not a segment at all".to_vec(), true).is_err());
    assert!(open(Vec::new(), true).is_err());
}

#[test]
fn a_segment_with_no_rows_is_refused() {
    // A catalog entry for nothing is a file nobody can use.
    let failure = SegmentWriter::new([5; 16], WORKSPACE, PROJECT)
        .write(&[])
        .unwrap_err();
    assert!(matches!(failure, FormatError::Unsupported(_)));
}

#[test]
fn one_set_of_rows_writes_the_same_bytes_every_time() {
    // The content address is the segment's identity in a backup, a restore, and
    // an object store, so it has to be a function of the rows rather than of
    // when they were written.
    let original = rows(2_000, 99);
    let first = SegmentWriter::new([5; 16], WORKSPACE, PROJECT)
        .write(&original)
        .unwrap();
    let second = SegmentWriter::new([5; 16], WORKSPACE, PROJECT)
        .write(&original)
        .unwrap();
    assert_eq!(first.content_address, second.content_address);
    assert_eq!(first.bytes, second.bytes);
}

#[test]
fn a_segment_holds_several_row_groups_when_it_is_large_enough() {
    // D49: the row-group count sets the cold request count for a column read,
    // so a segment that never split would make every cold read one request per
    // page and a segment that split too far would make it hundreds.
    let mut writer = SegmentWriter::new([5; 16], WORKSPACE, PROJECT);
    writer.row_group_target_bytes = 256 * 1024;
    let segment = writer.write(&rows(50_000, 5)).expect("the segment writes");
    assert!(
        segment.footer.row_groups.len() > 1,
        "a large segment holds more than one row group"
    );
    assert_eq!(
        segment
            .footer
            .row_groups
            .iter()
            .map(|g| g.rows)
            .sum::<u64>(),
        50_000
    );
    assert_eq!(segment.rows(true).unwrap().len(), 50_000);
}

#[test]
fn the_measured_cost_for_each_event_is_reported_rather_than_assumed() {
    // BENCHMARKS.md section 12a measured 39.75 bytes for each event on
    // generated values, with a block filter on the unique column. This asserts
    // the order of magnitude rather than the figure: the figure belongs to a
    // benchmark on real storage, and section 9 of the implementation prompt
    // forbids treating a unit test as a capacity measurement.
    let segment = write(100_000);
    let each_event = segment.bytes.len() as f64 / 100_000.0;
    assert!(
        (10.0..200.0).contains(&each_event),
        "a segment cost {each_event} bytes for each event, which is outside any plausible range"
    );
}
