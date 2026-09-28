//! Collector tests.
//!
//! These cover the branches `docs/DELIVERY.md` section 11 names, not the happy
//! path. The acknowledgement rule, the limits at the trust boundary, the
//! tenancy stamp, the timeout sweep, and quarantine each have a case here.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tallyowl_collector_api::types::{
    Batch, Envelope, EventPayload, PropertyOrigin, SubmitBatchRequest, TelemetryItem, TelemetryKind,
};
use tallyowl_obs::health::Health;
use tallyowl_obs::log::{Logger, Severity};
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_wire::{collector as wire, collector_items_bridge as items, Value};

use crate::durable::testing::FakeQueue;
use crate::durable::DurableQueue;
use crate::forwarder::{Forwarder, ForwarderState, DURABLE_STORE_CHECK, SWEEP_CHECK};
use crate::head_client::testing::{FakeHead, Outcome};
use crate::intake::{Intake, Limits};
use crate::tenancy::testing::FakeDirectory;
use crate::tenancy::{KeyDirectory, TenancyResolver};

const QUEUE: &str = "tallyowl-delivery";
const QUARANTINE: &str = "tallyowl-quarantine";

fn item(id: u8, name: &str) -> TelemetryItem {
    items::event(
        Envelope {
            event_id: vec![id; 16],
            kind: TelemetryKind::Event,
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

fn batch(id: u8, items: Vec<TelemetryItem>) -> SubmitBatchRequest {
    SubmitBatchRequest {
        batch: Batch {
            batch_id: vec![id; 16],
            items,
            common_properties: None,
            sealed_at: 1_785_628_800_000,
            compression: None,
        },
        policy_version: None,
        protocol_version: None,
    }
}

/// The same batch, from a client that declares which protocol it speaks.
fn batch_declaring(id: u8, items: Vec<TelemetryItem>, version: u64) -> SubmitBatchRequest {
    SubmitBatchRequest {
        protocol_version: Some(version),
        ..batch(id, items)
    }
}

fn intake_with(queue: Arc<FakeQueue>, metrics: Arc<Registry>) -> Intake {
    Intake::declare_metrics(&metrics);
    // The head owns the control catalog. These tests are about intake, so the
    // directory is a stand-in for that network hop and not for TallyOwl's own
    // storage, which AGENTS.md forbids mocking.
    let directory = FakeDirectory::new();
    directory.add("key-a", None);
    directory.add("key-b", None);
    Intake {
        queue: queue as Arc<dyn DurableQueue>,
        queue_name: QUEUE.into(),
        tenancy: Arc::new(TenancyResolver::new(
            directory as Arc<dyn KeyDirectory>,
            60_000,
        )),
        limits: Limits {
            max_batch_bytes: 512 * 1024,
            max_event_bytes: 64 * 1024,
            max_properties: 128,
        },
        durable_copies: 1,
        series: std::sync::Arc::new(crate::series::SeriesLedger::new(
            crate::series::SeriesBudget::default(),
        )),
        metrics,
        stamped: vec![
            ("cell".into(), "home".into()),
            ("region".into(), "home".into()),
        ],
        policy: None,
    }
}

// ---------------------------------------------------------------------------
// The acknowledgement rule
// ---------------------------------------------------------------------------

#[test]
fn a_receipt_arrives_only_after_the_durable_store_holds_the_batch() {
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());

    let accepted = intake
        .submit("key-a", batch(1, vec![item(1, "checkout-started")]))
        .expect("the batch is accepted");

    assert_eq!(accepted.response.accepted, 1);
    assert_eq!(
        queue.depth(),
        1,
        "the batch reached the durable store first"
    );
    // The receipt reports the configured requirement and never a stronger one.
    assert_eq!(accepted.response.durable_copies, 1);
}

#[test]
fn an_unreachable_durable_store_refuses_the_batch_rather_than_acknowledging_it() {
    // DELIVERY.md section 9: Corndogs unavailable at intake means refuse new
    // batches and never acknowledge. A receipt here would be a lie the app
    // driver acts on by discarding its copy.
    let queue = FakeQueue::new();
    queue.refuse(true);
    let metrics = Registry::new();
    let intake = intake_with(Arc::clone(&queue), Arc::clone(&metrics));

    let failure = intake
        .submit("key-a", batch(1, vec![item(1, "checkout-started")]))
        .expect_err("a batch cannot be accepted");

    assert_eq!(failure.code, tallyowl_obs::ErrorCode::Unavailable);
    assert!(failure.retryable, "the store can come back");
    assert_eq!(queue.depth(), 0);
    assert_eq!(
        metrics.counter_value(
            "tallyowl_batches_refused_total",
            &labels(&[("reason", "durable-store-unavailable")])
        ),
        1
    );
}

// ---------------------------------------------------------------------------
// Limits at the trust boundary
// ---------------------------------------------------------------------------

#[test]
fn a_batch_over_the_seal_is_refused_before_it_reaches_the_queue() {
    // Validation and hard limits run before the enqueue, so a poison payload
    // does not consume the delivery queue.
    let queue = FakeQueue::new();
    let mut intake = intake_with(Arc::clone(&queue), Registry::new());
    intake.limits.max_batch_bytes = 1024;

    let items: Vec<TelemetryItem> = (1..=40u8)
        .map(|n| item(n, "a-fairly-long-event-name"))
        .collect();
    let failure = intake.submit("key-a", batch(1, items)).unwrap_err();

    assert_eq!(failure.code, tallyowl_obs::ErrorCode::ResourceExhausted);
    assert!(!failure.retryable, "the same batch stays too large");
    assert!(failure.message.contains("KiB"), "{}", failure.message);
    assert!(failure.message.contains("limit"));
    assert_eq!(queue.depth(), 0, "nothing reached the durable store");
}

#[test]
fn an_invalid_item_is_rejected_by_identifier_and_the_rest_of_the_batch_commits() {
    // Bounded partial failure. DELIVERY.md section 9: commit the valid subset
    // and name the invalid IDs.
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());

    let mut bad = item(2, "no-time");
    bad.envelope.occurred_at = 0;

    let accepted = intake
        .submit(
            "key-a",
            batch(1, vec![item(1, "good"), bad, item(3, "good")]),
        )
        .expect("the batch is accepted");

    assert_eq!(accepted.response.accepted, 2);
    let rejected = accepted.response.rejected.expect("one item was rejected");
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].event_id, vec![2; 16]);
    assert!(rejected[0].message.contains("time"));
    assert_eq!(queue.depth(), 1);
}

#[test]
fn an_item_with_too_many_properties_is_rejected_and_names_both_numbers() {
    let queue = FakeQueue::new();
    let mut intake = intake_with(Arc::clone(&queue), Registry::new());
    intake.limits.max_properties = 2;

    let mut crowded = item(2, "crowded");
    for n in 0..5 {
        crowded.envelope.properties.push(wire::property(
            &format!("key{n}"),
            Value::Text("v".into()),
            PropertyOrigin::Client,
        ));
    }

    let accepted = intake
        .submit("key-a", batch(1, vec![item(1, "fine"), crowded]))
        .unwrap();
    let rejected = accepted.response.rejected.expect("one item was rejected");
    assert!(rejected[0].message.contains("5 properties"));
    assert!(rejected[0].message.contains("2 properties"));
}

#[test]
fn an_item_with_no_identifier_is_rejected() {
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());
    let mut nameless = item(1, "x");
    nameless.envelope.event_id = vec![1, 2, 3];
    let accepted = intake.submit("key-a", batch(1, vec![nameless])).unwrap();
    assert_eq!(accepted.response.accepted, 0);
    assert_eq!(accepted.response.rejected.unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Tenancy
// ---------------------------------------------------------------------------

#[test]
fn tenancy_comes_from_the_credential_and_a_payload_value_is_discarded() {
    // D32 and AGENTS.md: never accept tenancy from a payload. A client that
    // could set its own workspace could write into another tenant's data.
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());
    let real = intake.tenancy.resolve("key-a").unwrap();

    let mut forged = item(1, "checkout-started");
    forged.envelope.workspace_id = Some(vec![0xff; 16]);
    forged.envelope.project_id = Some(vec![0xff; 16]);
    forged.envelope.source_id = Some(vec![0xff; 16]);

    intake.submit("key-a", batch(1, vec![forged])).unwrap();

    let stored = stored_batch(&queue);
    let envelope = &stored.items[0].envelope;
    assert_eq!(envelope.workspace_id, Some(real.workspace_id.to_vec()));
    assert_eq!(envelope.project_id, Some(real.project_id.to_vec()));
    assert_ne!(envelope.workspace_id, Some(vec![0xff; 16]));
}

#[test]
fn the_collector_stamps_its_own_properties_and_refuses_a_client_value_for_a_protected_name() {
    // D38: a protected name refuses a client value and counts the refusal. It
    // does not accept the value and hide the conflict.
    let queue = FakeQueue::new();
    let metrics = Registry::new();
    let intake = intake_with(Arc::clone(&queue), Arc::clone(&metrics));

    let mut forged = item(1, "checkout-started");
    forged.envelope.properties.push(wire::property(
        "region",
        Value::Text("somewhere-else".into()),
        PropertyOrigin::Client,
    ));
    forged.envelope.properties.push(wire::property(
        "plan",
        Value::Text("pro".into()),
        PropertyOrigin::Client,
    ));

    intake.submit("key-a", batch(1, vec![forged])).unwrap();

    let stored = stored_batch(&queue);
    let properties = &stored.items[0].envelope.properties;

    let region = properties.iter().find(|p| p.key == "region").unwrap();
    assert_eq!(region.origin, PropertyOrigin::Collector);
    assert_eq!(
        wire::read(&region.value).unwrap(),
        Value::Text("home".into())
    );

    // An ordinary client property survives untouched.
    let plan = properties.iter().find(|p| p.key == "plan").unwrap();
    assert_eq!(plan.origin, PropertyOrigin::Client);

    assert_eq!(
        metrics.counter_value("tallyowl_protected_property_refused_total", &labels(&[])),
        1
    );
}

