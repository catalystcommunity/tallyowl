//! Compaction, and the four things that run alongside it.
//!
//! `docs/STORAGE.md` section 11 gives the sequence and `docs/FAILURE_MODES.md`
//! section 8 gives the races. Each of those is a correctness question rather
//! than a performance one, and the second is the dangerous one.
//!
//! # Against a running query (section 8.1)
//!
//! A query pins the generation it resolved. A pinned generation's segments are
//! never deleted, a deletion waits for a garbage-collection grace period after
//! the last pin is released, and a pin that outlives that period because a
//! process died holding it expires. The bound is what makes a leaked pin a
//! delay rather than a leak.
//!
//! # Against erasure (section 8.2)
//!
//! **This is the dangerous one.** Compaction reads a segment, spends time
//! rewriting it, and publishes the result. An erasure that lands during that
//! window applies to the segment being replaced, and a replacement written from
//! the pre-erasure snapshot brings erased data back.
//!
//! Three rules, and a fourth that catches a bug in the first three:
//!
//! 1. compaction records the tombstone generation it started from;
//! 2. before it publishes, it re-reads tombstones and applies every one
//!    committed since;
//! 3. the publish verifies the tombstone generation has not moved again, and
//!    restarts if it has;
//! 4. a tombstone is a standing predicate, so a miss hides the data on read
//!    rather than exposing it. That is the second line of defence, not the
//!    first, and both exist on purpose.
//!
//! # Against a cold upload (section 8.3)
//!
//! Not reachable yet, because the cold tier is not built. A segment is already
//! identified by its content address, which is the property that rule depends
//! on.
//!
//! # Against locator runs (section 8.4)
//!
//! A locator run is published in the same catalog transaction as the segments
//! it describes, which [`crate::catalog::Catalog::publish_with_locator`] does. A
//! compaction drops references to segments it retired.
//!
//! # Grouping by correlation value
//!
//! Compaction groups rows by the highest-cost correlation value before it
//! writes a segment. Ingest cannot do this, because it must not wait to sort,
//! and compaction rewrites the segment anyway so the grouping is close to free.
//!
//! This is a query cost decision rather than a compression one. The locator
//! holds one reference for each (value, segment) pair, so grouping collapses the
//! pair count for the retained majority of data. Measured at 100 million end
//! users: 228 candidate segments with grouping against 3,000 without. See
//! BENCHMARKS.md section 12b.

use std::collections::BTreeMap;

use crate::catalog::{Manifest, Tombstone};
use crate::row::EventRow;
use crate::segmented::SegmentedStore;
use crate::store::StoreError;

/// How compaction behaves. Every value is configurable.
#[derive(Debug, Clone, Copy)]
pub struct CompactionSettings {
    /// How long a retired segment's file stays after nothing pins it.
    ///
    /// Section 8.1 rule 3: the grace period exceeds the query budget's maximum
    /// runtime, so a long query cannot outlive its own inputs.
    pub grace_ms: i64,
    /// How long a pin may live before it is treated as leaked.
    pub pin_max_age_ms: i64,
    /// The property to group rows by, when a segment is rewritten. Normally the
    /// end-user identifier, because that is the highest-cost correlation value.
    pub group_by: Option<&'static str>,
    /// How many times to restart when an erasure lands mid-compaction. A
    /// restart is correct and a loop is not, so this bounds it.
    pub max_restarts: usize,
    /// How long the head remembers a batch ID. `storage.deduplicationWindow`.
    ///
    /// D36 pairs this with the retry age, and
    /// `crates/tallyowl-config/src/validate.rs` refuses a configuration where
    /// a retry could outlive it. Zero disables expiry, which is what every
    /// build did before this setting existed.
    pub deduplication_window_ms: i64,
    /// How long detailed telemetry is kept: events, spans, error occurrences,
    /// and metric points. `retention.detailed`. Zero keeps everything, which is
    /// what every build did before this existed.
    pub detailed_retention_ms: i64,
    /// How long aggregates and downsampled series are kept. `retention.rollup`.
    ///
    /// POLICY.md section 4 requires this to be at least `detailed`, because a
    /// rollup that expired first would leave a gap in the middle of a chart
    /// that no query could fill. `crates/tallyowl-config/src/validate.rs`
    /// refuses a configuration that breaks it.
    pub rollup_retention_ms: i64,
    /// How old a segment's newest row must be before cold consolidation may
    /// rewrite it. `compaction.coldGroupAfter`. Zero disables the pass, which
    /// is what every build did before this existed; the head passes 48 hours
    /// by default.
    ///
    /// HIGH_CARDINALITY.md: ingest cannot wait to sort, so hot segments hold
    /// scattered values, and only compaction changes which segment holds a
    /// row. Consolidating a cold day's segments grouped by the highest-cost
    /// correlation value is the 13-fold candidate reduction
    /// `prototypes/locator-bench` measured (BENCHMARKS.md section 12b).
    pub cold_group_after_ms: i64,
    /// The size cold consolidation aims each rewritten segment at.
    pub cold_group_target_bytes: u64,
    /// The most source bytes one consolidation pass rewrites. The pass runs
    /// on the maintenance interval, so a large backlog converges over passes
    /// instead of holding a day of rows in memory at once.
    pub cold_group_batch_bytes: u64,
}

