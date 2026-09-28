//! The driver: the buffer, the sealed batches, and the delivery state.
//!
//! # What a batch goes through
//!
//! `capture` puts an item in the buffer. A seal takes items out of the buffer,
//! up to the item and byte limits, and makes one batch with one stable ID. The
//! driver then **retains** that batch until the collector acknowledges it. D5:
//! "The app retains the stable batch until that acknowledgement, then moves on."
//!
//! A sealed batch ends in exactly one of two ways:
//!
//! - The collector acknowledges it. The driver forgets it.
//! - The driver gives it up and counts every item in it as lost. That happens
//!   when the collector refuses the batch for a reason that another attempt
//!   cannot change, or when the batch left `max_batch_attempts` times and no
//!   answer came back.
//!
//! Nothing else removes a batch. A collector that is unreachable, and a
//! collector that says "try again", keep the batch retained. The bound on
//! memory is `max_unacknowledged_bytes`: it covers the buffer and every sealed
//! batch together, and `capture` returns backpressure when it is reached.
//!
//! # Retries
//!
//! D19: "Retries use capped jitter while the process is alive." After a failed
//! attempt the driver records when the next attempt is permitted. Until then
//! `flush`, `submit`, and `drain` return at once with an error that carries the
//! wait. The driver never sleeps on the caller's path.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use tallyowl_collector_api::codec::{
    decode_service_error, decode_submit_batch_response, encode_batch, encode_submit_batch_request,
    encode_telemetry_item,
};
use tallyowl_collector_api::types::{Batch, PropertyOrigin, SubmitBatchRequest, TelemetryItem};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_obs::time::now_ms;
use tallyowl_rpc::{Pipeline, TransportStatus, SERVICE_ERROR_VARIANT};
use tallyowl_wire::{collector as wire, collector_items_bridge as items, Value};

use crate::transport::{self, Transport};
use crate::{metrics, new_batch_id, Capture, PROTOCOL_VERSION};

const SERVICE: &str = "TallyOwlCollector";

/// What one batch request adds around its items: the batch ID, the seal time,
/// the protocol version, and the field names. An item this close to the frame
/// limit cannot travel, so `capture` refuses it.
const FRAME_ALLOWANCE: usize = 512;

/// The detail key that carries how long to wait before the next attempt.
const RETRY_AFTER_DETAIL: &str = "retry_after_ms";

/// A function the driver calls for every failure it cannot return to a caller.
///
/// The driver calls it outside its own locks, so the function can call the
/// driver. Keep it fast: it runs on the thread that found the failure.
#[derive(Clone)]
pub struct ErrorHook(Arc<dyn Fn(&TallyOwlError) + Send + Sync>);

impl ErrorHook {
    pub fn new(hook: impl Fn(&TallyOwlError) + Send + Sync + 'static) -> ErrorHook {
        ErrorHook(Arc::new(hook))
    }
}

impl fmt::Debug for ErrorHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ErrorHook(..)")
    }
}

/// Where a dry run writes. One line for each item, and no socket is opened.
#[derive(Clone)]
pub struct DryRun(Arc<Mutex<dyn Write + Send>>);

impl DryRun {
    pub fn new(writer: impl Write + Send + 'static) -> DryRun {
        DryRun(Arc::new(Mutex::new(writer)))
    }

    /// A dry run that writes to the standard error stream.
    pub fn stderr() -> DryRun {
        DryRun::new(std::io::stderr())
    }
}

impl fmt::Debug for DryRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DryRun(..)")
    }
}

/// Driver settings. The defaults are D19's.
#[derive(Debug, Clone)]
pub struct Settings {
    pub collector_address: String,
    pub credential: String,
    pub max_items: usize,
    pub max_batch_bytes: usize,
    pub linger: Duration,
    pub max_frame_bytes: usize,
    /// The bound on everything this driver holds: the buffer, and every sealed
    /// batch the collector has not acknowledged.
    pub max_unacknowledged_bytes: usize,
    pub shutdown_flush_deadline: Duration,
    /// How many sealed batches may be outstanding at one time on the pipelined
    /// path. `docs/DELIVERY.md` section 3 permits "a configured number of
    /// correlated batch calls"; this is that number.
    ///
    /// One reproduces the synchronous behaviour. The default is four, which is
    /// what `submit` needs to stop being bounded by the round trip.
    pub max_in_flight_batches: usize,
    /// How many times a batch can leave and get no answer before the driver
    /// counts it as lost.
    ///
    /// Only an attempt with an unknown result counts: the batch was sent, and
    /// then the connection failed or the reply could not be read. A collector
    /// that is unreachable does not use an attempt, because the batch never
    /// left. A collector that says "try again" does not use one either.
    pub max_batch_attempts: u32,
    /// The deadline for one send and for one wait for a reply. A collector
    /// that accepts a connection and then never answers holds a caller for this
    /// long and no longer. Zero waits without limit.
    pub call_timeout: Duration,
    /// The first wait after a failed attempt. Each later wait doubles.
    pub retry_backoff_min: Duration,
    /// The longest wait between attempts.
    pub retry_backoff_max: Duration,
    /// Properties this driver adds to every item. Their origin is `driver`.
    pub properties: Vec<(String, String)>,
    /// Called for every failure the driver cannot return to a caller: a batch
    /// it gave up, and a failed attempt inside the flusher.
    pub on_error: Option<ErrorHook>,
    /// When set, the driver opens no socket. It writes each sealed item as one
    /// line and reports the batch as accepted with zero durable copies.
    pub dry_run: Option<DryRun>,
    /// How the driver reaches the collector. The default uses TLS with the
    /// authorities of the operating system for a network address, and
    /// plaintext for a loopback or `unix:` address. See [`Transport`].
    pub transport: Transport,
}

impl Settings {
    pub fn new(collector_address: impl Into<String>, credential: impl Into<String>) -> Settings {
        Settings {
            collector_address: collector_address.into(),
            credential: credential.into(),
            max_items: 256,
            max_batch_bytes: 512 * 1024,
            linger: Duration::from_millis(100),
            max_frame_bytes: 1024 * 1024,
            max_unacknowledged_bytes: 8 * 1024 * 1024,
            shutdown_flush_deadline: Duration::from_secs(2),
            max_in_flight_batches: tallyowl_rpc::DEFAULT_CLIENT_WINDOW,
            max_batch_attempts: 5,
            call_timeout: Duration::from_secs(10),
            retry_backoff_min: Duration::from_millis(100),
            retry_backoff_max: Duration::from_secs(30),
            properties: Vec::new(),
            on_error: None,
            dry_run: None,
            transport: Transport::default(),
        }
    }

    /// How many sealed batches may be outstanding at one time. One reproduces
    /// the synchronous behaviour of `flush`.
    pub fn with_max_in_flight_batches(mut self, batches: usize) -> Settings {
        self.max_in_flight_batches = batches.max(1);
        self
    }

    pub fn with_property(mut self, key: impl Into<String>, value: impl Into<String>) -> Settings {
        self.properties.push((key.into(), value.into()));
        self
    }

    /// The deadline for one send and for one wait for a reply.
    pub fn with_call_timeout(mut self, timeout: Duration) -> Settings {
        self.call_timeout = timeout;
        self
    }