#[test]
fn a_client_property_borrowing_a_correlation_name_is_refused_at_intake() {
    // L153, and the owner's decision of 2026-08-10: a client property named
    // after a correlation column is shadowed by the column and becomes
    // silently unreachable by filter. The refusal is visible at intake
    // instead, where an operator can act on the counter.
    let queue = FakeQueue::new();
    let metrics = Registry::new();
    let intake = intake_with(Arc::clone(&queue), Arc::clone(&metrics));

    let mut item = item(1, "checkout-started");
    for name in ["request_id", "session_id", "trace_id", "event_id"] {
        item.envelope.properties.push(wire::property(
            name,
            Value::Text("shadowed".into()),
            PropertyOrigin::Client,
        ));
    }

    intake.submit("key-a", batch(1, vec![item])).unwrap();

    let stored = stored_batch(&queue);
    let properties = &stored.items[0].envelope.properties;
    for name in ["request_id", "session_id", "trace_id", "event_id"] {
        assert!(
            !properties.iter().any(|p| p.key == name),
            "{name} was stored and would be unreachable by filter"
        );
    }
    assert_eq!(
        metrics.counter_value("tallyowl_protected_property_refused_total", &labels(&[])),
        4
    );
}

#[test]
fn the_collector_stamps_a_receive_time_and_keeps_the_producer_time() {
    // CONVENTIONS.md section 7: three time facts, never collapsed.
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());
    intake
        .submit("key-a", batch(1, vec![item(1, "checkout-started")]))
        .unwrap();

    let stored = stored_batch(&queue);
    let envelope = &stored.items[0].envelope;
    assert_eq!(envelope.occurred_at, 1_785_628_800_000);
    assert!(envelope.received_at.unwrap() > 1_785_628_800_000);
}

#[test]
fn a_batch_with_no_credential_is_refused() {
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());
    let failure = intake.submit("", batch(1, vec![item(1, "x")])).unwrap_err();
    assert_eq!(failure.code, tallyowl_obs::ErrorCode::Unauthenticated);
    assert_eq!(queue.depth(), 0);
}

/// The batch that is waiting in the queue, read back out of its delivery task.
///
/// `compat` builds the same shape against the same intake, so it borrows this
/// rather than writing a second one that could disagree.
pub(crate) fn stored_batch_for_compat(queue: &FakeQueue) -> Batch {
    stored_batch(queue)
}

/// An intake wired the way `compat` needs one.
pub(crate) fn intake_for_compat(queue: Arc<FakeQueue>, metrics: Arc<Registry>) -> Intake {
    intake_with(queue, metrics)
}

fn stored_batch(queue: &FakeQueue) -> Batch {
    let payload = queue
        .claim(QUEUE, 30)
        .unwrap()
        .expect("one batch is waiting")
        .payload;
    let task = crate::task::decode(&payload).expect("the task reads");
    crate::task::open(&task, 16 * 1024 * 1024).expect("the batch reads")
}

// ---------------------------------------------------------------------------
// The forwarder and the sweep
// ---------------------------------------------------------------------------

fn forwarder_with(queue: Arc<FakeQueue>, head: Arc<FakeHead>) -> (Arc<Forwarder>, Arc<Health>) {
    forwarder_aged(queue, head, 24 * 60 * 60 * 1000)
}

fn forwarder_aged(
    queue: Arc<FakeQueue>,
    head: Arc<FakeHead>,
    max_delivery_age_ms: i64,
) -> (Arc<Forwarder>, Arc<Health>) {
    let health = Health::new();
    health.declare(SWEEP_CHECK, "not yet");
    health.declare(DURABLE_STORE_CHECK, "not yet");
    let metrics = Registry::new();
    Forwarder::declare_metrics(&metrics);
    let forwarder = Arc::new(Forwarder {
        queue,
        queue_name: QUEUE.into(),
        quarantine_queue: QUARANTINE.into(),
        head,
        health: Arc::clone(&health),
        metrics,
        logger: Arc::new(Logger::new("test", "0.0.0", Severity::Error)),
        state: ForwarderState::new(),
        sweep_staleness_ms: 3_000,
        max_payload_bytes: 16 * 1024 * 1024,
        max_delivery_age_ms,
    });
    (forwarder, health)
}

/// Put one encoded batch in the queue, as intake would have.
fn queue_one_batch(queue: &FakeQueue) {
    let mut item = item(1, "checkout-started");
    item.envelope.source_id = Some(vec![7; 16]);
    let batch = Batch {
        batch_id: vec![1; 16],
        items: vec![item],
        common_properties: None,
        sealed_at: 1,
        compression: None,
    };
    // The queue payload is a delivery task, not a bare batch: a forwarder needs
    // the attempt count and the acceptance time that Corndogs does not hold.
    let payload = crate::task::encode(&crate::task::seal(
        &batch,
        &[7; 16],
        tallyowl_obs::time::now_ms(),
    ));
    queue.submit(QUEUE, payload, 0).unwrap();
}

#[test]
fn a_delivered_batch_completes_its_task() {
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    queue_one_batch(&queue);
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));

    assert!(forwarder.deliver_one());
    assert_eq!(head.call_count(), 1);
    assert_eq!(queue.completed().len(), 1, "the task finished");
    assert_eq!(queue.depth(), 0);
}

#[test]
fn a_retryable_failure_parks_the_batch_rather_than_losing_it() {
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    head.set(Outcome::Retryable("The head is not answering.".into()));
    queue_one_batch(&queue);
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));

    assert!(forwarder.deliver_one());
    assert!(queue.completed().is_empty(), "nothing was completed");
    assert_eq!(
        queue.parked_count(),
        1,
        "the batch waits for another attempt"
    );

    // The sweep is what returns it. Nothing retries without that call.
    forwarder.sweep_once();
    assert_eq!(queue.depth(), 1);

    // And it delivers on the next attempt.
    head.set(Outcome::Commit);
    assert!(forwarder.deliver_one());
    assert_eq!(queue.completed().len(), 1);
}

#[test]
fn the_queue_depth_is_the_queues_answer_and_a_restart_relearns_the_age() {
    // L146 for the collector: a depth counted in this process reads as zero
    // after a restart, and zero looks like "there is no work". The depth here
    // comes from the queue at each sweep, and the age is this process's own
    // lower bound, which a restart honestly loses and the next claim re-learns.
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    head.set(Outcome::Retryable("The head is not answering.".into()));
    queue_one_batch(&queue);
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));

    // The claim learns the age; the failure parks the batch; the sweep reads
    // the queue. A parked batch counts as waiting: it is going to run.
    assert!(forwarder.deliver_one());
    forwarder.sweep_once();
    assert_eq!(forwarder.state.queue_depth.load(Ordering::Relaxed), 1);
    assert!(forwarder.state.oldest_waiting_at.load(Ordering::Relaxed) > 0);

    // A restarted process. The depth is still the queue's answer; the age is
    // unknown, and reporting zero beside a depth of one is visibly incomplete,
    // which beats a plausible wrong number.
    let (restarted, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));
    restarted.sweep_once();
    assert_eq!(restarted.state.queue_depth.load(Ordering::Relaxed), 1);
    assert_eq!(restarted.state.oldest_waiting_at.load(Ordering::Relaxed), 0);

    // The next claim re-learns the age from the task's own acceptance time.
    assert!(restarted.deliver_one());
    assert!(restarted.state.oldest_waiting_at.load(Ordering::Relaxed) > 0);

    // Delivery empties the queue, and an empty queue has no oldest: without
    // the clearing, the age of a batch long delivered would rise for ever.
    head.set(Outcome::Commit);
    restarted.sweep_once();
    assert!(restarted.deliver_one());
    restarted.sweep_once();
    assert_eq!(restarted.state.queue_depth.load(Ordering::Relaxed), 0);
    assert_eq!(restarted.state.oldest_waiting_at.load(Ordering::Relaxed), 0);
}

#[test]
fn a_permanent_rejection_goes_to_quarantine_rather_than_a_retry_storm() {
    // DELIVERY.md section 9: a head that rejects authentication is a permanent
    // failure. Quarantine the safe metadata and stop the retry storm.
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    head.set(Outcome::Permanent("This batch is not valid.".into()));
    queue_one_batch(&queue);
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));

    assert!(forwarder.deliver_one());
    assert_eq!(queue.quarantined().len(), 1);
    assert_eq!(queue.depth(), 0);
    assert_eq!(head.call_count(), 1, "it is not tried again");
}

#[test]
fn a_payload_that_cannot_be_read_is_quarantined_rather_than_retried_forever() {
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    queue.submit(QUEUE, vec![0xff, 0xfe, 0xfd], 0).unwrap();
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));

    assert!(forwarder.deliver_one());
    assert_eq!(queue.quarantined().len(), 1);
    assert_eq!(head.call_count(), 0, "it never reached the head");
}

#[test]
fn readiness_fails_when_the_sweep_cannot_run() {
    // D33 and CONVENTIONS.md section 3: a stopped sweep stops retry, backoff,
    // and dead-worker recovery together, so it is a readiness failure.
    let queue = FakeQueue::new();
    let (forwarder, health) = forwarder_with(Arc::clone(&queue), FakeHead::new());

    forwarder.sweep_once();
    assert!(health.is_ready());

    queue.refuse(true);
    forwarder.sweep_once();
    assert!(!health.is_ready());
    let summary = health.report().summary();
    assert!(summary.contains("durable store"), "{summary}");
    assert!(summary.contains("tried again"), "{summary}");
}