impl Default for CompactionSettings {
    fn default() -> CompactionSettings {
        CompactionSettings {
            // Comfortably past the query budget's default maximum runtime.
            grace_ms: 5 * 60_000,
            pin_max_age_ms: 60 * 60_000,
            group_by: Some("end_user"),
            max_restarts: 3,
            // 72 hours, against a 24-hour retry age. See D36.
            deduplication_window_ms: 72 * 60 * 60_000,
            // Zero keeps everything, which is what a caller that has not
            // configured retention means. The head passes `retention.detailed`
            // and `retention.rollup`.
            detailed_retention_ms: 0,
            rollup_retention_ms: 0,
            cold_group_after_ms: 0,
            // 2 MiB: a cold exact hit decompresses every candidate segment
            // whole, so the target is also the price of a cold lookup. The
            // owner set both of these on 2026-08-12 from the section 23.1 and
            // 24.1 measurements; raising them is an operator choice.
            cold_group_target_bytes: 2 * 1024 * 1024,
            // 32 MiB stored bytes: a bite's rows are held decompressed at
            // many times their stored size (a 64 MiB bite measured 12 to
            // 22 GB resident), and the unbounded form took a machine down.
            cold_group_batch_bytes: 32 * 1024 * 1024,
        }
    }
}

/// What one compaction did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionOutcome {
    /// Segments read and replaced.
    pub rewritten: usize,
    /// Segments removed entirely, because every row in them was erased.
    pub removed: usize,
    /// Rows an erasure took out.
    pub rows_erased: u64,
    /// Files deleted after the grace period.
    pub files_reclaimed: usize,
    /// Times an erasure landed mid-compaction and the work restarted.
    pub restarts: usize,
    /// Receipts the deduplication window expired.
    pub receipts_expired: usize,
    /// Segments dropped whole because everything in them was past retention.
    pub segments_expired: usize,
    /// Rows removed because they were past the retention for their class.
    pub rows_expired: u64,
    /// True when a pin held a generation and the files stayed.
    pub held_by_a_pin: bool,
    /// True when the device had no room and the attempt stopped.
    ///
    /// FAILURE_MODES.md section 10: keep the source segments and alert.
    /// Compaction is never required for correctness, so this is a delay in
    /// reclaiming bytes and never a wrong answer. The erased rows stay hidden
    /// either way, because a tombstone is a standing predicate.
    pub abandoned_for_space: bool,
    /// Cold segments read and retired by consolidation.
    pub consolidated_sources: usize,
    /// Segments consolidation wrote in their place, grouped by the
    /// correlation value.
    pub consolidated_outputs: usize,
}

