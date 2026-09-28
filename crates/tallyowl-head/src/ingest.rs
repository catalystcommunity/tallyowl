//! Head ingest: the commit boundary.
//!
//! `docs/DELIVERY.md` section 5 gives the sequence and the receipt. Two rules
//! here matter more than the rest:
//!
//! - **the head must not acknowledge until the tablet satisfies its receipt
//!   policy.** A receipt that outruns the commit turns a collector's task
//!   completion into data loss;
//! - **a retried batch ID returns the prior commit.** If the connection dropped
//!   after the commit and before the receipt, the collector retries the same
//!   batch ID, and the durable receipt index answers rather than committing a
//!   second logical batch.

use std::sync::Arc;

use tallyowl_collector_api::types::{
    CommitBatchRequest, CommitBatchResponse, ErrorCode as WireCode, ReceiptPolicy, RejectedItem,
};
use tallyowl_obs::error::TallyOwlError;
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_store::{Store, StoreError};

use crate::project::project;

use tallyowl_wire::protocol;
/// The protocol and projector versions this head reports on every receipt.
///
/// The protocol version and the window around it live in `tallyowl_wire`,
/// because collector intake enforces the same window and two copies of a
/// window drift. This is the head's name for it.
pub use tallyowl_wire::protocol::PROTOCOL_VERSION;
pub const PROJECTOR_VERSION: u64 = 1;

pub struct Ingest {
    pub store: Arc<dyn Store>,
    pub metrics: Arc<Registry>,
    /// The policy this head satisfies. The receipt names it, so a caller never
    /// has to assume which durability it got.
    pub receipt_policy: ReceiptPolicy,
    /// The traces waiting for their decision window to close.
    ///
    /// A committed span enters the provisional retention class here, which is
    /// step 3 of D35. It is `None` for a head with no tail sampling, and then
    /// every trace is kept, which is what no sampling means.
    pub open_traces: Option<Arc<crate::sampling::OpenTraces>>,
    /// The window a golden-signal rollup covers, in milliseconds. Zero turns
    /// the rollup off, which is what a head that only stores events wants.
    pub golden_signal_bucket_ms: i64,
    /// The collection policy this head applies. `None` collects everything,
    /// which is what an installation that has configured no policy means.
    pub policy: Option<Arc<crate::policy::PolicyService>>,
    /// The check that a batch's rows belong to the source that sent it. `None`
    /// makes no check, which is what a test that has no control catalog wants.
    /// A running head always has one.
    pub sources: Option<SourceCheck>,
}

/// How the head checks tenancy at the durability boundary.
///
/// **The head never accepts tenancy from a payload either.** A collector
/// resolves a credential to a source, and stamps that source's workspace and
/// project on every item. D17 says the head verifies that each stamped
/// destination is one the sender may write to. Before this check the head
/// stored whatever workspace and project an envelope named, so anything that
/// reached `head.listen` wrote rows into any project of any tenant.
///
/// The source record is the permitted set: one source belongs to one project.
/// This does not identify the collector itself. That needs the connection to
/// carry an identity, which is the mutual TLS work; this is the half that the
/// catalog can answer today.
pub struct SourceCheck {
    pub store: Arc<tallyowl_store::SegmentedStore>,
    /// Refuse a batch whose source this head has no record of. A collector
    /// takes its source from this head's own `resolve-key` answer, so an
    /// unknown one did not come from a collector of this installation.
    pub require_known: bool,
}

/// The batch identifier a derived commit travels under.
///
/// It is derived from the batch that produced it, so a re-delivery of that
/// batch produces the same derived identifier and deduplicates to one logical
/// rollup. A random one would double every signal on a retry, and sharing the
/// original would make the rollup deduplicate itself away.
fn derived_batch_id(batch_id: [u8; 16]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let mut input = Vec::with_capacity(32);
    input.extend_from_slice(b"golden-signals\x01");
    input.extend_from_slice(&batch_id);
    out.copy_from_slice(&blake3::hash(&input).as_bytes()[..16]);
    out
}

