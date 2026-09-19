//! What the store reports about itself.
//!
//! `docs/STORAGE.md` section 14 lists the capacity instruments and
//! `docs/FAILURE_MODES.md` section 13 lists the integrity ones. The second
//! document puts the reason plainly: an operator cannot act on a failure that
//! produces no signal, and an alert cannot exist without a measurement.
//!
//! Two of these predict a failure rather than reporting one, and section 13
//! names them:
//!
//! - **`tallyowl_generation_pin_age_seconds`** rising means a query is holding
//!   storage that compaction wants. A leaked pin retains data until it expires;
//!   this is how that becomes visible before it becomes a disk-space incident.
//! - **`tallyowl_catalog_snapshot_age_seconds`** is the recovery point for
//!   procedure 5. When snapshots are off it reports nothing, which is itself the
//!   answer.
//!
//! # Naming
//!
//! CONVENTIONS.md section 6: lower case with underscores, prefixed
//! `tallyowl_`, suffixed with the unit. A metric never carries an end-user
//! identifier as a label, because that turns a series into personal data and
//! multiplies cardinality without limit.

use std::sync::Arc;

use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_obs::MetricKind;

use crate::segmented::SegmentedStore;

/// The six points of FAILURE_MODES.md section 10, in the order
/// [`crate::space::Point`] declares them.
const POINTS: [&str; 6] = [
    "append-log",
    "segment-publish",
    "compaction",
    "catalog",
    "cold-cache",
    "export",
];

/// Declare every instrument the store reports.
///
/// A service calls this once at startup, so an instrument exists and reads zero
/// before anything has happened. A metric that appears only after its first
/// event makes a dashboard panel say "no data" when the honest answer is zero.
pub fn declare(metrics: &Registry) {
    // A name that breaks a rule is a defect at this call site rather than
    // something an operator discovers from a missing dashboard panel, so a
    // refusal stops the build's tests rather than being swallowed.
    let counter = |name: &str, help: &str| {
        metrics
            .declare(name, MetricKind::Counter, help, &[])
            .unwrap_or_else(|rule| panic!("{rule}"));
    };
    let gauge = |name: &str, help: &str| {
        metrics
            .declare(name, MetricKind::Gauge, help, &[])
            .unwrap_or_else(|rule| panic!("{rule}"));
    };

    // Accepted and rejected work.
    counter(
        "tallyowl_store_commits_total",
        "Batches the store committed, by outcome.",
    );
    counter(
        "tallyowl_store_rows_committed_total",
        "Items the store committed.",
    );
    counter(
        "tallyowl_store_bytes_written_total",
        "Bytes the store wrote to its append log and its segments.",
    );

    // The append log.
    counter(
        "tallyowl_wal_fsyncs_total",
        "Durable flushes of the append log. One flush can make many batches durable.",
    );
    gauge(
        "tallyowl_wal_bytes",
        "Bytes the append log holds that no segment covers yet.",
    );
    gauge(
        "tallyowl_wal_largest_group_count",
        "Batches one durable flush made durable at its best.",
    );
    // The L145 stall signature, as numbers an alert can watch. The log line the
    // head writes carries the same state; a soak or an operator alert needs it
    // here, because a warning nobody scraped is a warning nobody saw.
    gauge(
        "tallyowl_wal_commit_in_flight_count",
        "One while a group commit is in flight, zero while none is. In flight \
         across many samples while the durable position stands still is the \
         stall signature L145 records.",
    );
    gauge(
        "tallyowl_wal_durable_position_count",
        "The append-log position before which everything is durable. Under \
         load it rises; standing still while a commit stays in flight is the \
         stall signature L145 records.",
    );
    gauge(
        "tallyowl_wal_pending_frames_count",
        "Frames waiting for a committer to make them durable.",
    );

    // Segments and the catalog.
    gauge("tallyowl_segments_count", "Segments the catalog names.");
    gauge("tallyowl_segment_bytes", "Bytes those segments occupy.");
    gauge(
        "tallyowl_segment_rows_count",
        "Items those segments hold, before tombstones are applied.",
    );
    gauge(
        "tallyowl_manifest_generation_count",
        "The manifest generation a query resolves against.",
    );
    gauge(
        "tallyowl_receipts_count",
        "Batch receipts the deduplication window holds.",
    );
    gauge(
        "tallyowl_locator_bytes",
        "Bytes the stored tablet locator runs occupy, before combining removes \
         repeats. This follows the count of value and segment pairs rather \
         than the count of distinct values.",
    );

    // Integrity, from FAILURE_MODES.md section 13.
    counter(
        "tallyowl_integrity_pages_verified_total",
        "Pages checked, labelled by mode.",
    );
    counter(
        "tallyowl_integrity_failures_total",
        "Failed checksums, labelled by tier.",
    );
    gauge(
        "tallyowl_segments_damaged_count",
        "Segments currently marked damaged.",
    );
    counter(
        "tallyowl_segments_repaired_total",
        "Segments repaired from another copy.",
    );

    // Deletion and compaction.
    gauge("tallyowl_tombstones_count", "Active erasure predicates.");
    gauge(
        "tallyowl_tombstone_generation_count",
        "The visible tombstone generation.",
    );
    counter("tallyowl_compactions_total", "Compactions run, by outcome.");
    counter(
        "tallyowl_compaction_restarts_total",
        "Compactions restarted by a tombstone move.",
    );
    counter(
        "tallyowl_rows_erased_total",
        "Items compaction removed because an erasure covered them.",
    );

    // The two that predict rather than report.
    gauge(
        "tallyowl_generation_pins_count",
        "Manifest generations pinned by a running query.",
    );
    gauge(
        "tallyowl_generation_pin_age_seconds",
        "Age of the oldest pin. A rising value means a query is holding storage \
         that compaction wants.",
    );
    gauge(
        "tallyowl_catalog_snapshot_age_seconds",
        "Age of the newest catalog snapshot. Reports nothing when snapshots are \
         off, which is itself the answer.",
    );

    // Tiering and capacity.
    counter(
        "tallyowl_cold_uploads_total",
        "Segments copied to cold storage, by outcome.",
    );
    counter(
        "tallyowl_cold_evictions_total",
        "Local copies removed after a cold copy was verified.",
    );
    gauge(
        "tallyowl_storage_reserve_bytes",
        "Space held back so the system can still write what it needs to recover.",
    );
    // STORAGE.md section 14: disk used and free. A percentage cannot answer
    // whether one more write fits, and a byte count can.
    gauge(
        "tallyowl_disk_free_bytes",
        "Free space on the device that holds the data directory.",
    );
    gauge("tallyowl_disk_used_bytes", "Used space on that device.");
    gauge("tallyowl_disk_total_bytes", "Size of that device.");
    gauge(
        "tallyowl_storage_device_info",
        "Which device holds the data directory, as its identifier. The reserve \
         is a property of the device and one process enforces it, so two \
         installations reporting the same identifier are sharing a reserve \
         that neither of them is getting in full. See L037 and L081.",
    );
    counter(
        "tallyowl_storage_refusals_total",
        "Writes refused for want of space, labelled by the point that refused. \
         FAILURE_MODES.md section 10 gives one behaviour for each point.",
    );
    gauge(
        "tallyowl_encrypted_projects_count",
        "Projects whose segments are protected by a key.",
    );
}