    /// Call `hook` for every failure the driver cannot return to a caller.
    pub fn with_on_error(
        mut self,
        hook: impl Fn(&TallyOwlError) + Send + Sync + 'static,
    ) -> Settings {
        self.on_error = Some(ErrorHook::new(hook));
        self
    }

    /// How the driver reaches the collector. See [`Transport`].
    pub fn with_transport(mut self, transport: Transport) -> Settings {
        self.transport = transport;
        self
    }

    /// Open no socket. Write each sealed item to `writer` as one line.
    ///
    /// Use this to confirm that the instrumentation produces what you expect
    /// before a collector exists.
    pub fn with_dry_run(mut self, writer: impl Write + Send + 'static) -> Settings {
        self.dry_run = Some(DryRun::new(writer));
        self
    }
}

/// One item the collector would not take, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub event_id: Vec<u8>,
    pub code: ErrorCode,
    pub message: String,
}

/// What one flush produced.
///
/// A flush that sent more than one batch reports them together: the accepted
/// counts are added, the rejected items are joined, the durable-copy count is
/// the smallest one reported, and the batch ID is the last batch's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub batch_id: [u8; 16],
    pub accepted: u64,
    /// The durable copies the collector reported. A caller that needs a
    /// stronger boundary reads this rather than assuming one.
    pub durable_copies: u64,
    pub rejected: Vec<Rejected>,
}

impl Receipt {
    fn merge(self, later: Receipt) -> Receipt {
        let mut rejected = self.rejected;
        rejected.extend(later.rejected);
        Receipt {
            batch_id: later.batch_id,
            accepted: self.accepted + later.accepted,
            durable_copies: self.durable_copies.min(later.durable_copies),
            rejected,
        }
    }
}

/// What the driver has done since it started.
///
/// Every number counts items, not batches. An item that `capture` took is
/// buffered, then unacknowledged, and then it is accepted, rejected, or lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stats {
    /// Items `capture` took.
    pub captured: u64,
    /// Items `capture` refused, for backpressure or for size.
    pub refused: u64,
    /// Items the collector acknowledged as durably accepted.
    pub accepted: u64,
    /// Items the collector named as rejected inside an acknowledged batch.
    pub rejected: u64,
    /// Items the driver gave up.
    pub lost: u64,
    /// Items in the buffer.
    pub buffered: usize,
    /// Items in sealed batches that the collector has not acknowledged.
    pub unacknowledged: usize,
    /// The most recent failure, or nothing when there was none.
    pub last_error: Option<TallyOwlError>,
}

/// How long to wait before the next attempt, when `error` says to wait.
///
/// `flush`, `submit`, and `drain` return such an error while the wait after a
/// failed attempt is open. A host that runs its own flush loop reads this and
/// does not treat the error as a new failure.
pub fn retry_after(error: &TallyOwlError) -> Option<Duration> {
    error
        .detail
        .iter()
        .find(|(key, _)| key == RETRY_AFTER_DETAIL)
        .and_then(|(_, value)| value.parse::<u64>().ok())
        .map(Duration::from_millis)
}

pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;
pub(crate) type Random = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The app driver.
///
/// A clone is another handle on the same driver. Give one to each thread that
/// captures.
#[derive(Clone)]
pub struct Driver {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    settings: Settings,
    pub(crate) buffer: Mutex<Buffer>,
    sequence: AtomicU64,
    delivery: Mutex<Delivery>,
    counters: Counters,
    last_error: Mutex<Option<TallyOwlError>>,
    clock: Clock,
    random: Random,
}

#[derive(Default)]
pub(crate) struct Buffer {
    pub(crate) items: VecDeque<TelemetryItem>,
    /// The encoded size of each item, in the same order. A seal uses these to
    /// stop at the byte limit without encoding anything twice.
    sizes: VecDeque<usize>,
    bytes: usize,
    opened_at: Option<Instant>,
    seal_now: bool,
}

#[derive(Default)]
struct Counters {
    captured: AtomicU64,
    refused: AtomicU64,
    accepted: AtomicU64,
    rejected: AtomicU64,
    lost: AtomicU64,
    /// The item bytes of every sealed batch that is not acknowledged. `capture`
    /// reads this without the delivery lock, so a slow collector never blocks
    /// the caller's path.
    sealed_bytes: AtomicUsize,
    unacknowledged_items: AtomicUsize,
    sealed: AtomicU64,
    /// How many losses the error hook was told about.
    told: AtomicU64,
}

/// One sealed batch, retained until it is acknowledged.
///
/// Retaining the encoded frame is what lets a connection failure be a resend
/// rather than a loss, and the batch ID does not change across it, so final
/// storage still deduplicates to one logical commit.
struct Pending {
    batch_id: [u8; 16],
    encoded: Vec<u8>,
    /// How many items this batch holds, so a shutdown reports unsent items
    /// rather than unsent batches.
    items: usize,
    /// The item bytes this batch holds against the unacknowledged bound.
    bytes: usize,
    /// How many times this batch left and got no answer.
    attempts: u32,
    /// The order this batch was sealed in. Retained batches go again oldest
    /// first.
    order: u64,
    /// The lines a dry run writes. Empty outside a dry run.
    described: String,
}

/// The pipelined connection and everything outstanding on it.
struct Outbox {
    pipeline: Pipeline,
    /// Why no connection of this kind can be made, for example TLS roots that
    /// did not load. The next send reports it and builds the pipeline again.
    broken: Option<TallyOwlError>,
    pending: HashMap<u64, Pending>,
    /// Whether a frame has left on the current connection. A failure on a
    /// connection that worked before is most often a collector that restarted,
    /// so it gets one attempt on a new connection before any wait.
    established: bool,
}

#[derive(Default)]
struct Delivery {
    outbox: Option<Outbox>,
    /// Sealed batches that are not on the wire: they failed, or they are waiting
    /// for their first attempt behind a failure. Oldest first.
    retained: VecDeque<Pending>,
    /// Receipts that arrived while `flush` waited for a different batch. The
    /// next `submit` or `drain` returns them.
    collected: Vec<Receipt>,
    /// Failed attempts in a row. A success sets it to zero.
    failures: u32,
    next_attempt: Option<Instant>,
    last_failure: Option<TallyOwlError>,
    /// Whether the one attempt without a wait has been used since the last
    /// success.
    free_retry_used: bool,
    /// Failures to hand to the error hook after the lock is released.
    notes: Vec<TallyOwlError>,
}

/// A failed attempt, and whether the caller can try again at once.
struct Failed {
    error: TallyOwlError,
    retry_now: bool,
}

/// One answer from the collector, matched to its batch.
struct Collected {
    order: u64,
    outcome: Result<Receipt, TallyOwlError>,
}

/// Why a reply did not become a receipt.
enum ReceiptFailure {
    /// The collector refused the batch and said so. Its `retryable` flag is a
    /// fact, and the driver acts on it.
    Refused(TallyOwlError),
    /// The batch left, and what happened to it is unknown.
    Ambiguous(TallyOwlError),
}

