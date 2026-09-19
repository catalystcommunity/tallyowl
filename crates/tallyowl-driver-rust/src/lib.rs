//! The Rust app driver.
//!
//! The generated client provides types, codecs, and routing seams. This driver
//! provides buffering, batching, configuration, and host integration. It is an
//! app driver: never an adapter, and never an SDK.
//!
//! # The promise, and the one thing it will not do
//!
//! `capture` buffers. `flush` returns only after collector intake acknowledges,
//! and that acknowledgement means Corndogs durably accepted the batch. Nothing
//! here reports success for data it discarded.
//!
//! When the unacknowledged bound is reached, a durable send returns a typed
//! backpressure error. It does not silently become best effort. See D19 and
//! `docs/DELIVERY.md` section 8.
//!
//! # Defaults
//!
//! D19 sets these, and every one is configurable:
//!
//! | Setting | Default |
//! | --- | --- |
//! | Seal a batch at | 256 items |
//! | Seal a batch at | 512 KiB |
//! | Seal a batch after | 100 ms |
//! | Maximum ordinary frame | 1 MiB |
//! | Unacknowledged data on one connection | 8 MiB |
//! | Shutdown flush deadline | 2 s |
//!
//! A critical event seals the current batch immediately, because the events
//! worth waiting for are the ones worth not losing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tallyowl_collector_api::codec::{
    decode_service_error, decode_submit_batch_response, encode_batch, encode_submit_batch_request,
    encode_telemetry_item,
};
use tallyowl_collector_api::types::{
    Batch, ConversionPayload, Envelope, ErrorPayload, EventPayload, PageViewPayload,
    PropertyOrigin, SessionEndPayload, SessionEndPayload_reason, SessionStartPayload, SpanKind,
    SpanPayload, SubmitBatchRequest, TelemetryItem, TelemetryKind,
};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_obs::time::now_ms;
use tallyowl_rpc::{Client, SERVICE_ERROR_VARIANT};
use tallyowl_wire::{collector as wire, collector_items_bridge as items, Value};

pub mod metrics;

pub const SDK_NAME: &str = "tallyowl-driver-rust";
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The protocol version this driver speaks. It travels on every batch, and the
/// collector and the head each accept it and the version before it. See
/// `tallyowl_wire::protocol` and D31.
pub use tallyowl_wire::protocol::PROTOCOL_VERSION;
const SERVICE: &str = "TallyOwlCollector";

/// Driver settings. The defaults are D19's.
#[derive(Debug, Clone)]
pub struct Settings {
    pub collector_address: String,
    pub credential: String,
    pub max_items: usize,
    pub max_batch_bytes: usize,
    pub linger: Duration,
    pub max_frame_bytes: usize,
    pub max_unacknowledged_bytes: usize,
    pub shutdown_flush_deadline: Duration,
    /// How many sealed batches may be outstanding at one time on the pipelined
    /// path. `docs/DELIVERY.md` section 3 permits "a configured number of
    /// correlated batch calls"; this is that number.
    ///
    /// One reproduces the synchronous behaviour. The default is four, which is
    /// what `submit` needs to stop being bounded by the round trip.
    pub max_in_flight_batches: usize,
    /// How many times a batch is sent again after a connection failure before
    /// the driver reports it as lost.
    pub max_batch_attempts: u32,
    /// Properties this driver adds to every item. Their origin is `driver`.
    pub properties: Vec<(String, String)>,
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
            max_batch_attempts: 3,
            properties: Vec::new(),
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
}

/// What one flush produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub batch_id: [u8; 16],
    pub accepted: u64,
    /// The durable copies the collector reported. A caller that needs a
    /// stronger boundary reads this rather than assuming one.
    pub durable_copies: u64,
    pub rejected: Vec<(Vec<u8>, String)>,
}

/// A telemetry item, before the driver seals it into a batch.
pub struct Capture {
    item: TelemetryItem,
    critical: bool,
}

impl Capture {
    /// A named product or behavior event.
    pub fn event(name: &str) -> Capture {
        Capture {
            item: items::event(
                envelope(TelemetryKind::Event),
                EventPayload {
                    name: name.to_string(),
                    route: None,
                    page_title: None,
                },
            ),
            critical: false,
        }
    }

    /// A page view or a screen view.
    pub fn page_view(route: &str) -> Capture {
        Capture {
            item: items::page_view(
                envelope(TelemetryKind::PageView),
                PageViewPayload {
                    route: route.to_string(),
                    page_title: None,
                    referrer: None,
                    campaign: None,
                },
            ),
            critical: false,
        }
    }

    /// The start of a session. The client library issues the ID; a person
    /// cannot select it. See D11.
    pub fn session_start(session_id: &str) -> Capture {
        let mut item = items::session_start(
            envelope(TelemetryKind::SessionStart),
            SessionStartPayload { entry_route: None },
        );
        item.envelope.session_id = Some(session_id.to_string());
        Capture {
            item,
            critical: false,
        }
    }

    /// The end of a session, and why it ended.
    pub fn session_end(session_id: &str, reason: SessionEndPayload_reason) -> Capture {
        let mut item = items::session_end(
            envelope(TelemetryKind::SessionEnd),
            SessionEndPayload { reason },
        );
        item.envelope.session_id = Some(session_id.to_string());
        Capture {
            item,
            critical: false,
        }
    }

    /// A conversion. Money travels as an exact decimal and never as a float, so
    /// the value arrives as its canonical text and is refused if it is not a
    /// number.
    pub fn conversion(goal: &str, value: Option<&str>, currency: Option<&str>) -> Capture {
        Capture::order(goal, value, currency, None)
    }

    /// A conversion that names its order.
    ///
    /// **This is the idempotent form and an application with orders should use
    /// it.** A checkout that retried, a webhook that arrived twice, and a person
    /// who refreshed the receipt page all produce the same order, and TallyOwl
    /// counts one conversion and one value for one goal and order pair. Without
    /// an order identifier a repeat is a second conversion, because nothing says
    /// otherwise.
    pub fn order(
        goal: &str,
        value: Option<&str>,
        currency: Option<&str>,
        order_id: Option<&str>,
    ) -> Capture {
        let decimal = value.and_then(Value::decimal_from_text).map(|v| match v {
            Value::Decimal { exponent, mantissa } => {
                tallyowl_collector_api::types::CsilDecimal { exponent, mantissa }
            }
            _ => unreachable!("decimal_from_text returns a decimal or nothing"),
        });
        Capture {
            item: items::conversion(
                envelope(TelemetryKind::Conversion),
                ConversionPayload {
                    goal: goal.to_string(),
                    value: decimal,
                    currency: currency.map(|c| c.to_string()),
                    order_id: order_id.map(|id| id.to_string()),
                    campaign: None,
                    touch_event_id: None,
                },
            ),
            // A conversion is the first priority class in DELIVERY.md section 8,
            // so it seals its batch rather than waiting behind page views.
            critical: true,
        }
    }

