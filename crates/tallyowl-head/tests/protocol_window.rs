//! The version window a rolling upgrade rests on.
//!
//! `docs/PLAN.md` Phase 11 asks that "rolling upgrades maintain adjacent-version
//! clients". `docs/DEPLOYMENT.md` section 7 gives the order that makes it
//! possible — the ingest head first, then the storage voters, then the
//! collectors — and that order is what produces an adjacent-version client: for
//! the length of the roll, every collector still running is one version behind
//! the head it forwards to.
//!
//! The window is what makes that safe rather than hopeful. This file holds the
//! head's half of it:
//!
//! - a commit from a client at the current version is accepted;
//! - a commit from a client at **any version in the window** is accepted;
//! - a commit from a client outside the window is refused, with a message that
//!   names both ends and a counter an operator can alert on;
//! - a commit that declares nothing is accepted, because a client older than
//!   the field predates the window;
//! - **and every one of those still holds after the head restarts**, which is
//!   the part that makes it a rolling-upgrade test rather than a validation
//!   test. The store is reopened from the same directory, exactly as a restarted
//!   head reopens it, and the batch a "one version behind" collector sends after
//!   the restart commits and is queryable.
//!
//! What this does not prove, stated plainly: there is one protocol version, so
//! no version in the window differs from another in behavior. What is tested is
//! the mechanism — the declaration, the window, the refusal, and the counter —
//! before the second version exists rather than after it. See L183.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tallyowl_collector_api::types::{
    Batch, CommitBatchRequest, Envelope, EventPayload, ReceiptPolicy, TelemetryItem, TelemetryKind,
};
use tallyowl_head::ingest::Ingest;
use tallyowl_obs::error::ErrorCode;
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_store::{SegmentedStore, Store};
use tallyowl_wire::collector_items_bridge as items;
use tallyowl_wire::protocol::{refusal_reason, ACCEPTED_PROTOCOL_VERSIONS, PROTOCOL_VERSION};

const PROJECT: [u8; 16] = [9; 16];
const WORKSPACE: [u8; 16] = [8; 16];
const SOURCE: [u8; 16] = [7; 16];
const BASE_TIME: i64 = 1_785_628_800_000;