impl Driver {
    pub fn new(settings: Settings) -> Driver {
        Driver::with_sources(
            settings,
            Arc::new(Instant::now),
            Arc::new(|| {
                let mut bytes = [0u8; 8];
                // A failed random source costs the jitter and nothing else.
                let _ = getrandom::fill(&mut bytes);
                u64::from_le_bytes(bytes)
            }),
        )
    }

    /// A driver that reads the time and the jitter from the caller. A test uses
    /// this, so that a retry schedule needs no real wait.
    pub(crate) fn with_sources(settings: Settings, clock: Clock, random: Random) -> Driver {
        Driver {
            inner: Arc::new(Inner {
                settings,
                buffer: Mutex::new(Buffer::default()),
                sequence: AtomicU64::new(0),
                delivery: Mutex::new(Delivery::default()),
                counters: Counters::default(),
                last_error: Mutex::new(None),
                clock,
                random,
            }),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.inner.settings
    }

    /// Buffer one item. This does not reach the collector, and it makes no
    /// durability claim.
    ///
    /// It returns a typed backpressure error rather than accepting data it would
    /// then drop, because a driver that reports success for a discarded event
    /// makes every count downstream wrong in a way nobody can find.
    pub fn capture(&self, capture: Capture) -> Result<(), TallyOwlError> {
        let (item, size) = self.inner.prepare(capture.item)?;
        let mut buffer = self.inner.buffer.lock().expect("driver lock");
        self.inner.admit(&mut buffer, item, size, capture.critical)
    }

    /// Buffer one item from a place that must never block and never panic.
    ///
    /// The panic hook uses this. It gives the item up when another thread holds
    /// the buffer, because a hook that waited on a lock the panicking thread
    /// holds would never return.
    pub(crate) fn capture_without_waiting(&self, capture: Capture) {
        let Ok((item, size)) = self.inner.prepare(capture.item) else {
            return;
        };
        if let Ok(mut buffer) = self.inner.buffer.try_lock() {
            let _ = self.inner.admit(&mut buffer, item, size, capture.critical);
        }
    }

    /// Take one snapshot of a meter and buffer every point it produced.
    ///
    /// A host calls this on its own period. The driver does not own a timer
    /// for the metric period, because a host that already has one would then
    /// have two, and the two would disagree about when a period ended.
    ///
    /// Returns how many points were buffered. A point refused for backpressure
    /// stops the snapshot and returns the error: the rest of the snapshot is
    /// still in the meter, and a cumulative meter reports it whole in the next
    /// period. That is why cumulative is the default.
    pub fn publish_metrics(&self, meter: &metrics::Meter) -> Result<usize, TallyOwlError> {
        let mut published = 0;
        for capture in meter.snapshot() {
            self.capture(capture)?;
            published += 1;
        }
        Ok(published)
    }

    /// Whether the buffer has reached a seal condition.
    pub fn should_flush(&self) -> bool {
        let buffer = self.inner.buffer.lock().expect("driver lock");
        self.inner.seal_reached(&buffer)
    }

    pub fn buffered(&self) -> usize {
        self.inner.buffered()
    }

    /// Send every retained batch and everything buffered, and wait for each
    /// durable acknowledgement.
    ///
    /// Returns `Ok(None)` when there was nothing to send. The buffer can hold
    /// more than one batch, so a flush can send several; the receipt reports
    /// them together.
    ///
    /// A batch that fails stays retained, with its ID, and the next call sends
    /// it again. While the wait after a failed attempt is open, this returns at
    /// once with an error that [`retry_after`] reads.
    pub fn flush(&self) -> Result<Option<Receipt>, TallyOwlError> {
        self.inner
            .with_delivery(|inner, delivery| inner.flush(delivery))
    }

    /// Seal the buffer and send it without waiting for its acknowledgement.
    ///
    /// This is the pipelined path, and it is the one an application with a
    /// single telemetry worker wants. `flush` waits for the durable receipt of
    /// each batch it sealed, so one worker is bounded by the round trip rather
    /// than by anything in TallyOwl: at the D19 defaults that is about 5,400
    /// events each second against a measured ceiling of 36,525. See
    /// `docs/ALPHA_REPORT.md` section 3.3.
    ///
    /// The returned receipts are for batches that finished while this one was
    /// making room in the window. It is normal for the list to be empty, and it
    /// is normal for a receipt to arrive for a batch sealed several calls ago.
    /// Nothing here reports a batch acknowledged before the collector did.
    pub fn submit(&self) -> Result<Vec<Receipt>, TallyOwlError> {
        self.inner
            .with_delivery(|inner, delivery| inner.submit(delivery))
    }

    /// Send every retained batch, wait for every outstanding batch, and return
    /// the receipts.
    pub fn drain(&self) -> Result<Vec<Receipt>, TallyOwlError> {
        self.inner
            .with_delivery(|inner, delivery| inner.drain(delivery))
    }

    /// How many sealed batches are waiting for an acknowledgement. This counts
    /// a batch on the wire and a batch that is retained for another attempt.
    pub fn outstanding(&self) -> usize {
        let delivery = self.inner.delivery.lock().expect("driver lock");
        delivery.retained.len()
            + delivery
                .outbox
                .as_ref()
                .map(|outbox| outbox.pending.len())
                .unwrap_or(0)
    }

    /// How many items sit in batches that are waiting for an acknowledgement.
    pub fn outstanding_items(&self) -> usize {
        self.inner
            .counters
            .unacknowledged_items
            .load(Ordering::SeqCst)
    }

    /// Flush when a seal condition is reached, and do nothing otherwise.
    pub fn flush_if_sealed(&self) -> Result<Option<Receipt>, TallyOwlError> {
        if self.should_flush() {
            self.flush()
        } else {
            Ok(None)
        }
    }

    /// What the driver has done since it started.
    pub fn stats(&self) -> Stats {
        let counters = &self.inner.counters;
        Stats {
            captured: counters.captured.load(Ordering::SeqCst),
            refused: counters.refused.load(Ordering::SeqCst),
            accepted: counters.accepted.load(Ordering::SeqCst),
            rejected: counters.rejected.load(Ordering::SeqCst),
            lost: counters.lost.load(Ordering::SeqCst),
            buffered: self.buffered(),
            unacknowledged: self.outstanding_items(),
            last_error: self.inner.last_error.lock().expect("driver lock").clone(),
        }
    }

    /// Start one thread that sends for this driver, and return its handle.
    ///
    /// Without this, or a loop of the host's own, nothing leaves the process:
    /// `capture` only buffers. The thread sends when a seal condition is
    /// reached or a retained batch is due, collects acknowledgements when it is
    /// idle, and reports each failure to the `on_error` hook. It never blocks
    /// `capture`.
    ///
    /// This is opt-in. A host that already runs a scheduler calls `submit` and
    /// `drain` from it and does not start this thread.
    pub fn spawn_flusher(&self) -> Result<Flusher, TallyOwlError> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let driver = self.clone();
        let pause = flusher_pause(self.inner.settings.linger);
        let thread = std::thread::Builder::new()
            .name("tallyowl-driver-flusher".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::SeqCst) {
                    driver.tick();
                    std::thread::park_timeout(pause);
                }
            })
            .map_err(|e| {
                TallyOwlError::internal(format!(
                    "The host did not give the app driver a thread for its flusher: {e}. Nothing will be sent until the application calls `flush` itself."
                ))
            })?;
        Ok(Flusher {
            driver: self.clone(),
            stop,
            thread: Some(thread),
        })
    }

    /// One turn of the flusher. A host with its own scheduler can call this on
    /// its own period and get the same behaviour.
    pub fn tick(&self) {
        let told_before = self.inner.counters.told.load(Ordering::SeqCst);
        let result = if self.should_flush() || self.inner.retained_is_due() {
            self.submit().map(|_| ())
        } else if self.inner.on_the_wire() > 0 {
            self.drain().map(|_| ())
        } else {
            Ok(())
        };
        if let Err(error) = result {
            // An open wait is not a new failure. The attempt that opened it
            // was reported when it failed. A batch the driver gave up was
            // reported with its item count, and once is enough.
            let told = self.inner.counters.told.load(Ordering::SeqCst) != told_before;
            if retry_after(&error).is_none() && !told {
                self.inner.report(&error);
            }
        }
    }

    /// Send what is left until the deadline, and report what did not go.
    ///
    /// The return value is the count of items that did not reach the collector:
    /// the items still buffered, the items in sealed batches with no
    /// acknowledgement, and the items the driver gave up during this call. A
    /// shutdown that reports nothing is a shutdown that hides data loss.
    ///
    /// It returns at `shutdown_flush_deadline` whatever the collector does.
    pub fn shutdown(&self) -> usize {
        let lost_before = self.inner.counters.lost.load(Ordering::SeqCst);
        let window = self.inner.settings.shutdown_flush_deadline;
        let deadline = Instant::now() + window;

        // The work runs on its own thread, so a collector that accepts the
        // connection and never answers cannot hold this call past its deadline.
        let (done, finished) = mpsc::channel();
        let worker = Arc::clone(&self.inner);
        let started = std::thread::Builder::new()
            .name("tallyowl-driver-shutdown".into())
            .spawn(move || {
                worker.finish(deadline);
                let _ = done.send(());
            });
        match started {
            Ok(_) => {
                let _ = finished.recv_timeout(window);
            }
            // No thread: do the work here. The call deadline still bounds it.
            Err(_) => self.inner.finish(deadline),
        }

        if let Ok(mut delivery) = self.inner.delivery.try_lock() {
            if delivery
                .outbox
                .as_ref()
                .is_some_and(|outbox| outbox.pending.is_empty())
            {
                delivery.outbox = None;
            }
        }

        // A seal moves items between these two accounts under the buffer lock,
        // so reading both under it counts each item once.
        let held = {
            let buffer = self.inner.buffer.lock().expect("driver lock");
            buffer.items.len() + self.outstanding_items()
        };
        let lost_now = self.inner.counters.lost.load(Ordering::SeqCst);
        held + (lost_now - lost_before) as usize
    }

    /// The encoded size of a set of items. A caller sizing its own limits uses
    /// this rather than guessing.
    pub fn encoded_size(items: &[TelemetryItem]) -> usize {
        encode_batch(&Batch {
            batch_id: vec![0; 16],
            items: items.to_vec(),
            common_properties: None,
            sealed_at: 0,
            compression: None,
        })
        .len()
    }

    /// The time this driver reads. A span guard measures with it.
    pub(crate) fn now(&self) -> Instant {
        (self.inner.clock)()
    }
}