    /// An error occurrence. The producer never supplies a group; the projector
    /// computes the fingerprint. See D39.
    pub fn error(error_type: &str, message: &str, handled: bool) -> Capture {
        use tallyowl_collector_api::types::ErrorPayload_severity;
        Capture {
            item: items::error(
                envelope(TelemetryKind::Error),
                ErrorPayload {
                    error_type: error_type.to_string(),
                    message: message.to_string(),
                    handled,
                    severity: if handled {
                        ErrorPayload_severity::Error
                    } else {
                        ErrorPayload_severity::Fatal
                    },
                    mechanism: None,
                    runtime: None,
                    frames: None,
                    breadcrumbs: None,
                },
            ),
            // An unhandled error is priority class two, and a handled one is
            // class three. Only the unhandled one seals the batch.
            critical: !handled,
        }
    }

    /// One span of a trace.
    ///
    /// The span's own identity comes from the context, so a caller passes the
    /// context it already has rather than assembling three identifiers. The
    /// head applies tail sampling after commit; this side applies none.
    pub fn span(
        context: &SpanContext,
        operation: &str,
        kind: SpanKind,
        start_at: i64,
        duration_ms: i64,
    ) -> Capture {
        use tallyowl_collector_api::types::SpanPayload_status;
        let mut envelope = envelope(TelemetryKind::Span);
        envelope.occurred_at = start_at;
        envelope.trace_id = Some(context.trace_id.to_vec());
        envelope.span_id = Some(context.span_id.to_vec());
        Capture {
            item: items::span(
                envelope,
                SpanPayload {
                    operation: operation.to_string(),
                    kind,
                    start_at,
                    duration_ms,
                    status: SpanPayload_status::Ok,
                    resource: None,
                    parent_span_id: context.parent_span_id.map(|id| id.to_vec()),
                    links: None,
                    error_event_id: None,
                    sampling_reason: None,
                },
            ),
            // A diagnostic span is the lowest priority class. It never seals a
            // batch, because a batch sealed by a span would send one span at a
            // time under load, which is the opposite of what it needs.
            critical: false,
        }
    }

    /// Mark this span as failed, and link it to the error that explains it.
    ///
    /// The link goes both ways without a join: the error carries the trace ID,
    /// and the span carries the error's event ID.
    pub fn failed(mut self, error_event_id: Option<[u8; 16]>) -> Capture {
        use tallyowl_collector_api::types::SpanPayload_status;
        if let Some(span) = self.item.span.as_mut() {
            span.status = SpanPayload_status::Error;
            span.error_event_id = error_event_id.map(|id| id.to_vec());
        }
        self
    }

    /// The stack this error carried.
    ///
    /// The frames decide the group, so a producer that has them should send
    /// them: without frames the projector falls back to the message, which
    /// groups less precisely. See D39.
    pub fn with_frames(mut self, frames: Vec<Frame>) -> Capture {
        if let Some(error) = self.item.error.as_mut() {
            error.frames = Some(frames.into_iter().map(Frame::into_wire).collect());
        }
        self
    }

    /// Put this item in a trace.
    pub fn in_span(mut self, context: &SpanContext) -> Capture {
        self.item.envelope.trace_id = Some(context.trace_id.to_vec());
        self.item.envelope.span_id = Some(context.span_id.to_vec());
        self
    }

    /// Seal the current batch as soon as this item enters it. A conversion and
    /// an unhandled error are the ordinary reasons.
    pub fn critical(mut self) -> Capture {
        self.critical = true;
        self
    }

    pub fn at(mut self, occurred_at: i64) -> Capture {
        self.item.envelope.occurred_at = occurred_at;
        self
    }

    pub fn with_event_id(mut self, event_id: [u8; 16]) -> Capture {
        self.item.envelope.event_id = event_id.to_vec();
        self
    }

    pub fn with_session(mut self, session_id: &str) -> Capture {
        self.item.envelope.session_id = Some(session_id.to_string());
        self
    }

    pub fn with_request(mut self, request_id: &str) -> Capture {
        self.item.envelope.request_id = Some(request_id.to_string());
        self
    }

    pub fn with_trace(mut self, trace_id: [u8; 16]) -> Capture {
        self.item.envelope.trace_id = Some(trace_id.to_vec());
        self
    }

    pub fn with_service(mut self, service_name: &str) -> Capture {
        self.item.envelope.service_name = Some(service_name.to_string());
        self
    }

    pub fn with_release(mut self, release: &str) -> Capture {
        self.item.envelope.release = Some(release.to_string());
        self
    }

    /// A typed property from the calling code, at the event site.
    pub fn with_property(mut self, key: &str, value: Value) -> Capture {
        self.item
            .envelope
            .properties
            .push(wire::property(key, value, PropertyOrigin::Client));
        self
    }

    pub fn event_id(&self) -> Vec<u8> {
        self.item.envelope.event_id.clone()
    }

    /// The item this capture will send. The meter builds a metric point and
    /// hands it here rather than repeating the buffering rules.
    pub(crate) fn from_item(item: TelemetryItem) -> Capture {
        Capture {
            item,
            // A metric point is priority class three in DELIVERY.md section 8.
            // It never seals a batch: a snapshot of ten thousand series that
            // sealed on the first point would send ten thousand batches.
            critical: false,
        }
    }

    /// Read the item, for a test and for a host that inspects before sending.
    pub fn item(&self) -> &TelemetryItem {
        &self.item
    }
}

pub(crate) fn envelope(kind: TelemetryKind) -> Envelope {
    Envelope {
        event_id: new_event_id(),
        kind,
        schema_version: 1,
        occurred_at: now_ms(),
        observed_at: None,
        // The collector stamps the receive time and the tenancy. A driver that
        // set them would be claiming something it cannot know.
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
        sdk_name: SDK_NAME.to_string(),
        sdk_version: SDK_VERSION.to_string(),
        properties: Vec::new(),
        measurements: None,
    }
}

/// The app driver.
pub struct Driver {
    settings: Settings,
    client: Client,
    buffer: Mutex<Buffer>,
    sequence: AtomicU64,
    /// The pipelined path. It opens on the first `submit` and stays closed for
    /// an application that only ever calls `flush`, so an application keeps one
    /// connection either way.
    outbox: Mutex<Option<Outbox>>,
}

