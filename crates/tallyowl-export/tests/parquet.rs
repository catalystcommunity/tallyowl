//! The Phase 3 exit criterion: a clean Parquet export is queryable in DuckDB.
//!
//! **The verification uses DuckDB rather than the library that wrote the file.**
//! Reading an export back with the same Parquet crate would prove the crate
//! round-trips, and the export contract is that stored data stays accessible to
//! a tool TallyOwl does not control. See `docs/IMPLEMENTATION_LOG.md` L025.
//!
//! A missing DuckDB fails this test rather than skipping it, for the reason L019
//! gives: a suite that quietly does not run reports the same green as one that
//! passed.

use std::path::{Path, PathBuf};
use std::process::Command;

use tallyowl_export::{export_events, manifest_path_for, ExportRequest};
use tallyowl_store::catalog::Tombstone;
use tallyowl_store::row::{EventRow, PropertyValue};
use tallyowl_store::segmented::{Sealing, SegmentedStore};
use tallyowl_store::wal::GroupCommit;
use tallyowl_store::{Store, TimeBasis};

const WORKSPACE: [u8; 16] = [8; 16];
const PROJECT: [u8; 16] = [9; 16];
const BASE_TIME: i64 = 1_785_628_800_000;

fn directory(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("export-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a place to work");
    path.canonicalize().expect("an absolute path")
}

/// DuckDB, from the operator's path or from where `./tools.sh deps` puts it.
fn duckdb() -> PathBuf {
    let fetched = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.deps/bin/duckdb")
        .canonicalize();
    if let Ok(path) = fetched {
        if path.is_file() {
            return path;
        }
    }
    let found = Command::new("sh")
        .args(["-c", "command -v duckdb"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    match found {
        Some(path) => PathBuf::from(path),
        None => panic!(
            "DuckDB is not installed, and this test proves an export is readable by a tool \
             TallyOwl does not control. Run `./tools.sh deps` to fetch it."
        ),
    }
}

/// Run one query and return what it printed.
fn query(file: &Path, sql: &str) -> String {
    let output = Command::new(duckdb())
        .args(["-noheader", "-list", "-c", sql])
        .env("HOME", file.parent().expect("a directory"))
        .output()
        .expect("DuckDB runs");
    assert!(
        output.status.success(),
        "DuckDB could not read the export.\nquery: {sql}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn store(place: &Path) -> SegmentedStore {
    SegmentedStore::open_with(
        place.join("data"),
        Sealing {
            max_open_rows: 100,
            max_open_ms: i64::MAX,
            verify_on_read: true,
            // Space is not what these tests are about, and a workstation
            // with less than the default reserve free would otherwise fail them.
            reserve_bytes: 0,
        },
        GroupCommit {
            linger: std::time::Duration::from_millis(0),
            ..GroupCommit::default()
        },
    )
    .expect("the store opens")
}

/// Rows with one of each property type, so the schema exercises every branch.
fn rows(count: usize) -> Vec<EventRow> {
    (0..count)
        .map(|index| {
            let n = index as u16;
            let mut row = EventRow::new(
                [1; 16],
                "event",
                "checkout-started",
                BASE_TIME + i64::from(n),
            );
            row.event_id[14..16].copy_from_slice(&n.to_be_bytes());
            row.workspace_id = WORKSPACE;
            row.project_id = PROJECT;
            row.received_at = row.occurred_at + 5;
            row.session_id = Some(format!("s-{}", index % 10));
            row.service_name = Some("checkout".into());
            row.trace_id = Some([3; 16]);
            row.with_property(
                "end_user",
                PropertyValue::Text(format!("u-{:04}", index % 5)),
                "client",
            )
            .with_property("value", PropertyValue::Decimal("19.99".into()), "client")
            .with_property(
                "attempts",
                PropertyValue::Unsigned(index as u64 % 7),
                "client",
            )
            .with_property("score", PropertyValue::Integer(-(index as i64)), "driver")
            .with_property("ratio", PropertyValue::Float(0.5), "client")
            .with_property("handled", PropertyValue::Boolean(index % 2 == 0), "client")
        })
        .collect()
}

fn export(place: &Path, store: &SegmentedStore) -> (PathBuf, tallyowl_export::ExportManifest) {
    let file = place.join("events.parquet");
    let manifest = export_events(
        store,
        &ExportRequest {
            project_id: PROJECT,
            range_start: BASE_TIME - 1,
            range_end: BASE_TIME + 1_000_000,
            basis: TimeBasis::OccurredAt,
            into: file.clone(),
            tombstone_generation: store.tombstone_generation().unwrap(),
            reserve_bytes: 0,
        },
    )
    .expect("the export runs");
    (file, manifest)
}

#[test]
fn a_clean_export_is_queryable_in_duckdb() {
    let place = directory("duckdb");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(250)).unwrap();

    let (file, manifest) = export(&place, &store);
    assert_eq!(manifest.rows, 250);
    assert!(file.is_file());

    let counted = query(
        &file,
        &format!("SELECT count(*) FROM read_parquet('{}')", file.display()),
    );
    assert_eq!(counted, "250", "DuckDB counted a different number of rows");
}

#[test]
fn duckdb_reads_every_column_with_its_own_type() {
    // The point of a typed export: a number arrives as a number, so DuckDB can
    // sum it without a cast, and money keeps its exact digits.
    let place = directory("types");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(100)).unwrap();
    let (file, _) = export(&place, &store);

    let path = file.display();

    // An unsigned property sums as a number rather than as text.
    let summed = query(
        &file,
        &format!("SELECT sum(p_attempts) FROM read_parquet('{path}')"),
    );
    let expected: u64 = (0..100u64).map(|index| index % 7).sum();
    assert_eq!(summed, expected.to_string());

    // A signed property keeps its sign.
    let smallest = query(
        &file,
        &format!("SELECT min(p_score) FROM read_parquet('{path}')"),
    );
    assert_eq!(smallest, "-99");

    // A boolean is a boolean.
    let trues = query(
        &file,
        &format!("SELECT count(*) FROM read_parquet('{path}') WHERE p_handled"),
    );
    assert_eq!(trues, "50");

    // Money keeps its exact digits, and DuckDB casts when a query asks.
    let revenue = query(
        &file,
        &format!("SELECT sum(CAST(p_value AS DECIMAL(18,2))) FROM read_parquet('{path}')"),
    );
    assert_eq!(revenue, "1999.00", "a revenue total was not exact");

    // A float is a float.
    let ratio = query(
        &file,
        &format!("SELECT DISTINCT p_ratio FROM read_parquet('{path}')"),
    );
    assert_eq!(ratio, "0.5");
}

#[test]
fn duckdb_groups_by_a_dimension_the_way_a_query_would() {
    let place = directory("group");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(100)).unwrap();
    let (file, _) = export(&place, &store);

    let grouped = query(
        &file,
        &format!(
            "SELECT p_end_user, count(*) FROM read_parquet('{}') GROUP BY 1 ORDER BY 1",
            file.display()
        ),
    );
    let lines: Vec<&str> = grouped.lines().collect();
    assert_eq!(
        lines.len(),
        5,
        "five people produced {} groups",
        lines.len()
    );
    assert!(lines[0].starts_with("u-0000|20"), "{}", lines[0]);
}