/// The handle of the flusher thread. Dropping it stops the thread and shuts the
/// driver down; [`Flusher::stop`] does the same and reports the unsent count.
pub struct Flusher {
    driver: Driver,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Flusher {
    /// Stop the thread, shut the driver down, and return how many items did not
    /// reach the collector.
    pub fn stop(mut self) -> usize {
        self.end().unwrap_or(0)
    }

    fn end(&mut self) -> Option<usize> {
        let thread = self.thread.take()?;
        self.stop.store(true, Ordering::SeqCst);
        thread.thread().unpark();
        // The shutdown runs here and not on the flusher thread. That thread can
        // be inside a call that a silent collector holds until the call
        // deadline, and the shutdown deadline is the shorter of the two. The
        // thread ends by itself when that call returns.
        Some(self.driver.shutdown())
    }
}

impl Drop for Flusher {
    fn drop(&mut self) {
        let _ = self.end();
    }
}

/// How long the flusher rests between turns: half the linger, inside bounds
/// that keep an idle driver cheap and a busy one prompt.
fn flusher_pause(linger: Duration) -> Duration {
    (linger / 2).clamp(Duration::from_millis(5), Duration::from_millis(250))
}

/// The wait before attempt `failures + 1`.
///
/// The ceiling doubles with each failure and stops at `max`. The wait is half
/// the ceiling plus a random part of the other half, so a fleet of applications
/// that lost one collector at one moment does not return at one moment.
pub(crate) fn backoff_delay(failures: u32, min: Duration, max: Duration, random: u64) -> Duration {
    let doubling = failures.saturating_sub(1).min(40);
    let ceiling = min
        .as_nanos()
        .saturating_mul(1u128 << doubling)
        .min(max.as_nanos())
        .max(1);
    let half = ceiling / 2;
    let jitter = u128::from(random) % (ceiling - half + 1);
    Duration::from_nanos(u64::try_from(half + jitter).unwrap_or(u64::MAX))
}

impl Inner {
    fn now(&self) -> Instant {
        (self.clock)()
    }

    fn buffered(&self) -> usize {
        self.buffer.lock().expect("driver lock").items.len()
    }

    /// Stamp the item and measure it. Nothing here takes a lock.
    fn prepare(&self, mut item: TelemetryItem) -> Result<(TelemetryItem, usize), TallyOwlError> {
        item.envelope.sequence = Some(self.sequence.fetch_add(1, Ordering::Relaxed));
        for (key, value) in &self.settings.properties {
            item.envelope.properties.push(wire::property(
                key,
                Value::Text(value.clone()),
                PropertyOrigin::Driver,
            ));
        }

        let size = encode_telemetry_item(&item).len();
        if size + FRAME_ALLOWANCE > self.settings.max_frame_bytes {
            self.counters.refused.fetch_add(1, Ordering::SeqCst);
            return Err(TallyOwlError::over_limit(
                "Event",
                &format!("{} KiB", size / 1024),
                &format!("{} KiB", self.settings.max_frame_bytes / 1024),
                "The event was not recorded. Send fewer or smaller properties, or raise `max_frame_bytes` for this driver.",
            ));
        }
        Ok((item, size))
    }