impl Ingest {
    pub fn declare_metrics(metrics: &Registry) {
        metrics
            .declare(
                "tallyowl_commits_total",
                tallyowl_obs::MetricKind::Counter,
                "Batches the head committed, by outcome.",
                &[],
            )
            .unwrap_or_else(|e| {
                panic!(
                    "the metric `tallyowl_commits_total` is not a name the registry accepts: {}",
                    e.0
                )
            });
        metrics.declare(
            "tallyowl_events_committed_total",
            tallyowl_obs::MetricKind::Counter,
            "Items the head committed to storage.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_events_committed_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_events_rejected_total",
            tallyowl_obs::MetricKind::Counter,
            "Items the head could not project, by reason.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_events_rejected_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_golden_signals_total",
            tallyowl_obs::MetricKind::Counter,
            "Service-operation golden-signal points the head derived from spans.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_golden_signals_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_properties_refused_total",
            tallyowl_obs::MetricKind::Counter,
            "Properties the collection policy blocked or redacted.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_properties_refused_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_protocol_version_refused_total",
            tallyowl_obs::MetricKind::Counter,
            "Commits refused because the declared protocol version is outside the window.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_protocol_version_refused_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_commit_watermark_count",
            tallyowl_obs::MetricKind::Gauge,
            "The head's current commit watermark.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_commit_watermark_count` is not a name the registry accepts: {}", e.0));
    }

    pub fn commit(
        &self,
        request: CommitBatchRequest,
    ) -> Result<CommitBatchResponse, TallyOwlError> {
        // The head is the authority on the protocol window, and it answers
        // before it reads anything else in the batch. The upgrade order in
        // DEPLOYMENT.md section 7 puts the head first, so a collector is
        // briefly one version behind it; the window is what makes that safe,
        // and the refusal is what keeps a collector further behind than that
        // from writing rows this build would read wrongly. See D31.
        if !protocol::accepts(request.protocol_version) {
            let declared = request
                .protocol_version
                .unwrap_or(protocol::PROTOCOL_VERSION);
            self.metrics.increment(
                "tallyowl_protocol_version_refused_total",
                &labels(&[("reason", protocol::refusal_reason(declared))]),
            );
            self.metrics.increment(
                "tallyowl_commits_total",
                &labels(&[("outcome", "protocol-version-refused")]),
            );
            return Err(TallyOwlError::new(
                tallyowl_obs::error::ErrorCode::SchemaUnsupported,
                protocol::refusal(declared),
            ));
        }

        let batch_id = to_id(&request.batch.batch_id).ok_or_else(|| {
            TallyOwlError::invalid_argument(
                "This batch has no usable identifier. Every batch needs a 16-byte batch ID.",
            )
        })?;
        let source_id = to_id(&request.source_id).unwrap_or([0; 16]);

        // The prior receipt answers first. A batch that already committed must
        // not commit again, whatever it now contains.
        //
        // One thing can still be owed. The golden signals of a batch travel in
        // a second commit, and a first delivery whose second commit failed was
        // answered with an error so that the collector delivers the batch
        // again. That delivery arrives here: the batch has its receipt and the
        // signals have none. It goes on, to commit the signals and nothing
        // else. Before this, the receipt answered, and the signals of that
        // batch were never produced.
        let prior = self.store.receipt(source_id, batch_id);
        if let Some(held) = &prior {
            let signals_owed = self.golden_signal_bucket_ms > 0
                && self
                    .store
                    .receipt(source_id, derived_batch_id(batch_id))
                    .is_none();
            if !signals_owed {
                return Ok(self.deduplicated(request.batch.batch_id, held));
            }
        }

        // Who may this source write for. Read once for the batch, and before
        // any item, so a batch from a source nobody knows costs one lookup.
        let permitted = match &self.sources {
            None => None,
            Some(check) => match check
                .store
                .catalog()
                .source(source_id)
                .map_err(|e| to_service_error(StoreError::Unavailable(e.to_string())))?
            {
                Some(source) => Some((source.workspace_id, source.project_id)),
                None if check.require_known => {
                    self.metrics.increment(
                        "tallyowl_commits_total",
                        &labels(&[("outcome", "unknown-source")]),
                    );
                    return Err(TallyOwlError::new(
                        tallyowl_obs::error::ErrorCode::PermissionDenied,
                        "This batch names a source this installation has no record of, so nothing in it was stored. A collector takes its source from the key an application presents. Check that the key still exists, and that the collector forwards to the head that issued it.",
                    )
                    .retryable(false));
                }
                None => None,
            },
        };

        let mut rows = Vec::with_capacity(request.batch.items.len());
        let mut rejected: Vec<RejectedItem> = Vec::new();
        for item in &request.batch.items {
            // An exact number names a count of digits, and the projection
            // writes that many. The bound is checked before the projection
            // reads the item, so one item with an exponent of a trillion is one
            // rejected item. Without it, the batch stopped the head, and the
            // forwarder delivered it again after every restart.
            let projected = payload_decimals_in_range(item)
                .map_err(|e| crate::project::ProjectionFailure { reason: e.message })
                .and_then(|()| project(item));
            match projected {
                Ok(row)
                    if permitted.is_some_and(|(workspace, project)| {
                        row.workspace_id != workspace || row.project_id != project
                    }) =>
                {
                    self.metrics.increment(
                        "tallyowl_events_rejected_total",
                        &labels(&[("reason", "tenancy-mismatch")]),
                    );
                    rejected.push(RejectedItem {
                        event_id: item.envelope.event_id.clone(),
                        code: WireCode::PermissionDenied,
                        message: "This item names a workspace or a project that its source does not belong to, so it was not stored. A collector stamps both from the credential and never from the payload."
                            .to_string(),
                    });
                }
                Ok(row) => rows.push(row),
                Err(failure) => {
                    self.metrics.increment(
                        "tallyowl_events_rejected_total",
                        &labels(&[("reason", "projection")]),
                    );
                    rejected.push(RejectedItem {
                        event_id: item.envelope.event_id.clone(),
                        code: WireCode::InvalidArgument,
                        message: failure.reason,
                    });
                }
            }
        }

        // Collection policy, applied before anything is durable.
        //
        // `docs/POLICY.md` puts this at the collector so a blocked event costs
        // no transport, and it is here as well because **this is the durability
        // boundary**: a rule enforced only at the edge is a rule an edge that
        // skipped it can break, and a row that reached storage cannot be
        // un-collected. A refused item is counted and named on the receipt
        // rather than dropped in silence, because a silent drop is how somebody
        // discovers a policy by noticing a gap months later.
        if let Some(policy) = &self.policy {
            let mut kept = Vec::with_capacity(rows.len());
            for mut row in rows {
                let compiled = policy.for_project(row.project_id);
                if !compiled.keeps(&row) {
                    self.metrics.increment(
                        "tallyowl_events_rejected_total",
                        &labels(&[("reason", "collection-policy")]),
                    );
                    rejected.push(RejectedItem {
                        event_id: row.event_id.to_vec(),
                        code: WireCode::PermissionDenied,
                        message: format!(
                            "The collection policy for this project does not collect `{}`.",
                            row.name
                        ),
                    });
                    continue;
                }
                // How much of a campaign touch this project keeps. D30 puts
                // consent at the point where campaign data joins an identified
                // person, and this is that point.
                match compiled.capture(&mut row) {
                    crate::policy::Captured::Refused => {
                        self.metrics.increment(
                            "tallyowl_events_rejected_total",
                            &labels(&[("reason", "campaign-capture")]),
                        );
                        rejected.push(RejectedItem {
                            event_id: row.event_id.to_vec(),
                            code: WireCode::PermissionDenied,
                            message:
                                "The collection policy for this project does not collect campaign touches."
                                    .to_string(),
                        });
                        continue;
                    }
                    crate::policy::Captured::Kept => {}
                    crate::policy::Captured::Unlinked(count) => self.metrics.add(
                        "tallyowl_properties_refused_total",
                        &labels(&[("reason", "campaign-unlinked")]),
                        count as u64,
                    ),
                    crate::policy::Captured::Stripped(count) => self.metrics.add(
                        "tallyowl_properties_refused_total",
                        &labels(&[("reason", "campaign-stripped")]),
                        count as u64,
                    ),
                }
                let (blocked, redacted) = compiled.apply_properties(&mut row);
                if blocked > 0 {
                    self.metrics.add(
                        "tallyowl_properties_refused_total",
                        &labels(&[("reason", "blocked")]),
                        blocked as u64,
                    );
                }
                if redacted > 0 {
                    self.metrics.add(
                        "tallyowl_properties_refused_total",
                        &labels(&[("reason", "redacted")]),
                        redacted as u64,
                    );
                }
                kept.push(row);
            }
            rows = kept;
        }

        // Golden signals, derived from the spans in this batch. They are
        // ordinary metric points, so `rate`, `increase`, `histogram_merge`, and
        // `quantile` read them exactly as they read a counter an application
        // sent. See `rollup`.
        //
        // They travel in their own commit under their own batch identifier,
        // which is derived from this one. A rollup that shared the batch would
        // make a re-delivery of the batch deduplicate the signals away, and a
        // rollup that used a random identifier would double them.
        //
        // **One rollup for each project in the batch.** A batch holds one
        // source, and one source is one project, so this is one call. It used
        // to name the first row's project for every row, which is only right
        // while that stays true, and nothing here depends on it now.
        let signals: Vec<tallyowl_store::row::EventRow> = if self.golden_signal_bucket_ms > 0 {
            let mut by_project: std::collections::BTreeMap<
                [u8; 16],
                Vec<tallyowl_store::row::EventRow>,
            > = std::collections::BTreeMap::new();
            for row in rows.iter().filter(|row| row.kind == "span") {
                by_project
                    .entry(row.project_id)
                    .or_default()
                    .push(row.clone());
            }
            by_project
                .iter()
                .flat_map(|(project_id, spans)| {
                    crate::rollup::golden_signals(spans, self.golden_signal_bucket_ms, *project_id)
                })
                .collect()
        } else {
            Vec::new()
        };

        // The batch committed on an earlier delivery, and only its signals are
        // owed. Nothing else below may run a second time.
        if let Some(held) = &prior {
            self.commit_signals(source_id, batch_id, signals)?;
            return Ok(self.deduplicated(request.batch.batch_id, held));
        }

        // The traces this batch touched enter the provisional class before the
        // commit returns, so a trace can never be committed and then forgotten
        // by a projector that was not told about it.
        if let Some(open) = &self.open_traces {
            open.observe(&rows);
        }

        let outcome = self
            .store
            .commit(source_id, batch_id, rows)
            .map_err(to_service_error)?;

        // After the batch, and an error when it fails. The batch is durable, so
        // the collector's next delivery deduplicates it and commits only this.
        self.commit_signals(source_id, batch_id, signals)?;

        self.metrics.increment(
            "tallyowl_commits_total",
            &labels(&[("outcome", "committed")]),
        );
        self.metrics.add(
            "tallyowl_events_committed_total",
            &labels(&[]),
            outcome.accepted,
        );
        self.metrics.set_gauge(
            "tallyowl_commit_watermark_count",
            &labels(&[]),
            outcome.commit_watermark as i64,
        );

        Ok(CommitBatchResponse {
            batch_id: request.batch.batch_id,
            accepted: outcome.accepted,
            committed_at: outcome.committed_at,
            satisfied_policy: self.receipt_policy.clone(),
            commit_watermark: outcome.commit_watermark,
            protocol_version: PROTOCOL_VERSION,
            projector_version: PROJECTOR_VERSION,
            rejected: (!rejected.is_empty()).then_some(rejected),
            deduplicated: Some(outcome.deduplicated),
        })
    }
}