#[test]
fn an_export_carries_no_erased_row() {
    // The rule that makes an export safe: an export that carried erased rows
    // out of the installation would make the erasure meaningless the moment
    // somebody opened the file.
    let place = directory("erased");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(100)).unwrap();

    store
        .erase(&Tombstone {
            tombstone_id: [1; 16],
            generation: 0,
            project_id: PROJECT,
            event_ids: Vec::new(),
            property: Some(("end_user".to_string(), "u-0002".to_string())),
            range: None,
            requested_at: BASE_TIME,
            horizon: BASE_TIME + 30 * 86_400_000,
            reason: "The end user asked for their data to be removed.".into(),
            except_kinds: Vec::new(),
        })
        .unwrap();

    let (file, manifest) = export(&place, &store);
    assert_eq!(manifest.rows, 80, "the export holds the erased rows");
    assert_eq!(
        manifest.tombstone_generation, 1,
        "the manifest does not say which erasures were applied"
    );

    let remaining = query(
        &file,
        &format!(
            "SELECT count(*) FROM read_parquet('{}') WHERE p_end_user = 'u-0002'",
            file.display()
        ),
    );
    assert_eq!(remaining, "0", "an erased person is in the export");
}

#[test]
fn an_export_carries_a_description_of_itself() {
    // STORAGE.md section 13: an export carries schemas, policy, checksums, and
    // the deletion high-water mark. Somebody who finds the file in a year needs
    // to know what it holds.
    let place = directory("manifest");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(100)).unwrap();
    let (file, manifest) = export(&place, &store);

    let text = std::fs::read_to_string(manifest_path_for(&file)).expect("a description");
    assert!(text.contains("rows: 100"));
    assert!(text.contains("basis: occurred_at"));
    assert!(text.contains("tombstone_generation: 0"));
    assert!(text.contains("commit_watermark:"));
    assert!(text.contains("p_end_user"), "the columns are not named");
    assert_eq!(manifest.file_bytes, std::fs::metadata(&file).unwrap().len());
}

