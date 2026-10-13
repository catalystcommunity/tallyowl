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

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use corndogs::tls::TlsOptions;
use corndogs::transport::{ConnectOptions, Transport};
use corndogs::{
    ClaimGuardedTaskRequest, CleanUpTimedOutRequest, ClientError, CompleteGuardedTaskRequest,
    CorndogsClient, GetQueueAndStateCountsRequest, GetServerInfoRequest, SubmitKeyedTaskRequest,
    UpdateGuardedTaskRequest,
};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_obs::metrics::{labels, MetricKind, Registry};

mod pool;

use pool::{Pool, PoolError};

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
    /// The priority the task was accepted with.
    ///
    /// An update names no priority, so Corndogs keeps this one. Before Corndogs
    /// `23caaf1` an update replaced it with whatever it named, and an update
    /// that named zero moved a retried conversion behind every ordinary event.
    pub priority: i64,
    /// The revision of the task when this worker claimed it.
    ///
    /// Every change names it. When the claim times out, the sweep returns the
    /// task and the revision moves on, so a slow worker that finishes late
    /// changes nothing and another worker's claim stands. Corndogs 0.8.0.
    pub revision: i64,
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

/// How a [`CorndogsQueue`] reaches the durable store.
#[derive(Clone)]
pub struct QueueOptions {
    /// How many connections the queue keeps. Corndogs coalesces commits across
    /// connections, so one connection is the slowest way to use it.
    pub connections: usize,
    /// How long one call may take before the caller is told the durable store
    /// did not answer.
    pub call_timeout: Duration,
    /// Where call latency is published, when a host wants it.
    pub metrics: Option<Arc<Registry>>,
    /// TLS to the durable store. `None` is plaintext, which D62 permits on a
    /// loopback endpoint or under `transport.allowPlaintext`. See
    /// [`QueueTls::for_endpoint`].
    pub tls: Option<QueueTls>,
}

impl Default for QueueOptions {
    fn default() -> QueueOptions {
        QueueOptions {
            connections: DEFAULT_CONNECTIONS,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            metrics: None,
            tls: None,
        }
    }
}

/// How the queue checks the durable store over TLS. The store shows a
/// certificate; the queue shows none. Its project key is not on this hop.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueueTls {
    /// A PEM file of the authorities that sign the store's certificate. `None`
    /// is the operating system's trusted authorities.
    pub ca_file: Option<std::path::PathBuf>,
    /// The name the store's certificate must carry. `None` is the host part of
    /// the endpoint.
    pub server_name: Option<String>,
}

impl QueueTls {
    /// D62 for the hop to the durable store: plaintext to a loopback endpoint,
    /// TLS to any other, and plaintext elsewhere only when the operator set
    /// `transport.allowPlaintext`.
    ///
    /// A configured CA file always means TLS, even to a loopback endpoint and
    /// even under `transport.allowPlaintext`. A Corndogs that serves TLS serves
    /// it on its one RPC port, so the head's own sidecar on loopback needs it
    /// too, and the setting never makes the hop less secure than its
    /// configuration can be.
    pub fn for_endpoint(
        endpoint: &str,
        ca_file: &str,
        server_name: &str,
        allow_plaintext: bool,
    ) -> Option<QueueTls> {
        if ca_file.is_empty() && (is_loopback(endpoint) || allow_plaintext) {
            return None;
        }
        Some(QueueTls {
            ca_file: (!ca_file.is_empty()).then(|| std::path::PathBuf::from(ca_file)),
            server_name: (!server_name.is_empty()).then(|| server_name.to_string()),
        })
    }

    fn options(&self) -> TlsOptions {
        let options = match &self.ca_file {
            Some(path) => TlsOptions::ca_file(path.clone()),
            None => TlsOptions::system_roots(),
        };
        match &self.server_name {
            Some(name) => options.server_name(name.clone()),
            None => options,
        }
    }
}

/// Whether `host:port` names this host.
fn is_loopback(endpoint: &str) -> bool {
    let host = endpoint
        .rsplit_once(':')
        .map_or(endpoint, |(host, _)| host)
        .trim_start_matches('[')
        .trim_end_matches(']');
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Eight matches the number of requests one intake connection serves at a time.
pub const DEFAULT_CONNECTIONS: usize = 8;
/// Shorter than the forwarder's claim timeout would be wrong the other way: a
/// slow durable write is not a dead store. Thirty seconds is the same bound the
/// RPC client uses.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

const CALL_SECONDS: &str = "tallyowl_queue_call_seconds";
const CALL_FAILURES: &str = "tallyowl_queue_call_failures_total";
const CALL_RESENDS: &str = "tallyowl_queue_call_resends_total";

/// The Corndogs client says "timed out" when its deadline ends a call.
fn timed_out(reason: &str) -> bool {
    reason.contains("timed out")
}

/// The Corndogs features this crate uses. A server that does not list both is
/// refused when the queue connects. Corndogs release 0.8.0 added them.
pub const REQUIRED_FEATURES: [&str; 2] = ["submission-keys-v1", "task-guards-v1"];

/// How many times one keyed or guarded request goes out. The Corndogs Go client
/// uses the same number.
const MAX_SENDS: u32 = 3;

/// Corndogs answers an operation it does not know with transport status 2. A
/// server older than 0.8.0 answers every keyed and guarded operation so.
fn unknown_operation(reason: &str) -> bool {
    reason.contains("transport status 2:")
}

/// Corndogs ServiceError codes that mean the claim this worker held has ended:
/// 5 is a revision or state conflict, 7 a task that is gone, and 8 a replayed
/// claim that no longer holds its task. See Corndogs `docs/resilience.md`.
fn claim_ended(code: i64) -> bool {
    matches!(code, 5 | 7 | 8)
}

/// A random identifier for one logical request: a submission key or an
/// operation ID. It is made once and sent again unchanged on each resend.
fn request_id(kind: &str) -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the system random source");
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("tallyowl-{kind}-{hex}")
}

