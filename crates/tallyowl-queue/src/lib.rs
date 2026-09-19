//! The durable task boundary: the one place TallyOwl reaches Corndogs.
//!
//! It is its own crate because more than one component needs it. A collector
//! puts a batch here; the head puts an alert evaluation, a notification, and
//! every projector workflow here. A second adapter would be a second set of
//! rules about retry and quarantine, and the two would go out of step. L137.
//!
//! Corndogs owns durable queue and workflow state. Once it accepts a telemetry
//! task, that task remains until final TallyOwl storage returns a committed
//! receipt. See D4.
//!
//! Two rules from `AGENTS.md` shape this module:
//!
//! - **the collector holds no durable state of its own.** The batch payload
//!   travels inside the Corndogs task. Corndogs stores a payload in its own
//!   bucket, so a large payload does not slow the timeout sweep;
//! - **Corndogs evaluates a task timeout only when a caller invokes
//!   `CleanUpTimedOut`.** The forwarder owns that sweep. Retry, backoff, and
//!   dead-worker recovery all stop when the sweep stops.
//!
//! The trait exists so a failure test can drive the collector against a queue
//! that refuses, stalls, or loses an acknowledgement, without a mock of
//! TallyOwl's own storage. Corndogs is another product's boundary, not
//! TallyOwl's, so a test double here is a stand-in for a dependency rather than
//! a mock of the thing under test.

use std::sync::Mutex;

use corndogs::transport::Transport;
use corndogs::{
    CleanUpTimedOutRequest, CompleteTaskRequest, CorndogsClient, GetNextTaskRequest,
    GetQueueAndStateCountsRequest, SubmitTaskRequest, UpdateTaskRequest,
};
use tallyowl_obs::error::TallyOwlError;

/// The states a delivery task moves through. `docs/DELIVERY.md` section 4 draws
/// the whole diagram; these are the names on it.
pub const STATE_QUEUED: &str = "queued";
pub const STATE_SENDING: &str = "sending";
pub const STATE_BACKOFF: &str = "backoff";

/// One claimed task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedTask {
    pub uuid: String,
    pub payload: Vec<u8>,
    pub queue: String,
    /// The state the task is in now, which is the state a completion must name.
    pub current_state: String,
}

/// The durable queue a collector uses.
pub trait DurableQueue: Send + Sync {
    /// Accept a batch durably. This returns only after the queue has stored it,
    /// because the collector acknowledgement rests on it.
    fn submit(&self, queue: &str, payload: Vec<u8>, priority: i64)
        -> Result<String, TallyOwlError>;

    /// Claim the next queued task, when one is waiting.
    fn claim(
        &self,
        queue: &str,
        timeout_seconds: i64,
    ) -> Result<Option<ClaimedTask>, TallyOwlError>;

    /// Finish a task. The batch reached final storage.
    fn complete(&self, task: &ClaimedTask) -> Result<(), TallyOwlError>;

    /// Park a task until a delay expires, and name the state it returns to.
    ///
    /// This is the whole of TallyOwl's backoff. No worker holds a claim during
    /// the wait and no component enumerates tasks. See D33.
    ///
    /// `payload` replaces the stored payload when it is present. That is how
    /// the attempt count survives the wait: Corndogs holds no count, so
    /// TallyOwl writes its own back with the task. See DELIVERY.md section 4.
    fn park(
        &self,
        task: &ClaimedTask,
        delay_seconds: i64,
        payload: Option<Vec<u8>>,
    ) -> Result<(), TallyOwlError>;

    /// Move a task to a queue it will not be retried from.
    fn quarantine(&self, task: &ClaimedTask, queue: &str) -> Result<(), TallyOwlError>;

    /// Return every expired task to its ready state, and say how many moved.
    ///
    /// Nothing happens without this call. A forwarder that stops calling it
    /// stops retry and dead-worker recovery at the same time, which is why
    /// readiness is tied to it.
    fn sweep(&self, queue: &str, at_nanos: i64) -> Result<i64, TallyOwlError>;

    /// How many tasks each queue holds, by the state they are in.
    ///
    /// **The queue is the one that knows.** A component that counted its own
    /// submissions would report zero after a restart and would count only its
    /// own share when a second one is running, and both would read as "there is
    /// no work" rather than as "this number is not the truth".
    fn counts(&self) -> Result<Vec<QueueCounts>, TallyOwlError>;
}

/// What one queue holds, by task state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueCounts {
    pub queue: String,
    pub total: i64,
    /// State name to how many tasks are in it. The names are the caller's own:
    /// this crate carries them and never interprets them.
    pub by_state: std::collections::BTreeMap<String, i64>,
}