#[test]
fn readiness_fails_when_the_sweep_stops_being_called_at_all() {
    // The harder case. The last sweep succeeded, so a check on the last result
    // says everything is fine while retry has been dead for minutes.
    let queue = FakeQueue::new();
    let (forwarder, health) = forwarder_with(Arc::clone(&queue), FakeHead::new());
    forwarder.sweep_once();
    assert!(health.is_ready());

    forwarder
        .state
        .last_sweep_ms
        .store(tallyowl_obs::time::now_ms() - 60_000, Ordering::Relaxed);
    forwarder.check_sweep_age();

    assert!(!health.is_ready());
    assert!(health.report().summary().contains("stopped running"));
}

#[test]
fn a_worker_that_dies_mid_send_releases_its_batch() {
    // DELIVERY.md section 9: worker dies while sending, and the Corndogs timeout
    // returns the task to `queued`.
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    queue_one_batch(&queue);

    // Claim it and then vanish, as a killed process does.
    let claimed = queue.claim(QUEUE, 30).unwrap().expect("one batch");
    assert_eq!(queue.depth(), 0);
    assert!(queue.completed().is_empty());
    let _ = claimed;

    queue.release_claimed();
    assert_eq!(queue.depth(), 1, "the batch is available again");

    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));
    assert!(forwarder.deliver_one());
    assert_eq!(queue.completed().len(), 1);
}

#[test]
fn an_empty_queue_is_not_an_error() {
    let queue = FakeQueue::new();
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), FakeHead::new());
    assert!(!forwarder.deliver_one(), "there was no work");
}

#[test]
fn a_commit_whose_completion_is_lost_leaves_the_batch_for_another_attempt() {
    // The duplicate case. The head committed, the queue did not hear the
    // completion, and the sweep returns the task. The head deduplicates the
    // batch ID, so the result is one logical commit. The collector must not
    // treat this as data loss and must not treat it as success either.
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    queue_one_batch(&queue);
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));

    assert!(forwarder.deliver_one());
    assert_eq!(head.call_count(), 1);

    // A second delivery of the same batch reaches the head again, which is
    // exactly what at-least-once delivery means.
    queue_one_batch(&queue);
    assert!(forwarder.deliver_one());
    assert_eq!(head.call_count(), 2);
}

// ---------------------------------------------------------------------------
// Backoff
//
// Corndogs holds no attempt count, so TallyOwl carries one in its own task
// payload. Without it every retryable failure used the first delay for ever,
// which meant a head that was down for an hour was asked once a second for
// that hour. See L012 and DELIVERY.md section 4.
// ---------------------------------------------------------------------------

/// The task waiting in the queue right now.
fn queued_task(queue: &FakeQueue) -> tallyowl_collector_api::types::DeliveryTask {
    let claimed = queue
        .claim(QUEUE, 30)
        .unwrap()
        .expect("one batch is waiting");
    let task = crate::task::decode(&claimed.payload).expect("the task reads");
    // Put it back the way the sweep would, so a caller can read and then
    // deliver.
    queue.release_claimed();
    task
}

#[test]
fn each_failed_attempt_is_counted_and_the_delay_grows() {
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    head.set(Outcome::Retryable("The head is not answering.".into()));
    queue_one_batch(&queue);
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));

    assert_eq!(
        queued_task(&queue).attempts,
        0,
        "nothing has been tried yet"
    );

    let mut delays = Vec::new();
    for expected in 1..=4u64 {
        assert!(forwarder.deliver_one());
        // A parked task returns to the queue at the next sweep.
        queue.sweep(QUEUE, 0).unwrap();
        let task = queued_task(&queue);
        assert_eq!(task.attempts, expected, "the count survives the wait");
        assert!(task.last_failure.is_some(), "and so does the reason");
        delays.push(task.next_attempt_at.unwrap() - task.last_attempt_at.unwrap());
    }

    // The delay grows. Jitter makes each one a range rather than a number, so
    // the assertion is on the shape rather than on an exact value.
    assert!(delays[0] < delays[2], "{delays:?}");
    assert!(delays[3] >= delays[2], "{delays:?}");
    assert!(
        delays[3] <= 60_000 + 15_000,
        "capped at a minute plus jitter"
    );
}

#[test]
fn the_acceptance_time_never_moves_across_retries() {
    // The age of a batch decides when automatic retry stops. An acceptance time
    // that moved with each attempt would make a batch immortal.
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    head.set(Outcome::Retryable("later".into()));
    queue_one_batch(&queue);
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));

    let first = queued_task(&queue).accepted_at;
    for _ in 0..3 {
        forwarder.deliver_one();
        queue.sweep(QUEUE, 0).unwrap();
    }
    assert_eq!(queued_task(&queue).accepted_at, first);
}

#[test]
fn a_batch_older_than_the_retry_window_goes_to_quarantine_rather_than_being_retried_for_ever() {
    // D36: an automatic retry must stop before the head forgets the batch ID,
    // or a later retry commits a second logical batch.
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    head.set(Outcome::Retryable("The head is not answering.".into()));
    queue_one_batch(&queue);
    // A window of nothing, so the first attempt is already past it.
    let (forwarder, _) = forwarder_aged(Arc::clone(&queue), Arc::clone(&head), -1);

    assert!(forwarder.deliver_one());
    assert_eq!(
        queue.quarantined().len(),
        1,
        "it stopped rather than parked"
    );
    assert_eq!(queue.parked_count(), 0);
}

#[test]
fn a_queue_payload_from_a_newer_collector_is_quarantined_rather_than_retried() {
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    let mut task = crate::task::seal(
        &Batch {
            batch_id: vec![1; 16],
            items: vec![item(1, "x")],
            common_properties: None,
            sealed_at: 1,
            compression: None,
        },
        &[7; 16],
        tallyowl_obs::time::now_ms(),
    );
    task.task_version = crate::task::TASK_VERSION + 1;
    queue.submit(QUEUE, crate::task::encode(&task), 0).unwrap();

    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));
    assert!(forwarder.deliver_one());
    assert_eq!(head.call_count(), 0, "it never reached the head");
    assert_eq!(queue.quarantined().len(), 1);
}

#[test]
fn a_delivered_batch_carries_its_attempt_number_to_the_head() {
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    head.set(Outcome::Retryable("later".into()));
    queue_one_batch(&queue);
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));

    forwarder.deliver_one();
    queue.sweep(QUEUE, 0).unwrap();
    head.set(Outcome::Commit);
    forwarder.deliver_one();

    let last = head
        .seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("a request");
    let request =
        tallyowl_collector_api::codec::decode_commit_batch_request(&last).expect("it decodes");
    assert_eq!(
        request.attempt,
        Some(1),
        "the head sees a retry for what it is"
    );
}

// ---------------------------------------------------------------------------
// The delivery loop: pacing, liveness, and the age it reports
// ---------------------------------------------------------------------------

#[test]
fn a_head_outage_is_probed_at_a_growing_interval_rather_than_in_a_hot_loop() {
    // With a backlog there was always another ready batch, so the loop claimed,
    // failed, rewrote the payload, and warned as fast as the durable store
    // allowed, for the whole outage.
    use crate::forwarder::Breaker;
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    head.set(Outcome::Retryable("The head is not answering.".into()));
    for _ in 0..5 {
        queue_one_batch(&queue);
    }
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));
    let mut breaker = Breaker::new();

    // The first failure opens the breaker for at least a second.
    let mut now = 1_000_000;
    forwarder.delivery_tick(&mut breaker, now);
    assert_eq!(head.call_count(), 1);
    let first_wait = breaker.wait_ms(now);
    assert!((1_000..=1_250).contains(&first_wait), "{first_wait}");

    // While it is open, a turn reaches neither the head nor the queue, however
    // many batches are ready.
    for step in 1..=9 {
        forwarder.delivery_tick(&mut breaker, now + step * 100);
    }
    assert_eq!(
        head.call_count(),
        1,
        "nothing was sent while the breaker was open"
    );
    assert_eq!(queue.depth(), 4, "the backlog stayed queued and untouched");

    // Each probe that fails doubles the wait, up to thirty seconds.
    let mut waits = vec![first_wait];
    for _ in 0..7 {
        now += breaker.wait_ms(now);
        // The sweep returns a parked batch when its delay expires.
        queue.release_parked();
        forwarder.delivery_tick(&mut breaker, now);
        waits.push(breaker.wait_ms(now));
    }
    assert_eq!(head.call_count(), 8, "one probe for each wait");
    assert!(waits[1] >= 2_000 && waits[2] >= 4_000, "{waits:?}");
    let longest = *waits.last().unwrap();
    assert!((30_000..=37_500).contains(&longest), "{waits:?}");

    // The first success closes it, and the backlog drains at full speed.
    head.set(Outcome::Commit);
    queue.release_parked();
    now += breaker.wait_ms(now);
    let mut turns = 0;
    while queue.completed().len() < 5 && turns < 20 {
        let pause = forwarder.delivery_tick(&mut breaker, now);
        assert!(pause.is_zero() || queue.completed().len() == 5 || queue.depth() == 0);
        turns += 1;
    }
    assert_eq!(queue.completed().len(), 5);
    assert!(!breaker.is_tripped());
}