/// What carries the bytes of one connection.
///
/// A real connection in service, and a scripted one in a test that has to lose
/// a reply after the store applied the request.
pub(crate) struct Carrier(Box<dyn corndogs::Transport + Send + Sync>);

impl corndogs::Transport for Carrier {
    fn call(&self, service: &str, op: &str, req: &[u8]) -> Result<Vec<u8>, ClientError> {
        self.0.call(service, op, req)
    }
}

type Client = CorndogsClient<Carrier>;

/// What the store said about itself when the queue connected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreInfo {
    pub version: String,
    pub submission_key_policy: String,
    pub task_guard_policy: String,
    /// How long the store keeps a submission key or an operation ID.
    pub receipt_retention: Duration,
}

/// The real Corndogs queue.
///
/// **Every request is safe to send again.** A submission carries a submission
/// key, and every claim and change carries an operation ID. When a reply is
/// lost, the queue sends the same bytes again and Corndogs returns the first
/// result. Before Corndogs 0.8.0 a lost reply to a submission left two tasks,
/// and a worker whose claim had ended could still complete the task that
/// another worker held. Every change now names the revision the claim
/// returned, so the store refuses a change from a worker whose claim ended.
pub struct CorndogsQueue {
    pool: Pool<Client>,
    endpoint: String,
    call_timeout: Duration,
    metrics: Option<Arc<Registry>>,
    secured: bool,
    store: StoreInfo,
}

impl CorndogsQueue {
    pub fn connect(endpoint: &str) -> Result<CorndogsQueue, TallyOwlError> {
        CorndogsQueue::connect_with(endpoint, QueueOptions::default())
    }

    /// Connect with the number of connections and the call deadline stated.
    ///
    /// The first connection opens here, so a durable store that cannot be
    /// reached fails the start. The rest open when load first needs them.
    pub fn connect_with(
        endpoint: &str,
        options: QueueOptions,
    ) -> Result<CorndogsQueue, TallyOwlError> {
        // The client's deadline covers the whole call: a re-dial, the write,
        // and the full read. A connect that takes longer than a call may is not
        // useful, so the dial takes the shorter of the two.
        let connect = ConnectOptions::new()
            .connect_timeout(options.call_timeout.min(Duration::from_secs(5)))
            .io_timeout(options.call_timeout);
        let connect = match &options.tls {
            Some(tls) => connect.tls(tls.options()),
            None => connect,
        };
        let secured = options.tls.is_some();
        let first = Transport::connect_options(endpoint, connect.clone()).map_err(|e| {
            TallyOwlError::unavailable(format!(
                "We could not reach the durable store at {endpoint}{}. {e}",
                if secured { " over TLS" } else { "" }
            ))
        })?;
        let address = endpoint.to_string();
        CorndogsQueue::from_carriers(
            endpoint,
            options,
            secured,
            Carrier(Box::new(first)),
            Box::new(move || {
                Transport::connect_options(address.clone(), connect.clone())
                    .map(|transport| CorndogsClient::new(Carrier(Box::new(transport))))
                    .map_err(|e| e.to_string())
            }),
        )
    }

    /// Ask the store what it is, refuse one that lacks a feature this crate
    /// uses, and build the queue around the first connection.
    pub(crate) fn from_carriers(
        endpoint: &str,
        options: QueueOptions,
        secured: bool,
        first: Carrier,
        more: pool::Factory<Client>,
    ) -> Result<CorndogsQueue, TallyOwlError> {
        let first = CorndogsClient::new(first);
        let info = first
            .get_server_info(GetServerInfoRequest {})
            .map_err(|e| match e {
                ClientError::Transport(reason) if unknown_operation(&reason) => {
                    too_old(endpoint, "a release older than 0.8.0")
                }
                other => TallyOwlError::unavailable(format!(
                    "We could not ask the durable store at {endpoint} what it is. {other}"
                )),
            })?;
        if let Some(missing) = REQUIRED_FEATURES
            .iter()
            .find(|feature| !info.features.iter().any(|held| held == *feature))
        {
            return Err(too_old(
                endpoint,
                &format!(
                    "release {} without the feature `{missing}`",
                    info.server_version
                ),
            ));
        }
        Ok(CorndogsQueue {
            pool: Pool::new(options.connections, options.call_timeout, Some(first), more),
            endpoint: endpoint.to_string(),
            call_timeout: options.call_timeout,
            metrics: options.metrics,
            secured,
            store: StoreInfo {
                version: info.server_version,
                submission_key_policy: info.submission_key_policy,
                task_guard_policy: info.task_guard_policy,
                receipt_retention: Duration::from_secs(info.receipt_retention_seconds.max(0) as u64),
            },
        })
    }