    fn admit(
        &self,
        buffer: &mut Buffer,
        item: TelemetryItem,
        size: usize,
        critical: bool,
    ) -> Result<(), TallyOwlError> {
        let sealed = self.counters.sealed_bytes.load(Ordering::SeqCst);
        if buffer.bytes + sealed + size > self.settings.max_unacknowledged_bytes {
            self.counters.refused.fetch_add(1, Ordering::SeqCst);
            return Err(TallyOwlError::new(
                ErrorCode::ResourceExhausted,
                "This application is producing telemetry faster than TallyOwl is accepting it. The event was not recorded. Send fewer events, or give the collector more capacity.",
            )
            .retryable(true));
        }

        if buffer.opened_at.is_none() {
            buffer.opened_at = Some(self.now());
        }
        buffer.bytes += size;
        buffer.items.push_back(item);
        buffer.sizes.push_back(size);
        if critical {
            buffer.seal_now = true;
        }
        self.counters.captured.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn seal_reached(&self, buffer: &Buffer) -> bool {
        if buffer.items.is_empty() {
            return false;
        }
        if buffer.seal_now {
            return true;
        }
        if buffer.items.len() >= self.settings.max_items {
            return true;
        }
        if buffer.bytes >= self.settings.max_batch_bytes {
            return true;
        }
        buffer
            .opened_at
            .map(|opened| self.now().saturating_duration_since(opened) >= self.settings.linger)
            .unwrap_or(false)
    }

    /// Run one delivery operation, then hand its failures to the error hook
    /// with no lock held.
    fn with_delivery<T>(
        &self,
        operation: impl FnOnce(&Inner, &mut Delivery) -> Result<T, TallyOwlError>,
    ) -> Result<T, TallyOwlError> {
        let (result, notes) = {
            let mut delivery = self.delivery.lock().expect("driver lock");
            let result = operation(self, &mut delivery);
            (result, std::mem::take(&mut delivery.notes))
        };
        for note in &notes {
            self.counters.told.fetch_add(1, Ordering::SeqCst);
            self.report(note);
        }
        result
    }

    /// Remember a failure and tell the host about it.
    fn report(&self, error: &TallyOwlError) {
        *self.last_error.lock().expect("driver lock") = Some(error.clone());
        if let Some(hook) = &self.settings.on_error {
            (hook.0)(error);
        }
    }

    fn retained_is_due(&self) -> bool {
        let delivery = self.delivery.lock().expect("driver lock");
        !delivery.retained.is_empty()
            && delivery
                .next_attempt
                .map(|at| self.now() >= at)
                .unwrap_or(true)
    }

    fn on_the_wire(&self) -> usize {
        self.delivery
            .lock()
            .expect("driver lock")
            .outbox
            .as_ref()
            .map(|outbox| outbox.pending.len())
            .unwrap_or(0)
    }

    /// The error a caller gets while the wait after a failed attempt is open.
    fn waiting(&self, delivery: &Delivery) -> Option<TallyOwlError> {
        let wait = delivery
            .next_attempt?
            .checked_duration_since(self.now())
            .filter(|wait| !wait.is_zero())?;
        let reason = delivery
            .last_failure
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_default();
        // Rounded up, so a caller that waits this long finds the wait over.
        let millis = wait.as_nanos().div_ceil(1_000_000).max(1);
        Some(
            TallyOwlError::unavailable(format!(
                "The last attempt to reach {} failed, and the next one is in {millis} ms. The telemetry is retained and will be sent again. {reason}",
                self.settings.collector_address
            ))
            .with_detail(RETRY_AFTER_DETAIL, millis.to_string()),
        )
    }

    /// Take items out of the buffer, up to the item and byte limits, and make
    /// one batch of them. Returns nothing when the buffer is empty.
    ///
    /// The limits are what keep a batch inside one frame. A buffer that holds
    /// more than one batch leaves the rest buffered, and the caller seals again.
    fn seal(&self, delivery: &mut Delivery) -> Result<Option<Pending>, TallyOwlError> {
        let budget = self.settings.max_batch_bytes.min(
            self.settings
                .max_frame_bytes
                .saturating_sub(FRAME_ALLOWANCE),
        );
        let (taken, bytes) = {
            let mut buffer = self.buffer.lock().expect("driver lock");
            if buffer.items.is_empty() {
                return Ok(None);
            }
            let mut count = 0;
            let mut bytes = 0;
            while count < buffer.items.len() && count < self.settings.max_items.max(1) {
                let next = buffer.sizes[count];
                // The first item always goes, so one large item cannot stop
                // every seal behind it.
                if count > 0 && bytes + next > budget {
                    break;
                }
                bytes += next;
                count += 1;
            }
            let taken: Vec<TelemetryItem> = buffer.items.drain(..count).collect();
            buffer.sizes.drain(..count);
            buffer.bytes -= bytes;
            if buffer.items.is_empty() {
                buffer.opened_at = None;
                buffer.seal_now = false;
            } else {
                buffer.opened_at = Some(self.now());
            }
            // The items move from the buffer's account to the sealed account
            // under the buffer lock, so a reader never sees them in neither.
            self.counters
                .sealed_bytes
                .fetch_add(bytes, Ordering::SeqCst);
            self.counters
                .unacknowledged_items
                .fetch_add(taken.len(), Ordering::SeqCst);
            (taken, bytes)
        };

        let described = if self.settings.dry_run.is_some() {
            taken.iter().map(describe).collect::<Vec<_>>().join("\n")
        } else {
            String::new()
        };

        // The batch keeps this ID across every retry and across a lost
        // connection, so final storage deduplicates it to one logical commit.
        let batch_id = new_batch_id();
        let item_count = taken.len();
        let request = SubmitBatchRequest {
            batch: Batch {
                batch_id: batch_id.to_vec(),
                items: taken,
                common_properties: None,
                sealed_at: now_ms(),
                compression: None,
            },
            policy_version: None,
            // What this driver speaks. A collector and the head each accept
            // the current version and the one before it, so an application
            // that upgrades after the installation keeps being accepted.
            protocol_version: Some(PROTOCOL_VERSION),
        };

        let batch = Pending {
            batch_id,
            encoded: encode_submit_batch_request(&request),
            items: item_count,
            bytes,
            attempts: 0,
            order: self.counters.sealed.fetch_add(1, Ordering::SeqCst),
            described,
        };
        if batch.encoded.len() > self.settings.max_frame_bytes {
            let error = TallyOwlError::over_limit(
                "Batch",
                &format!("{} KiB", batch.encoded.len() / 1024),
                &format!("{} KiB", self.settings.max_frame_bytes / 1024),
                "Set `max_batch_bytes` below `max_frame_bytes` for this driver.",
            );
            self.lose(delivery, &batch, &error);
            return Err(error);
        }
        Ok(Some(batch))
    }

    /// The next batch to send: the oldest retained one, or a new seal.
    fn next_batch(&self, delivery: &mut Delivery) -> Result<Option<Pending>, TallyOwlError> {
        match delivery.retained.pop_front() {
            Some(batch) => Ok(Some(batch)),
            None => self.seal(delivery),
        }
    }

    fn flush(&self, delivery: &mut Delivery) -> Result<Option<Receipt>, TallyOwlError> {
        if let Some(wait) = self.waiting(delivery) {
            return Err(wait);
        }
        let mut merged: Option<Receipt> = None;
        let mut add = |receipt: Receipt| {
            merged = Some(match merged.take() {
                Some(earlier) => earlier.merge(receipt),
                None => receipt,
            });
        };

        'batches: loop {
            if !self.has_work(delivery) {
                break;
            }
            // Room first, so a failure here never holds a batch that is in
            // neither the buffer nor the retained list.
            self.make_room(delivery)?;
            let Some(batch) = self.next_batch(delivery)? else {
                break;
            };
            if let Some(receipt) = self.dry_run(delivery, &batch) {
                add(receipt);
                continue;
            }
            let order = batch.order;
            if let Err(failed) = self.send(delivery, batch) {
                if failed.retry_now {
                    continue;
                }
                return Err(failed.error);
            }
            loop {
                match self.collect(delivery) {
                    Ok(collected) if collected.order == order => {
                        add(collected.outcome?);
                        continue 'batches;
                    }
                    // An answer for a batch `submit` sent. Its receipt goes to
                    // the next `submit` or `drain`; its failure is already
                    // counted and reported.
                    Ok(collected) => {
                        if let Ok(receipt) = collected.outcome {
                            delivery.collected.push(receipt);
                        }
                    }
                    Err(failed) if failed.retry_now => continue 'batches,
                    Err(failed) => return Err(failed.error),
                }
            }
        }
        Ok(merged)
    }