/// Rewrite the segments an erasure touches.
///
/// This is the physical half of deletion. The logical half already happened:
/// the tombstone hid the rows the moment it was committed, in every tier, and a
/// query has not seen them since. STORAGE.md section 11 gives the 24-hour
/// target for this work on hot and warm data.
pub fn compact(
    store: &SegmentedStore,
    settings: CompactionSettings,
) -> Result<CompactionOutcome, StoreError> {
    let mut outcome = CompactionOutcome::default();

    // Retention first, because a segment that is past retention does not need
    // to be read, rewritten, or considered by anything below it. It is the
    // cheapest work available and it is the only work that bounds growth.
    expire_by_retention(store, settings, &mut outcome)?;

    // Then cold consolidation, so the erasure pass below reads the layout it
    // will leave behind rather than the one it is about to replace.
    consolidate_cold(store, settings, &mut outcome)?;

    for attempt in 0..=settings.max_restarts {
        // Rule 1: record the tombstone generation this attempt started from.
        let started_at_generation = store.catalog().tombstone_generation()?;
        let tombstones = store.catalog().tombstones()?;

        if tombstones.is_empty() {
            outcome.files_reclaimed = reclaim(store, settings)?;
            outcome.receipts_expired = expire_receipts(store, settings)?;
            return Ok(outcome);
        }

        let affected: Vec<Manifest> = store
            .catalog()
            .manifests()?
            .into_iter()
            .filter(|manifest| {
                // A segment a tombstone cannot reach is not read at all. A
                // project and time-range deletion can remove fully covered
                // segments without reading them, and this is the prune that
                // makes the work bounded.
                tombstones
                    .iter()
                    .any(|predicate| intersects(manifest, predicate))
            })
            .collect();

        if affected.is_empty() {
            outcome.files_reclaimed = reclaim(store, settings)?;
            outcome.receipts_expired = expire_receipts(store, settings)?;
            return Ok(outcome);
        }

        let mut retire: Vec<[u8; 16]> = Vec::new();
        let mut replacements: Vec<(Manifest, Vec<EventRow>)> = Vec::new();
        let mut erased = 0u64;

        for manifest in &affected {
            let rows = store.read_segment_rows(manifest)?;
            let before = rows.len();

            // Rule 2: re-read tombstones and apply every one committed while
            // this attempt was reading.
            let current = store.catalog().tombstones()?;
            let kept: Vec<EventRow> = rows
                .into_iter()
                .filter(|row| !current.iter().any(|predicate| predicate.hides(row)))
                .collect();

            erased += (before - kept.len()) as u64;
            retire.push(manifest.segment_id);
            if kept.is_empty() {
                // Every row was erased, so the segment goes rather than being
                // rewritten empty.
                outcome.removed += 1;
                continue;
            }
            outcome.rewritten += 1;
            replacements.push((manifest.clone(), group_rows(kept, settings.group_by)));
        }

        // Rule 3: the tombstone generation must not have moved again. If it
        // has, an erasure landed while this attempt was writing, and the work
        // restarts rather than publishing a replacement built from a stale
        // view.
        if store.catalog().tombstone_generation()? != started_at_generation {
            outcome.restarts += 1;
            if attempt == settings.max_restarts {
                // Rule 4 is what makes stopping safe: the tombstone is a
                // standing predicate, so the rows stay hidden on read whether
                // or not this rewrote them.
                return Ok(outcome);
            }
            continue;
        }

        if let Err(refused) = store.swap_segments(&retire, replacements) {
            if matches!(refused, StoreError::Exhausted(_)) {
                // Nothing was written and nothing was retired, so the source
                // segments are exactly as they were.
                outcome.abandoned_for_space = true;
                return Ok(outcome);
            }
            return Err(refused);
        }
        outcome.rows_erased = erased;
        outcome.files_reclaimed = reclaim(store, settings)?;
        outcome.receipts_expired = expire_receipts(store, settings)?;
        return Ok(outcome);
    }

    Ok(outcome)
}