    /// Whether this queue reaches the durable store over TLS.
    pub fn secured(&self) -> bool {
        self.secured
    }

    /// What the store said about itself when the queue connected.
    pub fn store(&self) -> &StoreInfo {
        &self.store
    }

    /// Declare what this queue publishes. A host calls it once for a registry
    /// it then passes in [`QueueOptions`].
    pub fn declare_metrics(metrics: &Registry) {
        for (name, kind, help, buckets) in [
            (
                CALL_SECONDS,
                MetricKind::Histogram,
                "How long one call to the durable store took, by operation.",
                &[0.001, 0.005, 0.025, 0.1, 0.5, 2.0, 10.0, 30.0][..],
            ),
            (
                CALL_FAILURES,
                MetricKind::Counter,
                "Calls to the durable store that produced no answer, by operation and reason.",
                &[][..],
            ),
            (
                CALL_RESENDS,
                MetricKind::Counter,
                "Requests sent to the durable store again after a lost reply, by operation. The store answers a resend with the first result.",
                &[][..],
            ),
        ] {
            metrics
                .declare(name, kind, help, buckets)
                .unwrap_or_else(|e| {
                    panic!("the metric `{name}` is not a name the registry accepts: {}", e.0)
                });
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// How long a request may keep being sent again. A resend after the store
    /// forgot the first one would run it twice, so the window is half of what
    /// the store keeps.
    fn resend_window(&self) -> Duration {
        self.store.receipt_retention / 2
    }

    /// Run one call on a pooled connection, inside the deadline, and send it
    /// again when its reply was lost.
    ///
    /// `op` is the metric label and `what` finishes the sentence a person reads.
    /// `call` builds the same request each time it runs, so a resend carries
    /// the same submission key or operation ID and the same bytes.
    fn call<R, F>(&self, op: &str, what: &str, call: F) -> Result<R, TallyOwlError>
    where
        F: Fn(&Client) -> Result<R, ClientError>,
    {
        let started = Instant::now();
        let mut sends = 0;
        let outcome = loop {
            sends += 1;
            let outcome = match self.pool.run(|client| Ok(call(client)), |_| false) {
                Ok(answer) => answer.map_err(Failure::Client),
                Err(PoolError::Busy) => Err(Failure::Busy),
                Err(PoolError::TimedOut) => unreachable!("the closure never fails"),
                Err(PoolError::Failed(reason)) => Err(Failure::Dial(reason)),
            };
            // **Only a lost reply is sent again.** The request reached the
            // store or it did not, and the same key makes either case one
            // result. A ServiceError is the store's answer, an unknown
            // operation is a store too old to know it, and a dial that failed
            // sent nothing.
            let resend = matches!(
                &outcome,
                Err(Failure::Client(ClientError::Transport(reason))) if !unknown_operation(reason)
            ) && sends < MAX_SENDS
                && started.elapsed() + self.call_timeout < self.resend_window();
            if !resend {
                break outcome;
            }
            if let Some(metrics) = &self.metrics {
                metrics.increment(CALL_RESENDS, &labels(&[("op", op)]));
            }
        };
        if let Some(metrics) = &self.metrics {
            metrics.observe(
                CALL_SECONDS,
                &labels(&[("op", op)]),
                started.elapsed().as_secs_f64(),
            );
            if let Err(failure) = &outcome {
                metrics.increment(
                    CALL_FAILURES,
                    &labels(&[("op", op), ("reason", failure.label())]),
                );
            }
        }
        outcome.map_err(|failure| self.explain(failure, what))
    }

    /// The sentence a person reads for a call that produced no result.
    fn explain(&self, failure: Failure, what: &str) -> TallyOwlError {
        let endpoint = &self.endpoint;
        match failure {
            Failure::Client(ClientError::Service { code, message }) if claim_ended(code) => {
                TallyOwlError::failed_precondition(format!(
                    "The durable store did not {what}, because this worker's claim on the task had ended. The task timed out and went back to the queue, so another attempt runs it. The store said: {message}"
                ))
                .retryable(false)
            }
            Failure::Client(ClientError::Service { code, message }) => TallyOwlError::internal(
                format!(
                    "The durable store at {endpoint} refused to {what} with code {code}: {message}. This is a defect in TallyOwl, or a Corndogs policy that TallyOwl does not meet."
                ),
            ),
            Failure::Client(ClientError::Transport(reason)) if unknown_operation(&reason) => {
                too_old(endpoint, "a release older than 0.8.0")
            }
            Failure::Client(ClientError::Transport(reason)) if timed_out(&reason) => {
                TallyOwlError::unavailable(format!(
                    "We could not reach the durable store at {endpoint} to {what}. It did not answer within {} ms.",
                    self.call_timeout.as_millis()
                ))
            }
            Failure::Client(ClientError::Transport(reason)) | Failure::Dial(reason) => {
                TallyOwlError::unavailable(format!(
                    "We could not reach the durable store at {endpoint} to {what}. {reason}"
                ))
            }
            Failure::Busy => TallyOwlError::unavailable(format!(
                "We could not reach the durable store at {endpoint} to {what}. Every connection to it stayed busy for {} ms.",
                self.call_timeout.as_millis()
            )),
        }
    }
}

/// Why one call produced no result.
enum Failure {
    /// Every connection was in use for the whole wait.
    Busy,
    /// A connection could not be opened, so nothing was sent.
    Dial(String),
    /// The client's own failure, or the store's answer.
    Client(ClientError),
}

impl Failure {
    fn label(&self) -> &'static str {
        match self {
            Failure::Busy => "busy",
            Failure::Dial(_) => "failed",
            Failure::Client(ClientError::Service { code, .. }) if claim_ended(*code) => {
                "claim-ended"
            }
            Failure::Client(ClientError::Service { .. }) => "refused",
            Failure::Client(ClientError::Transport(reason)) if timed_out(reason) => "timed-out",
            Failure::Client(ClientError::Transport(_)) => "failed",
        }
    }
}

fn too_old(endpoint: &str, what: &str) -> TallyOwlError {
    TallyOwlError::new(
        ErrorCode::SchemaUnsupported,
        format!(
            "The durable store at {endpoint} is Corndogs {what}. TallyOwl needs submission keys and task guards, which Corndogs 0.8.0 added. Upgrade Corndogs to 0.8.0 or later; see docs/DEPLOYMENT.md."
        ),
    )
}

impl DurableQueue for CorndogsQueue {
    fn submit(
        &self,
        queue: &str,
        payload: Vec<u8>,
        priority: i64,
    ) -> Result<String, TallyOwlError> {
        let request = SubmitKeyedTaskRequest {
            // One key for this submission. A resend carries the same key, so a
            // lost reply leaves one task and not two.
            submission_key: request_id("submit"),
            guarded: true,
            queue: queue.to_string(),
            current_state: STATE_QUEUED.to_string(),
            auto_target_state: STATE_SENDING.to_string(),
            // A negative value means no timeout. Zero would mean "use the
            // Corndogs default", which is not what a queued task wants: a
            // queued task waits for a worker rather than for a clock.
            timeout: -1,
            payload,
            priority,
        };
        let response = self.call("submit", "accept a batch", |client| {
            client.submit_keyed_task(request.clone())
        })?;
        Ok(response.receipt.task_uuid)
    }

