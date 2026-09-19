//! Collector intake: accept a batch, or say plainly that it did not.
//!
//! The whole of this module exists to hold one rule from `docs/DELIVERY.md`
//! section 3:
//!
//! > The collector never returns a durable receipt before Corndogs acknowledges
//! > the task at the configured `durable_copies`.
//!
//! Everything else here serves it. Validation and hard limits run **before**
//! the enqueue, so a poison payload does not consume the delivery queue. The
//! collector stamps tenancy from the credential and discards any tenancy value
//! that arrived in a payload. And when the durable store is unreachable, intake
//! refuses the batch rather than acknowledging data it cannot keep.

use std::sync::Arc;

use tallyowl_collector_api::types::{
    Batch, PropertyOrigin, RejectedItem, SubmitBatchRequest, SubmitBatchResponse, TelemetryItem,
};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_obs::time::now_ms;
use tallyowl_wire::{collector as wire, protocol, scrub, Value};

use crate::durable::DurableQueue;
use crate::series::SeriesLedger;
use crate::tenancy::{Tenancy, TenancyResolver};

/// The limits intake enforces at the trust boundary.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_batch_bytes: i64,
    pub max_event_bytes: i64,
    pub max_properties: i64,
}

/// Names an application cannot set.
///
/// The list is in `tallyowl_wire::scrub`, because the head needs the same one
/// for the policy snapshot it distributes and two copies would drift.
pub use tallyowl_wire::scrub::PROTECTED_KEYS;

/// Everything intake needs to answer one `submit-batch`.
pub struct Intake {
    pub queue: Arc<dyn DurableQueue>,
    pub queue_name: String,
    pub tenancy: Arc<TenancyResolver>,
    pub limits: Limits,
    pub durable_copies: u64,
    /// Exact per-project metric-series accounting. It refuses a new series in
    /// the open when a budget is full, and it never folds one into an overflow
    /// series. See `series`.
    pub series: Arc<SeriesLedger>,
    pub metrics: Arc<Registry>,
    /// Properties the collector stamps from operator configuration. Their origin
    /// is `collector`, and a query can filter on that, so an operator can trust
    /// that `region=us-west2` came from configuration.
    pub stamped: Vec<(String, String)>,
    /// The collection policy this collector applies. `None` collects
    /// everything, which is what a collector that has been given no policy
    /// means. See `crate::policy`.
    pub policy: Option<Arc<crate::policy::Held>>,
}

/// What one accepted batch produced, before it becomes a wire response.
#[derive(Debug, Clone, PartialEq)]
pub struct Accepted {
    pub response: SubmitBatchResponse,
    pub task_uuid: String,
}