impl QueueCounts {
    pub fn in_state(&self, state: &str) -> i64 {
        self.by_state.get(state).copied().unwrap_or(0)
    }
}

/// The real Corndogs queue.
pub struct CorndogsQueue {
    client: Mutex<CorndogsClient<Transport>>,
    endpoint: String,
}

impl CorndogsQueue {
    pub fn connect(endpoint: &str) -> Result<CorndogsQueue, TallyOwlError> {
        let transport = Transport::connect(endpoint).map_err(|e| {
            TallyOwlError::unavailable(format!(
                "We could not reach the durable store at {endpoint}. {e}"
            ))
        })?;
        Ok(CorndogsQueue {
            client: Mutex::new(CorndogsClient::new(transport)),
            endpoint: endpoint.to_string(),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn unavailable(&self, what: &str, error: impl std::fmt::Display) -> TallyOwlError {
        TallyOwlError::unavailable(format!(
            "We could not reach the durable store at {} to {what}. {error}",
            self.endpoint
        ))
    }
}

impl DurableQueue for CorndogsQueue {
    fn submit(
        &self,
        queue: &str,
        payload: Vec<u8>,
        priority: i64,
    ) -> Result<String, TallyOwlError> {
        let client = self.client.lock().expect("queue lock");
        let response = client
            .submit_task(SubmitTaskRequest {
                queue: queue.to_string(),
                current_state: STATE_QUEUED.to_string(),
                auto_target_state: STATE_SENDING.to_string(),
                // A negative value means no timeout. Zero would mean "use the
                // Corndogs default", which is not what a queued task wants: a
                // queued task waits for a worker rather than for a clock.
                timeout: -1,
                payload,
                priority,
            })
            .map_err(|e| self.unavailable("accept a batch", e))?;
        response.task.map(|t| t.uuid).ok_or_else(|| {
            TallyOwlError::internal("The durable store accepted a batch and returned no task.")
        })
    }

    fn claim(
        &self,
        queue: &str,
        timeout_seconds: i64,
    ) -> Result<Option<ClaimedTask>, TallyOwlError> {
        let client = self.client.lock().expect("queue lock");
        let response = client
            .get_next_task(GetNextTaskRequest {
                queue: queue.to_string(),
                current_state: STATE_QUEUED.to_string(),
                // The claim carries a timeout, so a worker that dies mid-send
                // releases the task at the next sweep rather than holding it.
                override_timeout: timeout_seconds,
                override_current_state: String::new(),
                override_auto_target_state: STATE_QUEUED.to_string(),
            })
            .map_err(|e| self.unavailable("claim a batch", e))?;
        Ok(response.delivery.map(|d| ClaimedTask {
            uuid: d.task.uuid,
            payload: d.payload,
            queue: d.task.queue,
            current_state: d.task.current_state,
        }))
    }

    fn complete(&self, task: &ClaimedTask) -> Result<(), TallyOwlError> {
        let client = self.client.lock().expect("queue lock");
        client
            .complete_task(CompleteTaskRequest {
                uuid: task.uuid.clone(),
                queue: task.queue.clone(),
                current_state: task.current_state.clone(),
            })
            .map_err(|e| self.unavailable("finish a batch", e))?;
        Ok(())
    }

    fn park(
        &self,
        task: &ClaimedTask,
        delay_seconds: i64,
        payload: Option<Vec<u8>>,
    ) -> Result<(), TallyOwlError> {
        let client = self.client.lock().expect("queue lock");
        client
            .update_task(UpdateTaskRequest {
                uuid: task.uuid.clone(),
                queue: task.queue.clone(),
                current_state: task.current_state.clone(),
                // The sweep swaps these back when the timeout expires, which is
                // how a delay is expressed without a polling loop.
                new_state: STATE_BACKOFF.to_string(),
                auto_target_state: STATE_QUEUED.to_string(),
                timeout: delay_seconds.max(1),
                payload,
                priority: 0,
            })
            .map_err(|e| self.unavailable("delay a retry", e))?;
        Ok(())
    }

    fn quarantine(&self, task: &ClaimedTask, queue: &str) -> Result<(), TallyOwlError> {
        let client = self.client.lock().expect("queue lock");
        client
            .update_task(UpdateTaskRequest {
                uuid: task.uuid.clone(),
                queue: task.queue.clone(),
                current_state: task.current_state.clone(),
                new_state: format!("quarantined-into-{queue}"),
                auto_target_state: String::new(),
                timeout: -1,
                payload: None,
                priority: 0,
            })
            .map_err(|e| self.unavailable("quarantine a batch", e))?;
        Ok(())
    }

    fn sweep(&self, queue: &str, at_nanos: i64) -> Result<i64, TallyOwlError> {
        let client = self.client.lock().expect("queue lock");
        let response = client
            .clean_up_timed_out(CleanUpTimedOutRequest {
                at_time: at_nanos,
                queue: queue.to_string(),
            })
            .map_err(|e| self.unavailable("return expired batches for retry", e))?;
        Ok(response.timed_out)
    }

    fn counts(&self) -> Result<Vec<QueueCounts>, TallyOwlError> {
        let client = self.client.lock().expect("queue lock");
        let response = client
            .get_queue_and_state_counts(GetQueueAndStateCountsRequest {})
            .map_err(|e| self.unavailable("count what is waiting", e))?;
        let mut out: Vec<QueueCounts> = response
            .queue_and_state_counts
            .into_values()
            .map(|held| QueueCounts {
                queue: held.queue,
                total: held.count,
                by_state: held.state_counts.into_iter().collect(),
            })
            .collect();
        // A map has no order and an operator interface does. Sorting here means
        // two reads of one unchanged installation draw the same table.
        out.sort_by(|left, right| left.queue.cmp(&right.queue));
        Ok(out)
    }
}

pub mod testing {
    //! An in-memory stand-in for Corndogs.
    //!
    //! This is a double for another product's durability boundary, not for
    //! TallyOwl's own storage interface. `AGENTS.md` forbids the second and this
    //! is the first: it lets a test kill a worker, lose an acknowledgement, or
    //! refuse a submission without running a second process.

    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    /// Where a quarantined task lands. The caller names its own queue and this
    /// double has to agree with it, so the name is one constant.
    pub const QUARANTINE_QUEUE_NAME: &str = "tallyowl-workflow-quarantine";

    #[derive(Default)]
    struct Inner {
        /// Which queue each task was submitted to, so `counts` can group.
        task_queue: std::collections::BTreeMap<String, String>,
        queued: VecDeque<(String, Vec<u8>)>,
        claimed: Vec<(String, Vec<u8>)>,
        parked: Vec<(String, Vec<u8>)>,
        completed: Vec<String>,
        quarantined: Vec<String>,
    }

    #[derive(Default)]
    pub struct FakeQueue {
        inner: Mutex<Inner>,
        next_id: AtomicU64,
        pub refusing: AtomicBool,
        pub sweeps: AtomicU64,
    }

    impl FakeQueue {
        pub fn new() -> Arc<FakeQueue> {
            Arc::new(FakeQueue::default())
        }

        pub fn refuse(&self, refusing: bool) {
            self.refusing.store(refusing, Ordering::Relaxed);
        }

        pub fn depth(&self) -> usize {
            self.inner.lock().unwrap().queued.len()
        }

        /// The payload of the first task still waiting, without claiming it.
        ///
        /// A test that wants to see what actually reached the durable store
        /// needs the bytes rather than the count: a policy that removed a
        /// property is only proved by the property not being there.
        pub fn first(&self) -> Option<Vec<u8>> {
            self.inner
                .lock()
                .unwrap()
                .queued
                .front()
                .map(|(_, payload)| payload.clone())
        }

        pub fn completed(&self) -> Vec<String> {
            self.inner.lock().unwrap().completed.clone()
        }

        pub fn quarantined(&self) -> Vec<String> {
            self.inner.lock().unwrap().quarantined.clone()
        }

        pub fn parked_count(&self) -> usize {
            self.inner.lock().unwrap().parked.len()
        }

        pub fn sweep_count(&self) -> u64 {
            self.sweeps.load(Ordering::Relaxed)
        }

        /// How many tasks are in quarantine.
        pub fn quarantined_count(&self) -> usize {
            self.inner.lock().unwrap().quarantined.len()
        }

        /// Return every parked task to the queue, as the sweep does when a
        /// backoff timeout expires.
        pub fn release_parked(&self) {
            let mut inner = self.inner.lock().unwrap();
            let parked = std::mem::take(&mut inner.parked);
            for entry in parked {
                inner.queued.push_back(entry);
            }
        }

        /// Return every claimed task to the queue, as the sweep does when a
        /// worker died holding a claim. It is the same act as
        /// [`FakeQueue::release_claimed`], named for what a caller is proving.
        pub fn expire_claims(&self) {
            self.release_claimed();
        }

        /// Return every claimed task to the queue, as a real sweep does after a
        /// worker dies without finishing.
        pub fn release_claimed(&self) {
            let mut inner = self.inner.lock().unwrap();
            let claimed = std::mem::take(&mut inner.claimed);
            for entry in claimed {
                inner.queued.push_back(entry);
            }
        }
    }

    impl DurableQueue for FakeQueue {
        fn submit(
            &self,
            queue: &str,
            payload: Vec<u8>,
            _priority: i64,
        ) -> Result<String, TallyOwlError> {
            if self.refusing.load(Ordering::Relaxed) {
                return Err(TallyOwlError::unavailable(
                    "We could not reach the durable store.",
                ));
            }
            let uuid = format!("task-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
            let mut inner = self.inner.lock().unwrap();
            inner.task_queue.insert(uuid.clone(), queue.to_string());
            inner.queued.push_back((uuid.clone(), payload));
            Ok(uuid)
        }

        fn claim(
            &self,
            queue: &str,
            _timeout_seconds: i64,
        ) -> Result<Option<ClaimedTask>, TallyOwlError> {
            if self.refusing.load(Ordering::Relaxed) {
                return Err(TallyOwlError::unavailable(
                    "We could not reach the durable store.",
                ));
            }
            let mut inner = self.inner.lock().unwrap();
            let Some((uuid, payload)) = inner.queued.pop_front() else {
                return Ok(None);
            };
            inner.claimed.push((uuid.clone(), payload.clone()));
            Ok(Some(ClaimedTask {
                uuid,
                payload,
                queue: queue.to_string(),
                current_state: STATE_SENDING.to_string(),
            }))
        }

        fn complete(&self, task: &ClaimedTask) -> Result<(), TallyOwlError> {
            let mut inner = self.inner.lock().unwrap();
            inner.claimed.retain(|(uuid, _)| uuid != &task.uuid);
            inner.completed.push(task.uuid.clone());
            Ok(())
        }

        fn park(
            &self,
            task: &ClaimedTask,
            _delay_seconds: i64,
            payload: Option<Vec<u8>>,
        ) -> Result<(), TallyOwlError> {
            let mut inner = self.inner.lock().unwrap();
            if let Some(position) = inner.claimed.iter().position(|(u, _)| u == &task.uuid) {
                let (uuid, held) = inner.claimed.remove(position);
                // A replaced payload is how the attempt count survives the
                // wait. A double that dropped it would make every retry look
                // like a first attempt, which is the defect this replaces.
                inner.parked.push((uuid, payload.unwrap_or(held)));
            }
            Ok(())
        }

        fn quarantine(&self, task: &ClaimedTask, _queue: &str) -> Result<(), TallyOwlError> {
            let mut inner = self.inner.lock().unwrap();
            inner.claimed.retain(|(uuid, _)| uuid != &task.uuid);
            inner.quarantined.push(task.uuid.clone());
            Ok(())
        }

        /// Counts, grouped by the queue each task was submitted to.
        ///
        /// The real one answers for every queue at once, so this does too. A
        /// double that reported one queue would let a caller pass a test by
        /// looking at the wrong number.
        fn counts(&self) -> Result<Vec<QueueCounts>, TallyOwlError> {
            let inner = self.inner.lock().unwrap();
            let mut out: std::collections::BTreeMap<String, QueueCounts> =
                std::collections::BTreeMap::new();
            let mut add = |queue: &str, state: &str| {
                let held = out.entry(queue.to_string()).or_insert_with(|| QueueCounts {
                    queue: queue.to_string(),
                    ..QueueCounts::default()
                });
                *held.by_state.entry(state.to_string()).or_insert(0) += 1;
                held.total += 1;
            };
            let queue_of = |uuid: &String| {
                inner
                    .task_queue
                    .get(uuid)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string())
            };
            for (uuid, _) in &inner.queued {
                add(&queue_of(uuid), STATE_QUEUED);
            }
            for (uuid, _) in &inner.claimed {
                add(&queue_of(uuid), STATE_SENDING);
            }
            for (uuid, _) in &inner.parked {
                add(&queue_of(uuid), STATE_BACKOFF);
            }
            for uuid in &inner.quarantined {
                // A quarantined task is in the quarantine queue, which is where
                // an operator looks for it.
                let _ = uuid;
                add(QUARANTINE_QUEUE_NAME, STATE_QUEUED);
            }
            Ok(out.into_values().collect())
        }

        fn sweep(&self, _queue: &str, _at_nanos: i64) -> Result<i64, TallyOwlError> {
            if self.refusing.load(Ordering::Relaxed) {
                return Err(TallyOwlError::unavailable(
                    "We could not reach the durable store.",
                ));
            }
            self.sweeps.fetch_add(1, Ordering::Relaxed);
            let mut inner = self.inner.lock().unwrap();
            let parked = std::mem::take(&mut inner.parked);
            let moved = parked.len() as i64;
            for entry in parked {
                inner.queued.push_back(entry);
            }
            Ok(moved)
        }
    }
}