    fn claim(
        &self,
        queue: &str,
        timeout_seconds: i64,
    ) -> Result<Option<ClaimedTask>, TallyOwlError> {
        let request = ClaimGuardedTaskRequest {
            operation_id: request_id("claim"),
            queue: queue.to_string(),
            current_state: STATE_QUEUED.to_string(),
            // The claim carries a timeout, so a worker that dies mid-send
            // releases the task at the next sweep rather than holding it.
            override_timeout: timeout_seconds,
            override_current_state: String::new(),
            override_auto_target_state: STATE_QUEUED.to_string(),
        };
        let response = match self.call("claim", "claim a batch", |client| {
            client.claim_guarded_task(request.clone())
        }) {
            Ok(response) => response,
            // A resent claim whose task went back to the queue in between. The
            // task is not this worker's, and the next claim finds it.
            Err(e) if e.code == ErrorCode::FailedPrecondition => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(response.delivery.map(|d| ClaimedTask {
            uuid: d.task.task.uuid,
            payload: d.payload,
            queue: d.task.task.queue,
            current_state: d.task.task.current_state,
            priority: d.task.task.priority,
            revision: d.task.revision,
        }))
    }

    fn complete(&self, task: &ClaimedTask) -> Result<(), TallyOwlError> {
        let request = CompleteGuardedTaskRequest {
            operation_id: request_id("complete"),
            uuid: task.uuid.clone(),
            queue: task.queue.clone(),
            expected_revision: task.revision,
            expected_state: Some(task.current_state.clone()),
        };
        self.call("complete", "finish a batch", |client| {
            client.complete_guarded_task(request.clone())
        })?;
        Ok(())
    }

    fn park(
        &self,
        task: &ClaimedTask,
        delay_seconds: i64,
        payload: Option<Vec<u8>>,
    ) -> Result<(), TallyOwlError> {
        let request = park_request(task, delay_seconds, payload);
        self.call("park", "delay a retry", |client| {
            client.update_guarded_task(request.clone())
        })?;
        Ok(())
    }

    fn quarantine(&self, task: &ClaimedTask, queue: &str) -> Result<(), TallyOwlError> {
        let request = quarantine_request(task, queue);
        self.call("quarantine", "quarantine a batch", |client| {
            client.update_guarded_task(request.clone())
        })?;
        Ok(())
    }

    fn sweep(&self, queue: &str, at_nanos: i64) -> Result<i64, TallyOwlError> {
        let request = CleanUpTimedOutRequest {
            at_time: at_nanos,
            queue: queue.to_string(),
        };
        // A sweep is safe to run again, so a lost reply needs no key.
        let response = self.call("sweep", "return expired batches for retry", |client| {
            client.clean_up_timed_out(request.clone())
        })?;
        Ok(response.timed_out)
    }

    fn counts(&self) -> Result<Vec<QueueCounts>, TallyOwlError> {
        let response = self.call("counts", "count what is waiting", |client| {
            client.get_queue_and_state_counts(GetQueueAndStateCountsRequest {})
        })?;
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

/// Park a claimed task until a delay expires.
///
/// It names no priority, so Corndogs keeps the one the task was accepted with,
/// and no payload unless the caller has a new one, so Corndogs keeps the stored
/// payload. Both rules are Corndogs' own from commit `23caaf1`. It names the
/// revision of the claim, so a worker whose claim ended changes nothing.
fn park_request(
    task: &ClaimedTask,
    delay_seconds: i64,
    payload: Option<Vec<u8>>,
) -> UpdateGuardedTaskRequest {
    UpdateGuardedTaskRequest {
        operation_id: request_id("park"),
        uuid: task.uuid.clone(),
        queue: task.queue.clone(),
        expected_revision: task.revision,
        expected_state: Some(task.current_state.clone()),
        // The sweep swaps these back when the timeout expires, which is how a
        // delay is expressed without a polling loop.
        new_state: STATE_BACKOFF.to_string(),
        auto_target_state: STATE_QUEUED.to_string(),
        timeout: delay_seconds.max(1),
        payload,
        priority: None,
    }
}

/// Move a claimed task out of the retry path, keeping its priority for the
/// person who replays it.
fn quarantine_request(task: &ClaimedTask, queue: &str) -> UpdateGuardedTaskRequest {
    UpdateGuardedTaskRequest {
        operation_id: request_id("quarantine"),
        uuid: task.uuid.clone(),
        queue: task.queue.clone(),
        expected_revision: task.revision,
        expected_state: Some(task.current_state.clone()),
        new_state: format!("quarantined-into-{queue}"),
        auto_target_state: String::new(),
        timeout: -1,
        payload: None,
        priority: None,
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
        /// The priority each task holds now. An update that names no priority
        /// keeps it, which is the rule Corndogs follows from commit `23caaf1`,
        /// and `CorndogsQueue` names none.
        priority: std::collections::BTreeMap<String, i64>,
        /// The revision each task holds now. A claim and every release move it
        /// on, as Corndogs does, so a change from a worker whose claim ended
        /// is refused here too.
        revision: std::collections::BTreeMap<String, i64>,
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

        /// The priority a task holds now, wherever it is.
        pub fn priority_of(&self, uuid: &str) -> Option<i64> {
            self.inner.lock().unwrap().priority.get(uuid).copied()
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
                inner.bump(&entry.0);
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
                inner.bump(&entry.0);
                inner.queued.push_back(entry);
            }
        }
    }

    impl Inner {
        fn bump(&mut self, uuid: &str) -> i64 {
            let held = self.revision.entry(uuid.to_string()).or_insert(0);
            *held += 1;
            *held
        }

        /// Take a claimed task out of the claimed set, if the claim still
        /// holds it. The same refusal Corndogs gives, in the words
        /// `CorndogsQueue` uses.
        fn take_claimed(&mut self, task: &ClaimedTask) -> Result<Vec<u8>, TallyOwlError> {
            let holds = self.revision.get(&task.uuid) == Some(&task.revision);
            match self.claimed.iter().position(|(u, _)| u == &task.uuid) {
                Some(position) if holds => {
                    self.bump(&task.uuid);
                    Ok(self.claimed.remove(position).1)
                }
                _ => Err(TallyOwlError::failed_precondition(format!(
                    "The durable store did not change the task {}, because this worker's claim on it had ended.",
                    task.uuid
                ))
                .retryable(false)),
            }
        }
    }

    impl DurableQueue for FakeQueue {
        fn submit(
            &self,
            queue: &str,
            payload: Vec<u8>,
            priority: i64,
        ) -> Result<String, TallyOwlError> {
            if self.refusing.load(Ordering::Relaxed) {
                return Err(TallyOwlError::unavailable(
                    "We could not reach the durable store.",
                ));
            }
            let uuid = format!("task-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
            let mut inner = self.inner.lock().unwrap();
            inner.task_queue.insert(uuid.clone(), queue.to_string());
            inner.priority.insert(uuid.clone(), priority);
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
            // Highest priority first, and the oldest of those, which is the
            // order the real store claims in.
            let mut best: Option<(usize, i64)> = None;
            for (position, (uuid, _)) in inner.queued.iter().enumerate() {
                let priority = inner.priority.get(uuid).copied().unwrap_or(0);
                if best.is_none_or(|(_, held)| priority > held) {
                    best = Some((position, priority));
                }
            }
            let Some((position, priority)) = best else {
                return Ok(None);
            };
            let (uuid, payload) = inner.queued.remove(position).expect("found above");
            inner.claimed.push((uuid.clone(), payload.clone()));
            let revision = inner.bump(&uuid);
            Ok(Some(ClaimedTask {
                uuid,
                payload,
                queue: queue.to_string(),
                current_state: STATE_SENDING.to_string(),
                priority,
                revision,
            }))
        }

        fn complete(&self, task: &ClaimedTask) -> Result<(), TallyOwlError> {
            let mut inner = self.inner.lock().unwrap();
            inner.take_claimed(task)?;
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
            let held = inner.take_claimed(task)?;
            // A replaced payload is how the attempt count survives the wait. A
            // double that dropped it would make every retry look like a first
            // attempt, which is the defect this replaces.
            inner
                .parked
                .push((task.uuid.clone(), payload.unwrap_or(held)));
            Ok(())
        }

        fn quarantine(&self, task: &ClaimedTask, _queue: &str) -> Result<(), TallyOwlError> {
            let mut inner = self.inner.lock().unwrap();
            inner.take_claimed(task)?;
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
                inner.bump(&entry.0);
                inner.queued.push_back(entry);
            }
            Ok(moved)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeQueue;
    use super::*;

    #[test]
    fn a_retried_task_keeps_the_priority_it_was_accepted_with() {
        // A conversion that failed one delivery came back at priority zero,
        // behind every ordinary event accepted during the catch-up.
        let queue = FakeQueue::new();
        let conversion = queue.submit("delivery", b"conversion".to_vec(), 5).unwrap();
        let claimed = queue.claim("delivery", 30).unwrap().expect("a task");
        assert_eq!(claimed.priority, 5);
        queue.park(&claimed, 1, None).unwrap();
        assert_eq!(queue.priority_of(&conversion), Some(5));

        // An ordinary event arrives while the conversion waits out its backoff.
        queue.submit("delivery", b"event".to_vec(), 2).unwrap();
        queue.release_parked();
        let next = queue.claim("delivery", 30).unwrap().expect("a task");
        assert_eq!(next.uuid, conversion, "the retried conversion goes first");
    }

    #[test]
    fn a_quarantined_task_keeps_its_priority_for_the_person_who_replays_it() {
        let queue = FakeQueue::new();
        let uuid = queue.submit("delivery", b"x".to_vec(), 4).unwrap();
        let claimed = queue.claim("delivery", 30).unwrap().expect("a task");
        queue.quarantine(&claimed, "quarantine").unwrap();
        assert_eq!(queue.priority_of(&uuid), Some(4));
    }

    #[test]
    fn an_update_names_no_priority_so_the_store_keeps_the_one_it_holds() {
        // Before Corndogs `23caaf1`, an update replaced the stored priority
        // with whatever it named. It keeps it now when none is named, so the
        // safe request names none, and a caller cannot get it wrong.
        let task = ClaimedTask {
            uuid: "t".into(),
            payload: Vec::new(),
            queue: "delivery".into(),
            current_state: STATE_SENDING.into(),
            priority: 5,
            revision: 3,
        };
        assert_eq!(park_request(&task, 30, None).priority, None);
        assert_eq!(park_request(&task, 30, None).payload, None);
        assert_eq!(quarantine_request(&task, "quarantine").priority, None);
    }

    #[test]
    fn a_worker_whose_claim_ended_changes_nothing() {
        // The sweep returned a slow worker's task and a second worker claimed
        // it. Before Corndogs 0.8.0 the slow worker's completion still applied,
        // and the second worker's claim was lost under it.
        let queue = FakeQueue::new();
        queue.submit("delivery", b"x".to_vec(), 0).unwrap();
        let slow = queue.claim("delivery", 30).unwrap().expect("a task");
        queue.expire_claims();
        let second = queue
            .claim("delivery", 30)
            .unwrap()
            .expect("the task again");
        assert_eq!(second.uuid, slow.uuid);

        let refused = queue
            .complete(&slow)
            .expect_err("the slow worker's claim ended");
        assert_eq!(refused.code, ErrorCode::FailedPrecondition);
        assert!(!refused.retryable);
        assert!(queue.park(&slow, 1, None).is_err());
        assert!(queue.quarantine(&slow, "quarantine").is_err());

        queue.complete(&second).expect("the second claim stands");
        assert_eq!(queue.completed(), vec![second.uuid]);
    }

    /// Every request a scripted connection received: the operation and the
    /// bytes.
    type Sent = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// A connection that answers from a script and keeps every request.
    struct Scripted {
        sent: Sent,
        answer: Answer,
    }

    type Answer = Arc<dyn Fn(&str, usize) -> Result<Vec<u8>, ClientError> + Send + Sync>;

    impl corndogs::Transport for Scripted {
        fn call(&self, _service: &str, op: &str, req: &[u8]) -> Result<Vec<u8>, ClientError> {
            let mut sent = self.sent.lock().unwrap();
            sent.push((op.to_string(), req.to_vec()));
            let seen = sent.iter().filter(|(held, _)| held == op).count();
            drop(sent);
            (self.answer)(op, seen)
        }
    }

    fn server_info(features: &[&str], retention_seconds: i64) -> Vec<u8> {
        corndogs::encode_get_server_info_response(&corndogs::GetServerInfoResponse {
            server_version: "0.8.0".into(),
            features: features.iter().map(|f| f.to_string()).collect(),
            submission_key_policy: "required".into(),
            task_guard_policy: "required".into(),
            receipt_retention_seconds: retention_seconds,
        })
    }

    fn task(uuid: &str) -> corndogs::GuardedTask {
        corndogs::GuardedTask {
            task: corndogs::Task {
                uuid: uuid.into(),
                queue: "delivery".into(),
                current_state: STATE_QUEUED.into(),
                auto_target_state: STATE_SENDING.into(),
                submit_time: 0,
                update_time: 0,
                timeout: -1,
                priority: 0,
            },
            guarded: true,
            revision: 1,
            terminal: false,
        }
    }

    fn submitted(uuid: &str) -> Vec<u8> {
        corndogs::encode_submit_keyed_task_response(&corndogs::SubmitKeyedTaskResponse {
            receipt: corndogs::SubmissionReceipt {
                queue: "delivery".into(),
                submission_key: "k".into(),
                task_uuid: uuid.into(),
                accepted_at: 0,
                expires_at: 0,
                guarded: true,
            },
            replayed: false,
            task: Some(task(uuid)),
        })
    }

    const LOST: &str = "corndogs: outcome uncertain: connection reset by peer";

    /// A queue over one scripted connection. `answer` sees every operation
    /// except the server check, which reports `retention_seconds`.
    fn scripted(
        retention_seconds: i64,
        answer: impl Fn(&str, usize) -> Result<Vec<u8>, ClientError> + Send + Sync + 'static,
    ) -> (Result<CorndogsQueue, TallyOwlError>, Sent) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let carrier = Scripted {
            sent: Arc::clone(&sent),
            answer: Arc::new(move |op, seen| match op {
                "GetServerInfo" => Ok(server_info(&REQUIRED_FEATURES, retention_seconds)),
                _ => answer(op, seen),
            }),
        };
        let queue = CorndogsQueue::from_carriers(
            "corndogs.test:5080",
            QueueOptions {
                connections: 1,
                call_timeout: Duration::from_secs(5),
                metrics: None,
                tls: None,
            },
            false,
            Carrier(Box::new(carrier)),
            Box::new(|| Err("one connection only".to_string())),
        );
        (queue, sent)
    }

    fn sends_of(sent: &Mutex<Vec<(String, Vec<u8>)>>, op: &str) -> Vec<Vec<u8>> {
        sent.lock()
            .unwrap()
            .iter()
            .filter(|(held, _)| held == op)
            .map(|(_, bytes)| bytes.clone())
            .collect()
    }

    #[test]
    fn a_lost_submission_reply_is_sent_again_with_the_same_key_and_leaves_one_task() {
        // Before Corndogs 0.8.0, a lost reply left the caller two choices: a
        // retry that could make a second task, or a refusal for a batch the
        // store already held.
        let (queue, sent) = scripted(3_600, |op, seen| match (op, seen) {
            ("SubmitKeyedTask", 1) => Err(ClientError::Transport(LOST.into())),
            ("SubmitKeyedTask", _) => Ok(submitted("t-1")),
            _ => unreachable!("{op}"),
        });
        let queue = queue.expect("a 0.8.0 store");
        assert_eq!(
            queue.submit("delivery", b"batch".to_vec(), 4).unwrap(),
            "t-1"
        );

        let sends = sends_of(&sent, "SubmitKeyedTask");
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[0], sends[1], "a resend carries the same bytes");
        let request = corndogs::decode_submit_keyed_task_request(&sends[0]).unwrap();
        assert!(request.guarded, "a stale worker cannot change the task");
        assert!(!request.submission_key.is_empty());
        assert!(request.submission_key.len() <= 128);
        assert_eq!(request.priority, 4);
    }

    #[test]
    fn two_submissions_carry_two_keys() {
        // One key for two batches would make the second one the first one's
        // replay, and the second batch would be gone.
        let (queue, sent) = scripted(3_600, |_, _| Ok(submitted("t")));
        let queue = queue.unwrap();
        queue.submit("delivery", b"one".to_vec(), 0).unwrap();
        queue.submit("delivery", b"two".to_vec(), 0).unwrap();
        let keys: Vec<String> = sends_of(&sent, "SubmitKeyedTask")
            .iter()
            .map(|bytes| {
                corndogs::decode_submit_keyed_task_request(bytes)
                    .unwrap()
                    .submission_key
            })
            .collect();
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1]);
    }