impl Intake {
    pub fn declare_metrics(metrics: &Registry) {
        metrics.declare(
            "tallyowl_batches_accepted_total",
            tallyowl_obs::MetricKind::Counter,
            "Batches the collector accepted into the durable store.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_batches_accepted_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_batches_refused_total",
            tallyowl_obs::MetricKind::Counter,
            "Batches the collector refused, by reason.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_batches_refused_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_items_rejected_total",
            tallyowl_obs::MetricKind::Counter,
            "Items rejected inside an otherwise accepted batch, by reason.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_items_rejected_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_protected_property_refused_total",
            tallyowl_obs::MetricKind::Counter,
            "Client property values refused because the name is protected.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_protected_property_refused_total` is not a name the registry accepts: {}", e.0));
        // The window D31 promises, made visible. A count that moves says an
        // application is outside the version window, which is the one thing a
        // rolling upgrade can get wrong.
        metrics.declare(
            "tallyowl_protocol_version_refused_total",
            tallyowl_obs::MetricKind::Counter,
            "Batches refused because the declared protocol version is outside the window.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_protocol_version_refused_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_scrubbed_values_total",
            tallyowl_obs::MetricKind::Counter,
            "Values the collector removed because they must never be stored.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_scrubbed_values_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_items_accepted_total",
            tallyowl_obs::MetricKind::Counter,
            "Items the collector accepted into the durable store.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_items_accepted_total` is not a name the registry accepts: {}", e.0));
        // The metric-cost counters DATA_MODEL.md section 3.4 asks for. They are
        // the visible half of the budget: an operator sees what a metric costs
        // before a refusal rather than after one.
        metrics.declare(
            "tallyowl_metric_series_active_count",
            tallyowl_obs::MetricKind::Gauge,
            "Active metric series this collector is accounting for.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_metric_series_active_count` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_metric_series_bytes",
            tallyowl_obs::MetricKind::Gauge,
            "Bytes of metric points this collector is accounting for.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_metric_series_bytes` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_metric_series_refused_total",
            tallyowl_obs::MetricKind::Counter,
            "Metric points refused because a series budget was full, by reason.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_metric_series_refused_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_metric_points_merged_total",
            tallyowl_obs::MetricKind::Counter,
            "Metric points that another point of the same series carried.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_metric_points_merged_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_metric_merge_skipped_total",
            tallyowl_obs::MetricKind::Counter,
            "Batches that travelled unmerged because they held more points than the work budget.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_metric_merge_skipped_total` is not a name the registry accepts: {}", e.0));
    }

    /// Accept a batch, or refuse it.
    ///
    /// `credential` comes from the connection, never from the payload.
    pub fn submit(
        &self,
        credential: &str,
        request: SubmitBatchRequest,
    ) -> Result<Accepted, TallyOwlError> {
        let tenancy = self.tenancy.resolve(credential).inspect_err(|_| {
            self.metrics.increment(
                "tallyowl_batches_refused_total",
                &labels(&[("reason", "unauthenticated")]),
            );
        })?;

        // The protocol version, before anything else looks at the batch. A
        // client this build cannot read gets one clear answer rather than a
        // partial acceptance, and it gets it here instead of after the batch
        // is durable and on its way to the head. The head checks it again at
        // the commit, because the head is the authority.
        if !protocol::accepts(request.protocol_version) {
            let declared = request
                .protocol_version
                .unwrap_or(protocol::PROTOCOL_VERSION);
            self.metrics.increment(
                "tallyowl_batches_refused_total",
                &labels(&[("reason", "protocol-version")]),
            );
            self.metrics.increment(
                "tallyowl_protocol_version_refused_total",
                &labels(&[("reason", protocol::refusal_reason(declared))]),
            );
            return Err(TallyOwlError::new(
                ErrorCode::SchemaUnsupported,
                protocol::refusal(declared),
            ));
        }

        let batch_id = request.batch.batch_id.clone();
        let mut batch = request.batch;

        // **Read once, for the whole batch.** `docs/POLICY.md` section 7: a
        // collector applies a new snapshot at a batch boundary, never inside a
        // batch. A fetch that landed halfway through would otherwise make the
        // first half of this batch obey one policy and the second half another.
        let policy = self.policy.as_ref().and_then(|held| held.current());

        // Hard limits first. A batch over the seal cannot be split here, because
        // the driver owns batching and only the driver can preserve event IDs
        // across a rebatch. See DELIVERY.md section 2.
        let encoded_size = tallyowl_collector_api::codec::encode_batch(&batch).len() as i64;
        if encoded_size > self.limits.max_batch_bytes {
            self.metrics.increment(
                "tallyowl_batches_refused_total",
                &labels(&[("reason", "batch-too-large")]),
            );
            return Err(TallyOwlError::over_limit(
                "Batch",
                &format!("{} KiB", encoded_size / 1024),
                &format!("{} KiB", self.limits.max_batch_bytes / 1024),
                "Reduce the batch size or raise the limit for this project.",
            ));
        }

        // Per-item validation. An invalid item is rejected by ID and the rest of
        // the batch commits, which is the bounded partial failure DELIVERY.md
        // section 9 requires.
        let mut rejected: Vec<RejectedItem> = Vec::new();
        let mut kept: Vec<TelemetryItem> = Vec::new();
        let now = now_ms();
        let mut refused_by_policy = 0u64;
        for mut item in batch.items.drain(..) {
            // The policy first. An item the policy refuses costs nothing more:
            // no size check, no series accounting, no queue write, and no
            // delivery. That saving is the whole reason a collector holds a
            // policy at all, since the head enforces the same rules at commit.
            //
            // **It is not a rejection on the receipt.** A rejected item tells a
            // producer that something went wrong and invites a retry; a policy
            // refusal is the installation working as its operator configured
            // it, and a driver that retried it would retry it for ever. The
            // count reaches the metric, which is where an operator looks.
            if let Some(policy) = &policy {
                if !policy.keeps(&item) {
                    refused_by_policy += 1;
                    continue;
                }
            }
            let outcome = self
                .check_item(&item)
                .and_then(|()| self.admit_metric_point(&item, tenancy, now));
            match outcome {
                Ok(()) => {
                    self.stamp(&mut item, tenancy);
                    self.scrub(&mut item);
                    if let Some(policy) = &policy {
                        let (blocked, redacted) = policy.apply_properties(&mut item);
                        let unlinked = policy.unlink_campaign(&mut item);
                        for (reason, count) in [
                            ("blocked", blocked),
                            ("redacted", redacted),
                            ("campaign-unlinked", unlinked),
                        ] {
                            if count > 0 {
                                self.metrics.add(
                                    "tallyowl_policy_properties_refused_total",
                                    &labels(&[("reason", reason)]),
                                    count,
                                );
                            }
                        }
                    }
                    kept.push(item);
                }
                Err(reason) => {
                    self.metrics.increment(
                        "tallyowl_items_rejected_total",
                        &labels(&[("reason", reason.code.as_str())]),
                    );
                    rejected.push(RejectedItem {
                        event_id: item.envelope.event_id.clone(),
                        code: to_wire_code(reason.code),
                        message: reason.message,
                    });
                }
            }
        }
        // `accepted` counts what the caller sent and TallyOwl kept. Merging
        // happens after it, because two snapshots of one series that became one
        // point were both accepted, and a receipt that said otherwise would
        // read as a loss.
        if refused_by_policy > 0 {
            self.metrics.add(
                "tallyowl_items_dropped_by_policy_total",
                &labels(&[]),
                refused_by_policy,
            );
        }
        // A batch the policy emptied is not written to the durable queue at
        // all. A kill switch that still cost a queue write and a delivery would
        // not be a kill switch.
        if kept.is_empty() && rejected.is_empty() && refused_by_policy > 0 {
            return Ok(Accepted {
                response: SubmitBatchResponse {
                    batch_id,
                    accepted: 0,
                    durable_copies: self.durable_copies,
                    queued_at: now_ms(),
                    rejected: None,
                    policy_version: self.policy.as_ref().map(|held| held.version()),
                },
                task_uuid: String::new(),
            });
        }
        let accepted = kept.len() as u64;
        let (merged_items, report) = crate::series::merge(kept, &self.series.budget());
        if report.merged > 0 {
            self.metrics.add(
                "tallyowl_metric_points_merged_total",
                &labels(&[]),
                report.merged,
            );
        }
        if report.over_work_budget {
            self.metrics
                .increment("tallyowl_metric_merge_skipped_total", &labels(&[]));
        }
        batch.items = merged_items;
        self.publish_series_cost();

        // The batch reaches the durable store before the caller hears anything.
        // A failure here is a refusal, never a receipt: a collector that cannot
        // retain data must not acknowledge it.
        //
        // It travels as a delivery task rather than as a bare batch, because a
        // forwarder needs the attempt count and the acceptance time that
        // Corndogs does not hold. See DELIVERY.md section 4 and `task`.
        let queued_at = now_ms();
        let payload =
            crate::task::encode(&crate::task::seal(&batch, &tenancy.source_id, queued_at));
        let task_uuid = self
            .queue
            .submit(&self.queue_name, payload, priority_of(&batch))
            .inspect_err(|_| {
                self.metrics.increment(
                    "tallyowl_batches_refused_total",
                    &labels(&[("reason", "durable-store-unavailable")]),
                );
            })?;

        self.metrics
            .increment("tallyowl_batches_accepted_total", &labels(&[]));
        self.metrics
            .add("tallyowl_items_accepted_total", &labels(&[]), accepted);

        Ok(Accepted {
            response: SubmitBatchResponse {
                batch_id,
                accepted,
                // The receipt reports the configured total durable-copy
                // requirement, and never claims a stronger boundary than the
                // configuration required.
                durable_copies: self.durable_copies,
                queued_at,
                rejected: (!rejected.is_empty()).then_some(rejected),
                // The version this batch was judged against. A driver that sees
                // it rise knows the rules changed, and one that never sees it
                // knows this collector holds no policy.
                policy_version: self.policy.as_ref().map(|held| held.version()),
            },
            task_uuid,
        })
    }

    /// Charge one metric point against its project's budget, or refuse it.
    ///
    /// An item that is not a metric point costs nothing here. A refusal becomes
    /// a rejected item on the receipt with `resource-exhausted`, which is the
    /// explicit backpressure the phase requires: the producer learns which item
    /// did not fit, by ID, and the rest of the batch commits.
    fn admit_metric_point(
        &self,
        item: &TelemetryItem,
        tenancy: Tenancy,
        now: i64,
    ) -> Result<(), TallyOwlError> {
        let Some(point) = item.metric_point.as_ref() else {
            return Ok(());
        };
        let size = tallyowl_collector_api::codec::encode_telemetry_item(item).len() as u64;
        self.series
            .admit(&tenancy.project_id, point, size, now)
            .inspect_err(|error| {
                self.metrics.increment(
                    "tallyowl_metric_series_refused_total",
                    &labels(&[("reason", error.code.as_str())]),
                );
            })
    }

    /// Publish what the ledger holds, so an operator sees the cost through the
    /// same endpoint as everything else.
    fn publish_series_cost(&self) {
        let pressure = self.series.pressure();
        self.metrics.set_gauge(
            "tallyowl_metric_series_active_count",
            &labels(&[]),
            pressure.active_series as i64,
        );
        self.metrics.set_gauge(
            "tallyowl_metric_series_bytes",
            &labels(&[]),
            pressure.active_bytes as i64,
        );
    }

    fn check_item(&self, item: &TelemetryItem) -> Result<(), TallyOwlError> {
        let envelope = &item.envelope;
        if envelope.event_id.len() != 16 {
            return Err(TallyOwlError::invalid_argument(
                "This item has no usable identifier. Every item needs a 16-byte event ID.",
            ));
        }
        if envelope.properties.len() as i64 > self.limits.max_properties {
            return Err(TallyOwlError::over_limit(
                "Item",
                &format!("{} properties", envelope.properties.len()),
                &format!("{} properties", self.limits.max_properties),
                "Send fewer properties, or raise the limit for this project.",
            ));
        }
        let size = tallyowl_collector_api::codec::encode_telemetry_item(item).len() as i64;
        if size > self.limits.max_event_bytes {
            return Err(TallyOwlError::over_limit(
                "Item",
                &format!("{size} bytes"),
                &format!("{} bytes", self.limits.max_event_bytes),
                "Send a smaller item, or raise the limit for this project.",
            ));
        }
        if envelope.occurred_at <= 0 {
            return Err(TallyOwlError::invalid_argument(
                "This item has no time. Set the time the thing happened, in milliseconds since 1970.",
            ));
        }
        Ok(())
    }

    /// Stamp tenancy and operator properties, and discard what a client must not
    /// set.
    fn stamp(&self, item: &mut TelemetryItem, tenancy: Tenancy) {
        let envelope = &mut item.envelope;

        // Whatever arrived in these three, it goes. Never accept tenancy from a
        // payload.
        envelope.workspace_id = Some(tenancy.workspace_id.to_vec());
        envelope.project_id = Some(tenancy.project_id.to_vec());
        envelope.source_id = Some(tenancy.source_id.to_vec());
        envelope.received_at = Some(now_ms());

        // A protected name refuses a client value and counts the refusal.
        let before = envelope.properties.len();
        envelope.properties.retain(|p| {
            !(PROTECTED_KEYS.contains(&p.key.as_str()) && p.origin == PropertyOrigin::Client)
        });
        let refused = before - envelope.properties.len();
        if refused > 0 {
            self.metrics.add(
                "tallyowl_protected_property_refused_total",
                &labels(&[]),
                refused as u64,
            );
        }

        for (key, value) in &self.stamped {
            envelope.properties.retain(|p| &p.key != key);
            envelope.properties.push(wire::property(
                key,
                Value::Text(value.clone()),
                PropertyOrigin::Collector,
            ));
        }
    }
}