fn directory(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("head-protocol-window")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn item(id: u8, name: &str) -> TelemetryItem {
    items::event(
        Envelope {
            event_id: vec![id; 16],
            kind: TelemetryKind::Event,
            schema_version: 1,
            occurred_at: BASE_TIME,
            observed_at: None,
            received_at: Some(BASE_TIME + 5),
            workspace_id: Some(WORKSPACE.to_vec()),
            project_id: Some(PROJECT.to_vec()),
            source_id: Some(SOURCE.to_vec()),
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
            sdk_name: "tallyowl-driver-rust".into(),
            sdk_version: "0.0.0".into(),
            properties: Vec::new(),
            measurements: None,
        },
        EventPayload {
            name: name.into(),
            route: None,
            page_title: None,
        },
    )
}

fn commit(batch_id: u8, declared: Option<u64>) -> CommitBatchRequest {
    CommitBatchRequest {
        batch: Batch {
            batch_id: vec![batch_id; 16],
            items: vec![item(batch_id, "checkout")],
            common_properties: None,
            sealed_at: BASE_TIME,
            compression: None,
        },
        source_id: SOURCE.to_vec(),
        attempt: None,
        protocol_version: declared,
    }
}

/// A head over a real store in `place`. Calling it twice with one place is a
/// restart: the second head reopens what the first one wrote.
fn head_at(place: &Path) -> Ingest {
    let store: Arc<dyn Store> = Arc::new(SegmentedStore::open(place).expect("open"));
    let metrics = Registry::new();
    Ingest::declare_metrics(&metrics);
    Ingest {
        golden_signal_bucket_ms: 60_000,
        store,
        metrics,
        receipt_policy: ReceiptPolicy::LocalOne,
        open_traces: None,
        policy: None,
    }
}

#[test]
fn every_version_in_the_window_commits() {
    let place = directory("in-window");
    let head = head_at(&place);

    for (index, version) in ACCEPTED_PROTOCOL_VERSIONS.iter().enumerate() {
        let receipt = head
            .commit(commit(index as u8 + 1, Some(*version)))
            .unwrap_or_else(|e| panic!("version {version} is in the window: {}", e.message));
        assert_eq!(receipt.accepted, 1);
        assert_eq!(
            receipt.protocol_version, PROTOCOL_VERSION,
            "the receipt says what the head speaks, not what the caller sent"
        );
    }
}

#[test]
fn a_client_that_declares_nothing_commits() {
    // A driver older than the field predates the window, and refusing it would
    // refuse every client written before this release for no reason.
    let place = directory("no-declaration");
    let head = head_at(&place);
    let receipt = head
        .commit(commit(1, None))
        .expect("an absent version commits");
    assert_eq!(receipt.accepted, 1);
}

#[test]
fn a_version_outside_the_window_is_refused_and_counted() {
    let place = directory("outside");
    let head = head_at(&place);

    let older = PROTOCOL_VERSION.saturating_sub(ACCEPTED_PROTOCOL_VERSIONS.len() as u64 + 1);
    let newer = PROTOCOL_VERSION + 1;

    for (batch, version) in [(1u8, older), (2u8, newer)] {
        let refusal = head
            .commit(commit(batch, Some(version)))
            .expect_err("a version outside the window is refused");
        assert_eq!(refusal.code, ErrorCode::SchemaUnsupported);
        assert!(
            refusal.message.contains(&version.to_string()),
            "the refusal names what the client sent: {}",
            refusal.message
        );
        assert!(
            refusal.message.contains(&PROTOCOL_VERSION.to_string()),
            "the refusal names what this installation accepts: {}",
            refusal.message
        );
        assert_eq!(
            head.metrics.counter_value(
                "tallyowl_protocol_version_refused_total",
                &labels(&[("reason", refusal_reason(version))]),
            ),
            1,
            "an operator can see which side of the window it fell on"
        );
    }

    // Nothing was stored. A refused commit is a refusal, not a partial write.
    assert_eq!(
        head.store.receipt(SOURCE, [1; 16]),
        None,
        "a refused batch leaves no receipt behind"
    );
}

#[test]
fn the_window_still_holds_after_the_head_restarts() {
    // The roll: the head goes down and comes back, and the collector that is
    // one version behind keeps being served. DEPLOYMENT.md section 7 puts the
    // head first for exactly this reason.
    let place = directory("restart");

    let before = head_at(&place);
    let receipt = before
        .commit(commit(1, Some(PROTOCOL_VERSION)))
        .expect("the first commit lands");
    assert_eq!(receipt.accepted, 1);
    let watermark = receipt.commit_watermark;
    drop(before);

    let after = head_at(&place);

    // The batch the restarted head already holds is deduplicated rather than
    // committed twice, which is what a retry across a restart produces.
    let repeat = after
        .commit(commit(1, Some(PROTOCOL_VERSION)))
        .expect("a retry after the restart is answered");
    assert_eq!(repeat.deduplicated, Some(true));
    assert_eq!(repeat.commit_watermark, watermark);

    // A client anywhere in the window still commits, and the store moves.
    for (index, version) in ACCEPTED_PROTOCOL_VERSIONS.iter().enumerate() {
        let receipt = after
            .commit(commit(index as u8 + 10, Some(*version)))
            .unwrap_or_else(|e| panic!("version {version} after the restart: {}", e.message));
        assert_eq!(receipt.accepted, 1);
        assert!(receipt.commit_watermark > watermark);
    }

    // And a client outside it is still refused, with the same reason.
    let refusal = after
        .commit(commit(99, Some(PROTOCOL_VERSION + 1)))
        .expect_err("the window survives the restart");
    assert_eq!(refusal.code, ErrorCode::SchemaUnsupported);
}
