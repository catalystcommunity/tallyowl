//! The queue against a real Corndogs server.
//!
//! It runs only when `CORNDOGS_TEST_ADDR` names a server, so the ordinary test
//! run starts no process and waits for nothing. Start Corndogs 0.8.0 or later
//! with both policies `required`, then:
//!
//! ```sh
//! CORNDOGS_TEST_ADDR=127.0.0.1:5080 cargo test -p tallyowl-queue --test live
//! ```
//!
//! With `CORNDOGS_TEST_OLD=1` the server is one older than 0.8.0, and the test
//! proves that the queue refuses it when it connects.

use tallyowl_obs::error::ErrorCode;
use tallyowl_queue::{CorndogsQueue, DurableQueue, STATE_BACKOFF};

fn server() -> Option<String> {
    std::env::var("CORNDOGS_TEST_ADDR")
        .ok()
        .filter(|addr| !addr.is_empty())
}

/// A queue name no other run uses, so a run finds only its own tasks.
fn fresh_queue(what: &str) -> String {
    format!("tallyowl-live-{what}-{}", tallyowl_obs::time::now_nanos())
}

#[test]
fn a_task_goes_through_its_whole_life_with_keys_and_guards() {
    let Some(addr) = server() else { return };
    if std::env::var("CORNDOGS_TEST_OLD").is_ok() {
        return;
    }
    let queue = CorndogsQueue::connect(&addr).expect("a 0.8.0 server");
    let store = queue.store();
    assert_eq!(store.submission_key_policy, "required", "{store:?}");
    assert_eq!(store.task_guard_policy, "required", "{store:?}");

    let name = fresh_queue("life");
    let uuid = queue
        .submit(&name, b"batch".to_vec(), 3)
        .expect("a keyed submission");
    let claimed = queue.claim(&name, 30).unwrap().expect("the task");
    assert_eq!(claimed.uuid, uuid);
    assert_eq!(claimed.priority, 3);
    assert!(claimed.revision > 0);

    // Park it, as a retry does, with a new payload. The sweep returns it.
    queue
        .park(&claimed, 1, Some(b"attempt 2".to_vec()))
        .expect("a guarded park");
    let parked = queue
        .counts()
        .unwrap()
        .into_iter()
        .find(|held| held.queue == name)
        .expect("the queue is counted");
    assert_eq!(parked.in_state(STATE_BACKOFF), 1);
    // The parked task's timeout is one second, and the sweep is told the time
    // rather than waiting for it.
    let later = tallyowl_obs::time::now_nanos() + 5_000_000_000;
    assert_eq!(queue.sweep(&name, later).unwrap(), 1);

    let again = queue.claim(&name, 30).unwrap().expect("the task again");
    assert_eq!(again.payload, b"attempt 2");
    assert_eq!(again.priority, 3, "the priority survived the park");

    // The first claim ended when the task was parked. Its holder changes
    // nothing now.
    let stale = queue.complete(&claimed).expect_err("a stale claim");
    assert_eq!(
        stale.code,
        ErrorCode::FailedPrecondition,
        "{}",
        stale.message
    );
    assert!(!stale.retryable);

    queue.complete(&again).expect("the current claim completes");
    assert_eq!(queue.claim(&name, 30).unwrap(), None);
}

#[test]
fn a_claim_that_timed_out_cannot_complete_after_another_worker_took_the_task() {
    let Some(addr) = server() else { return };
    if std::env::var("CORNDOGS_TEST_OLD").is_ok() {
        return;
    }
    let queue = CorndogsQueue::connect(&addr).unwrap();
    let name = fresh_queue("stale");
    queue.submit(&name, b"x".to_vec(), 0).unwrap();
    let slow = queue.claim(&name, 1).unwrap().expect("a claim");
    let later = tallyowl_obs::time::now_nanos() + 5_000_000_000;
    assert_eq!(queue.sweep(&name, later).unwrap(), 1, "the claim timed out");
    let second = queue.claim(&name, 30).unwrap().expect("another worker");

    assert!(
        queue.complete(&slow).is_err(),
        "the slow worker changed nothing"
    );
    assert!(queue.quarantine(&slow, "quarantine").is_err());
    queue.complete(&second).expect("the second claim stands");
}

#[test]
fn a_server_older_than_0_8_0_is_refused_when_the_queue_connects() {
    let Some(addr) = server() else { return };
    if std::env::var("CORNDOGS_TEST_OLD").is_err() {
        return;
    }
    let refused = CorndogsQueue::connect(&addr)
        .err()
        .expect("an old server is refused");
    assert_eq!(
        refused.code,
        ErrorCode::SchemaUnsupported,
        "{}",
        refused.message
    );
    assert!(refused.message.contains("0.8.0"), "{}", refused.message);
}

/// A task that an older release wrote without a guard, found after the server
/// changed to `required`. Write one first with the Corndogs command line while
/// the server is in `compatibility`, then restart it in `required`:
///
/// ```sh
/// corndogs submit-task -p 5080 -q legacy -c queued -t sending -o -1 -l old
/// CORNDOGS_TEST_ADDR=127.0.0.1:5080 CORNDOGS_TEST_LEGACY_QUEUE=legacy \
///   cargo test -p tallyowl-queue --test live
/// ```
#[test]
fn a_task_from_an_older_release_is_claimed_and_finished() {
    let Some(addr) = server() else { return };
    let Ok(name) = std::env::var("CORNDOGS_TEST_LEGACY_QUEUE") else {
        return;
    };
    let queue = CorndogsQueue::connect(&addr).unwrap();
    let claimed = queue
        .claim(&name, 30)
        .unwrap()
        .expect("the unguarded task is claimed by a guarded claim");
    assert_eq!(claimed.payload, b"old");
    queue.complete(&claimed).expect("and completed");
    assert_eq!(queue.claim(&name, 30).unwrap(), None);
}