impl Ingest {
    /// The answer to a batch that committed on an earlier delivery.
    fn deduplicated(
        &self,
        batch_id: Vec<u8>,
        prior: &tallyowl_store::Receipt,
    ) -> CommitBatchResponse {
        self.metrics.increment(
            "tallyowl_commits_total",
            &labels(&[("outcome", "deduplicated")]),
        );
        CommitBatchResponse {
            batch_id,
            accepted: prior.accepted,
            committed_at: prior.committed_at,
            satisfied_policy: self.receipt_policy.clone(),
            commit_watermark: prior.commit_watermark,
            protocol_version: PROTOCOL_VERSION,
            projector_version: PROJECTOR_VERSION,
            rejected: None,
            deduplicated: Some(true),
        }
    }

    /// Commit the golden signals of one batch, under the identifier derived
    /// from the batch.
    ///
    /// **A failure here is an error to the collector, and it used to be
    /// nothing.** The failure was counted and the batch was answered as
    /// committed, so the collector finished its task and nothing ever produced
    /// those signals: a gap in a request-rate chart with no cause anybody could
    /// find. The batch itself is durable by now, so the retry costs one
    /// deduplicated commit and the signals.
    fn commit_signals(
        &self,
        source_id: [u8; 16],
        batch_id: [u8; 16],
        signals: Vec<tallyowl_store::row::EventRow>,
    ) -> Result<(), TallyOwlError> {
        if signals.is_empty() {
            return Ok(());
        }
        let count = signals.len() as u64;
        match self
            .store
            .commit(source_id, derived_batch_id(batch_id), signals)
        {
            Ok(_) => {
                self.metrics
                    .add("tallyowl_golden_signals_total", &labels(&[]), count);
                Ok(())
            }
            Err(e) => {
                self.metrics.increment(
                    "tallyowl_events_rejected_total",
                    &labels(&[("reason", "golden-signal-rollup")]),
                );
                Err(to_service_error(e))
            }
        }
    }
}