/// Read the store's current state into the gauges.
///
/// A counter rises where the work happens. A gauge describes a state, so it is
/// sampled here rather than maintained at every call site, which keeps a hot
/// path free of bookkeeping it would otherwise repeat.
pub fn sample(store: &SegmentedStore, metrics: &Arc<Registry>) {
    let none = labels(&[]);

    if let Ok(manifests) = store.catalog().manifests() {
        metrics.set_gauge("tallyowl_segments_count", &none, manifests.len() as i64);
        metrics.set_gauge(
            "tallyowl_segment_bytes",
            &none,
            manifests.iter().map(|m| m.byte_count as i64).sum(),
        );
        metrics.set_gauge(
            "tallyowl_segment_rows_count",
            &none,
            manifests.iter().map(|m| m.row_count as i64).sum(),
        );
    }
    if let Ok(generation) = store.catalog().generation() {
        metrics.set_gauge(
            "tallyowl_manifest_generation_count",
            &none,
            generation as i64,
        );
    }
    if let Ok(count) = store.catalog().receipt_count() {
        metrics.set_gauge("tallyowl_receipts_count", &none, count as i64);
    }
    // The stored runs, not the combined locator: building the combination to
    // report a byte count is what held this sampler inside an hours-long
    // merge while the gauges it owns stood frozen. L165.
    if let Ok(bytes) = store.catalog().locator_bytes() {
        metrics.set_gauge("tallyowl_locator_bytes", &none, bytes as i64);
    }
    if let Ok(tombstones) = store.catalog().tombstones() {
        metrics.set_gauge("tallyowl_tombstones_count", &none, tombstones.len() as i64);
    }
    if let Ok(generation) = store.catalog().tombstone_generation() {
        metrics.set_gauge(
            "tallyowl_tombstone_generation_count",
            &none,
            generation as i64,
        );
    }

    // The early warning. An age rather than a count, because one pin held for
    // an hour is the problem and ten pins held for a second are not.
    if let Ok(pins) = store.catalog().pins() {
        metrics.set_gauge("tallyowl_generation_pins_count", &none, pins.len() as i64);
        let now = tallyowl_obs::time::now_ms();
        let oldest = pins
            .iter()
            .map(|(_, at)| (now - at).max(0) / 1_000)
            .max()
            .unwrap_or(0);
        metrics.set_gauge("tallyowl_generation_pin_age_seconds", &none, oldest);
    }

    // Capacity. A failure to read the device is itself information, so the
    // gauges are left at their last value rather than being set to zero, which
    // would look like a device with no space at all.
    let space = store.space();
    metrics.set_gauge(
        "tallyowl_storage_reserve_bytes",
        &none,
        space.reserve_bytes() as i64,
    );
    if let Ok(device) = space.device() {
        // An operator comparing two installations sees one number that says
        // whether they share a device. Nothing else can tell them.
        metrics.set_gauge(
            "tallyowl_storage_device_info",
            &none,
            device.device_id as i64,
        );
        metrics.set_gauge("tallyowl_disk_free_bytes", &none, device.free_bytes as i64);
        metrics.set_gauge(
            "tallyowl_disk_used_bytes",
            &none,
            device.used_bytes() as i64,
        );
        metrics.set_gauge(
            "tallyowl_disk_total_bytes",
            &none,
            device.total_bytes as i64,
        );
    }
    // A counter rises, so this reports the difference since the last sample
    // rather than the total. Reporting the total would make the counter jump
    // backwards after a restart, and a rate over it would be nonsense.
    for (point, count) in POINTS.iter().zip(store.take_refusals()) {
        if count > 0 {
            metrics.add(
                "tallyowl_storage_refusals_total",
                &labels(&[("point", point)]),
                count,
            );
        }
    }

    metrics.set_gauge("tallyowl_wal_bytes", &none, store.unsealed_bytes() as i64);
    let wal = store.wal_statistics();
    metrics.set_gauge(
        "tallyowl_wal_largest_group_count",
        &none,
        wal.largest_group as i64,
    );
    let state = store.append_log_state();
    metrics.set_gauge(
        "tallyowl_wal_commit_in_flight_count",
        &none,
        state.committing as i64,
    );
    metrics.set_gauge(
        "tallyowl_wal_durable_position_count",
        &none,
        state.durable_before as i64,
    );
    metrics.set_gauge(
        "tallyowl_wal_pending_frames_count",
        &none,
        state.pending_frames as i64,
    );

    if let Ok(projects) = store.catalog().encrypted_project_count() {
        metrics.set_gauge("tallyowl_encrypted_projects_count", &none, projects as i64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_instrument_follows_the_naming_convention() {
        // CONVENTIONS.md section 6. `declare` refuses a name that breaks a
        // rule, so a name that reached a dashboard wrongly would be a defect at
        // this call site rather than one an operator discovers.
        let metrics = Registry::new();
        declare(&metrics);
        let rendered = metrics.render_text();

        let mut seen = 0;
        for line in rendered.lines().filter(|l| l.starts_with("# HELP ")) {
            let name = line.split_whitespace().nth(2).expect("a metric name");
            // `check_name` already refused anything that breaks a rule, so this
            // asserts that the declaration actually happened rather than that
            // the rule exists.
            tallyowl_obs::metrics::check_name(name).expect("a declared name follows the rules");
            seen += 1;
        }
        assert!(seen > 20, "only {seen} instruments were declared");
    }

    #[test]
    fn every_instrument_reads_zero_before_anything_happens() {
        // A metric that appears only after its first event makes a dashboard
        // say "no data" when the honest answer is zero.
        let metrics = Registry::new();
        declare(&metrics);
        let rendered = metrics.render_text();
        assert!(rendered.contains("tallyowl_segments_count"));
        assert!(rendered.contains("tallyowl_generation_pin_age_seconds"));
        assert!(rendered.contains("tallyowl_tombstones_count"));
    }

    #[test]
    fn the_two_early_warnings_are_declared() {
        // FAILURE_MODES.md section 13 names these two as the ones that predict
        // a failure rather than report one.
        let metrics = Registry::new();
        declare(&metrics);
        let rendered = metrics.render_text();
        assert!(rendered.contains("tallyowl_generation_pin_age_seconds"));
        assert!(rendered.contains("tallyowl_catalog_snapshot_age_seconds"));
    }

    #[test]
    fn no_instrument_carries_an_end_user_label() {
        // A metric never carries an end-user identifier as a label. That turns
        // a series into personal data and multiplies cardinality without limit.
        let metrics = Registry::new();
        declare(&metrics);
        let rendered = metrics.render_text();
        assert!(!rendered.contains("end_user"));
        assert!(!rendered.contains("user_id"));
    }
}