/// Remove what is past its retention class.
///
/// `docs/POLICY.md` section 4 maps a telemetry kind to a class, and the two
/// classes that reach a segment are `detailed` and `rollup`. A derived rollup
/// carries the `derived` property; everything else is detailed.
///
/// **The prune comes before the read**, exactly as it does for a tombstone. A
/// segment whose whole range is older than the longest retention cannot hold a
/// row worth keeping, so it is dropped without opening it. Only a segment that
/// straddles the two cutoffs is read, and only to keep the rollups in it.
///
/// A zero retention keeps everything. An installation that has not chosen a
/// retention must not lose data because a default expired it.
fn expire_by_retention(
    store: &SegmentedStore,
    settings: CompactionSettings,
    outcome: &mut CompactionOutcome,
) -> Result<(), StoreError> {
    let detailed = settings.detailed_retention_ms;
    let rollup = settings.rollup_retention_ms;
    if detailed <= 0 && rollup <= 0 {
        return Ok(());
    }
    // A rollup must outlive the detailed data it summarises, and configuration
    // refuses the other order. Taking the larger here means a misconfiguration
    // that reached this far still cannot delete a rollup early.
    let longest = detailed.max(rollup);
    let now = tallyowl_obs::time::now_ms();

    let mut retire: Vec<[u8; 16]> = Vec::new();
    let mut replacements: Vec<(Manifest, Vec<EventRow>)> = Vec::new();

    for manifest in store.catalog().manifests()? {
        // The newest row in the segment decides. A segment is dropped only when
        // **everything** in it is past the cutoff, never when part of it is.
        let newest = manifest.occurred_range.1;

        if longest > 0 && now - newest > longest {
            outcome.segments_expired += 1;
            outcome.rows_expired += manifest.row_count;
            retire.push(manifest.segment_id);
            continue;
        }
        // Past the detailed cutoff and inside the rollup one: the derived rows
        // stay and the rest go, so the segment is rewritten rather than dropped.
        if detailed > 0 && rollup > detailed && now - newest > detailed {
            let rows = store.read_segment_rows(&manifest)?;
            let before = rows.len();
            let kept: Vec<EventRow> = rows.into_iter().filter(is_rollup).collect();
            if kept.len() == before {
                continue;
            }
            outcome.rows_expired += (before - kept.len()) as u64;
            retire.push(manifest.segment_id);
            if kept.is_empty() {
                outcome.segments_expired += 1;
            } else {
                outcome.rewritten += 1;
                replacements.push((manifest, group_rows(kept, settings.group_by)));
            }
        }
    }

    if retire.is_empty() {
        return Ok(());
    }
    if let Err(refused) = store.swap_segments(&retire, replacements) {
        if matches!(refused, StoreError::Exhausted(_)) {
            // Nothing was written and nothing was retired. Expiry is the one
            // pass that frees space, so it will be tried again and it is the
            // caller's job not to have waited this long.
            outcome.abandoned_for_space = true;
            outcome.segments_expired = 0;
            outcome.rows_expired = 0;
            return Ok(());
        }
        return Err(refused);
    }
    Ok(())
}

/// Whether a row belongs to the `rollup` retention class.
///
/// A derived rollup says so on the row. Nothing else does, which is why the
/// marker exists: a class that had to be inferred from a metric name would put
/// an application's own counter in the wrong one.
fn is_rollup(row: &EventRow) -> bool {
    row.properties.contains_key("derived")
}