    #[test]
    fn a_reply_lost_every_time_is_sent_three_times_and_then_reported() {
        let (queue, sent) = scripted(3_600, |_, _| Err(ClientError::Transport(LOST.into())));
        let failure = queue
            .unwrap()
            .submit("delivery", b"x".to_vec(), 0)
            .expect_err("no reply");
        assert_eq!(failure.code, ErrorCode::Unavailable);
        assert!(failure.retryable);
        assert_eq!(sends_of(&sent, "SubmitKeyedTask").len(), MAX_SENDS as usize);
    }

    #[test]
    fn a_store_that_keeps_receipts_briefly_gets_no_resend() {
        // A resend after the store forgot the key would make a second task.
        // One minute of retention and a five-second deadline leave room; one
        // that is shorter than two deadlines does not.
        let (queue, sent) = scripted(5, |_, _| Err(ClientError::Transport(LOST.into())));
        queue
            .unwrap()
            .submit("delivery", b"x".to_vec(), 0)
            .expect_err("no reply");
        assert_eq!(sends_of(&sent, "SubmitKeyedTask").len(), 1);
    }

    #[test]
    fn the_stores_answer_is_not_sent_again() {
        // A ServiceError is final. Code 5 means the claim ended: the task timed
        // out and went back to the queue.
        let (queue, sent) = scripted(3_600, |_, _| {
            Err(ClientError::Service {
                code: 5,
                message: "revision conflict".into(),
            })
        });
        let queue = queue.unwrap();
        let claimed = ClaimedTask {
            uuid: "t".into(),
            payload: Vec::new(),
            queue: "delivery".into(),
            current_state: STATE_SENDING.into(),
            priority: 0,
            revision: 2,
        };
        let failure = queue.complete(&claimed).expect_err("the claim ended");
        assert_eq!(failure.code, ErrorCode::FailedPrecondition);
        assert!(!failure.retryable);
        assert!(failure.message.contains("claim"), "{}", failure.message);
        let sends = sends_of(&sent, "CompleteGuardedTask");
        assert_eq!(sends.len(), 1);
        let request = corndogs::decode_complete_guarded_task_request(&sends[0]).unwrap();
        assert_eq!(request.expected_revision, 2);
        assert_eq!(request.expected_state.as_deref(), Some(STATE_SENDING));
    }

