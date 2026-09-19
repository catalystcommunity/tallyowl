//! The Rust half of the cross-language agreement.
//!
//! This test writes `golden/vectors.json` when `TALLYOWL_UPDATE_GOLDEN` is set,
//! and otherwise asserts that Rust still produces exactly what the file holds.
//! The Go and TypeScript suites read the same file and assert the same thing.

use std::path::PathBuf;

use tallyowl_golden::{to_json, vectors, Vector};

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("golden/vectors.json")
}

/// Read the file without a JSON dependency. The shape is fixed and written by
/// this crate, so a full parser would buy nothing.
fn read_golden() -> Vec<(String, String)> {
    let text = std::fs::read_to_string(golden_path()).unwrap_or_default();
    let mut out = Vec::new();
    let mut name = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = field(line, "\"name\": \"") {
            name = value;
        } else if let Some(value) = field(line, "\"bytes\": \"") {
            out.push((std::mem::take(&mut name), value));
        }
    }
    out
}

fn field(line: &str, prefix: &str) -> Option<String> {
    let rest = line.strip_prefix(prefix)?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[test]
fn every_vector_matches_the_committed_bytes() {
    let built = vectors();
    if std::env::var("TALLYOWL_UPDATE_GOLDEN").is_ok() {
        std::fs::write(golden_path(), to_json(&built)).expect("write the vectors");
        return;
    }

    let committed = read_golden();
    assert!(
        !committed.is_empty(),
        "golden/vectors.json is empty or missing. Run `TALLYOWL_UPDATE_GOLDEN=1 cargo test -p tallyowl-golden` to write it."
    );
    assert_eq!(
        committed.len(),
        built.len(),
        "the file holds {} vectors and this build produced {}",
        committed.len(),
        built.len()
    );

    for (vector, (name, bytes)) in built.iter().zip(committed) {
        assert_eq!(vector.name, name, "the vectors are out of order");
        assert_eq!(
            tallyowl_golden::hex(&vector.bytes),
            bytes,
            "the bytes for `{name}` changed. If that was deliberate, the wire changed."
        );
    }
}

#[test]
fn every_vector_has_a_distinct_name() {
    // A repeated name would make one of the two invisible in the other
    // languages, which is the failure this whole suite exists to prevent.
    let built = vectors();
    let mut names: Vec<&str> = built.iter().map(|v| v.name).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(before, names.len(), "two vectors share a name");
}

#[test]
fn no_vector_is_empty() {
    for Vector { name, bytes, .. } in vectors() {
        assert!(!bytes.is_empty(), "`{name}` encoded to nothing");
    }
}

#[test]
fn the_three_packages_encode_one_envelope_identically() {
    // Each package carries its own copy of the shared types. This is the check
    // that a copy has not drifted, and it needs no golden file, because the two
    // sides are both in this build.
    use tallyowl_collector_api::types as ct;
    use tallyowl_ingest_api::types as it;

    let collector = ct::Envelope {
        event_id: vec![1; 16],
        kind: ct::TelemetryKind::Event,
        schema_version: 1,
        occurred_at: 1_785_628_800_000,
        observed_at: None,
        received_at: None,
        workspace_id: None,
        project_id: None,
        source_id: None,
        sequence: None,
        release: None,
        service_name: None,
        request_id: None,
        session_id: None,
        end_user_id: None,
        anonymous_id: None,
        trace_id: None,
        span_id: None,
        consent: None,
        sdk_name: "x".into(),
        sdk_version: "0".into(),
        properties: Vec::new(),
        measurements: None,
    };
    let ingest = it::Envelope {
        event_id: vec![1; 16],
        kind: it::TelemetryKind::Event,
        schema_version: 1,
        occurred_at: 1_785_628_800_000,
        observed_at: None,
        received_at: None,
        workspace_id: None,
        project_id: None,
        source_id: None,
        sequence: None,
        release: None,
        service_name: None,
        request_id: None,
        session_id: None,
        end_user_id: None,
        anonymous_id: None,
        trace_id: None,
        span_id: None,
        consent: None,
        sdk_name: "x".into(),
        sdk_version: "0".into(),
        properties: Vec::new(),
        measurements: None,
    };

    assert_eq!(
        tallyowl_collector_api::codec::encode_envelope(&collector),
        tallyowl_ingest_api::codec::encode_envelope(&ingest),
    );
}

#[test]
fn every_vector_decodes_back_to_the_value_it_came_from() {
    // Encoding agreement alone would not prove a reader can use the bytes.
    use tallyowl_collector_api::codec as cc;

    for vector in vectors() {
        if vector.type_name == "TypedValue" && vector.package == "tallyowl-collector-api" {
            let decoded = cc::decode_typed_value(&vector.bytes)
                .unwrap_or_else(|e| panic!("`{}` did not decode: {e}", vector.name));
            assert_eq!(
                cc::encode_typed_value(&decoded),
                vector.bytes,
                "`{}` did not encode back to itself",
                vector.name
            );
        }
    }
}