#[test]
fn a_probe_that_fails_does_not_write_the_batch_again() {
    // The attempt count lives in the payload, so counting an attempt rewrites
    // up to 512 KiB and syncs it. During an outage that write competes with
    // intake for the one thing still protecting data.
    use crate::forwarder::Breaker;
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    head.set(Outcome::Retryable("The head is not answering.".into()));
    queue_one_batch(&queue);
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));
    let mut breaker = Breaker::new();

    let mut now = 1_000_000;
    forwarder.delivery_tick(&mut breaker, now);
    queue.release_parked();
    assert_eq!(
        queued_task(&queue).attempts,
        1,
        "the first failure is counted"
    );

    now += breaker.wait_ms(now);
    forwarder.delivery_tick(&mut breaker, now);
    assert_eq!(head.call_count(), 2, "the probe reached the head");
    assert_eq!(queue.parked_count(), 1, "and the batch was parked again");
    queue.release_parked();
    assert_eq!(
        queued_task(&queue).attempts,
        1,
        "the probe said nothing new about the batch, so the payload was left alone"
    );
}

#[test]
fn a_durable_store_that_cannot_be_claimed_from_opens_the_breaker_too() {
    // A claim error used to retry every 25 ms with a warning each time.
    use crate::forwarder::Breaker;
    let queue = FakeQueue::new();
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), FakeHead::new());
    let mut breaker = Breaker::new();
    queue.refuse(true);
    forwarder.delivery_tick(&mut breaker, 1_000_000);
    assert!(breaker.wait_ms(1_000_000) >= 1_000);
}

#[test]
fn the_oldest_waiting_age_falls_when_the_oldest_batch_is_delivered() {
    // It kept the minimum it had ever seen and cleared only at a depth of zero,
    // which a busy queue never reads. The gauge rose for ever, long after the
    // batch it described had committed.
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));

    let seal_at = |accepted_at: i64, id: u8| {
        let batch = Batch {
            batch_id: vec![id; 16],
            items: vec![item(1, "checkout-started")],
            common_properties: None,
            sealed_at: 1,
            compression: None,
        };
        let payload = crate::task::encode(&crate::task::seal(&batch, &[7; 16], accepted_at));
        queue.submit(QUEUE, payload, 0).unwrap();
    };
    let now = tallyowl_obs::time::now_ms();
    seal_at(now - 60_000, 1);
    seal_at(now - 5_000, 2);

    // Both fail once, so both are known and waiting.
    head.set(Outcome::Retryable("later".into()));
    assert!(forwarder.deliver_one());
    assert!(forwarder.deliver_one());
    assert_eq!(
        forwarder.state.oldest_waiting_at.load(Ordering::Relaxed),
        now - 60_000
    );

    // The old one is delivered. The queue is not empty, and the age still falls.
    head.set(Outcome::Commit);
    queue.release_parked();
    assert!(forwarder.deliver_one());
    assert_eq!(queue.completed().len(), 1);
    assert_eq!(
        forwarder.state.oldest_waiting_at.load(Ordering::Relaxed),
        now - 5_000,
        "the age now describes the batch that is still waiting"
    );

    assert!(forwarder.deliver_one());
    assert_eq!(forwarder.state.oldest_waiting_at.load(Ordering::Relaxed), 0);
}

#[test]
fn a_quarantined_batch_stops_being_the_oldest_waiting_batch() {
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    head.set(Outcome::Permanent("This batch is not valid.".into()));
    queue_one_batch(&queue);
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));
    assert!(forwarder.deliver_one());
    assert_eq!(queue.quarantined().len(), 1);
    assert_eq!(forwarder.state.oldest_waiting_at.load(Ordering::Relaxed), 0);
}

#[test]
fn readiness_fails_when_the_delivery_loop_stops_turning() {
    // The sweep kept readiness green while a delivery thread sat inside one
    // call for two days. Nothing judged the delivery loop, and the delivery
    // loop was the only judge of the sweep.
    use crate::forwarder::DELIVERY_CHECK;
    let queue = FakeQueue::new();
    let (forwarder, health) = forwarder_with(Arc::clone(&queue), FakeHead::new());
    forwarder.sweep_once();
    health.declare(DELIVERY_CHECK, "not yet");

    let turned_at = 1_000_000;
    forwarder
        .state
        .last_delivery_loop_ms
        .store(turned_at, Ordering::Relaxed);
    forwarder.check_delivery_age(turned_at + 1_000);
    assert!(
        health.is_ready(),
        "a loop that turned a second ago is running"
    );

    forwarder.check_delivery_age(turned_at + 10 * 60 * 1_000);
    assert!(!health.is_ready());
    assert!(health.report().summary().contains("stopped turning"));

    // And it recovers when the loop turns again.
    forwarder
        .state
        .last_delivery_loop_ms
        .store(turned_at + 10 * 60 * 1_000, Ordering::Relaxed);
    forwarder.check_delivery_age(turned_at + 10 * 60 * 1_000 + 500);
    assert!(health.is_ready());
}

#[test]
fn a_slow_depth_count_cannot_make_a_sweep_that_ran_look_stopped() {
    // The sweep time was stamped and then the count ran on the same thread. A
    // count that outlasted the staleness window failed readiness while the
    // sweep was running.
    let queue = FakeQueue::new();
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), FakeHead::new());
    queue_one_batch(&queue);
    assert!(forwarder.sweep_only());
    assert!(forwarder.state.last_sweep_ms.load(Ordering::Relaxed) > 0);
    assert_eq!(
        forwarder.state.queue_depth.load(Ordering::Relaxed),
        0,
        "the sweep alone does not count"
    );
    forwarder.count_once();
    assert_eq!(forwarder.state.queue_depth.load(Ordering::Relaxed), 1);
}

#[test]
fn a_retried_conversion_is_claimed_before_an_ordinary_event_that_arrived_later() {
    // The durable store writes whatever priority an update names. The park
    // named zero, so after one failed delivery a conversion went behind every
    // ordinary event, which is the reverse of DELIVERY.md section 8.
    let queue = FakeQueue::new();
    let head = FakeHead::new();
    head.set(Outcome::Retryable("later".into()));
    let (forwarder, _) = forwarder_with(Arc::clone(&queue), Arc::clone(&head));

    let payload_for = |id: u8| {
        let batch = Batch {
            batch_id: vec![id; 16],
            items: vec![item(1, "purchase")],
            common_properties: None,
            sealed_at: 1,
            compression: None,
        };
        crate::task::encode(&crate::task::seal(
            &batch,
            &[7; 16],
            tallyowl_obs::time::now_ms(),
        ))
    };
    let conversion = queue.submit(QUEUE, payload_for(1), 5).unwrap();
    assert!(forwarder.deliver_one());
    assert_eq!(queue.priority_of(&conversion), Some(5));

    queue.submit(QUEUE, payload_for(2), 2).unwrap();
    queue.release_parked();
    let next = queue.claim(QUEUE, 30).unwrap().expect("a task");
    assert_eq!(next.uuid, conversion);
}

#[test]
fn a_head_that_accepts_a_connection_and_never_answers_costs_one_deadline() {
    // This is the wedge that held a delivery thread for two days. The error has
    // to be retryable, because the batch is fine and the head may only be slow.
    use crate::head_client::{HeadClient, RemoteHead};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });

    let metrics = Registry::new();
    RemoteHead::declare_metrics(&metrics);
    let head = RemoteHead::new(&address, 1024 * 1024)
        .with_call_timeout(std::time::Duration::from_millis(40))
        .with_metrics(Arc::clone(&metrics));
    let failure = head.commit_batch(vec![0xa0]).expect_err("no answer");
    assert!(failure.retryable);
    assert!(
        failure.message.contains("did not answer"),
        "{}",
        failure.message
    );
    assert_eq!(
        metrics.counter_value(
            "tallyowl_head_call_failures_total",
            &labels(&[("op", "commit-batch")])
        ),
        1
    );
}

// ---------------------------------------------------------------------------
// The scrubber
//
// AGENTS.md: "Never record secrets, credentials, request bodies, claim values,
// or raw personal data by default." The collector is the trust boundary, so
// these fixtures go through the real intake path rather than through the
// scrubber directly.
// ---------------------------------------------------------------------------

fn error_item(id: u8, message: &str, file: &str) -> TelemetryItem {
    use tallyowl_collector_api::types::{
        ErrorPayload, ErrorPayload_severity as Severity, StackFrame,
    };
    let mut item = items::empty_item(item(id, "x").envelope);
    item.envelope.kind = TelemetryKind::Error;
    item.error = Some(ErrorPayload {
        error_type: "Timeout".into(),
        message: message.into(),
        handled: false,
        severity: Severity::Error,
        mechanism: None,
        runtime: None,
        frames: Some(vec![StackFrame {
            module: Some("checkout".into()),
            function: Some("charge".into()),
            file: Some(file.into()),
            line: Some(40),
            in_app: true,
        }]),
        breadcrumbs: None,
    });
    item
}

#[test]
fn a_credential_in_an_error_message_never_reaches_the_durable_store() {
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());

    intake
        .submit(
            "key-a",
            batch(
                1,
                vec![error_item(
                    1,
                    "dial postgres://app:s3cret@db.internal/orders failed for Bearer eyJhbGci.abc",
                    "/app/checkout.rs",
                )],
            ),
        )
        .expect("the batch is accepted");

    let stored = stored_batch(&queue);
    let message = &stored.items[0].error.as_ref().unwrap().message;
    assert!(!message.contains("s3cret"), "{message}");
    assert!(!message.contains("eyJhbGci.abc"), "{message}");
    // The host stays, because the host is what helps somebody diagnose.
    assert!(message.contains("db.internal"), "{message}");
}