    #[test]
    fn a_resent_claim_whose_task_went_back_finds_nothing() {
        // Code 8: the first claim reached the store, its reply was lost, and
        // the task timed out before the resend. It is not this worker's.
        let (queue, _) = scripted(3_600, |_, seen| match seen {
            1 => Err(ClientError::Transport(LOST.into())),
            _ => Err(ClientError::Service {
                code: 8,
                message: "claim superseded".into(),
            }),
        });
        assert_eq!(queue.unwrap().claim("delivery", 30).unwrap(), None);
    }

    #[test]
    fn a_store_older_than_0_8_0_is_refused_when_the_queue_connects() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let carrier = Scripted {
            sent,
            answer: Arc::new(|_, _| {
                Err(ClientError::Transport(
                    "corndogs: transport status 2: unknown operation GetServerInfo".into(),
                ))
            }),
        };
        let refused = CorndogsQueue::from_carriers(
            "corndogs.test:5080",
            QueueOptions::default(),
            false,
            Carrier(Box::new(carrier)),
            Box::new(|| Err("no".to_string())),
        )
        .err()
        .expect("an old store is refused");
        assert_eq!(refused.code, ErrorCode::SchemaUnsupported);
        assert!(refused.message.contains("0.8.0"), "{}", refused.message);
    }