#[derive(Default)]
struct Buffer {
    items: Vec<TelemetryItem>,
    bytes: usize,
    opened_at: Option<Instant>,
    seal_now: bool,
}

/// One sealed batch, retained until it is acknowledged.
///
/// D5: "The app retains the stable batch until that acknowledgement, then moves
/// on." Retaining the encoded frame is what lets a connection failure be a
/// resend rather than a loss, and the batch ID does not change across it, so
/// final storage still deduplicates to one logical commit.
struct Pending {
    batch_id: [u8; 16],
    encoded: Vec<u8>,
    /// How many items this batch holds, so a shutdown reports unsent items
    /// rather than unsent batches.
    items: usize,
    attempts: u32,
}

/// The pipelined connection and everything outstanding on it.
struct Outbox {
    pipeline: tallyowl_rpc::Pipeline,
    pending: HashMap<u64, Pending>,
}

/// What a batch that never reached the durable boundary cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LostBatch {
    pub batch_id: [u8; 16],
    pub attempts: u32,
}

impl Driver {
    pub fn new(settings: Settings) -> Driver {
        // The credential rides on the connection, so every batch carries it.
        // The collector resolves tenancy from it and stamps the result; the
        // driver never sends a workspace, a project, or a source.
        let client = Client::new(settings.collector_address.clone(), settings.max_frame_bytes)
            .with_credential(settings.credential.clone());
        Driver {
            settings,
            client,
            buffer: Mutex::new(Buffer::default()),
            sequence: AtomicU64::new(0),
            outbox: Mutex::new(None),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Buffer one item. This does not reach the collector, and it makes no
    /// durability claim.
    ///
    /// It returns a typed backpressure error rather than accepting data it would
    /// then drop, because a driver that reports success for a discarded event
    /// makes every count downstream wrong in a way nobody can find.
    pub fn capture(&self, capture: Capture) -> Result<(), TallyOwlError> {
        let mut item = capture.item;
        item.envelope.sequence = Some(self.sequence.fetch_add(1, Ordering::Relaxed));
        for (key, value) in &self.settings.properties {
            item.envelope.properties.push(wire::property(
                key,
                Value::Text(value.clone()),
                PropertyOrigin::Driver,
            ));
        }

        let size = encode_telemetry_item(&item).len();
        let mut buffer = self.buffer.lock().expect("driver lock");

        if buffer.bytes + size > self.settings.max_unacknowledged_bytes {
            return Err(TallyOwlError::new(
                ErrorCode::ResourceExhausted,
                "This application is producing telemetry faster than TallyOwl is accepting it. The event was not recorded. Send fewer events, or give the collector more capacity.",
            )
            .retryable(true));
        }

        if buffer.opened_at.is_none() {
            buffer.opened_at = Some(Instant::now());
        }
        buffer.bytes += size;
        buffer.items.push(item);
        if capture.critical {
            buffer.seal_now = true;
        }
        Ok(())
    }

    /// Take one snapshot of a meter and buffer every point it produced.
    ///
    /// A host calls this on its own period. The driver does not own a timer,
    /// because a host that already has one would then have two, and the two
    /// would disagree about when a period ended.
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
        let buffer = self.buffer.lock().expect("driver lock");
        seal_reached(&buffer, &self.settings)
    }

    pub fn buffered(&self) -> usize {
        self.buffer.lock().expect("driver lock").items.len()
    }

    /// Send everything buffered and wait for the durable acknowledgement.
    ///
    /// Returns `Ok(None)` when there was nothing to send.
    pub fn flush(&self) -> Result<Option<Receipt>, TallyOwlError> {
        let Some(batch) = self.seal()? else {
            return Ok(None);
        };
        let response = self.client.call(SERVICE, "submit-batch", batch.encoded)?;
        read_receipt(batch.batch_id, &response).map(Some)
    }

    /// Seal the buffer and take the encoded frame, or nothing when the buffer
    /// is empty.
    fn seal(&self) -> Result<Option<Pending>, TallyOwlError> {
        let items = {
            let mut buffer = self.buffer.lock().expect("driver lock");
            if buffer.items.is_empty() {
                return Ok(None);
            }
            let items = std::mem::take(&mut buffer.items);
            buffer.bytes = 0;
            buffer.opened_at = None;
            buffer.seal_now = false;
            items
        };

        // The batch keeps this ID across every retry and across a lost
        // connection, so final storage deduplicates it to one logical commit.
        let batch_id = new_batch_id();
        let item_count = items.len();
        let request = SubmitBatchRequest {
            batch: Batch {
                batch_id: batch_id.to_vec(),
                items,
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

        let encoded = encode_submit_batch_request(&request);
        if encoded.len() > self.settings.max_frame_bytes {
            return Err(TallyOwlError::over_limit(
                "Batch",
                &format!("{} KiB", encoded.len() / 1024),
                &format!("{} KiB", self.settings.max_frame_bytes / 1024),
                "Seal a smaller batch, or raise the frame limit for this driver.",
            ));
        }
        Ok(Some(Pending {
            batch_id,
            encoded,
            items: item_count,
            attempts: 0,
        }))
    }

    /// Seal the buffer and send it without waiting for its acknowledgement.
    ///
    /// This is the pipelined path, and it is the one an application with a
    /// single telemetry worker wants. `flush` waits for the durable receipt of
    /// the batch it sealed, so one worker is bounded by the round trip rather
    /// than by anything in TallyOwl: at the D19 defaults that is about 5,400
    /// events each second against a measured ceiling of 36,525. See
    /// `docs/ALPHA_REPORT.md` section 3.3.
    ///
    /// The returned receipts are for batches that finished while this one was
    /// making room in the window. It is normal for the list to be empty, and it
    /// is normal for a receipt to arrive for a batch sealed several calls ago.
    /// Nothing here reports a batch acknowledged before the collector did.
    pub fn submit(&self) -> Result<Vec<Receipt>, TallyOwlError> {
        let Some(batch) = self.seal()? else {
            return Ok(Vec::new());
        };

        let mut guard = self.outbox.lock().expect("driver lock");
        let outbox = guard.get_or_insert_with(|| Outbox {
            pipeline: tallyowl_rpc::Pipeline::new(
                self.settings.collector_address.clone(),
                self.settings.max_frame_bytes,
                self.settings.max_in_flight_batches,
            )
            .with_credential(self.settings.credential.clone()),
            pending: HashMap::new(),
        });

        let mut receipts = Vec::new();
        // Make room in the window before adding to it. Collecting a receipt is
        // what frees a slot, so a caller that never collects never sends.
        while !outbox.pipeline.has_room() {
            receipts.push(self.collect_one(outbox)?);
        }
        self.send_pending(outbox, batch)?;
        Ok(receipts)
    }

    /// Wait for every outstanding batch and return its receipt.
    pub fn drain(&self) -> Result<Vec<Receipt>, TallyOwlError> {
        let mut guard = self.outbox.lock().expect("driver lock");
        let Some(outbox) = guard.as_mut() else {
            return Ok(Vec::new());
        };
        let mut receipts = Vec::new();
        while !outbox.pending.is_empty() {
            receipts.push(self.collect_one(outbox)?);
        }
        Ok(receipts)
    }

    /// How many sealed batches are waiting for an acknowledgement.
    pub fn outstanding(&self) -> usize {
        self.outbox
            .lock()
            .expect("driver lock")
            .as_ref()
            .map(|outbox| outbox.pending.len())
            .unwrap_or(0)
    }

    /// How many items sit in batches that are waiting for an acknowledgement.
    pub fn outstanding_items(&self) -> usize {
        self.outbox
            .lock()
            .expect("driver lock")
            .as_ref()
            .map(|outbox| outbox.pending.values().map(|p| p.items).sum())
            .unwrap_or(0)
    }

    /// Send one batch, and send every retained batch again if the connection
    /// failed under it.
    fn send_pending(&self, outbox: &mut Outbox, batch: Pending) -> Result<(), TallyOwlError> {
        let mut queue = vec![batch];
        loop {
            let Some(mut batch) = queue.pop() else {
                return Ok(());
            };
            batch.attempts += 1;
            match outbox
                .pipeline
                .send(SERVICE, "submit-batch", batch.encoded.clone())
            {
                Ok(id) => {
                    outbox.pending.insert(id, batch);
                }
                Err(e) => {
                    // The connection took everything outstanding with it. Each
                    // retained batch keeps its ID, so sending it again stays one
                    // logical commit.
                    let mut lost = Vec::new();
                    for (_, held) in outbox.pending.drain() {
                        if held.attempts >= self.settings.max_batch_attempts {
                            lost.push(LostBatch {
                                batch_id: held.batch_id,
                                attempts: held.attempts,
                            });
                        } else {
                            queue.push(held);
                        }
                    }
                    if batch.attempts >= self.settings.max_batch_attempts {
                        lost.push(LostBatch {
                            batch_id: batch.batch_id,
                            attempts: batch.attempts,
                        });
                    } else {
                        queue.push(batch);
                    }
                    if !lost.is_empty() {
                        outbox.pipeline.reset();
                        return Err(TallyOwlError::unavailable(format!(
                            "{} batches were not recorded after {} attempts each. {e}",
                            lost.len(),
                            self.settings.max_batch_attempts
                        )));
                    }
                }
            }
        }
    }

    /// Wait for the next acknowledgement and match it to the batch it answers.
    fn collect_one(&self, outbox: &mut Outbox) -> Result<Receipt, TallyOwlError> {
        loop {
            match outbox.pipeline.recv() {
                Ok(Some((id, response))) => {
                    let Some(batch) = outbox.pending.remove(&id) else {
                        // A reply for a call this driver never made. The
                        // connection is no longer trustworthy.
                        outbox.pipeline.reset();
                        outbox.pending.clear();
                        return Err(TallyOwlError::internal(
                            "The collector answered a batch this application did not send."
                                .to_string(),
                        ));
                    };
                    return read_receipt(batch.batch_id, &response);
                }
                Ok(None) => {
                    return Err(TallyOwlError::internal(
                        "There was no batch waiting for an acknowledgement.".to_string(),
                    ))
                }
                Err(e) => {
                    // Everything outstanding goes again on a fresh connection.
                    let held: Vec<Pending> = outbox.pending.drain().map(|(_, p)| p).collect();
                    if held.is_empty() {
                        return Err(e);
                    }
                    for batch in held {
                        if batch.attempts >= self.settings.max_batch_attempts {
                            return Err(TallyOwlError::unavailable(format!(
                                "A batch was not recorded after {} attempts. {e}",
                                self.settings.max_batch_attempts
                            )));
                        }
                        self.send_pending(outbox, batch)?;
                    }
                }
            }
        }
    }

    /// Flush when a seal condition is reached, and do nothing otherwise.
    pub fn flush_if_sealed(&self) -> Result<Option<Receipt>, TallyOwlError> {
        if self.should_flush() {
            self.flush()
        } else {
            Ok(None)
        }
    }

    /// Stop accepting, drain until the deadline, and report what did not go.
    ///
    /// The count of unsent items is the return value, because a shutdown that
    /// reports nothing is a shutdown that hides data loss.
    pub fn shutdown(&self) -> usize {
        let deadline = Instant::now() + self.settings.shutdown_flush_deadline;
        // A batch that was already sent is not in the buffer, so a shutdown that
        // ignored the pipeline would report zero unsent items while several
        // batches were still waiting for their acknowledgement.
        let unacknowledged = if self.outstanding() > 0 {
            match self.drain() {
                Ok(_) => 0,
                Err(_) => self.outstanding_items(),
            }
        } else {
            0
        };
        while Instant::now() < deadline {
            match self.flush() {
                Ok(None) => return unacknowledged,
                Ok(Some(_)) => {
                    if self.buffered() == 0 {
                        return unacknowledged;
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        self.buffered() + unacknowledged
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
}

/// Read one collector reply into a receipt, or into the typed error it carried.
fn read_receipt(
    batch_id: [u8; 16],
    response: &tallyowl_rpc::Response,
) -> Result<Receipt, TallyOwlError> {
    if response.variant.as_deref() == Some(SERVICE_ERROR_VARIANT) {
        let error = decode_service_error(&response.payload).map_err(|e| {
            TallyOwlError::internal(format!(
                "The collector returned an error we could not read: {e}"
            ))
        })?;
        return Err(
            TallyOwlError::new(from_wire(&error.code), error.message).retryable(error.retryable)
        );
    }

    let receipt = decode_submit_batch_response(&response.payload).map_err(|e| {
        TallyOwlError::internal(format!(
            "The collector returned a receipt we could not read: {e}"
        ))
    })?;

    Ok(Receipt {
        batch_id,
        accepted: receipt.accepted,
        durable_copies: receipt.durable_copies,
        rejected: receipt
            .rejected
            .unwrap_or_default()
            .into_iter()
            .map(|r| (r.event_id, r.message))
            .collect(),
    })
}

fn seal_reached(buffer: &Buffer, settings: &Settings) -> bool {
    if buffer.items.is_empty() {
        return false;
    }
    if buffer.seal_now {
        return true;
    }
    if buffer.items.len() >= settings.max_items {
        return true;
    }
    if buffer.bytes >= settings.max_batch_bytes {
        return true;
    }
    buffer
        .opened_at
        .map(|opened| opened.elapsed() >= settings.linger)
        .unwrap_or(false)
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

/// A UUIDv7: 48 bits of milliseconds, then randomness, with the version and
/// variant bits set. Time-ordered, so a segment sorts by it for free.
pub fn new_event_id() -> Vec<u8> {
    uuid_v7().to_vec()
}

pub fn new_batch_id() -> [u8; 16] {
    uuid_v7()
}

fn uuid_v7() -> [u8; 16] {
    let ms = now_ms() as u64;
    let mut bytes = [0u8; 16];
    bytes[0..6].copy_from_slice(&ms.to_be_bytes()[2..8]);
    let random = process_random();
    bytes[6..16].copy_from_slice(&random[0..10]);
    // Version 7 in the high nibble of byte 6.
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    // Variant 10 in the top bits of byte 8.
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
}

/// Ten bytes of randomness.
///
/// This mixes a per-process seed with a monotonic counter. It is not a
/// cryptographic generator and does not need to be: it separates IDs produced in
/// the same millisecond by the same process, and D9 forbids an ID that carries
/// meaning anyway.
fn process_random() -> [u8; 10] {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

    let seed = *SEED.get_or_init(|| {
        let mut hash = 0xcbf29ce484222325u64;
        for value in [
            std::process::id() as u64,
            tallyowl_obs::time::now_nanos() as u64,
            &COUNTER as *const _ as u64,
        ] {
            hash ^= value;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    });

    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut state = seed ^ counter.wrapping_mul(0x9e3779b97f4a7c15);
    // A short xorshift, so consecutive counter values do not give adjacent IDs.
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;

    let high = state.to_be_bytes();
    let low = (state ^ counter).to_be_bytes();
    let mut out = [0u8; 10];
    out[0..8].copy_from_slice(&high);
    out[8..10].copy_from_slice(&low[0..2]);
    out
}

/// A session, in the shape D11 requires. The client library issues the ID; a
/// person cannot select it. It works on every surface, including a terminal user
/// interface, because it assumes no cookie and no browser storage.
/// One stack frame, in the shape the group fingerprint reads.
///
/// `in_app` is the field that matters most. Two defects in one application
/// throw through the same framework, and a fingerprint over the top frames
/// alone would group them together. See D39.
#[derive(Debug, Clone)]
pub struct Frame {
    pub module: Option<String>,
    pub function: Option<String>,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub in_app: bool,
}

impl Frame {
    /// A frame in the application's own code.
    pub fn in_app(module: &str, function: &str) -> Frame {
        Frame {
            module: Some(module.to_string()),
            function: Some(function.to_string()),
            file: None,
            line: None,
            in_app: true,
        }
    }

    /// A frame in a library or the runtime.
    pub fn library(module: &str, function: &str) -> Frame {
        Frame {
            in_app: false,
            ..Frame::in_app(module, function)
        }
    }

    pub fn at(mut self, file: &str, line: u64) -> Frame {
        self.file = Some(file.to_string());
        self.line = Some(line);
        self
    }

    fn into_wire(self) -> tallyowl_collector_api::types::StackFrame {
        tallyowl_collector_api::types::StackFrame {
            module: self.module,
            function: self.function,
            file: self.file,
            line: self.line,
            in_app: self.in_app,
        }
    }
}

/// Where one span sits in its trace.
///
/// # Propagation
///
/// A trace crosses a service TallyOwl does not own, so the identifiers travel
/// in the W3C `traceparent` header form that other tracing systems already
/// read. The text form exists only at that boundary: `csil/types/common.csil`
/// says an ID travels as raw bytes on the ingest path, because 16 bytes costs
/// 53 percent less than 36 characters after encoding and compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanContext {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent_span_id: Option<[u8; 8]>,
    /// Whether the producer decided to record this trace. The head decides
    /// again after the trace is complete; this is the head-sampling half.
    pub sampled: bool,
}

impl SpanContext {
    /// Start a new trace.
    pub fn root() -> SpanContext {
        SpanContext {
            trace_id: random_bytes::<16>(),
            span_id: random_bytes::<8>(),
            parent_span_id: None,
            sampled: true,
        }
    }

    /// A span inside this one. The trace ID never changes down a trace: it is
    /// what routes every span of one trace to one tablet. See D16.
    pub fn child(&self) -> SpanContext {
        SpanContext {
            trace_id: self.trace_id,
            span_id: random_bytes::<8>(),
            parent_span_id: Some(self.span_id),
            sampled: self.sampled,
        }
    }

    /// The W3C `traceparent` header value for this context.
    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            hex(&self.trace_id),
            hex(&self.span_id),
            u8::from(self.sampled)
        )
    }

    /// Read an incoming `traceparent`.
    ///
    /// Returns `None` for anything this version cannot read, and a caller then
    /// starts a new trace. Continuing a trace from a header nobody could parse
    /// would join two unrelated traces, which is worse than starting one.
    pub fn from_traceparent(header: &str) -> Option<SpanContext> {
        let parts: Vec<&str> = header.trim().split('-').collect();
        if parts.len() < 4 || parts[0] != "00" {
            return None;
        }
        let trace_id: [u8; 16] = from_hex(parts[1])?.try_into().ok()?;
        let span_id: [u8; 8] = from_hex(parts[2])?.try_into().ok()?;
        // An all-zero identifier is the "not a trace" value in the W3C form.
        if trace_id == [0; 16] || span_id == [0; 8] {
            return None;
        }
        let flags = u8::from_str_radix(parts[3], 16).ok()?;
        Some(SpanContext {
            trace_id,
            // The incoming span is this one's parent. A service that reused the
            // incoming span ID would give two spans one identity and break
            // every waterfall that held them.
            span_id: random_bytes::<8>(),
            parent_span_id: Some(span_id),
            sampled: flags & 1 == 1,
        })
    }

    /// Continue an incoming trace, or start one when there is no usable header.
    pub fn continue_or_start(header: Option<&str>) -> SpanContext {
        header
            .and_then(SpanContext::from_traceparent)
            .unwrap_or_else(SpanContext::root)
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("the system random source");
    out
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

pub struct Session {
    id: String,
}

impl Session {
    pub fn start() -> Session {
        Session {
            id: hex(&new_event_id()),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Default for Session {
    fn default() -> Self {
        Session::start()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn settings() -> Settings {
        // Port 1 on the loopback refuses at once, so a test that expects no
        // network traffic fails loudly instead of hanging.
        Settings::new("127.0.0.1:1", "key-a")
    }

    #[test]
    fn an_identifier_is_a_uuid_version_7() {
        let id = uuid_v7();
        assert_eq!(id[6] >> 4, 7, "version 7");
        assert_eq!(id[8] >> 6, 0b10, "variant 10");
    }

    #[test]
    fn identifiers_do_not_repeat_inside_one_millisecond() {
        let ids: HashSet<[u8; 16]> = (0..10_000).map(|_| uuid_v7()).collect();
        assert_eq!(ids.len(), 10_000, "every identifier is distinct");
    }

    #[test]
    fn identifiers_sort_by_time() {
        let first = uuid_v7();
        std::thread::sleep(Duration::from_millis(2));
        let second = uuid_v7();
        assert!(first[0..6] < second[0..6]);
    }

    #[test]
    fn capture_buffers_and_does_not_send() {
        // The address is a closed port. If capture sent anything, this would
        // fail rather than pass.
        let driver = Driver::new(settings());
        driver.capture(Capture::event("checkout-started")).unwrap();
        driver
            .capture(Capture::event("checkout-completed"))
            .unwrap();
        assert_eq!(driver.buffered(), 2);
    }

    #[test]
    fn a_batch_seals_at_the_item_count() {
        let mut settings = settings();
        settings.max_items = 3;
        settings.linger = Duration::from_secs(3600);
        let driver = Driver::new(settings);

        for _ in 0..2 {
            driver.capture(Capture::event("a")).unwrap();
        }
        assert!(!driver.should_flush());
        driver.capture(Capture::event("a")).unwrap();
        assert!(driver.should_flush());
    }

    #[test]
    fn a_batch_seals_at_the_byte_count() {
        let mut settings = settings();
        settings.max_items = 100_000;
        settings.max_batch_bytes = 512;
        settings.linger = Duration::from_secs(3600);
        let driver = Driver::new(settings);

        while !driver.should_flush() {
            driver
                .capture(Capture::event("a-reasonably-long-name"))
                .unwrap();
            assert!(driver.buffered() < 1000, "the byte seal never fired");
        }
    }

    #[test]
    fn a_batch_seals_after_the_linger() {
        let mut settings = settings();
        settings.max_items = 100_000;
        settings.linger = Duration::from_millis(20);
        let driver = Driver::new(settings);

        driver.capture(Capture::event("a")).unwrap();
        assert!(!driver.should_flush());
        std::thread::sleep(Duration::from_millis(30));
        assert!(driver.should_flush());
    }

    #[test]
    fn a_critical_event_seals_the_batch_at_once() {
        // The events worth waiting for are the ones worth not losing.
        let mut settings = settings();
        settings.max_items = 100_000;
        settings.linger = Duration::from_secs(3600);
        let driver = Driver::new(settings);

        driver.capture(Capture::event("page-view")).unwrap();
        assert!(!driver.should_flush());
        driver
            .capture(Capture::event("purchase").critical())
            .unwrap();
        assert!(driver.should_flush());
    }

    #[test]
    fn an_empty_buffer_never_seals() {
        let driver = Driver::new(settings());
        assert!(!driver.should_flush());
        assert_eq!(driver.flush().unwrap(), None, "and a flush sends nothing");
    }

    #[test]
    fn a_full_buffer_returns_backpressure_rather_than_dropping_the_event() {
        // D19 and DELIVERY.md section 8: never silently become best effort, and
        // never return success for discarded data.
        let mut settings = settings();
        settings.max_unacknowledged_bytes = 400;
        let driver = Driver::new(settings);

        let mut accepted = 0;
        let mut failure = None;
        for _ in 0..100 {
            match driver.capture(Capture::event("a-reasonably-long-event-name")) {
                Ok(()) => accepted += 1,
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        let failure = failure.expect("the bound is reached");
        assert_eq!(failure.code, ErrorCode::ResourceExhausted);
        assert!(failure.retryable, "capacity can free up");
        assert!(failure.message.contains("was not recorded"));
        assert_eq!(
            driver.buffered(),
            accepted,
            "the refused event is not in the buffer"
        );
    }

    #[test]
    fn a_flush_against_an_unreachable_collector_reports_a_retryable_failure() {
        let driver = Driver::new(settings());
        driver.capture(Capture::event("a")).unwrap();
        let failure = driver.flush().unwrap_err();
        assert_eq!(failure.code, ErrorCode::Unavailable);
        assert!(failure.retryable);
    }

    #[test]
    fn the_driver_stamps_its_own_properties_with_a_driver_origin() {
        let settings = settings().with_property("service", "checkout");
        let driver = Driver::new(settings);
        driver.capture(Capture::event("a")).unwrap();

        let buffer = driver.buffer.lock().unwrap();
        let property = buffer.items[0]
            .envelope
            .properties
            .iter()
            .find(|p| p.key == "service")
            .expect("the driver property");
        assert_eq!(property.origin, PropertyOrigin::Driver);
    }

    #[test]
    fn a_client_property_keeps_its_type_and_its_client_origin() {
        let driver = Driver::new(settings());
        driver
            .capture(
                Capture::event("purchase")
                    .with_property("value", Value::Float(19.99))
                    .with_property("plan", Value::Text("pro".into())),
            )
            .unwrap();

        let buffer = driver.buffer.lock().unwrap();
        let properties = &buffer.items[0].envelope.properties;
        let value = properties.iter().find(|p| p.key == "value").unwrap();
        assert_eq!(wire::read(&value.value).unwrap(), Value::Float(19.99));
        assert_eq!(value.origin, PropertyOrigin::Client);
    }

    #[test]
    fn a_driver_never_sets_tenancy_or_a_receive_time() {
        // The collector stamps both. A driver that set them would be claiming
        // something it cannot know, and the collector would discard it anyway.
        let driver = Driver::new(settings());
        driver.capture(Capture::event("a")).unwrap();
        let buffer = driver.buffer.lock().unwrap();
        let envelope = &buffer.items[0].envelope;
        assert!(envelope.workspace_id.is_none());
        assert!(envelope.project_id.is_none());
        assert!(envelope.received_at.is_none());
    }

    #[test]
    fn a_sequence_rises_within_one_process() {
        // A sequence diagnoses a gap. It is not a global ordering guarantee.
        let driver = Driver::new(settings());
        for _ in 0..3 {
            driver.capture(Capture::event("a")).unwrap();
        }
        let buffer = driver.buffer.lock().unwrap();
        let sequences: Vec<u64> = buffer
            .items
            .iter()
            .map(|i| i.envelope.sequence.unwrap())
            .collect();
        assert_eq!(sequences, vec![0, 1, 2]);
    }

    #[test]
    fn a_session_identifier_is_opaque_and_issued_by_the_library() {
        let first = Session::start();
        let second = Session::start();
        assert_ne!(first.id(), second.id());
        assert_eq!(first.id().len(), 32);
    }

    #[test]
    fn every_correlation_field_reaches_the_envelope() {
        let driver = Driver::new(settings());
        driver
            .capture(
                Capture::event("checkout-started")
                    .with_session("s-1")
                    .with_request("r-1")
                    .with_trace([3; 16])
                    .with_service("checkout")
                    .with_release("2026.8.1")
                    .at(1_785_628_800_000),
            )
            .unwrap();
        let buffer = driver.buffer.lock().unwrap();
        let envelope = &buffer.items[0].envelope;
        assert_eq!(envelope.session_id.as_deref(), Some("s-1"));
        assert_eq!(envelope.request_id.as_deref(), Some("r-1"));
        assert_eq!(envelope.trace_id, Some(vec![3; 16]));
        assert_eq!(envelope.occurred_at, 1_785_628_800_000);
    }
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use tallyowl_collector_api::codec::{
        decode_submit_batch_request, encode_submit_batch_response,
    };
    use tallyowl_collector_api::types::SubmitBatchResponse;
    use tallyowl_rpc::{reply, Dispatcher, Request};

    /// A collector that takes `delay` to make each batch durable. It is the
    /// durable write that a synchronous driver waits behind, so this is the
    /// shape that matters.
    fn slow_collector(delay: Duration) -> (tallyowl_rpc::Server, Arc<AtomicUsize>) {
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let server = tallyowl_rpc::serve(
            "127.0.0.1:0",
            Arc::new(move |request: &Request| {
                let batch = decode_submit_batch_request(&request.payload).expect("a batch");
                std::thread::sleep(delay);
                counter.fetch_add(1, Ordering::SeqCst);
                reply(
                    "SubmitBatchResponse",
                    encode_submit_batch_response(&SubmitBatchResponse {
                        batch_id: batch.batch.batch_id,
                        accepted: batch.batch.items.len() as u64,
                        durable_copies: 1,
                        queued_at: now_ms(),
                        rejected: None,
                        policy_version: None,
                    }),
                )
            }) as Arc<dyn Dispatcher>,
            1024 * 1024,
        )
        .expect("serve");
        (server, seen)
    }

    fn driver_for(server: &tallyowl_rpc::Server, in_flight: usize) -> Driver {
        let mut settings = Settings::new(server.local_address().to_string(), "key-a")
            .with_max_in_flight_batches(in_flight);
        settings.max_items = 2;
        settings.linger = Duration::from_secs(3600);
        Driver::new(settings)
    }

    #[test]
    fn a_submitted_batch_is_acknowledged_and_the_receipt_names_it() {
        let (server, _) = slow_collector(Duration::from_millis(1));
        let driver = driver_for(&server, 4);
        driver.capture(Capture::event("a")).unwrap();
        assert!(driver.submit().unwrap().is_empty(), "nothing has finished");
        assert_eq!(driver.outstanding(), 1);

        let receipts = driver.drain().unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].accepted, 1);
        assert_eq!(receipts[0].durable_copies, 1);
        assert_eq!(driver.outstanding(), 0);
    }

    #[test]
    fn submitting_an_empty_buffer_sends_nothing() {
        let (server, seen) = slow_collector(Duration::from_millis(1));
        let driver = driver_for(&server, 4);
        assert!(driver.submit().unwrap().is_empty());
        assert_eq!(driver.outstanding(), 0);
        assert_eq!(seen.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn four_batches_in_flight_beat_four_sent_one_at_a_time() {
        // The whole point of the change. Each batch costs the collector 80 ms,
        // so four in sequence cannot finish in under 320 ms and four in flight
        // should finish in little more than 80.
        let delay = Duration::from_millis(80);

        let (server, _) = slow_collector(delay);
        let synchronous = driver_for(&server, 1);
        let started = Instant::now();
        for _ in 0..4 {
            synchronous.capture(Capture::event("a")).unwrap();
            synchronous.capture(Capture::event("b")).unwrap();
            synchronous.flush().unwrap().expect("a receipt");
        }
        let sequential = started.elapsed();

        let (server, _) = slow_collector(delay);
        let pipelined = driver_for(&server, 4);
        let started = Instant::now();
        for _ in 0..4 {
            pipelined.capture(Capture::event("a")).unwrap();
            pipelined.capture(Capture::event("b")).unwrap();
            pipelined.submit().unwrap();
        }
        let receipts = pipelined.drain().unwrap();
        let overlapped = started.elapsed();

        assert_eq!(receipts.len(), 4, "every batch was acknowledged");
        assert!(
            sequential >= delay * 4,
            "the sequential run should pay for each batch: {sequential:?}"
        );
        assert!(
            overlapped < sequential / 2,
            "pipelining gained nothing: {overlapped:?} against {sequential:?}"
        );
    }

    #[test]
    fn the_window_bounds_what_is_outstanding_and_every_batch_is_still_acknowledged() {
        let (server, seen) = slow_collector(Duration::from_millis(20));
        let driver = driver_for(&server, 2);

        let mut receipts = Vec::new();
        for _ in 0..6 {
            driver.capture(Capture::event("a")).unwrap();
            driver.capture(Capture::event("b")).unwrap();
            receipts.extend(driver.submit().unwrap());
            assert!(
                driver.outstanding() <= 2,
                "the window was exceeded: {}",
                driver.outstanding()
            );
        }
        receipts.extend(driver.drain().unwrap());

        assert_eq!(
            receipts.len(),
            6,
            "every batch was acknowledged exactly once"
        );
        assert_eq!(seen.load(Ordering::SeqCst), 6, "and reached the collector");
        let distinct: std::collections::HashSet<[u8; 16]> =
            receipts.iter().map(|r| r.batch_id).collect();
        assert_eq!(distinct.len(), 6, "each receipt names a different batch");
    }

    #[test]
    fn a_submitted_batch_that_the_collector_rejects_arrives_as_a_typed_error() {
        let server = tallyowl_rpc::serve(
            "127.0.0.1:0",
            Arc::new(|_: &Request| {
                tallyowl_rpc::error_outcome(tallyowl_collector_api::codec::encode_service_error(
                    &tallyowl_collector_api::types::ServiceError {
                        code: tallyowl_collector_api::types::ErrorCode::PermissionDenied,
                        message: "This key does not reach that project.".to_string(),
                        retryable: false,
                        detail: None,
                    },
                ))
            }) as Arc<dyn Dispatcher>,
            1024 * 1024,
        )
        .expect("serve");

        let driver = driver_for(&server, 4);
        driver.capture(Capture::event("a")).unwrap();
        driver.submit().unwrap();
        let failure = driver.drain().unwrap_err();
        assert_eq!(failure.code, ErrorCode::PermissionDenied);
        assert!(!failure.retryable);
    }

    #[test]
    fn a_shutdown_waits_for_what_was_already_sent() {
        // A shutdown that ignored the pipeline would report zero unsent items
        // while several batches were still waiting for an acknowledgement.
        let (server, seen) = slow_collector(Duration::from_millis(30));
        let driver = driver_for(&server, 4);
        for _ in 0..3 {
            driver.capture(Capture::event("a")).unwrap();
            driver.capture(Capture::event("b")).unwrap();
            driver.submit().unwrap();
        }
        assert_eq!(driver.shutdown(), 0, "nothing was left behind");
        assert_eq!(seen.load(Ordering::SeqCst), 3, "every batch reached it");
    }

    #[test]
    fn a_collector_that_never_answers_reports_the_loss_rather_than_hanging() {
        let driver = {
            let mut settings = Settings::new("127.0.0.1:1", "key-a");
            settings.max_items = 1;
            settings.max_batch_attempts = 2;
            Driver::new(settings)
        };
        driver.capture(Capture::event("a")).unwrap();
        let failure = driver.submit().unwrap_err();
        assert_eq!(failure.code, ErrorCode::Unavailable);
        assert!(failure.message.contains("not recorded"));
    }
}

#[cfg(test)]
mod trace_tests {
    use super::*;

    #[test]
    fn a_child_keeps_the_trace_and_takes_a_new_span() {
        // The trace ID is what routes every span of one trace to one tablet, so
        // it never changes down a trace. See D16 and D35.
        let root = SpanContext::root();
        let child = root.child();
        assert_eq!(child.trace_id, root.trace_id);
        assert_ne!(child.span_id, root.span_id);
        assert_eq!(child.parent_span_id, Some(root.span_id));
    }

    #[test]
    fn a_context_survives_the_header_form_a_service_we_do_not_own_reads() {
        let outbound = SpanContext::root();
        let header = outbound.traceparent();
        assert_eq!(header.len(), 55, "the W3C form is fixed width: {header}");

        let inbound = SpanContext::from_traceparent(&header).expect("it reads");
        assert_eq!(inbound.trace_id, outbound.trace_id);
        // The incoming span becomes this one's parent. Reusing its ID would
        // give two spans one identity and break every waterfall holding them.
        assert_eq!(inbound.parent_span_id, Some(outbound.span_id));
        assert_ne!(inbound.span_id, outbound.span_id);
        assert!(inbound.sampled);
    }

    #[test]
    fn the_sampled_flag_survives_the_header() {
        let mut context = SpanContext::root();
        context.sampled = false;
        assert!(context.traceparent().ends_with("-00"));
        assert!(
            !SpanContext::from_traceparent(&context.traceparent())
                .unwrap()
                .sampled
        );
    }

    #[test]
    fn a_header_this_version_cannot_read_starts_a_new_trace_rather_than_joining_one() {
        // Joining a trace from a header nobody could parse would put two
        // unrelated traces together, which is worse than starting one.
        for header in [
            "",
            "not a header",
            "01-aabb-ccdd-01",
            "00-0000000000000000000000000000-0000000000000000-01",
            &format!("00-{}-{}-01", "0".repeat(32), "1".repeat(16)),
            &format!("00-{}-{}-01", "1".repeat(32), "0".repeat(16)),
        ] {
            assert_eq!(SpanContext::from_traceparent(header), None, "{header}");
            let started = SpanContext::continue_or_start(Some(header));
            assert_eq!(started.parent_span_id, None, "{header}");
        }
    }

    #[test]
    fn a_span_carries_its_context_onto_the_wire() {
        let root = SpanContext::root();
        let child = root.child();
        let capture = Capture::span(&child, "GET /orders", SpanKind::Server, 1_000, 12);
        assert_eq!(
            capture.item.envelope.trace_id.as_deref(),
            Some(child.trace_id.as_slice())
        );
        assert_eq!(
            capture.item.envelope.span_id.as_deref(),
            Some(child.span_id.as_slice())
        );
        let span = capture.item.span.as_ref().expect("a span payload");
        assert_eq!(
            span.parent_span_id.as_deref(),
            Some(root.span_id.as_slice())
        );
        assert_eq!(span.duration_ms, 12);
    }

    #[test]
    fn a_failed_span_names_the_error_that_explains_it() {
        let context = SpanContext::root();
        let capture =
            Capture::span(&context, "charge", SpanKind::Client, 1, 5).failed(Some([7; 16]));
        let span = capture.item.span.as_ref().unwrap();
        assert_eq!(
            span.status,
            tallyowl_collector_api::types::SpanPayload_status::Error
        );
        assert_eq!(span.error_event_id.as_deref(), Some([7u8; 16].as_slice()));
    }

    #[test]
    fn an_error_carries_its_frames_and_says_which_are_the_applications_own() {
        let capture = Capture::error("Timeout", "took too long", false).with_frames(vec![
            Frame::library("hyper", "poll"),
            Frame::in_app("checkout", "charge").at("/app/checkout.rs", 40),
        ]);
        let frames = capture
            .item
            .error
            .as_ref()
            .unwrap()
            .frames
            .as_ref()
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert!(!frames[0].in_app);
        assert!(frames[1].in_app);
        assert_eq!(frames[1].line, Some(40));
    }

    #[test]
    fn an_error_inside_a_span_joins_its_trace() {
        let context = SpanContext::root();
        let capture = Capture::error("Timeout", "slow", false).in_span(&context);
        assert_eq!(
            capture.item.envelope.trace_id.as_deref(),
            Some(context.trace_id.as_slice())
        );
    }

    #[test]
    fn a_span_never_seals_a_batch() {
        // A batch sealed by a span would send one span at a time under load,
        // which is the opposite of what a high-volume diagnostic path needs.
        let context = SpanContext::root();
        assert!(!Capture::span(&context, "op", SpanKind::Internal, 1, 1).critical);
        // An unhandled error still does.
        assert!(Capture::error("Timeout", "slow", false).critical);
    }
}
