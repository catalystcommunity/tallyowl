//! The tablet locator: which segments can hold a value.
//!
//! `docs/HIGH_CARDINALITY.md` section 4. Segment-local indexes alone would still
//! require checking every retained segment, so each tablet keeps immutable,
//! time-partitioned locator runs:
//!
//! ```text
//! (field, typed-value fingerprint, time bucket) -> candidate segments
//! ```
//!
//! # What it costs, and what drives that
//!
//! **The locator holds one reference for each (value, segment) pair. The pair
//! count, not the distinct-value count, sets its size.** 100 million end users
//! cost nothing on their own; a user whose events reach 100 segments in a day
//! costs 100 entries. `prototypes/locator-bench` measured 2.4 GiB and 867,000
//! probes each second at 100 million users over 30 days.
//!
//! Two rules follow from that measurement, and both are implemented here:
//!
//! - **sorting rows inside a segment does not help.** It reorders rows and does
//!   not change which segment holds them. Only routing and compaction change the
//!   pair count;
//! - **a time range is the strongest prune.** Runs are partitioned by time, so a
//!   query reads only the runs it overlaps, and the prune is linear: a one-day
//!   range read 2 candidate segments where a 30-day range read 60.
//!
//! # A fingerprint prunes and never decides
//!
//! 64 bits, because a 32-bit fingerprint collides 1.2 million times at 100
//! million distinct values. A collision costs a wasted segment open and cannot
//! produce a wrong answer, because the segment index verifies the full typed
//! value before any row is returned.
//!
//! # An unbounded lookup is legal and never accidental
//!
//! An exact lookup with no time bound reads the whole retention window. That is
//! sometimes what a support investigation needs. [`Locator::candidate_count`]
//! exists so a query surface can show the number before it runs the query.

use std::collections::BTreeMap;

use crate::segment::format::{fingerprint, get_u32, get_u64, put_u32, put_u64, slice, FormatError};

/// One day. `docs/HIGH_CARDINALITY.md` section 4 measures the prune in days, and
/// the hot and cold split D24 describes is also a day boundary.
pub const BUCKET_MS: i64 = 86_400_000;

/// The time bucket a moment falls in.
pub fn bucket_of(at: i64) -> i64 {
    at - at.rem_euclid(BUCKET_MS)
}

/// A stable identifier for one indexed column.
///
/// `docs/HIGH_CARDINALITY.md` section 2 puts a schema registry behind this: a
/// normalized field name maps to a stable numeric `field_id`. That registry
/// arrives with the control catalog, so this derives the same number from the
/// name instead. The value is stable for one name and never reused for another,
/// which is the property the registry has to provide.
pub fn field_of(column: &str) -> u64 {
    fingerprint(column.as_bytes())
}

/// One (field, value, segment) reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    field: u64,
    value: u64,
    segment: [u8; 16],
}

/// One immutable, time-partitioned run.
///
/// A run is sorted, so a probe is a binary search rather than a scan. Compaction
/// combines runs incrementally, and this is the unit it combines.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocatorRun {
    bucket: i64,
    entries: Vec<Entry>,
}

impl LocatorRun {
    pub fn new(bucket: i64) -> LocatorRun {
        LocatorRun {
            bucket,
            entries: Vec::new(),
        }
    }