#[test]
fn a_protected_property_keeps_its_name_and_loses_its_value() {
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());
    let mut sensitive = item(1, "checkout-started");
    sensitive.envelope.properties.push(wire::property(
        "api_key",
        Value::Text("sk-live-abcdef".into()),
        tallyowl_collector_api::types::PropertyOrigin::Client,
    ));
    sensitive.envelope.properties.push(wire::property(
        "route",
        Value::Text("/checkout".into()),
        tallyowl_collector_api::types::PropertyOrigin::Client,
    ));

    intake.submit("key-a", batch(1, vec![sensitive])).unwrap();

    let stored = stored_batch(&queue);
    let properties = &stored.items[0].envelope.properties;
    let held = |key: &str| {
        properties
            .iter()
            .find(|p| p.key == key)
            .map(|p| wire::read(&p.value).unwrap().to_display())
    };
    // The name stays. An empty value looks like a defect in the producer and
    // costs somebody an afternoon.
    assert_eq!(held("api_key").as_deref(), Some("<removed>"));
    assert_eq!(held("route").as_deref(), Some("/checkout"));
}

#[test]
fn a_token_in_a_route_goes_and_the_route_stays() {
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());
    let mut page = items::empty_item(item(1, "x").envelope);
    page.envelope.kind = TelemetryKind::PageView;
    page.page_view = Some(tallyowl_collector_api::types::PageViewPayload {
        route: "/reset?token=abc123".into(),
        page_title: None,
        referrer: Some("https://mail.example.com/read?id=99".into()),
        campaign: None,
    });

    intake.submit("key-a", batch(1, vec![page])).unwrap();

    let stored = stored_batch(&queue);
    let view = stored.items[0].page_view.as_ref().unwrap();
    assert_eq!(view.route, "/reset?<removed>");
    assert!(!view.referrer.as_ref().unwrap().contains("id=99"));
}

#[test]
fn a_stack_frame_keeps_its_file_because_a_path_is_not_personal_data() {
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());
    intake
        .submit(
            "key-a",
            batch(1, vec![error_item(1, "no value", "/app/checkout.rs")]),
        )
        .unwrap();
    let stored = stored_batch(&queue);
    let frames = stored.items[0]
        .error
        .as_ref()
        .unwrap()
        .frames
        .as_ref()
        .unwrap();
    assert_eq!(frames[0].file.as_deref(), Some("/app/checkout.rs"));
}

#[test]
fn an_ordinary_batch_passes_through_the_scrubber_unchanged() {
    // A scrubber that mangled ordinary telemetry would be turned off, and a
    // scrubber that is turned off protects nothing.
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());
    let mut ordinary = item(1, "checkout-started");
    ordinary.envelope.properties.push(wire::property(
        "route",
        Value::Text("/pricing".into()),
        tallyowl_collector_api::types::PropertyOrigin::Client,
    ));
    intake.submit("key-a", batch(1, vec![ordinary])).unwrap();

    let stored = stored_batch(&queue);
    let held = &stored.items[0].envelope.properties;
    assert!(held
        .iter()
        .any(|p| p.key == "route" && wire::read(&p.value).unwrap().to_display() == "/pricing"));
}

#[test]
fn the_collector_counts_what_it_removed() {
    let metrics = Registry::new();
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Arc::clone(&metrics));
    intake
        .submit(
            "key-a",
            batch(1, vec![error_item(1, "password=hunter2", "/app/x.rs")]),
        )
        .unwrap();
    assert_eq!(
        metrics.counter_value("tallyowl_scrubbed_values_total", &labels(&[])),
        1
    );
}

// ---------------------------------------------------------------------------
// Metric series cost at the trust boundary
//
// The budgets themselves are covered in `series`. These cover what a producer
// sees: an over-budget point becomes a rejected item on the receipt, the rest
// of the batch commits, and a merged batch reports what the caller sent.
// ---------------------------------------------------------------------------

fn metric_item(id: u8, name: &str, route: &str, value: f64) -> TelemetryItem {
    use tallyowl_collector_api::types::{
        MetricKind, MetricPointPayload, MetricPointPayload_temporality as Temporality,
    };
    let mut item = item(id, "unused");
    item.event = None;
    item.envelope.kind = TelemetryKind::MetricPoint;
    item.metric_point = Some(MetricPointPayload {
        metric_name: name.to_string(),
        metric_kind: MetricKind::Counter,
        unit: None,
        description: None,
        monotonic: true,
        temporality: Temporality::Delta,
        start_at: 1_785_628_800_000,
        end_at: 1_785_628_860_000,
        labels: vec![wire::property(
            "route",
            Value::Text(route.to_string()),
            PropertyOrigin::Client,
        )],
        number_value: Some(value),
        histogram_value: None,
        exemplar_trace_id: None,
    });
    item
}

fn histogram_item(id: u8, bounds: &[f64], counts: &[u64]) -> TelemetryItem {
    use tallyowl_collector_api::types::{HistogramValue, MetricKind};
    let mut item = metric_item(id, "request_seconds", "/a", 0.0);
    let point = item.metric_point.as_mut().expect("a metric point");
    point.metric_kind = MetricKind::Histogram;
    point.monotonic = false;
    point.number_value = None;
    point.histogram_value = Some(HistogramValue {
        count: counts.iter().sum(),
        sum: 1.0,
        bounds: bounds.to_vec(),
        counts: counts.to_vec(),
    });
    item
}

#[test]
fn a_histogram_whose_counts_do_not_match_its_bounds_is_rejected_by_identifier() {
    // Two delta points of one series, the second with more counts than bounds.
    // Merging them once read past the end of the first and stopped the worker,
    // and the driver then sent the same batch again for ever.
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());

    let accepted = intake
        .submit(
            "key-a",
            batch(
                1,
                vec![
                    histogram_item(1, &[0.1, 0.5], &[1, 2]),
                    histogram_item(2, &[0.1, 0.5], &[1, 2, 3, 4]),
                    histogram_item(3, &[0.1, 0.5], &[1]),
                ],
            ),
        )
        .expect("the batch commits around the two bad shapes");

    assert_eq!(accepted.response.accepted, 1);
    let rejected = accepted.response.rejected.expect("two items were refused");
    assert_eq!(
        rejected.iter().map(|r| r.event_id[0]).collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(
        rejected[0].code,
        tallyowl_collector_api::types::ErrorCode::InvalidArgument
    );
    assert!(
        rejected[0].message.contains("4 bucket counts") && rejected[0].message.contains("2 bounds"),
        "the message gives both numbers: {}",
        rejected[0].message
    );
    assert_eq!(stored_batch(&queue).items.len(), 1);
}

#[test]
fn a_batch_whose_every_item_is_rejected_writes_nothing_to_the_durable_store() {
    // DELIVERY.md section 3: validation runs before the enqueue so a poison
    // payload does not consume the delivery queue. An empty task is a queue
    // write, a claim, a head call, and a completion, all for nothing.
    let queue = FakeQueue::new();
    let metrics = Registry::new();
    let intake = intake_with(Arc::clone(&queue), Arc::clone(&metrics));

    let mut no_time = item(1, "checkout-started");
    no_time.envelope.occurred_at = 0;
    let mut no_id = item(2, "checkout-started");
    no_id.envelope.event_id = vec![2u8; 3];

    let accepted = intake
        .submit("key-a", batch(1, vec![no_time, no_id]))
        .expect("the receipt still names what was rejected");

    assert_eq!(accepted.response.accepted, 0);
    assert_eq!(accepted.response.rejected.map(|r| r.len()), Some(2));
    assert_eq!(queue.depth(), 0, "nothing reached the durable store");
    assert!(accepted.task_uuid.is_empty());
    assert_eq!(
        metrics.counter_value("tallyowl_batches_accepted_total", &labels(&[])),
        0
    );
}

fn narrow_intake(queue: Arc<FakeQueue>, metrics: Arc<Registry>, series: u64) -> Intake {
    let mut intake = intake_with(queue, metrics);
    intake.series = Arc::new(crate::series::SeriesLedger::new(
        crate::series::SeriesBudget {
            max_series_for_each_metric: series,
            ..crate::series::SeriesBudget::default()
        },
    ));
    intake
}

#[test]
fn a_metric_point_past_the_series_budget_is_refused_by_identifier_and_the_batch_commits() {
    let metrics = Registry::new();
    let queue = FakeQueue::new();
    let intake = narrow_intake(Arc::clone(&queue), Arc::clone(&metrics), 2);

    let accepted = intake
        .submit(
            "key-a",
            batch(
                1,
                vec![
                    metric_item(1, "requests_total", "/a", 1.0),
                    metric_item(2, "requests_total", "/b", 1.0),
                    metric_item(3, "requests_total", "/c", 1.0),
                    item(4, "checkout-started"),
                ],
            ),
        )
        .expect("the batch commits around the refusal");

    // Three of four survive, and the fourth is named by ID with a code a caller
    // can act on. That is the explicit backpressure, rather than a silent drop.
    assert_eq!(accepted.response.accepted, 3);
    let rejected = accepted.response.rejected.expect("one item was refused");
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].event_id, vec![3u8; 16]);
    assert_eq!(
        rejected[0].code,
        tallyowl_collector_api::types::ErrorCode::ResourceExhausted
    );
    assert!(
        rejected[0].message.contains("requests_total"),
        "the message names the metric that ran out of budget: {}",
        rejected[0].message
    );

    // The refusal is visible through the same endpoint as everything else.
    assert_eq!(
        metrics.counter_value(
            "tallyowl_metric_series_refused_total",
            &labels(&[("reason", "resource-exhausted")])
        ),
        1
    );
    assert_eq!(
        metrics.gauge_value("tallyowl_metric_series_active_count", &labels(&[])),
        2
    );

    // The ordinary event is untouched by a metric budget.
    let stored = stored_batch(&queue);
    assert!(stored.items.iter().any(|i| i.event.is_some()));
}