    fn submit(&self, delivery: &mut Delivery) -> Result<Vec<Receipt>, TallyOwlError> {
        if let Some(wait) = self.waiting(delivery) {
            return Err(wait);
        }
        let mut receipts = std::mem::take(&mut delivery.collected);
        while self.has_work(delivery) {
            // Make room in the window before adding to it. Collecting a
            // receipt is what frees a slot, so a caller that never collects
            // never sends. Room comes before the seal, so a failure here never
            // holds a batch that is in neither the buffer nor the retained list.
            while !self.outbox(delivery).pipeline.has_room() {
                match self.collect(delivery) {
                    Ok(collected) => match collected.outcome {
                        Ok(receipt) => receipts.push(receipt),
                        Err(error) => {
                            delivery.collected.extend(receipts);
                            return Err(error);
                        }
                    },
                    Err(failed) if failed.retry_now => break,
                    Err(failed) => {
                        delivery.collected.extend(receipts);
                        return Err(failed.error);
                    }
                }
            }
            let batch = match self.next_batch(delivery) {
                Ok(Some(batch)) => batch,
                Ok(None) => break,
                Err(error) => {
                    delivery.collected.extend(receipts);
                    return Err(error);
                }
            };
            if let Some(receipt) = self.dry_run(delivery, &batch) {
                receipts.push(receipt);
                continue;
            }
            if let Err(failed) = self.send(delivery, batch) {
                if failed.retry_now {
                    continue;
                }
                delivery.collected.extend(receipts);
                return Err(failed.error);
            }
        }
        Ok(receipts)
    }

    fn drain(&self, delivery: &mut Delivery) -> Result<Vec<Receipt>, TallyOwlError> {
        let mut receipts = std::mem::take(&mut delivery.collected);
        loop {
            let on_the_wire = delivery
                .outbox
                .as_ref()
                .map(|outbox| outbox.pending.len())
                .unwrap_or(0);
            if on_the_wire == 0 {
                if delivery.retained.is_empty() {
                    return Ok(receipts);
                }
                if let Some(wait) = self.waiting(delivery) {
                    delivery.collected.extend(receipts);
                    return Err(wait);
                }
                // A retained batch goes again before there is anything to
                // wait for.
                while self.outbox(delivery).pipeline.has_room() {
                    let Some(batch) = delivery.retained.pop_front() else {
                        break;
                    };
                    if let Some(receipt) = self.dry_run(delivery, &batch) {
                        receipts.push(receipt);
                        continue;
                    }
                    if let Err(failed) = self.send(delivery, batch) {
                        if failed.retry_now {
                            continue;
                        }
                        delivery.collected.extend(receipts);
                        return Err(failed.error);
                    }
                }
                continue;
            }
            match self.collect(delivery) {
                Ok(collected) => match collected.outcome {
                    Ok(receipt) => receipts.push(receipt),
                    Err(error) => {
                        delivery.collected.extend(receipts);
                        return Err(error);
                    }
                },
                Err(failed) if failed.retry_now => {}
                Err(failed) => {
                    delivery.collected.extend(receipts);
                    return Err(failed.error);
                }
            }
        }
    }

    /// Whether there is a batch to send: a retained one, or items to seal.
    fn has_work(&self, delivery: &Delivery) -> bool {
        !delivery.retained.is_empty() || self.buffered() > 0
    }

    /// Collect until the window has room for one more batch. `flush` keeps the
    /// receipts for the caller that sent those batches.
    fn make_room(&self, delivery: &mut Delivery) -> Result<(), TallyOwlError> {
        while !self.outbox(delivery).pipeline.has_room() {
            match self.collect(delivery) {
                Ok(collected) => {
                    if let Ok(receipt) = collected.outcome {
                        delivery.collected.push(receipt);
                    }
                }
                Err(failed) if failed.retry_now => return Ok(()),
                Err(failed) => return Err(failed.error),
            }
        }
        Ok(())
    }