    #[test]
    fn a_store_without_task_guards_is_refused_by_name() {
        let carrier = Scripted {
            sent: Arc::new(Mutex::new(Vec::new())),
            answer: Arc::new(|_, _| Ok(server_info(&["submission-keys-v1"], 3_600))),
        };
        let refused = CorndogsQueue::from_carriers(
            "corndogs.test:5080",
            QueueOptions::default(),
            false,
            Carrier(Box::new(carrier)),
            Box::new(|| Err("no".to_string())),
        )
        .err()
        .expect("refused");
        assert!(
            refused.message.contains("task-guards-v1"),
            "{}",
            refused.message
        );
    }

    #[test]
    fn a_loopback_store_is_plaintext_and_a_network_store_uses_tls() {
        assert_eq!(
            QueueTls::for_endpoint("127.0.0.1:5080", "", "", false),
            None
        );
        assert_eq!(QueueTls::for_endpoint("[::1]:5080", "", "", false), None);
        assert_eq!(
            QueueTls::for_endpoint("localhost:5080", "", "", false),
            None
        );
        assert_eq!(
            QueueTls::for_endpoint("corndogs.tallyowl:5080", "", "", false),
            Some(QueueTls::default()),
            "the system roots and the endpoint's own name"
        );
        assert_eq!(
            QueueTls::for_endpoint("corndogs:5080", "/tls/ca.crt", "corndogs.internal", false),
            Some(QueueTls {
                ca_file: Some("/tls/ca.crt".into()),
                server_name: Some("corndogs.internal".into()),
            })
        );
        assert_eq!(
            QueueTls::for_endpoint("corndogs:5080", "", "", true),
            None,
            "plaintext on a network only when the operator said so"
        );
        assert!(
            QueueTls::for_endpoint("corndogs:5080", "/tls/ca.crt", "", true).is_some(),
            "a configured authority is used even under transport.allowPlaintext"
        );
        assert_eq!(
            QueueTls::for_endpoint(
                "127.0.0.1:5080",
                "/tls/ca.crt",
                "home-corndogs.ns.svc",
                false
            ),
            Some(QueueTls {
                ca_file: Some("/tls/ca.crt".into()),
                server_name: Some("home-corndogs.ns.svc".into()),
            }),
            "a sidecar that serves TLS on loopback is reached over TLS, by its Service name"
        );
    }
}