#[test]
fn an_export_of_a_sealed_segment_and_an_open_buffer_holds_both() {
    let place = directory("mixed");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(120)).unwrap();
    assert!(store.segment_count() > 0);
    // A few more that stay in the open buffer.
    let mut later = rows(5);
    for (index, row) in later.iter_mut().enumerate() {
        row.event_id[12..14].copy_from_slice(&(9_000u16 + index as u16).to_be_bytes());
    }
    store.commit([1; 16], [2; 16], later).unwrap();

    let (file, manifest) = export(&place, &store);
    assert_eq!(manifest.rows, 125);
    let counted = query(
        &file,
        &format!("SELECT count(*) FROM read_parquet('{}')", file.display()),
    );
    assert_eq!(counted, "125");
}

#[test]
fn an_export_of_another_project_is_empty() {
    // Tenant isolation reaches the export path too.
    let place = directory("isolation");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(50)).unwrap();

    let file = place.join("other.parquet");
    let manifest = export_events(
        &store,
        &ExportRequest {
            project_id: [1; 16],
            range_start: BASE_TIME - 1,
            range_end: BASE_TIME + 1_000_000,
            basis: TimeBasis::OccurredAt,
            into: file.clone(),
            tombstone_generation: 0,
            reserve_bytes: 0,
        },
    )
    .unwrap();
    assert_eq!(manifest.rows, 0);

    // An empty export is still a readable file with a schema, rather than
    // nothing at all.
    let counted = query(
        &file,
        &format!("SELECT count(*) FROM read_parquet('{}')", file.display()),
    );
    assert_eq!(counted, "0");
}

#[test]
fn one_property_name_holding_two_types_exports_as_text() {
    // D20 keeps typed variants in storage. A flat file cannot, so the one
    // representation that carries both is text, and the export says so by
    // giving the column that type rather than dropping a row.
    let place = directory("conflict");
    let store = store(&place);
    let mut mixed = rows(20);
    mixed[0].properties.insert(
        "attempts".into(),
        (PropertyValue::Text("many".into()), "client".into()),
    );
    store.commit([1; 16], [1; 16], mixed).unwrap();

    let (file, _) = export(&place, &store);
    let values = query(
        &file,
        &format!(
            "SELECT count(*) FROM read_parquet('{}') WHERE p_attempts = 'many'",
            file.display()
        ),
    );
    assert_eq!(values, "1", "the conflicting value was dropped");
}

#[test]
fn an_export_stops_rather_than_displacing_live_data() {
    // FAILURE_MODES.md section 10, export: fail the export. An export is the
    // one write nobody is waiting on and nothing depends on, so it is the first
    // to give way. Required test 13 covers the other five points in
    // `tallyowl-store/tests/exhaustion.rs`.
    let place = directory("export-no-room");
    let store = store(&place);
    store.commit([1; 16], [1; 16], rows(50)).unwrap();

    let file = place.join("refused.parquet");
    let refused = export_events(
        &store,
        &ExportRequest {
            project_id: PROJECT,
            range_start: BASE_TIME - 1,
            range_end: BASE_TIME + 1_000_000,
            basis: TimeBasis::OccurredAt,
            into: file.clone(),
            tombstone_generation: 0,
            // A reserve larger than the device, so nothing bulk can be written.
            reserve_bytes: u64::MAX,
        },
    )
    .unwrap_err();

    assert!(
        refused
            .to_string()
            .contains("never takes space from live data"),
        "{refused}"
    );
    assert!(!file.exists(), "a refused export left a file behind");

    // The live data is untouched, which is the property the rule protects.
    assert_eq!(
        store
            .scan(
                PROJECT,
                BASE_TIME - 1,
                BASE_TIME + 1_000_000,
                TimeBasis::OccurredAt
            )
            .unwrap()
            .rows
            .len(),
        50
    );
}