    /// The pipelined connection. It opens on the first send, so an application
    /// keeps one connection to its collector.
    fn outbox<'a>(&self, delivery: &'a mut Delivery) -> &'a mut Outbox {
        delivery.outbox.get_or_insert_with(|| {
            // The credential rides on the connection, so every batch carries
            // it. The collector resolves tenancy from it and stamps the result;
            // the driver never sends a workspace, a project, or a source.
            let built = transport::pipeline(
                &self.settings.collector_address,
                self.settings.max_frame_bytes,
                self.settings.max_in_flight_batches,
                &self.settings.transport,
            );
            let (pipeline, broken) = match built {
                Ok(pipeline) => (pipeline, None),
                // A pipeline that is never sent on, so every other caller
                // still finds an outbox with room.
                Err(error) => (
                    Pipeline::new(
                        self.settings.collector_address.clone(),
                        self.settings.max_frame_bytes,
                        self.settings.max_in_flight_batches,
                    ),
                    Some(error),
                ),
            };
            let mut pipeline = pipeline
                .with_credential(self.settings.credential.clone())
                .with_io_timeout(self.settings.call_timeout);
            if !self.settings.call_timeout.is_zero() {
                // A short call deadline is not spent waiting on a connect.
                pipeline = pipeline
                    .with_connect_timeout(self.settings.call_timeout.min(Duration::from_secs(5)));
            }
            Outbox {
                pipeline,
                broken,
                pending: HashMap::new(),
                established: false,
            }
        })
    }

    /// Write a batch to the dry-run writer and count it as accepted. Returns
    /// nothing outside a dry run.
    fn dry_run(&self, delivery: &mut Delivery, batch: &Pending) -> Option<Receipt> {
        let dry_run = self.settings.dry_run.as_ref()?;
        if let Ok(mut writer) = dry_run.0.lock() {
            let _ = writeln!(writer, "{}", batch.described);
            let _ = writer.flush();
        }
        let receipt = Receipt {
            batch_id: batch.batch_id,
            accepted: batch.items as u64,
            // Nothing is durable, and the receipt says so.
            durable_copies: 0,
            rejected: Vec::new(),
        };
        self.acknowledged(delivery, batch, &receipt);
        Some(receipt)
    }

    /// Send one batch. On failure the batch is retained, or counted as lost
    /// when no other attempt can change the result.
    fn send(&self, delivery: &mut Delivery, batch: Pending) -> Result<(), Failed> {
        let started = self.now();
        let outbox = self.outbox(delivery);
        if let Some(error) = outbox.broken.clone() {
            // Nothing was sent, so the batch is retained and pays no attempt.
            delivery.outbox = None;
            self.retain(delivery, batch);
            self.note_failure(delivery, &error, false);
            return Err(Failed {
                error,
                retry_now: false,
            });
        }
        let established = outbox.established;
        match outbox
            .pipeline
            .send(SERVICE, "submit-batch", batch.encoded.clone())
        {
            Ok(id) => {
                outbox.established = true;
                outbox.pending.insert(id, batch);
                Ok(())
            }
            Err(error) => {
                // The frame did not leave whole, so the batch is retained and
                // this attempt does not count against it. That is true for an
                // address that does not resolve too: a name service that fails
                // for a minute must not cost the telemetry of that minute. A
                // TLS handshake the collector refused sent nothing either.
                self.retain(delivery, batch);
                let error = transport::explain_handshake(error);
                Err(self.connection_failed(delivery, error, established, started))
            }
        }
    }

    /// Wait for the next answer and match it to its batch.
    ///
    /// `Err` means the connection failed. `Ok` means the collector answered,
    /// and the outcome inside says what it answered.
    fn collect(&self, delivery: &mut Delivery) -> Result<Collected, Failed> {
        let started = self.now();
        let outbox = self.outbox(delivery);
        let established = outbox.established;
        let (id, response) = match outbox.pipeline.recv() {
            Ok(Some(answer)) => answer,
            Ok(None) => {
                return Err(Failed {
                    error: TallyOwlError::internal(
                        "There was no batch waiting for an acknowledgement.".to_string(),
                    ),
                    retry_now: false,
                })
            }
            Err(error) => {
                return Err(self.connection_failed(delivery, error, established, started))
            }
        };

        let Some(mut batch) = outbox.pending.remove(&id) else {
            // A reply for a call this driver never made. The connection is no
            // longer trustworthy.
            outbox.pipeline.reset();
            let error = TallyOwlError::internal(
                "The collector answered a batch this application did not send.".to_string(),
            );
            return Err(self.connection_failed(delivery, error, false, started));
        };

        let order = batch.order;
        let outcome = match read_receipt(batch.batch_id, &response) {
            Ok(receipt) => {
                self.acknowledged(delivery, &batch, &receipt);
                Ok(receipt)
            }
            Err(ReceiptFailure::Refused(error)) if error.retryable => {
                // The collector said "try again", so the batch is whole and
                // retained, and the attempt does not count against it.
                self.retain(delivery, batch);
                self.note_failure(delivery, &error, false);
                Err(error)
            }
            Err(ReceiptFailure::Refused(error)) => {
                self.lose(delivery, &batch, &error);
                Err(error)
            }
            Err(ReceiptFailure::Ambiguous(error)) => {
                batch.attempts += 1;
                self.retain_or_lose(delivery, batch, &error);
                self.note_failure(delivery, &error, false);
                Err(error)
            }
        };
        Ok(Collected { order, outcome })
    }

    /// The connection failed. Everything on the wire has an unknown result, so
    /// each of those batches used one attempt. Each keeps its ID, so sending it
    /// again stays one logical commit.
    fn connection_failed(
        &self,
        delivery: &mut Delivery,
        error: TallyOwlError,
        established: bool,
        started: Instant,
    ) -> Failed {
        let held: Vec<Pending> = match delivery.outbox.as_mut() {
            Some(outbox) => {
                outbox.established = false;
                outbox.pipeline.reset();
                outbox.pending.drain().map(|(_, batch)| batch).collect()
            }
            None => Vec::new(),
        };
        for mut batch in held {
            batch.attempts += 1;
            self.retain_or_lose(delivery, batch, &error);
        }

        // A connection that worked and then failed fast is most often a
        // collector that restarted, so one attempt on a new connection goes at
        // once. A failure that used up the call deadline does not get one: a
        // second wait of that length is the caller's decision.
        let elapsed = self.now().saturating_duration_since(started);
        let fast = self.settings.call_timeout.is_zero() || elapsed < self.settings.call_timeout / 2;
        let retry_now = established && fast && !delivery.free_retry_used && delivery.failures == 0;
        self.note_failure(delivery, &error, retry_now);
        Failed { error, retry_now }
    }

    fn note_failure(&self, delivery: &mut Delivery, error: &TallyOwlError, retry_now: bool) {
        delivery.last_failure = Some(error.clone());
        *self.last_error.lock().expect("driver lock") = Some(error.clone());
        if retry_now {
            delivery.free_retry_used = true;
            delivery.next_attempt = None;
            return;
        }
        delivery.failures = delivery.failures.saturating_add(1);
        let wait = backoff_delay(
            delivery.failures,
            self.settings.retry_backoff_min,
            self.settings.retry_backoff_max,
            (self.random)(),
        );
        delivery.next_attempt = Some(self.now() + wait);
    }

    /// Put a batch back among the retained ones, oldest first.
    fn retain(&self, delivery: &mut Delivery, batch: Pending) {
        let at = delivery
            .retained
            .iter()
            .position(|held| held.order > batch.order)
            .unwrap_or(delivery.retained.len());
        delivery.retained.insert(at, batch);
    }

    fn retain_or_lose(&self, delivery: &mut Delivery, batch: Pending, error: &TallyOwlError) {
        if batch.attempts >= self.settings.max_batch_attempts {
            let reason = TallyOwlError::unavailable(format!(
                "A batch left {} times and no answer came back. {}",
                batch.attempts, error.message
            ));
            self.lose(delivery, &batch, &reason);
        } else {
            self.retain(delivery, batch);
        }
    }

    fn acknowledged(&self, delivery: &mut Delivery, batch: &Pending, receipt: &Receipt) {
        self.settle(batch);
        self.counters
            .accepted
            .fetch_add(receipt.accepted, Ordering::SeqCst);
        self.counters
            .rejected
            .fetch_add(receipt.rejected.len() as u64, Ordering::SeqCst);
        delivery.failures = 0;
        delivery.free_retry_used = false;
        delivery.next_attempt = None;
    }

    /// Give a batch up. Every item in it is counted, and the host is told.
    fn lose(&self, delivery: &mut Delivery, batch: &Pending, reason: &TallyOwlError) {
        self.settle(batch);
        self.counters
            .lost
            .fetch_add(batch.items as u64, Ordering::SeqCst);
        let mut note = TallyOwlError::new(
            reason.code,
            format!(
                "{} events were not recorded, and the app driver will not send them again. {}",
                batch.items, reason.message
            ),
        )
        .retryable(false);
        note.detail = reason.detail.clone();
        delivery.notes.push(note);
    }

    /// Take a batch off the unacknowledged account.
    fn settle(&self, batch: &Pending) {
        self.counters
            .sealed_bytes
            .fetch_sub(batch.bytes, Ordering::SeqCst);
        self.counters
            .unacknowledged_items
            .fetch_sub(batch.items, Ordering::SeqCst);
    }

    /// Send what is left until the deadline. `shutdown` runs this.
    fn finish(&self, deadline: Instant) {
        loop {
            let left = self.buffered() + self.counters.unacknowledged_items.load(Ordering::SeqCst);
            if left == 0 || Instant::now() >= deadline {
                return;
            }
            let outcome = self
                .with_delivery(|inner, delivery| inner.drain(delivery))
                .and_then(|_| self.with_delivery(|inner, delivery| inner.flush(delivery)));
            let Err(error) = outcome else {
                continue;
            };
            // An error with no wait is a batch the driver gave up, or the one
            // attempt without a wait. Either way the next turn makes progress.
            let Some(wait) = retry_after(&error) else {
                continue;
            };
            if Instant::now() + wait >= deadline {
                return;
            }
            std::thread::sleep(wait);
        }
    }
}