fn to_id(bytes: &[u8]) -> Option<[u8; 16]> {
    bytes.try_into().ok()
}

/// The exact numbers a payload carries outside a typed value: a conversion's
/// value and a campaign cost. A property and a measurement are checked where
/// `tallyowl_wire` reads them.
fn payload_decimals_in_range(
    item: &tallyowl_collector_api::types::TelemetryItem,
) -> Result<(), tallyowl_wire::WireError> {
    // Money has a second bound: the digits before the decimal point. A value
    // past it is refused here, where it costs its sender one rejected item. A
    // stored one fails every attribution question of its project until
    // somebody erases it, because a credited value has to add up.
    let check = |decimal: &tallyowl_collector_api::types::CsilDecimal,
                 named: &str|
     -> Result<(), tallyowl_wire::WireError> {
        tallyowl_wire::check_decimal(decimal.exponent)?;
        let digits = decimal.mantissa.unsigned_abs().to_string().len() as i64;
        let most = i64::from(crate::attribution::MAX_VALUE_DIGITS);
        if decimal.mantissa != 0 && digits + decimal.exponent > most {
            return Err(tallyowl_wire::WireError {
                message: format!(
                    "The {named} has more than {most} digits before the decimal point. Send the amount in the currency's own units, such as 19.99."
                ),
            });
        }
        Ok(())
    };
    if let Some(value) = item.conversion.as_ref().and_then(|c| c.value.as_ref()) {
        check(value, "value of this conversion")?;
    }
    if let Some(cost) = &item.campaign_cost {
        check(&cost.cost, "cost of this campaign")?;
    }
    Ok(())
}

/// Translate a store failure into the error taxonomy a caller acts on.
///
/// The retry fact is the part that matters. A store that cannot write can
/// succeed later, so the collector must keep the batch. A store that refused the
/// request cannot, so the collector must stop trying.
pub fn to_service_error(error: StoreError) -> TallyOwlError {
    match error {
        StoreError::Unavailable(message) => {
            TallyOwlError::unavailable(format!("We could not store this data right now. {message}"))
        }
        StoreError::Damaged(message) => TallyOwlError::new(
            tallyowl_obs::ErrorCode::IncompleteResult,
            format!("Some stored data could not be read. {message}"),
        ),
        StoreError::InvalidArgument(message) => TallyOwlError::invalid_argument(message),
        // Retryable, and not the same retry as `unavailable`. A device with no
        // room does not clear in a second, so a caller that treats this as a
        // brief outage will hammer a node that cannot answer. The backoff for
        // `resource-exhausted` is the one this needs.
        StoreError::Exhausted(message) => TallyOwlError::new(
            tallyowl_obs::ErrorCode::ResourceExhausted,
            format!("We could not store this data. {message}"),
        ),
    }
}