impl Intake {
    /// Remove what must never be stored.
    ///
    /// **The collector is the trust boundary.** `AGENTS.md` says to normalize
    /// here, and it says never to record secrets, credentials, request bodies,
    /// claim values, or raw personal data by default. An error message is where
    /// those arrive, because an error message is written by whoever wrote the
    /// code that threw, and a connection string in an exception is the ordinary
    /// case rather than the exotic one.
    ///
    /// A driver may scrub as well, and the maintained ones do. That is a
    /// courtesy: an application that does not use a maintained driver still
    /// reaches this, so this cannot rely on anything having happened before it.
    fn scrub(&self, item: &mut TelemetryItem) {
        let mut removed = 0u64;

        for property in &mut item.envelope.properties {
            if scrub::is_protected(&property.key) {
                property.value = wire::write(&Value::Text(scrub::REMOVED.to_string()));
                removed += 1;
                continue;
            }
            // A protected name inside a value, such as a message property that
            // holds `password=...`. The name did not say it was a secret and
            // the value does.
            if let Ok(Value::Text(text)) = wire::read(&property.value) {
                let cleaned = scrub::text(&text);
                if cleaned != text {
                    property.value = wire::write(&Value::Text(cleaned));
                    removed += 1;
                }
            }
        }

        if let Some(error) = item.error.as_mut() {
            let cleaned = scrub::text(&error.message);
            if cleaned != error.message {
                error.message = cleaned;
                removed += 1;
            }
            for frame in error.frames.iter_mut().flatten() {
                if let Some(file) = frame.file.as_mut() {
                    let cleaned = scrub::path(file);
                    if &cleaned != file {
                        *file = cleaned;
                        removed += 1;
                    }
                }
            }
        }

        // A page view and an event both carry a route, and a route is where a
        // reset token ends up.
        if let Some(page) = item.page_view.as_mut() {
            page.route = scrub::text(&page.route);
            if let Some(referrer) = page.referrer.as_mut() {
                *referrer = scrub::text(referrer);
            }
        }
        if let Some(event) = item.event.as_mut() {
            if let Some(route) = event.route.as_mut() {
                *route = scrub::text(route);
            }
        }

        if removed > 0 {
            self.metrics
                .add("tallyowl_scrubbed_values_total", &labels(&[]), removed);
        }
    }
}