#[test]
fn a_high_cardinality_metric_inside_the_budget_is_never_refused_or_rewritten() {
    // AGENTS.md: do not silently drop, coalesce, or reject a value because it
    // has high cardinality. Every distinct label value keeps its own series.
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());
    let items: Vec<TelemetryItem> = (0..64)
        .map(|index| metric_item(index as u8, "requests_total", &format!("/r/{index}"), 1.0))
        .collect();

    let accepted = intake.submit("key-a", batch(1, items)).unwrap();
    assert_eq!(accepted.response.accepted, 64);
    assert!(accepted.response.rejected.is_none());
    let stored = stored_batch(&queue);
    assert_eq!(stored.items.len(), 64, "no two series were coalesced");
}

#[test]
fn two_snapshots_of_one_series_merge_and_the_receipt_still_counts_both() {
    let metrics = Registry::new();
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Arc::clone(&metrics));

    let accepted = intake
        .submit(
            "key-a",
            batch(
                1,
                vec![
                    metric_item(1, "requests_total", "/a", 3.0),
                    metric_item(2, "requests_total", "/a", 4.0),
                ],
            ),
        )
        .unwrap();

    // Both were accepted. A receipt that said one would read as a loss.
    assert_eq!(accepted.response.accepted, 2);
    let stored = stored_batch(&queue);
    assert_eq!(stored.items.len(), 1, "one series travels as one point");
    assert_eq!(
        stored.items[0].metric_point.as_ref().unwrap().number_value,
        Some(7.0)
    );
    assert_eq!(
        metrics.counter_value("tallyowl_metric_points_merged_total", &labels(&[])),
        1
    );
}

#[test]
fn one_projects_metric_budget_does_not_bind_another_project() {
    let queue = FakeQueue::new();
    let intake = narrow_intake(Arc::clone(&queue), Registry::new(), 1);
    intake
        .submit(
            "key-a",
            batch(1, vec![metric_item(1, "requests_total", "/a", 1.0)]),
        )
        .unwrap();
    // `key-b` resolves to its own project, so it has its own budget.
    let accepted = intake
        .submit(
            "key-b",
            batch(2, vec![metric_item(2, "requests_total", "/b", 1.0)]),
        )
        .unwrap();
    assert_eq!(accepted.response.accepted, 1);
    assert!(accepted.response.rejected.is_none());
}

// ---------------------------------------------------------------------------
// Collection policy at the collector. Phase 9.
//
// `docs/POLICY.md` section 8 names seven required tests. Five of them belong
// here, because they are about what a collector does with a snapshot; the other
// two are about the head compiling one, and they are in the head's tests.
// ---------------------------------------------------------------------------

mod collection_policy {
    use super::*;
    use crate::policy::testing::{snapshot, FakeSource};
    use crate::policy::{Held, PolicySource, Refresher, Snapshot};
    use tallyowl_collector_api::types::CampaignLinking;
    use tallyowl_obs::error::TallyOwlError;

    fn refresher(held: &Arc<Held>, source: Arc<FakeSource>) -> Refresher {
        let metrics = Registry::new();
        crate::policy::Refresher::declare_metrics(&metrics);
        Refresher {
            held: Arc::clone(held),
            source: source as Arc<dyn PolicySource>,
            tenancy: Arc::new(TenancyResolver::new(
                {
                    let directory = FakeDirectory::new();
                    directory.add("key-a", None);
                    directory
                } as Arc<dyn KeyDirectory>,
                60_000,
            )),
            credential: "key-a".to_string(),
            metrics,
            logger: Arc::new(Logger::new(
                "collector-policy-test",
                "0.0.0",
                Severity::Error,
            )),
            stopping: ForwarderState::new(),
        }
    }

    /// An intake with a policy it can be given.
    fn intake_with_policy(queue: Arc<FakeQueue>, held: Arc<Held>) -> Intake {
        let metrics = Registry::new();
        crate::policy::Refresher::declare_metrics(&metrics);
        Intake {
            policy: Some(held),
            ..intake_with(queue, metrics)
        }
    }

    #[test]
    fn an_invalid_snapshot_never_replaces_a_valid_one() {
        // POLICY.md section 8, required test 1. A broken snapshot would stop
        // collection everywhere at once, so the last good one keeps working.
        let held = Arc::new(Held::new(60_000));

        let mut good = snapshot(4);
        good.blocked_event_names = Some(vec!["debug-ping".into()]);
        held.apply(&good).expect("a good snapshot applies");
        assert_eq!(held.version(), 4);

        // A snapshot with no version at all. The head raises the version on
        // every write and never hands out 0, so a 0 here was never compiled.
        let refusal = held
            .apply(&snapshot(0))
            .expect_err("a snapshot with no version is refused");
        assert!(refusal.message.contains("carries no version"));
        assert_eq!(
            held.version(),
            4,
            "the refused snapshot replaced the good one"
        );
        assert!(held
            .current()
            .expect("the good snapshot is still in force")
            .blocked_event_names
            .contains("debug-ping"));

        // A sampling rate that is not a rate.
        let mut nonsense = snapshot(5);
        nonsense.head_sample_rate = 4.0;
        assert!(held.apply(&nonsense).is_err());
        assert_eq!(held.version(), 4);
    }

    #[test]
    fn a_collector_without_a_head_keeps_its_last_good_snapshot_and_reports_staleness() {
        // POLICY.md section 8, required test 2. Falling back to collecting
        // nothing, or to collecting everything, would both be a policy change
        // that nobody made and a network fault caused.
        let held = Arc::new(Held::new(1_000));
        let mut good = snapshot(7);
        good.blocked_event_names = Some(vec!["debug-ping".into()]);

        let source = FakeSource::new(vec![
            Ok(Some(good)),
            Err(TallyOwlError::unavailable("the head is not reachable")),
        ]);
        let refresher = refresher(&held, Arc::clone(&source));

        assert!(refresher.fetch_once(), "the first fetch applies a policy");
        assert_eq!(held.version(), 7);
        let reached_at = held.fetched_at();
        assert!(reached_at > 0);

        assert!(
            !refresher.fetch_once(),
            "an unreachable head changes nothing"
        );
        assert_eq!(
            held.version(),
            7,
            "the last good snapshot is still in force"
        );
        assert!(held
            .current()
            .expect("still holding a policy")
            .blocked_event_names
            .contains("debug-ping"));
        assert_eq!(
            held.fetched_at(),
            reached_at,
            "a failed fetch must not look like a successful one"
        );

        // Staleness is what reports the fault, and it is time that decides.
        assert!(!held.is_stale(reached_at + 500));
        assert!(held.is_stale(reached_at + 5_000));
    }

    #[test]
    fn a_collector_that_has_never_fetched_is_not_stale() {
        // It has not lost touch with the head; it has not started asking. A
        // collector that reported stale before its first fetch would never
        // become ready.
        let held = Held::new(1_000);
        assert!(!held.is_stale(1_000_000));
        assert_eq!(held.version(), 0);
    }

    #[test]
    fn the_fetch_passes_the_version_it_holds_so_an_unchanged_policy_costs_nothing() {
        // POLICY.md section 7: "A collector fetches a snapshot over CSIL-RPC
        // and passes its known version. The head returns nothing when the
        // version is current."
        let held = Arc::new(Held::new(60_000));
        let source = FakeSource::new(vec![Ok(Some(snapshot(3))), Ok(None)]);
        let refresher = refresher(&held, Arc::clone(&source));

        refresher.fetch_once();
        refresher.fetch_once();

        assert_eq!(
            *source.asked.lock().unwrap(),
            vec![None, Some(3)],
            "the first fetch holds nothing and the second names version 3"
        );
        assert_eq!(held.version(), 3);
    }

    #[test]
    fn a_policy_change_applies_at_a_batch_boundary_and_not_inside_a_batch() {
        // POLICY.md section 8, required test 6. The batch is judged against one
        // snapshot from end to end. A fetch that lands halfway through must not
        // make the first half obey one policy and the second half another.
        let queue = Arc::new(FakeQueue::new());
        let held = Arc::new(Held::new(60_000));
        let intake = intake_with_policy(Arc::clone(&queue), Arc::clone(&held));

        held.apply(&snapshot(1)).unwrap();
        let accepted = intake
            .submit(
                "key-a",
                batch(1, vec![item(1, "checkout"), item(2, "debug-ping")]),
            )
            .expect("the batch is accepted");
        assert_eq!(accepted.response.accepted, 2, "nothing is blocked yet");
        assert_eq!(accepted.response.policy_version, Some(1));

        // Now block it, between batches.
        let mut blocking = snapshot(2);
        blocking.blocked_event_names = Some(vec!["debug-ping".into()]);
        held.apply(&blocking).unwrap();

        let accepted = intake
            .submit(
                "key-a",
                batch(2, vec![item(3, "checkout"), item(4, "debug-ping")]),
            )
            .expect("the batch is accepted");
        assert_eq!(
            accepted.response.accepted, 1,
            "the blocked event costs no transport"
        );
        assert_eq!(accepted.response.policy_version, Some(2));
    }

    #[test]
    fn a_kill_switch_stops_collection_at_the_next_fetch_and_writes_nothing_to_the_queue() {
        // POLICY.md section 8, required test 7. A kill switch that still cost a
        // queue write, a delivery, and a commit would not be a kill switch.
        let queue = Arc::new(FakeQueue::new());
        let held = Arc::new(Held::new(60_000));
        let intake = intake_with_policy(Arc::clone(&queue), Arc::clone(&held));

        held.apply(&snapshot(1)).unwrap();
        intake
            .submit("key-a", batch(1, vec![item(1, "checkout")]))
            .expect("the batch is accepted");
        let before = queue.depth();
        assert_eq!(before, 1);

        let mut stopped = snapshot(2);
        stopped.kill_switch = Some(true);
        held.apply(&stopped).unwrap();

        let accepted = intake
            .submit("key-a", batch(2, vec![item(2, "checkout")]))
            .expect("the batch is answered rather than refused");
        assert_eq!(accepted.response.accepted, 0);
        assert_eq!(
            queue.depth(),
            before,
            "a killed batch reached the durable queue"
        );
        assert!(
            accepted.task_uuid.is_empty(),
            "a killed batch was written as a task"
        );
    }