/// Consolidate a cold time bucket's segments, grouped by the correlation value.
///
/// Rewriting a segment in place cannot change which segment holds a row, and
/// HIGH_CARDINALITY.md says plainly that only routing and compaction change
/// the (value, segment) pair count. This is the compaction half: every cold
/// segment of one project and one locator bucket is read together, the rows
/// are ordered by the grouping value, and the bucket is rewritten as few
/// segments in which one person's rows sit together. `prototypes/locator-bench`
/// measured the result as a 13-fold candidate reduction at 100 million users.
///
/// **No restart dance.** The erasure loop re-checks the tombstone generation
/// because it publishes the *removal* of rows; this pass keeps every visible
/// row. A tombstone that lands mid-pass still hides its rows on read — a
/// tombstone is a standing predicate — and the erasure pass will reach the
/// rewritten segments on its own schedule, exactly as it would have reached
/// the sources.
///
/// **Idempotent by shape.** A bucket is due only while it holds more segments
/// than its bytes need at the target size; once consolidated it stops
/// qualifying, so the pass converges instead of rewriting for ever.
fn consolidate_cold(
    store: &SegmentedStore,
    settings: CompactionSettings,
    outcome: &mut CompactionOutcome,
) -> Result<(), StoreError> {
    if settings.cold_group_after_ms <= 0 {
        return Ok(());
    }
    let Some(group_by) = settings.group_by else {
        return Ok(());
    };
    let now = tallyowl_obs::time::now_ms();
    let cutoff = now - settings.cold_group_after_ms;

    // Cold segments of one project and one locator bucket, oldest bucket
    // first so the backlog drains from the far end.
    let mut buckets: BTreeMap<(i64, [u8; 16], [u8; 16]), Vec<Manifest>> = BTreeMap::new();
    for manifest in store.catalog().manifests()? {
        if manifest.occurred_range.1 >= cutoff {
            continue;
        }
        buckets
            .entry((
                crate::locator::bucket_of(manifest.occurred_range.0),
                manifest.workspace_id,
                manifest.project_id,
            ))
            .or_default()
            .push(manifest);
    }

    let mut batch = 0u64;
    for (_, mut group) in buckets {
        let bytes: u64 = group.iter().map(|m| m.byte_count).sum();
        // Due only while the bucket holds more segments than its bytes need.
        // This is what makes the pass idempotent.
        let needed = bytes.div_ceil(settings.cold_group_target_bytes).max(1) as usize;
        if group.len() <= needed {
            continue;
        }

        // One bucket can exceed the pass budget on its own — a soak-aged day
        // held two gigabytes in a single group, and the read below keeps a
        // segment's rows decompressed, several times the stored bytes. So the
        // budget bounds what one pass takes *from* a group, not only how many
        // groups it takes: the smallest segments first, because many small
        // segments shrink toward the target and the passes converge. A taken
        // set that would not shrink is left for a later pass instead of being
        // rewritten in place.
        group.sort_by_key(|manifest| manifest.byte_count);
        let mut taken_bytes = 0u64;
        let mut take = 0usize;
        for manifest in &group {
            if take > 0 && taken_bytes + manifest.byte_count > settings.cold_group_batch_bytes {
                break;
            }
            taken_bytes += manifest.byte_count;
            take += 1;
        }
        group.truncate(take);
        let needed_for_taken = taken_bytes
            .div_ceil(settings.cold_group_target_bytes)
            .max(1) as usize;
        if group.len() <= needed_for_taken {
            continue;
        }
        if batch + taken_bytes > settings.cold_group_batch_bytes && batch > 0 {
            break;
        }
        batch += taken_bytes;

        let tombstones = store.catalog().tombstones()?;
        let mut rows: Vec<EventRow> = Vec::new();
        let mut retire: Vec<[u8; 16]> = Vec::new();
        let mut log_range = (u64::MAX, 0u64);
        for manifest in &group {
            let read = store.read_segment_rows(manifest)?;
            let before = read.len();
            let kept_from = rows.len();
            rows.extend(
                read.into_iter()
                    .filter(|row| !tombstones.iter().any(|predicate| predicate.hides(row))),
            );
            outcome.rows_erased += (before - (rows.len() - kept_from)) as u64;
            retire.push(manifest.segment_id);
            log_range.0 = log_range.0.min(manifest.log_range.0);
            log_range.1 = log_range.1.max(manifest.log_range.1);
        }

        rows.sort_by_cached_key(|row| {
            (
                row.properties
                    .get(group_by)
                    .map(|(value, _)| value.to_display()),
                row.occurred_at,
            )
        });

        // Cut at the target size, estimated from what the sources measured.
        let total = rows.len().max(1);
        let bytes_for_each_row = (taken_bytes / total as u64).max(1);
        let for_each_output =
            ((settings.cold_group_target_bytes / bytes_for_each_row) as usize).clamp(1, total);

        // Every output borrows the group's identity and carries the whole
        // union of the retired log ranges, so checkpoint coverage never
        // shrinks.
        let mut source = group[0].clone();
        source.log_range = log_range;
        let replacements: Vec<(Manifest, Vec<EventRow>)> = rows
            .chunks(for_each_output)
            .map(|chunk| (source.clone(), chunk.to_vec()))
            .collect();

        let outputs = replacements.len();
        if let Err(refused) = store.swap_segments(&retire, replacements) {
            if matches!(refused, StoreError::Exhausted(_)) {
                // Nothing was written and nothing was retired.
                outcome.abandoned_for_space = true;
                return Ok(());
            }
            return Err(refused);
        }
        outcome.consolidated_sources += retire.len();
        outcome.consolidated_outputs += outputs;
    }
    Ok(())
}