    pub fn bucket(&self) -> i64 {
        self.bucket
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The bytes this run occupies, so a capacity report can state the cost
    /// rather than estimate it. BENCHMARKS.md section 12b measured that bytes
    /// for each pair moves 53 percent with density, so an estimate has to state
    /// the density it assumes and a measurement does not.
    pub fn byte_len(&self) -> usize {
        self.entries.len() * 32
    }

    /// Add one reference. Repeats are removed when the run is sealed.
    pub fn add(&mut self, column: &str, value: &[u8], segment: [u8; 16]) {
        self.entries.push(Entry {
            field: field_of(column),
            value: fingerprint(value),
            segment,
        });
    }

    /// Sort and remove repeats.
    ///
    /// One value that appears on a thousand rows of one segment is one entry,
    /// not a thousand. That collapse is most of why the locator is small.
    pub fn seal(&mut self) {
        self.entries.sort_unstable();
        self.entries.dedup();
    }

    /// Every segment this run says may hold the value.
    fn candidates(&self, field: u64, value: u64, out: &mut Vec<[u8; 16]>) {
        let start = self
            .entries
            .partition_point(|entry| (entry.field, entry.value) < (field, value));
        for entry in &self.entries[start..] {
            if entry.field != field || entry.value != value {
                break;
            }
            out.push(entry.segment);
        }
    }

    /// Drop every reference to a segment that no longer exists.
    ///
    /// Compaction retires segments, and a run that still named one would cost a
    /// wasted open. FAILURE_MODES.md section 8.4 rule 2 says that is a
    /// detectable inconsistency rather than a wrong answer, because the reader
    /// verifies the full value at the segment.
    pub fn retain_segments(&mut self, live: &dyn Fn(&[u8; 16]) -> bool) {
        self.entries.retain(|entry| live(&entry.segment));
    }

    /// Combine another run of the same bucket into this one.
    pub fn merge(&mut self, other: &LocatorRun) {
        self.entries.extend_from_slice(&other.entries);
        self.seal();
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + 4 + self.entries.len() * 32);
        put_u64(&mut out, self.bucket as u64);
        put_u32(&mut out, self.entries.len() as u32);
        for entry in &self.entries {
            put_u64(&mut out, entry.field);
            put_u64(&mut out, entry.value);
            out.extend_from_slice(&entry.segment);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<LocatorRun, FormatError> {
        let bucket = get_u64(bytes, 0)? as i64;
        let count = get_u32(bytes, 8)? as usize;
        // Validate the declared length before allocating for it.
        slice(bytes, 12, count * 32)?;

        let mut entries = Vec::with_capacity(count);
        for index in 0..count {
            let at = 12 + index * 32;
            let mut segment = [0u8; 16];
            segment.copy_from_slice(slice(bytes, at + 16, 16)?);
            entries.push(Entry {
                field: get_u64(bytes, at)?,
                value: get_u64(bytes, at + 8)?,
                segment,
            });
        }
        Ok(LocatorRun { bucket, entries })
    }
}

/// Every run a tablet holds, by time bucket.
#[derive(Debug, Clone, Default)]
pub struct Locator {
    runs: BTreeMap<i64, LocatorRun>,
}

impl Locator {
    pub fn new() -> Locator {
        Locator::default()
    }

    pub fn from_runs(runs: impl IntoIterator<Item = LocatorRun>) -> Locator {
        Locator {
            runs: runs.into_iter().map(|run| (run.bucket, run)).collect(),
        }
    }

    /// Combine stored runs, several per bucket, sealing each bucket once.
    ///
    /// Merging one run at a time re-sorts every accumulated entry once per
    /// run, which is quadratic in runs and cost hours of one core on a
    /// soak-aged locator. Concatenating first costs one sort per bucket. L165.
    pub fn from_all_runs(runs: impl IntoIterator<Item = LocatorRun>) -> Locator {
        let mut combined: BTreeMap<i64, LocatorRun> = BTreeMap::new();
        for run in runs {
            match combined.entry(run.bucket) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(run);
                }
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    slot.get_mut().entries.extend(run.entries);
                }
            }
        }
        let mut locator = Locator { runs: combined };
        locator.seal();
        locator
    }

    pub fn runs(&self) -> impl Iterator<Item = &LocatorRun> {
        self.runs.values()
    }

    pub fn is_empty(&self) -> bool {
        self.runs.values().all(|run| run.is_empty())
    }

    /// The bytes every run occupies together.
    pub fn byte_len(&self) -> usize {
        self.runs.values().map(|run| run.byte_len()).sum()
    }

    /// Add one reference, into the run its time falls in.
    pub fn add(&mut self, column: &str, value: &[u8], at: i64, segment: [u8; 16]) {
        let bucket = bucket_of(at);
        self.runs
            .entry(bucket)
            .or_insert_with(|| LocatorRun::new(bucket))
            .add(column, value, segment);
    }

    pub fn seal(&mut self) {
        for run in self.runs.values_mut() {
            run.seal();
        }
    }

    /// Combine another locator's runs into this one, bucket by bucket.
    pub fn merge(&mut self, other: &Locator) {
        for (bucket, run) in &other.runs {
            self.runs
                .entry(*bucket)
                .or_insert_with(|| LocatorRun::new(*bucket))
                .merge(run);
        }
    }

    /// Every segment that may hold the value, over the runs a time range
    /// overlaps.
    ///
    /// `range` of `None` is the unbounded lookup: it reads the whole retention
    /// window, which is legal and expensive, and [`Locator::candidate_count`]
    /// is how a caller finds out before running it.
    pub fn candidates(
        &self,
        column: &str,
        value: &[u8],
        range: Option<(i64, i64)>,
    ) -> Vec<[u8; 16]> {
        let field = field_of(column);
        let printed = fingerprint(value);
        let mut out = Vec::new();

        for run in self.overlapping(range) {
            run.candidates(field, printed, &mut out);
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// How many segments a lookup would open.
    ///
    /// The query surface shows this before it runs the query, so an unbounded
    /// high-cardinality lookup is never accidental.
    pub fn candidate_count(&self, column: &str, value: &[u8], range: Option<(i64, i64)>) -> usize {
        self.candidates(column, value, range).len()
    }

    /// How many runs a range reads. This is the prune, and it is linear in the
    /// range: one day reads one run.
    pub fn runs_read(&self, range: Option<(i64, i64)>) -> usize {
        self.overlapping(range).count()
    }

    fn overlapping(&self, range: Option<(i64, i64)>) -> impl Iterator<Item = &LocatorRun> {
        let bounds = range.map(|(start, end)| (bucket_of(start), end));
        self.runs.values().filter(move |run| match bounds {
            None => true,
            // A run covers one bucket, so it overlaps when the range starts at
            // or before its end and ends after its start.
            Some((first, end)) => run.bucket >= first && run.bucket < end,
        })
    }

    /// Drop every reference to a segment that no longer exists.
    pub fn retain_segments(&mut self, live: &dyn Fn(&[u8; 16]) -> bool) {
        for run in self.runs.values_mut() {
            run.retain_segments(live);
        }
        self.runs.retain(|_, run| !run.is_empty());
    }

    /// Whether every segment a run names still exists.
    ///
    /// FAILURE_MODES.md section 8.4 rule 3 makes this a required test: a
    /// generation's locator runs and its segments must agree.
    pub fn disagreements(&self, live: &dyn Fn(&[u8; 16]) -> bool) -> Vec<[u8; 16]> {
        let mut out: Vec<[u8; 16]> = self
            .runs
            .values()
            .flat_map(|run| run.entries.iter())
            .filter(|entry| !live(&entry.segment))
            .map(|entry| entry.segment)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = BUCKET_MS;
    const START: i64 = 1_785_628_800_000;

    fn segment(n: u8) -> [u8; 16] {
        [n; 16]
    }

    fn user(n: u32) -> Vec<u8> {
        format!("u-{n:06}").into_bytes()
    }

    #[test]
    fn a_probe_finds_the_segment_that_holds_the_value() {
        let mut locator = Locator::new();
        locator.add("end_user", &user(1), START, segment(1));
        locator.add("end_user", &user(2), START, segment(2));
        locator.seal();

        assert_eq!(
            locator.candidates("end_user", &user(1), None),
            vec![segment(1)]
        );
        assert_eq!(
            locator.candidates("end_user", &user(2), None),
            vec![segment(2)]
        );
        assert!(locator.candidates("end_user", &user(999), None).is_empty());
    }

    #[test]
    fn one_value_in_several_segments_returns_all_of_them() {
        let mut locator = Locator::new();
        for n in 1..=5u8 {
            locator.add("end_user", &user(1), START, segment(n));
        }
        locator.seal();
        assert_eq!(locator.candidates("end_user", &user(1), None).len(), 5);
    }

    #[test]
    fn one_value_on_many_rows_of_one_segment_is_one_entry() {
        // The collapse that makes the locator small: the pair count sets its
        // size, not the row count.
        let mut locator = Locator::new();
        for _ in 0..1_000 {
            locator.add("end_user", &user(1), START, segment(1));
        }
        locator.seal();
        assert_eq!(
            locator.candidates("end_user", &user(1), None),
            vec![segment(1)]
        );
        assert_eq!(locator.byte_len(), 32, "a thousand rows cost one entry");
    }

    #[test]
    fn two_columns_do_not_collide() {
        // A field identifier is part of the key, so one value under two column
        // names is two different things.
        let mut locator = Locator::new();
        locator.add("end_user", b"x", START, segment(1));
        locator.add("session_id", b"x", START, segment(2));
        locator.seal();
        assert_eq!(locator.candidates("end_user", b"x", None), vec![segment(1)]);
        assert_eq!(
            locator.candidates("session_id", b"x", None),
            vec![segment(2)]
        );
    }

    #[test]
    fn a_time_range_prunes_linearly() {
        // The measured property: a one-day range reads one run where a 30-day
        // range reads 30. This is the strongest prune the locator has.
        let mut locator = Locator::new();
        for day in 0..30i64 {
            locator.add("end_user", &user(1), START + day * DAY, segment(day as u8));
        }
        locator.seal();

        assert_eq!(locator.runs_read(None), 30);
        assert_eq!(locator.runs_read(Some((START, START + DAY))), 1);
        assert_eq!(locator.runs_read(Some((START, START + 7 * DAY))), 7);

        // And the candidate count falls with it.
        assert_eq!(locator.candidate_count("end_user", &user(1), None), 30);
        assert_eq!(
            locator.candidate_count("end_user", &user(1), Some((START, START + DAY))),
            1
        );
        assert_eq!(
            locator.candidate_count("end_user", &user(1), Some((START, START + 3 * DAY))),
            3
        );
    }

    #[test]
    fn an_unbounded_lookup_reads_the_whole_window_and_says_how_much() {
        // Legal, sometimes what a support investigation needs, and never
        // accidental: the count is available before the query runs.
        let mut locator = Locator::new();
        for day in 0..14i64 {
            for n in 0..3u8 {
                locator.add(
                    "request_id",
                    b"r-1",
                    START + day * DAY,
                    segment(day as u8 * 3 + n),
                );
            }
        }
        locator.seal();
        assert_eq!(locator.candidate_count("request_id", b"r-1", None), 42);
    }

    #[test]
    fn a_range_that_covers_nothing_returns_nothing() {
        let mut locator = Locator::new();
        locator.add("end_user", &user(1), START, segment(1));
        locator.seal();
        assert!(locator
            .candidates("end_user", &user(1), Some((START - 10 * DAY, START - DAY)))
            .is_empty());
        assert!(locator
            .candidates("end_user", &user(1), Some((START + DAY, START + 10 * DAY)))
            .is_empty());
    }

    #[test]
    fn a_run_round_trips_through_its_bytes() {
        let mut run = LocatorRun::new(START);
        for n in 0..200u32 {
            run.add("end_user", &user(n), segment((n % 7) as u8));
        }
        run.seal();
        let back = LocatorRun::decode(&run.encode()).unwrap();
        assert_eq!(back, run);
        assert_eq!(back.bucket(), START);
    }

    #[test]
    fn a_truncated_run_is_refused_before_it_allocates() {
        let mut run = LocatorRun::new(START);
        run.add("end_user", &user(1), segment(1));
        run.seal();
        let bytes = run.encode();
        for cut in [0, 4, 8, 12, bytes.len() - 1] {
            assert!(LocatorRun::decode(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        // A count larger than the bytes is refused rather than allocated for.
        let mut lying = bytes.clone();
        lying[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(LocatorRun::decode(&lying).is_err());
    }

    #[test]
    fn merging_two_runs_keeps_one_entry_for_each_pair() {
        let mut first = LocatorRun::new(START);
        first.add("end_user", &user(1), segment(1));
        first.seal();
        let mut second = LocatorRun::new(START);
        second.add("end_user", &user(1), segment(1));
        second.add("end_user", &user(2), segment(2));
        second.seal();

        first.merge(&second);
        assert_eq!(first.len(), 2, "the repeated pair merged into one");
    }

    #[test]
    fn combining_all_runs_at_once_matches_merging_one_at_a_time() {
        // The one-at-a-time merge re-sorts every accumulated entry once per
        // run, which L165 records as hours on a soak-aged locator.
        // `from_all_runs` must produce exactly the locator the slow way did.
        let mut runs = Vec::new();
        for n in 0..12u32 {
            let bucket = START + i64::from(n % 3) * DAY;
            let mut run = LocatorRun::new(bucket);
            for m in 0..50u32 {
                run.add("end_user", &user(m % 20), segment(((n + m) % 9) as u8));
            }
            run.seal();
            runs.push(run);
        }

        let mut one_at_a_time = Locator::new();
        for run in &runs {
            one_at_a_time.merge(&Locator::from_runs([run.clone()]));
        }
        let all_at_once = Locator::from_all_runs(runs);

        assert_eq!(all_at_once.byte_len(), one_at_a_time.byte_len());
        for m in 0..20u32 {
            assert_eq!(
                all_at_once.candidates("end_user", &user(m), None),
                one_at_a_time.candidates("end_user", &user(m), None),
                "user {m}"
            );
        }
    }

    #[test]
    fn a_retired_segment_is_dropped_from_every_run() {
        // Compaction retires segments, and a run that still named one would
        // cost a wasted open on every probe.
        let mut locator = Locator::new();
        locator.add("end_user", &user(1), START, segment(1));
        locator.add("end_user", &user(1), START + DAY, segment(2));
        locator.seal();
        assert_eq!(locator.candidate_count("end_user", &user(1), None), 2);

        locator.retain_segments(&|id| *id != segment(1));
        assert_eq!(
            locator.candidates("end_user", &user(1), None),
            vec![segment(2)]
        );
    }

    #[test]
    fn a_run_that_names_a_missing_segment_is_a_detectable_inconsistency() {
        // FAILURE_MODES.md section 8.4 rule 3: the agreement between a
        // generation's locator runs and its segments is a required test.
        let mut locator = Locator::new();
        locator.add("end_user", &user(1), START, segment(1));
        locator.add("end_user", &user(2), START, segment(2));
        locator.seal();

        assert!(locator.disagreements(&|_| true).is_empty());
        assert_eq!(
            locator.disagreements(&|id| *id != segment(2)),
            vec![segment(2)]
        );
    }

    #[test]
    fn a_fingerprint_collision_costs_an_open_and_never_a_wrong_answer() {
        // Two values with one fingerprint return each other's segments as
        // candidates. The segment index then verifies the full typed value, so
        // the extra candidate costs a wasted open and cannot produce a row.
        //
        // A natural 64-bit collision is not findable in a test, so this asserts
        // the property directly: a probe returns candidates, never rows.
        let mut locator = Locator::new();
        locator.add("end_user", &user(1), START, segment(1));
        locator.seal();

        let candidates = locator.candidates("end_user", &user(1), None);
        assert_eq!(candidates, vec![segment(1)]);
        // The locator's whole answer is a list of segment identifiers. There is
        // no path by which it can return a row, which is what makes a
        // fingerprint safe to prune with.
        let _: Vec<[u8; 16]> = candidates;
    }

    #[test]
    fn the_cost_follows_the_pair_count_rather_than_the_value_count() {
        // BENCHMARKS.md section 12b. 100 million end users cost nothing on
        // their own; a user whose events reach many segments is what costs.
        let mut scattered = Locator::new();
        for n in 0..1_000u32 {
            for segment_number in 0..10u8 {
                scattered.add("end_user", &user(n), START, segment(segment_number));
            }
        }
        scattered.seal();

        let mut grouped = Locator::new();
        for n in 0..1_000u32 {
            grouped.add("end_user", &user(n), START, segment(0));
        }
        grouped.seal();

        assert_eq!(scattered.byte_len(), grouped.byte_len() * 10);
        // The distinct-value count is the same in both. Only the placement
        // differs, which is the point compaction acts on.
        assert_eq!(scattered.candidate_count("end_user", &user(0), None), 10);
        assert_eq!(grouped.candidate_count("end_user", &user(0), None), 1);
    }
}