    #[test]
    fn a_blocked_property_never_leaves_the_collector_and_a_redacted_one_keeps_its_name() {
        // The difference between blocking and redacting is what a query can
        // still see: a blocked key is gone, and a redacted one says the field
        // was present without saying what was in it.
        let queue = Arc::new(FakeQueue::new());
        let held = Arc::new(Held::new(60_000));
        let intake = intake_with_policy(Arc::clone(&queue), Arc::clone(&held));

        let mut policy = snapshot(1);
        policy.blocked_property_keys = Some(vec!["home_address".into()]);
        policy.redact_keys = Some(vec!["email".into()]);
        held.apply(&policy).unwrap();

        let mut held_item = item(1, "checkout");
        held_item.envelope.properties = vec![
            wire::property(
                "home_address",
                Value::Text("12 Owl Lane".into()),
                PropertyOrigin::Client,
            ),
            wire::property(
                "email",
                Value::Text("someone@example.com".into()),
                PropertyOrigin::Client,
            ),
            wire::property("plan", Value::Text("pro".into()), PropertyOrigin::Client),
        ];
        intake
            .submit("key-a", batch(1, vec![held_item]))
            .expect("the batch is accepted");

        let task = queue.first().expect("one delivery task");
        let delivered = crate::task::decode(&task).expect("the task reads");
        let batch = crate::task::open(&delivered, 16 * 1024 * 1024).expect("the batch reads");
        let properties = &batch.items[0].envelope.properties;

        assert!(
            !properties.iter().any(|p| p.key == "home_address"),
            "a blocked property left the collector"
        );
        let email = properties
            .iter()
            .find(|p| p.key == "email")
            .expect("a redacted key keeps its name");
        assert_eq!(
            wire::read(&email.value).unwrap(),
            Value::Text(crate::policy::REDACTED.to_string())
        );
        assert!(properties.iter().any(|p| p.key == "plan"));
    }

    #[test]
    fn unlinked_campaign_linking_keeps_the_campaign_and_removes_the_person() {
        // D30. An operator who turns off session-linked campaign data still
        // measures whether a campaign works.
        let snapshot_policy = {
            let mut held = snapshot(1);
            held.campaign_linking = Some(CampaignLinking::Unlinked);
            held
        };
        let read = Snapshot::read(&snapshot_policy).expect("the snapshot applies");

        let mut touch = items::campaign_touch(
            {
                let mut envelope = item(1, "spring").envelope;
                envelope.kind = TelemetryKind::CampaignTouch;
                envelope.session_id = Some("session-1".into());
                envelope.anonymous_id = Some("anon-1".into());
                envelope
            },
            tallyowl_collector_api::types::CampaignTouchPayload {
                campaign: tallyowl_collector_api::types::CampaignParameters {
                    source: Some("google".into()),
                    medium: Some("cpc".into()),
                    campaign: Some("spring".into()),
                    term: None,
                    content: None,
                    click_id: None,
                },
                referrer: None,
                referrer_domain: Some("google.com".into()),
                landing_route: Some("/spring".into()),
            },
        );

        assert!(read.keeps(&touch), "an unlinked touch is still collected");
        assert_eq!(read.unlink_campaign(&mut touch), 2);
        assert_eq!(touch.envelope.session_id, None);
        assert_eq!(touch.envelope.anonymous_id, None);
        assert_eq!(
            touch
                .campaign_touch
                .as_ref()
                .unwrap()
                .campaign
                .campaign
                .as_deref(),
            Some("spring"),
            "the campaign fact went with the person"
        );
    }

    #[test]
    fn campaign_linking_of_none_refuses_the_touch_and_leaves_everything_else() {
        let mut policy = snapshot(1);
        policy.campaign_linking = Some(CampaignLinking::None);
        let read = Snapshot::read(&policy).expect("the snapshot applies");

        let mut touch = item(1, "spring");
        touch.envelope.kind = TelemetryKind::CampaignTouch;
        assert!(!read.keeps(&touch));
        assert!(read.keeps(&item(2, "checkout")));
    }

    #[test]
    fn the_name_a_policy_blocks_by_is_the_name_the_head_stores() {
        // A rule that blocked `debug-ping` at the head and something else here
        // would make the saving silent and partial, and the two ends would
        // disagree about what was collected.
        assert_eq!(
            crate::policy::item_name(&item(1, "debug-ping")),
            "debug-ping"
        );

        let mut conversion = item(2, "unused");
        conversion.envelope.kind = TelemetryKind::Conversion;
        conversion.event = None;
        conversion.conversion = Some(tallyowl_collector_api::types::ConversionPayload {
            goal: "purchase".into(),
            value: None,
            currency: None,
            order_id: None,
            campaign: None,
            touch_event_id: None,
        });
        assert_eq!(crate::policy::item_name(&conversion), "purchase");
    }

    #[test]
    fn every_metric_this_module_declares_is_one_the_registry_accepts() {
        // `declare` returns a refusal for a name that breaks the rules, and the
        // call site discards it. A refused declaration is a metric that is
        // silently dropped when it is set, so a policy that blocked a thousand
        // events would report nothing.
        //
        // The running loop found exactly that: `tallyowl_policy_version` does
        // not end with a permitted unit suffix, the declaration was refused,
        // and the gauge was missing from the exposition while the block itself
        // worked. This asserts the declaration rather than the exposition,
        // because that is where the refusal is.
        let metrics = Registry::new();
        for (name, kind) in [
            (
                "tallyowl_policy_fetches_total",
                tallyowl_obs::MetricKind::Counter,
            ),
            (
                "tallyowl_policy_generation_count",
                tallyowl_obs::MetricKind::Gauge,
            ),
            (
                "tallyowl_items_dropped_by_policy_total",
                tallyowl_obs::MetricKind::Counter,
            ),
            (
                "tallyowl_policy_properties_refused_total",
                tallyowl_obs::MetricKind::Counter,
            ),
        ] {
            metrics
                .declare(name, kind, "declared by crate::policy", &[])
                .unwrap_or_else(|e| panic!("`{name}` is not a name the registry accepts: {}", e.0));
        }
    }
}

/// Collector intake's half of the version window Phase 11's third exit
/// criterion rests on. The head's half is
/// `crates/tallyowl-head/tests/protocol_window.rs`, and both read the window
/// from `tallyowl_wire::protocol` so the two cannot disagree.
mod protocol_window {
    use super::*;
    use tallyowl_obs::error::ErrorCode;
    use tallyowl_wire::protocol::{ACCEPTED_PROTOCOL_VERSIONS, PROTOCOL_VERSION};

    #[test]
    fn every_version_in_the_window_is_accepted() {
        let queue = FakeQueue::new();
        let intake = intake_with(Arc::clone(&queue), Registry::new());

        for (index, version) in ACCEPTED_PROTOCOL_VERSIONS.iter().enumerate() {
            let id = index as u8 + 1;
            let request = batch_declaring(id, vec![item(id, "checkout-started")], *version);
            let accepted = intake
                .submit("key-a", request)
                .unwrap_or_else(|e| panic!("version {version} is in the window: {}", e.message));
            assert_eq!(accepted.response.accepted, 1);
        }
        assert_eq!(queue.depth(), ACCEPTED_PROTOCOL_VERSIONS.len());
    }

    #[test]
    fn a_client_that_declares_nothing_is_accepted() {
        // Every driver written before the field predates the window.
        let queue = FakeQueue::new();
        let intake = intake_with(Arc::clone(&queue), Registry::new());
        let accepted = intake
            .submit("key-a", batch(1, vec![item(1, "checkout-started")]))
            .expect("an absent version is accepted");
        assert_eq!(accepted.response.accepted, 1);
        assert_eq!(queue.depth(), 1);
    }

    #[test]
    fn a_version_outside_the_window_is_refused_before_anything_is_durable() {
        let queue = FakeQueue::new();
        let metrics = Registry::new();
        let intake = intake_with(Arc::clone(&queue), Arc::clone(&metrics));

        let ahead = PROTOCOL_VERSION + 1;
        let refusal = intake
            .submit(
                "key-a",
                batch_declaring(1, vec![item(1, "checkout-started")], ahead),
            )
            .expect_err("a version this build cannot read is refused");

        assert_eq!(refusal.code, ErrorCode::SchemaUnsupported);
        assert!(
            refusal.message.contains(&ahead.to_string())
                && refusal.message.contains(&PROTOCOL_VERSION.to_string()),
            "the refusal names both ends: {}",
            refusal.message
        );
        // **Nothing was enqueued.** The refusal comes before the durable write,
        // so an application learns immediately rather than after the head
        // quarantines the batch.
        assert_eq!(queue.depth(), 0);
        assert_eq!(
            metrics.counter_value(
                "tallyowl_protocol_version_refused_total",
                &labels(&[("reason", "too-new")]),
            ),
            1
        );
        assert_eq!(
            metrics.counter_value(
                "tallyowl_batches_refused_total",
                &labels(&[("reason", "protocol-version")]),
            ),
            1
        );
    }
}