/// Read one collector reply into a receipt, or into the reason there is none.
fn read_receipt(
    batch_id: [u8; 16],
    response: &tallyowl_rpc::Response,
) -> Result<Receipt, ReceiptFailure> {
    if response.status != TransportStatus::Ok {
        let said = response.error.clone().unwrap_or_default();
        return Err(match response.status {
            TransportStatus::Unavailable | TransportStatus::DeadlineExceeded => {
                ReceiptFailure::Refused(TallyOwlError::unavailable(format!(
                    "The collector could not take the batch now. {said}"
                )))
            }
            TransportStatus::Unauthenticated => ReceiptFailure::Refused(unauthenticated(&said)),
            TransportStatus::Forbidden => ReceiptFailure::Refused(TallyOwlError::new(
                ErrorCode::PermissionDenied,
                format!("The collector refused the batch. {said}"),
            )),
            // The collector failed while it held the batch, so what it did with
            // the batch is unknown. A batch that makes the collector fail every
            // time must not go again without limit.
            TransportStatus::Internal => ReceiptFailure::Ambiguous(TallyOwlError::internal(
                format!("The collector failed while it handled the batch. {said}"),
            )),
            _ => ReceiptFailure::Refused(TallyOwlError::invalid_argument(format!(
                "The collector could not read the batch, and it will not read it on another attempt either. {said}"
            ))),
        });
    }

    if response.variant.as_deref() == Some(SERVICE_ERROR_VARIANT) {
        let error = decode_service_error(&response.payload).map_err(|e| {
            ReceiptFailure::Ambiguous(TallyOwlError::internal(format!(
                "The collector returned an error we could not read: {e}"
            )))
        })?;
        let code = from_wire(&error.code);
        let failure = if code == ErrorCode::Unauthenticated {
            unauthenticated(&error.message)
        } else {
            TallyOwlError::new(code, error.message).retryable(error.retryable)
        };
        return Err(ReceiptFailure::Refused(failure));
    }

    let receipt = decode_submit_batch_response(&response.payload).map_err(|e| {
        ReceiptFailure::Ambiguous(TallyOwlError::internal(format!(
            "The collector returned a receipt we could not read: {e}"
        )))
    })?;

    Ok(Receipt {
        batch_id,
        accepted: receipt.accepted,
        durable_copies: receipt.durable_copies,
        rejected: receipt
            .rejected
            .unwrap_or_default()
            .into_iter()
            .map(|r| Rejected {
                event_id: r.event_id,
                code: from_wire(&r.code),
                message: r.message,
            })
            .collect(),
    })
}

/// The collector did not accept the key. The message names the key, because
/// that is the thing the developer has to change.
fn unauthenticated(said: &str) -> TallyOwlError {
    TallyOwlError::new(
        ErrorCode::Unauthenticated,
        format!(
            "The collector did not accept the key this application holds. Check the credential in the driver settings, and ask the person who runs TallyOwl whether the key was revoked. {said}"
        ),
    )
    .retryable(false)
}

fn from_wire(code: &tallyowl_collector_api::types::ErrorCode) -> ErrorCode {
    use tallyowl_collector_api::types::ErrorCode as Wire;
    match code {
        Wire::InvalidArgument => ErrorCode::InvalidArgument,
        Wire::Unauthenticated => ErrorCode::Unauthenticated,
        Wire::PermissionDenied => ErrorCode::PermissionDenied,
        Wire::NotFound => ErrorCode::NotFound,
        Wire::AlreadyExists => ErrorCode::AlreadyExists,
        Wire::ResourceExhausted => ErrorCode::ResourceExhausted,
        Wire::FailedPrecondition => ErrorCode::FailedPrecondition,
        Wire::Unavailable => ErrorCode::Unavailable,
        Wire::SchemaUnsupported => ErrorCode::SchemaUnsupported,
        Wire::BudgetExceeded => ErrorCode::BudgetExceeded,
        Wire::IncompleteResult => ErrorCode::IncompleteResult,
        Wire::Internal => ErrorCode::Internal,
    }
}

/// One item as one line, for a dry run.
///
/// The line names what a developer checks: the kind, the identifiers that
/// correlate, and each property with its origin. It is a line for a person to
/// read, not a format to parse.
fn describe(item: &TelemetryItem) -> String {
    let envelope = &item.envelope;
    let kind = items::payload(item)
        .map(|payload| payload.kind_name())
        .unwrap_or("unreadable");
    let mut line = format!(
        "tallyowl dry-run kind={kind} event_id={} occurred_at={}",
        hex(&envelope.event_id),
        envelope.occurred_at
    );
    let name = item
        .event
        .as_ref()
        .map(|p| p.name.as_str())
        .or(item.page_view.as_ref().map(|p| p.route.as_str()))
        .or(item.conversion.as_ref().map(|p| p.goal.as_str()))
        .or(item.error.as_ref().map(|p| p.error_type.as_str()))
        .or(item.span.as_ref().map(|p| p.operation.as_str()))
        .or(item.metric_point.as_ref().map(|p| p.metric_name.as_str()));
    if let Some(name) = name {
        line.push_str(&format!(" name={name:?}"));
    }
    for (label, value) in [
        ("session_id", &envelope.session_id),
        ("request_id", &envelope.request_id),
        ("end_user_id", &envelope.end_user_id),
        ("anonymous_id", &envelope.anonymous_id),
        ("service", &envelope.service_name),
        ("release", &envelope.release),
    ] {
        if let Some(value) = value {
            line.push_str(&format!(" {label}={value:?}"));
        }
    }
    if let Some(trace_id) = &envelope.trace_id {
        line.push_str(&format!(" trace_id={}", hex(trace_id)));
    }
    if let Some(span_id) = &envelope.span_id {
        line.push_str(&format!(" span_id={}", hex(span_id)));
    }
    for property in &envelope.properties {
        let value = match wire::read(&property.value) {
            // A decimal is written as its parts, so a line never depends on
            // how long the number is.
            Ok(Value::Decimal { exponent, mantissa }) => format!("{mantissa}e{exponent}"),
            Ok(Value::Text(text)) => format!("{text:?}"),
            Ok(value) => value.to_display(),
            Err(_) => "unreadable".to_string(),
        };
        line.push_str(&format!(
            " {}.{}={value}",
            wire::origin_name(&property.origin),
            property.key
        ));
    }
    line
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