/// Priority defaults, from DELIVERY.md section 8. A conversion outranks an
/// ordinary behavior event when the queue is under pressure.
fn priority_of(batch: &Batch) -> i64 {
    use tallyowl_collector_api::types::TelemetryKind;
    let mut priority = 0;
    for item in &batch.items {
        let item_priority = match item.envelope.kind {
            TelemetryKind::Conversion => 5,
            TelemetryKind::Error => 4,
            TelemetryKind::MetricPoint => 3,
            TelemetryKind::Span => 1,
            _ => 2,
        };
        priority = priority.max(item_priority);
    }
    priority
}

fn to_wire_code(code: ErrorCode) -> tallyowl_collector_api::types::ErrorCode {
    use tallyowl_collector_api::types::ErrorCode as Wire;
    match code {
        ErrorCode::InvalidArgument => Wire::InvalidArgument,
        ErrorCode::Unauthenticated => Wire::Unauthenticated,
        ErrorCode::PermissionDenied => Wire::PermissionDenied,
        ErrorCode::NotFound => Wire::NotFound,
        ErrorCode::AlreadyExists => Wire::AlreadyExists,
        ErrorCode::ResourceExhausted => Wire::ResourceExhausted,
        ErrorCode::FailedPrecondition => Wire::FailedPrecondition,
        ErrorCode::Unavailable => Wire::Unavailable,
        ErrorCode::SchemaUnsupported => Wire::SchemaUnsupported,
        ErrorCode::BudgetExceeded => Wire::BudgetExceeded,
        ErrorCode::IncompleteResult => Wire::IncompleteResult,
        ErrorCode::Internal => Wire::Internal,
    }
}

/// Turn a TallyOwl error into the wire error type.
pub fn to_wire_error(error: &TallyOwlError) -> tallyowl_collector_api::types::ServiceError {
    tallyowl_collector_api::types::ServiceError {
        code: to_wire_code(error.code),
        message: error.message.clone(),
        retryable: error.retryable,
        detail: (!error.detail.is_empty()).then(|| {
            error
                .detail
                .iter()
                .map(|(k, v)| wire::property(k, Value::Text(v.clone()), PropertyOrigin::Collector))
                .collect()
        }),
    }
}