#[test]
fn an_exact_number_with_an_absurd_exponent_is_rejected_by_identifier_and_never_queued() {
    // A few bytes here. The head renders the value, so there it was a terabyte
    // of zeros, a stopped process, and the same durable batch again after every
    // restart. The head now refuses it too. Refusing it here names the item to
    // its sender, and keeps it out of the durable queue.
    use tallyowl_collector_api::types::{
        CsilDecimal, Property, PropertyOrigin, TypedValue, TypedValueKind,
    };
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());

    let decimal = |exponent: i64| Property {
        key: "price".into(),
        value: TypedValue {
            kind: TypedValueKind::Decimal,
            bool_value: None,
            int_value: None,
            uint_value: None,
            float_value: None,
            decimal_value: Some(CsilDecimal {
                exponent,
                mantissa: 1,
            }),
            text_value: None,
            bytes_value: None,
        },
        origin: PropertyOrigin::Client,
    };
    let mut ordinary = item(1, "priced");
    ordinary.envelope.properties.push(decimal(-2));
    let mut huge = item(2, "priced");
    huge.envelope.properties.push(decimal(1_000_000_000_000));
    let mut tiny = item(3, "priced");
    tiny.envelope.properties.push(decimal(i64::MIN));

    let accepted = intake
        .submit("key-a", batch(1, vec![ordinary, huge, tiny]))
        .expect("the batch commits around the two bad numbers");

    assert_eq!(accepted.response.accepted, 1);
    let rejected = accepted.response.rejected.expect("two items were refused");
    assert_eq!(
        rejected.iter().map(|r| r.event_id[0]).collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert!(
        rejected[0].message.contains("exponent"),
        "the refusal should say what is wrong: {}",
        rejected[0].message
    );
    assert_eq!(stored_batch(&queue).items.len(), 1);
}

// ---------------------------------------------------------------------------
// D62: how an application reaches intake
// ---------------------------------------------------------------------------

fn transport_service(queue: Arc<FakeQueue>) -> Arc<crate::service::CollectorService> {
    Arc::new(crate::service::CollectorService {
        intake: Arc::new(intake_with(queue, Registry::new())),
        health: Health::new(),
        logger: Arc::new(Logger::new("tallyowl-collector", "test", Severity::Error)),
        forwarder_state: ForwarderState::new(),
        credential: "key-a".into(),
        roles: vec!["intake".into()],
        policy: None,
    })
}

fn submit_over(
    client: &tallyowl_rpc::Client,
) -> Result<tallyowl_rpc::Response, tallyowl_obs::error::TallyOwlError> {
    client.call(
        "TallyOwlCollector",
        "submit-batch",
        tallyowl_collector_api::codec::encode_submit_batch_request(&batch(
            7,
            vec![item(1, "over-the-wire")],
        )),
    )
}

#[test]
fn an_application_on_the_same_host_reaches_intake_on_a_unix_socket() {
    let queue = FakeQueue::new();
    let dir = crate::transport::tests::scratch("unix-intake");
    std::fs::create_dir_all(&dir).expect("dir");
    let address = format!("unix:{}", dir.join("intake.sock").display());
    let (server, exposure) = crate::transport::serve_intake(
        &address,
        transport_service(Arc::clone(&queue)),
        tallyowl_rpc::ServerOptions::new(1024 * 1024),
        None,
        false,
    )
    .expect("intake listens on a unix socket");
    assert_eq!(exposure, crate::transport::Exposure::Local);

    let client = tallyowl_rpc::Client::new(&address, 1024 * 1024).with_credential("key-a");
    let reply = submit_over(&client).expect("a receipt");
    assert_ne!(
        reply.variant.as_deref(),
        Some(tallyowl_rpc::SERVICE_ERROR_VARIANT)
    );
    assert_eq!(stored_batch(&queue).items.len(), 1);
    server.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn intake_on_a_network_address_serves_tls_and_the_batch_still_reaches_the_durable_store() {
    let queue = FakeQueue::new();
    let now = tallyowl_rpc::material::now_ms();
    let dir = crate::transport::tests::scratch("tls-intake");
    let authority = crate::transport::tests::write_pair(
        &dir,
        now - crate::transport::tests::HOUR_MS,
        2 * crate::transport::tests::HOUR_MS,
    );
    let certificates =
        crate::transport::application_certificates(&[dir.display().to_string()], now)
            .expect("loads")
            .expect("a set");
    let (server, exposure) = crate::transport::serve_intake(
        "0.0.0.0:0",
        transport_service(Arc::clone(&queue)),
        tallyowl_rpc::ServerOptions::new(1024 * 1024),
        Some(certificates),
        false,
    )
    .expect("intake listens");
    assert_eq!(exposure, crate::transport::Exposure::Tls);
    let port = server.local_address().port();
    let address = format!("127.0.0.1:{port}");

    let secure = tallyowl_rpc::Client::server_auth(
        &address,
        1024 * 1024,
        "127.0.0.1",
        Some(vec![authority]),
    )
    .expect("a client")
    .with_credential("key-a");
    let reply = submit_over(&secure).expect("a receipt over TLS");
    assert_ne!(
        reply.variant.as_deref(),
        Some(tallyowl_rpc::SERVICE_ERROR_VARIANT)
    );
    assert_eq!(queue.depth(), 1);

    // A client that does not use TLS reaches nothing, and nothing is stored.
    let plain = tallyowl_rpc::Client::new(&address, 1024 * 1024).with_credential("key-a");
    assert!(
        submit_over(&plain).is_err(),
        "a plaintext client was served by a TLS listener"
    );
    assert_eq!(
        queue.depth(),
        1,
        "a plaintext batch reached the durable store"
    );
    assert_eq!(stored_batch(&queue).items.len(), 1);
    server.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_network_intake_with_no_certificate_does_not_listen_at_all() {
    let refused = crate::transport::serve_intake(
        "0.0.0.0:0",
        transport_service(FakeQueue::new()),
        tallyowl_rpc::ServerOptions::new(1024 * 1024),
        None,
        false,
    )
    .err()
    .expect("refused");
    assert!(
        refused.message.contains("tls.certificateDirectories"),
        "{}",
        refused.message
    );
}

/// A sink that keeps nothing and says so honestly.
struct Nothing;
impl tallyowl_compat::receiver::Sink for Nothing {
    fn accept(
        &self,
        _items: Vec<TelemetryItem>,
    ) -> Result<tallyowl_compat::receiver::Kept, String> {
        Ok(tallyowl_compat::receiver::Kept::default())
    }
}

#[test]
fn an_opentelemetry_push_crosses_tls_with_the_intake_certificate() {
    use std::io::{Read, Write};
    let now = tallyowl_rpc::material::now_ms();
    let dir = crate::transport::tests::scratch("tls-otlp");
    let authority = crate::transport::tests::write_pair(
        &dir,
        now - crate::transport::tests::HOUR_MS,
        2 * crate::transport::tests::HOUR_MS,
    );
    let certificates =
        crate::transport::application_certificates(&[dir.display().to_string()], now)
            .expect("loads")
            .expect("a set");
    let (tls, exposure) = crate::transport::receiver_tls("0.0.0.0:0", Some(&certificates), false)
        .expect("a TLS configuration");
    assert_eq!(exposure, crate::transport::Exposure::Tls);
    let receiver = tallyowl_compat::receiver::start_secured("0.0.0.0:0", Arc::new(Nothing), tls)
        .expect("the receiver listens");
    let port = receiver.local_address().port();

    tallyowl_rpc::tls::install_crypto_provider();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(authority))
        .expect("the authority");
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1").expect("a name");
    let session = rustls::ClientConnection::new(Arc::new(config), name).expect("a session");
    let socket = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let mut stream = rustls::StreamOwned::new(session, socket);
    stream
        .write_all(b"POST /v1/metrics HTTP/1.1\r\nHost: t\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\n\r\n")
        .expect("send");
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    assert!(
        answer.starts_with("HTTP/1.1 200"),
        "the answer over TLS was {answer:?}"
    );

    // Plaintext to a TLS receiver gets no HTTP answer.
    let mut plain = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    plain
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("deadline");
    let _ = plain.write_all(b"POST /v1/metrics HTTP/1.1\r\nHost: t\r\nContent-Length: 0\r\n\r\n");
    let mut reply = String::new();
    let _ = plain.read_to_string(&mut reply);
    assert!(
        !reply.starts_with("HTTP/1.1 200"),
        "a plaintext push was served: {reply:?}"
    );
    receiver.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_collector_with_no_identity_yet_still_acknowledges_into_the_durable_store() {
    // The enrollment has not finished, so the head cannot be reached. Intake
    // needs no identity, so a batch is still made durable, and the head call
    // fails as retryable, which the forwarder's breaker waits out.
    let queue = FakeQueue::new();
    let intake = intake_with(Arc::clone(&queue), Registry::new());
    let accepted = intake
        .submit("key-a", batch(9, vec![item(1, "before-enrollment")]))
        .expect("intake does not wait for the collector's identity");
    assert_eq!(accepted.response.accepted, 1);
    assert_eq!(queue.depth(), 1);

    let no_identity = Arc::new(tallyowl_rpc::material::SwappableIdentity::new(None));
    let trust = Arc::new(tallyowl_rpc::trust::StaticTrust(Vec::new()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
    let head = crate::head_client::RemoteHead::mutual(
        &listener.local_addr().expect("address").to_string(),
        1024 * 1024,
        "head.tallyowl.internal",
        no_identity,
        trust,
    )
    .with_call_timeout(std::time::Duration::from_secs(2));
    let outcome = crate::head_client::HeadClient::commit_batch(&head, vec![0u8; 4]);
    let refused = outcome.expect_err("a head call with no identity must not succeed");
    assert!(
        refused.retryable,
        "a head call with no identity must be retried later: {}",
        refused.message
    );
    assert_eq!(
        queue.depth(),
        1,
        "the batch stays durable while the forwarder waits"
    );
}