/// Whether a tombstone can reach any row of a segment.
fn intersects(manifest: &Manifest, tombstone: &Tombstone) -> bool {
    if manifest.project_id != tombstone.project_id {
        return false;
    }
    match tombstone.range {
        None => true,
        Some((start, end)) => manifest.occurred_range.0 < end && manifest.occurred_range.1 >= start,
    }
}

/// Group rows by the highest-cost correlation value.
///
/// The grouping does not change event-time ordering, which the query path
/// re-establishes on merge, and it never crosses a project because a segment
/// holds one.
fn group_rows(rows: Vec<EventRow>, group_by: Option<&str>) -> Vec<EventRow> {
    let Some(key) = group_by else {
        return rows;
    };
    let mut grouped: BTreeMap<Option<String>, Vec<EventRow>> = BTreeMap::new();
    for row in rows {
        let value = row.properties.get(key).map(|(value, _)| value.to_display());
        grouped.entry(value).or_default().push(row);
    }
    grouped.into_values().flatten().collect()
}

/// Delete the files of retired segments, once nothing holds them.
///
/// Section 8.1 rules 1, 3, and 4 together: a pinned generation's segments are
/// never deleted, a deletion waits a grace period, and a leaked pin expires so
/// the wait is bounded.
fn reclaim(store: &SegmentedStore, settings: CompactionSettings) -> Result<usize, StoreError> {
    let now = tallyowl_obs::time::now_ms();
    store.catalog().expire_pins(now, settings.pin_max_age_ms)?;

    if store.catalog().oldest_pinned_generation()?.is_some() {
        // A query is still reading. Nothing goes until it finishes, and the
        // grace period then starts.
        return Ok(0);
    }
    store.reclaim_retired_files(settings.grace_ms)
}

/// Expire receipts the deduplication window has passed.
///
/// This does not depend on compaction and does not wait for a pin: a receipt is
/// not a segment and no query reads one. It runs in the same pass because the
/// same maintenance tick is what an operator schedules.
fn expire_receipts(
    store: &SegmentedStore,
    settings: CompactionSettings,
) -> Result<usize, StoreError> {
    if settings.deduplication_window_ms <= 0 {
        return Ok(0);
    }
    let before = tallyowl_obs::time::now_ms() - settings.deduplication_window_ms;
    Ok(store.catalog().expire_receipts(before)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row::PropertyValue;

    fn row(n: u8, user: &str) -> EventRow {
        let mut row = EventRow::new([n; 16], "event", "a", 1_785_628_800_000);
        row.project_id = [9; 16];
        row.with_property("end_user", PropertyValue::Text(user.into()), "client")
    }

    #[test]
    fn grouping_puts_one_persons_rows_together() {
        // The property BENCHMARKS.md section 12b measured: grouping collapses
        // the (value, segment) pair count for the retained majority of data.
        let rows = vec![
            row(1, "u-1"),
            row(2, "u-2"),
            row(3, "u-1"),
            row(4, "u-2"),
            row(5, "u-1"),
        ];
        let grouped = group_rows(rows, Some("end_user"));
        let users: Vec<String> = grouped
            .iter()
            .map(|row| row.properties["end_user"].0.to_display())
            .collect();
        assert_eq!(users, vec!["u-1", "u-1", "u-1", "u-2", "u-2"]);
    }

    #[test]
    fn grouping_keeps_every_row() {
        let rows: Vec<EventRow> = (0..20u8).map(|n| row(n, &format!("u-{}", n % 3))).collect();
        let grouped = group_rows(rows.clone(), Some("end_user"));
        assert_eq!(grouped.len(), rows.len());
    }

    #[test]
    fn a_row_without_the_grouping_property_is_still_kept() {
        let mut plain = EventRow::new([9; 16], "event", "a", 1);
        plain.project_id = [9; 16];
        let grouped = group_rows(vec![row(1, "u-1"), plain.clone()], Some("end_user"));
        assert_eq!(grouped.len(), 2);
    }

    #[test]
    fn no_grouping_leaves_the_order_alone() {
        let rows = vec![row(1, "u-2"), row(2, "u-1")];
        let grouped = group_rows(rows.clone(), None);
        assert_eq!(grouped[0].event_id, rows[0].event_id);
    }

    #[test]
    fn a_tombstone_with_no_range_reaches_every_segment_of_its_project() {
        let manifest = Manifest {
            segment_id: [1; 16],
            content_address: [0; 32],
            tablet_id: 0,
            virtual_shard: 0,
            workspace_id: [8; 16],
            project_id: [9; 16],
            kinds: vec!["event".into()],
            occurred_range: (1_000, 2_000),
            received_range: (1_000, 2_000),
            committed_range: (1_000, 2_000),
            log_range: (0, 1),
            row_count: 10,
            byte_count: 100,
            generation: 1,
            tier: "local".into(),
            relative_path: "segments/1.tos".into(),
        };
        let mut tombstone = Tombstone {
            tombstone_id: [1; 16],
            generation: 1,
            project_id: [9; 16],
            event_ids: Vec::new(),
            property: Some(("end_user".into(), "u-1".into())),
            range: None,
            requested_at: 0,
            horizon: i64::MAX,
            reason: "asked".into(),
            except_kinds: Vec::new(),
        };
        assert!(intersects(&manifest, &tombstone));

        // Another project is never touched, which is tenant isolation holding
        // through erasure.
        tombstone.project_id = [1; 16];
        assert!(!intersects(&manifest, &tombstone));

        // A range that misses the segment prunes it without a read.
        tombstone.project_id = [9; 16];
        tombstone.range = Some((5_000, 6_000));
        assert!(!intersects(&manifest, &tombstone));
        tombstone.range = Some((1_500, 6_000));
        assert!(intersects(&manifest, &tombstone));
    }
}

#[cfg(test)]
mod consolidation_probe {
    //! A measurement, not a regression test. It consolidates a **lab copy**
    //! of a soak-aged store in place — never a live data directory — so the
    //! `aged_probe` in `catalog.rs` can measure the same catalog before and
    //! after consolidation without waiting for wall-clock age. The threshold
    //! decides when the pass is due, never what it does, so a lab pass over
    //! cold rows is the pass the head runs. Build the lab from a quiescent
    //! catalog copy plus hard links of the sealed segments it names, and run:
    //!
    //! ```sh
    //! CONSOLIDATION_LAB_DIR=run/bench/consolidation-lab cargo test -p tallyowl-store \
    //!     --release consolidation_probe -- --ignored --nocapture
    //! ```

    use std::time::Instant;

    use super::{compact, CompactionSettings};
    use crate::segmented::{Sealing, SegmentedStore};
    use crate::wal::GroupCommit;

    #[test]
    #[ignore = "a measurement that consolidates a lab store copy, run by hand with CONSOLIDATION_LAB_DIR set"]
    fn consolidate_an_aged_store_copy() {
        let directory = match std::env::var("CONSOLIDATION_LAB_DIR") {
            Ok(directory) => directory,
            Err(_) => panic!("set CONSOLIDATION_LAB_DIR to a lab copy of a soak-aged store"),
        };
        let store = SegmentedStore::open_with(
            std::path::PathBuf::from(&directory),
            Sealing {
                max_open_rows: 40,
                max_open_ms: i64::MAX,
                verify_on_read: true,
                reserve_bytes: 0,
            },
            GroupCommit::default(),
        )
        .expect("the lab store opens");

        // The head's defaults, except the threshold. `cold_group_batch_bytes`
        // caps one pass, so a soak-aged store converges over several passes,
        // exactly as it does on the maintenance interval.
        let settings = CompactionSettings {
            cold_group_after_ms: 12 * 3600 * 1000,
            ..CompactionSettings::default()
        };

        let started_all = Instant::now();
        let mut pass = 0usize;
        let mut sources = 0usize;
        let mut outputs = 0usize;
        loop {
            pass += 1;
            let started = Instant::now();
            let outcome = compact(&store, settings).expect("the pass finishes");
            println!(
                "pass {pass}: consolidated_sources {} consolidated_outputs {} in {:?}",
                outcome.consolidated_sources,
                outcome.consolidated_outputs,
                started.elapsed(),
            );
            sources += outcome.consolidated_sources;
            outputs += outcome.consolidated_outputs;
            if outcome.consolidated_sources == 0 {
                break;
            }
        }
        println!(
            "converged: {sources} sources became {outputs} outputs over {} passes in {:?}",
            pass - 1,
            started_all.elapsed(),
        );
    }
}
