//! The collection policy a collector holds, and how it gets it.
//!
//! `docs/POLICY.md` section 7 states the whole contract in three sentences and
//! this module holds all three:
//!
//! > A collector fetches a snapshot over CSIL-RPC and passes its known version.
//! > The head returns nothing when the version is current.
//! >
//! > A collector applies a new snapshot at a batch boundary, never inside a
//! > batch.
//! >
//! > A kill switch takes effect at the next fetch. It is not a substitute for
//! > revocation, which the head enforces at the next batch.
//!
//! # Why this exists when the head already enforces the policy
//!
//! The head applies the same policy at commit, so nothing wrong is stored
//! either way. What this saves is everything before the commit: **a blocked
//! event that only the head refuses has already cost a batch, a durable queue
//! write, a delivery attempt, and a receipt.** A kill switch that only takes
//! effect after transport is not a kill switch; it is a filter on what gets
//! stored, and the operator who pulled it wanted collection to stop.
//!
//! # The three rules that make this safe
//!
//! **Invalid policy never replaces valid policy.** A snapshot that will not
//! read, or that names no version, is refused and the last good one keeps
//! working. `docs/POLICY.md` section 8 makes this the first required test,
//! because a broken snapshot would stop collection everywhere at once.
//!
//! **A collector with no head keeps its last good snapshot and reports
//! staleness.** It does not fall back to collecting nothing, and it does not
//! fall back to collecting everything. Both would be a policy change that
//! nobody made, caused by a network fault.
//!
//! **A policy applies at a batch boundary.** [`Held::current`] is read once
//! when a batch arrives and the whole batch is judged against that one
//! snapshot, so a fetch that lands halfway through cannot make the first half
//! of a batch obey one policy and the second half another.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tallyowl_collector_api::types::{
    CampaignLinking, CollectionPolicy, TelemetryItem, TelemetryKind,
};
use tallyowl_obs::error::TallyOwlError;
use tallyowl_obs::log::Logger;
use tallyowl_obs::metrics::{labels, Registry};

/// Where a snapshot comes from.
///
/// It is a trait so that a test can make the head answer with a broken
/// snapshot, an old one, or nothing at all. This is a network boundary rather
/// than the storage interface, so `AGENTS.md` permits the seam.
pub trait PolicySource: Send + Sync {
    /// Ask for a snapshot, passing the version already held.
    ///
    /// `Ok(None)` means the held version is current. It is not the same as an
    /// empty policy, and the difference is the whole point of the operation.
    fn fetch_policy(
        &self,
        source_id: &[u8],
        known_version: Option<u64>,
    ) -> Result<Option<CollectionPolicy>, TallyOwlError>;
}

/// One compiled snapshot, in the form intake reads.
///
/// It is the wire type with the lookups turned into sets, because intake asks
/// "is this name blocked" for every item of every batch and a linear scan of a
/// list would put the policy on the hot path.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub version: u64,
    pub enabled_kinds: std::collections::BTreeSet<String>,
    pub blocked_event_names: std::collections::BTreeSet<String>,
    pub blocked_property_keys: std::collections::BTreeSet<String>,
    pub redact_property_keys: std::collections::BTreeSet<String>,
    pub campaign_linking: CampaignLinking,
    pub kill_switch: bool,
    pub max_event_bytes: u64,
    pub max_properties: u64,
}

/// What a redacted value becomes. The same text the head writes, because a
/// query cannot tell which side redacted a value and should not have to.
pub const REDACTED: &str = "(redacted)";

impl Snapshot {
    /// Read a wire snapshot, or say why it cannot be applied.
    ///
    /// A snapshot with **version 0** is refused. The head raises the version on
    /// every write and never hands out 0 for a policy somebody set, so a zero
    /// here is a snapshot that was never compiled. Applying it would let a
    /// half-built response switch off collection.
    pub fn read(policy: &CollectionPolicy) -> Result<Snapshot, TallyOwlError> {
        if policy.policy_version == 0 {
            return Err(TallyOwlError::invalid_argument(
                "This collection policy carries no version, so there is no way to tell it from an older one. It is not applied and the last good policy is still in force.",
            ));
        }
        if !(0.0..=1.0).contains(&policy.head_sample_rate) {
            return Err(TallyOwlError::invalid_argument(format!(
                "This collection policy asks for a sampling rate of {}, which is not between 0 and 1. It is not applied.",
                policy.head_sample_rate
            )));
        }
        Ok(Snapshot {
            version: policy.policy_version,
            enabled_kinds: policy
                .enabled_kinds
                .iter()
                .map(kind_name)
                .map(String::from)
                .collect(),
            blocked_event_names: policy
                .blocked_event_names
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            blocked_property_keys: policy
                .blocked_property_keys
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            redact_property_keys: policy
                .redact_keys
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            campaign_linking: policy
                .campaign_linking
                .clone()
                .unwrap_or(CampaignLinking::Linked),
            kill_switch: policy.kill_switch.unwrap_or(false),
            max_event_bytes: policy.max_event_bytes,
            max_properties: policy.max_properties,
        })
    }

    /// Whether this snapshot collects one item.
    ///
    /// An empty `enabled_kinds` means every kind, because a project that has
    /// never listed one has not said "none" — it has said nothing. The head
    /// reads it the same way, and the two must agree.
    pub fn keeps(&self, item: &TelemetryItem) -> bool {
        if self.kill_switch {
            return false;
        }
        let kind = kind_name(&item.envelope.kind);
        if !self.enabled_kinds.is_empty() && !self.enabled_kinds.contains(kind) {
            return false;
        }
        if item.envelope.kind == TelemetryKind::CampaignTouch
            && self.campaign_linking == CampaignLinking::None
        {
            return false;
        }
        !self.blocked_event_names.contains(&item_name(item))
    }

    /// Apply the property rules to one item, in place.
    ///
    /// Returns how many properties were dropped and how many were replaced. A
    /// refusal is counted rather than silent: `AGENTS.md` requires it, because
    /// a silent drop is how somebody discovers a policy by noticing a gap.
    pub fn apply_properties(&self, item: &mut TelemetryItem) -> (u64, u64) {
        if self.blocked_property_keys.is_empty() && self.redact_property_keys.is_empty() {
            return (0, 0);
        }
        let mut blocked = 0;
        let mut redacted = 0;
        let before = item.envelope.properties.len();
        item.envelope
            .properties
            .retain(|property| !self.blocked_property_keys.contains(&property.key));
        blocked += (before - item.envelope.properties.len()) as u64;

        for property in &mut item.envelope.properties {
            if self.redact_property_keys.contains(&property.key) {
                property.value = tallyowl_wire::collector::write(&tallyowl_wire::Value::Text(
                    REDACTED.to_string(),
                ));
                redacted += 1;
            }
        }
        (blocked, redacted)
    }

    /// Remove the identity links from a campaign touch, when the policy asks.
    ///
    /// D30 puts consent at the point where campaign data joins an identified
    /// end user, and this is the collector's half of that: the campaign facts
    /// travel and the person does not. Returns how many links came off.
    pub fn unlink_campaign(&self, item: &mut TelemetryItem) -> u64 {
        if self.campaign_linking != CampaignLinking::Unlinked
            || item.envelope.kind != TelemetryKind::CampaignTouch
        {
            return 0;
        }
        let mut removed = 0;
        for held in [
            &mut item.envelope.session_id,
            &mut item.envelope.end_user_id,
            &mut item.envelope.anonymous_id,
        ] {
            if held.take().is_some() {
                removed += 1;
            }
        }
        removed
    }
}

/// The snapshot a collector holds, and how old it is.
pub struct Held {
    current: Mutex<Option<Arc<Snapshot>>>,
    /// The version being applied. Zero when none is.
    version: AtomicU64,
    /// When the last successful fetch returned, in milliseconds since the
    /// epoch. Zero when none has.
    fetched_at: AtomicI64,
    /// How long a snapshot may go without a successful fetch before readiness
    /// reports it as stale.
    pub staleness_ms: i64,
}

impl Held {
    pub fn new(staleness_ms: i64) -> Held {
        Held {
            current: Mutex::new(None),
            version: AtomicU64::new(0),
            fetched_at: AtomicI64::new(0),
            staleness_ms,
        }
    }

    /// The snapshot to judge one batch against.
    ///
    /// Read **once** at the start of a batch. A batch judged against two
    /// snapshots would obey one policy in its first half and another in its
    /// second, which is what "never inside a batch" forbids.
    pub fn current(&self) -> Option<Arc<Snapshot>> {
        self.current.lock().expect("policy").clone()
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// Whether the last successful fetch is older than the staleness bound.
    ///
    /// A collector that has never fetched is **not** stale: it has not lost
    /// touch with the head, it has not started asking yet. The distinction
    /// matters because readiness reads this and a collector that reported stale
    /// before its first fetch would never start.
    pub fn is_stale(&self, now: i64) -> bool {
        let at = self.fetched_at.load(Ordering::Relaxed);
        at > 0 && now - at > self.staleness_ms
    }

    pub fn fetched_at(&self) -> i64 {
        self.fetched_at.load(Ordering::Relaxed)
    }

    /// Apply a snapshot, or refuse it and keep the one in force.
    pub fn apply(&self, policy: &CollectionPolicy) -> Result<u64, TallyOwlError> {
        let snapshot = Snapshot::read(policy)?;
        let version = snapshot.version;
        *self.current.lock().expect("policy") = Some(Arc::new(snapshot));
        self.version.store(version, Ordering::Relaxed);
        Ok(version)
    }

    /// Record that a fetch reached the head, whatever it returned.
    pub fn saw_head(&self, now: i64) {
        self.fetched_at.store(now, Ordering::Relaxed);
    }
}

/// The fetch loop. `docs/POLICY.md` section 7.
pub struct Refresher {
    pub held: Arc<Held>,
    pub source: Arc<dyn PolicySource>,
    /// Which source this collector fetches for.
    ///
    /// It is resolved from the collector's own credential rather than
    /// configured, for the same reason a batch's tenancy is: a collector never
    /// holds a source identifier of its own and never sends tenancy. See D32.
    /// It is resolved on each fetch because the resolver caches it, and because
    /// a collector that could not reach the head at start-up must still be able
    /// to fetch a policy once the head comes back.
    pub tenancy: Arc<crate::tenancy::TenancyResolver>,
    pub credential: String,
    pub metrics: Arc<Registry>,
    pub logger: Arc<Logger>,
    /// The forwarder's stop flag. One flag stops every thread this process
    /// runs, so a shutdown cannot leave the policy loop fetching against a head
    /// that nothing else is talking to.
    pub stopping: Arc<crate::forwarder::ForwarderState>,
}

impl Refresher {
    pub fn declare_metrics(metrics: &Registry) {
        metrics.declare(
            "tallyowl_policy_fetches_total",
            tallyowl_obs::MetricKind::Counter,
            "Collection policy fetches, by outcome.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_policy_fetches_total` is not a name the registry accepts: {}", e.0));
        // `_count` rather than `_version`, because `check_name` permits only
        // the unit suffixes in `tallyowl_obs::metrics` and `declare` returns a
        // refusal for anything else. The first name here was
        // `tallyowl_policy_version`, the declaration was refused, and the gauge
        // was missing from the exposition while everything it measures worked.
        // The running loop found it. See L124.
        metrics.declare(
            "tallyowl_policy_generation_count",
            tallyowl_obs::MetricKind::Gauge,
            "The collection policy generation this collector applies. Zero means it holds none.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_policy_generation_count` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_items_dropped_by_policy_total",
            tallyowl_obs::MetricKind::Counter,
            "Items the collector did not accept because the collection policy refuses them.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_items_dropped_by_policy_total` is not a name the registry accepts: {}", e.0));
        metrics.declare(
            "tallyowl_policy_properties_refused_total",
            tallyowl_obs::MetricKind::Counter,
            "Properties the collection policy dropped or replaced at the collector, by reason.",
            &[],
        )
        .unwrap_or_else(|e| panic!("the metric `tallyowl_policy_properties_refused_total` is not a name the registry accepts: {}", e.0));
    }

    /// One fetch. It returns whether the held snapshot changed.
    pub fn fetch_once(&self) -> bool {
        let source_id = match self.tenancy.resolve(&self.credential) {
            Ok(tenancy) => tenancy.source_id.to_vec(),
            Err(e) => {
                self.metrics.increment(
                    "tallyowl_policy_fetches_total",
                    &labels(&[("outcome", "unresolved")]),
                );
                self.logger.warning(
                    "Could not resolve this collector's own credential, so it could not ask for a collection policy. The last one it was given is still in force.",
                    &[("reason", &e.message)],
                );
                return false;
            }
        };
        let held_version = match self.held.version() {
            0 => None,
            version => Some(version),
        };
        match self.source.fetch_policy(&source_id, held_version) {
            Err(e) => {
                // The last good snapshot stays in force and staleness is what
                // reports the fault. Falling back to no policy would be a policy
                // change caused by a network fault.
                self.metrics.increment(
                    "tallyowl_policy_fetches_total",
                    &labels(&[("outcome", "unreachable")]),
                );
                self.logger.warning(
                    "Could not fetch the collection policy. The last one this collector was given is still in force.",
                    &[
                        ("reason", &e.message),
                        ("applied_version", &self.held.version().to_string()),
                    ],
                );
                false
            }
            Ok(None) => {
                self.held.saw_head(tallyowl_obs::time::now_ms());
                self.metrics.increment(
                    "tallyowl_policy_fetches_total",
                    &labels(&[("outcome", "unchanged")]),
                );
                false
            }
            Ok(Some(policy)) => {
                self.held.saw_head(tallyowl_obs::time::now_ms());
                match self.held.apply(&policy) {
                    Err(e) => {
                        self.metrics.increment(
                            "tallyowl_policy_fetches_total",
                            &labels(&[("outcome", "refused")]),
                        );
                        self.logger.error(
                            "The collection policy the head sent could not be applied. The last good one is still in force.",
                            &[
                                ("reason", &e.message),
                                ("applied_version", &self.held.version().to_string()),
                            ],
                        );
                        false
                    }
                    Ok(version) => {
                        self.metrics.increment(
                            "tallyowl_policy_fetches_total",
                            &labels(&[("outcome", "applied")]),
                        );
                        self.metrics.set_gauge(
                            "tallyowl_policy_generation_count",
                            &labels(&[]),
                            version as i64,
                        );
                        // The field is `policy_version` rather than `version`,
                        // because every log line already carries the software
                        // version under that name and the second one silently
                        // replaced the first. The running loop showed it.
                        self.logger.info(
                            "Applied a collection policy.",
                            &[("policy_version", &version.to_string())],
                        );
                        true
                    }
                }
            }
        }
    }
}

/// Run the fetch loop until the collector stops.
pub fn run(refresher: Arc<Refresher>, interval: Duration) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("tallyowl-policy".into())
        .spawn(move || {
            // It sleeps first. The caller fetches once before the listener
            // opens, so the first batch this collector accepts is judged
            // against a policy; fetching again immediately would only ask the
            // head the same question twice.
            while !refresher.stopping.stopping.load(Ordering::Relaxed) {
                std::thread::sleep(interval);
                refresher.fetch_once();
            }
        })
        .expect("the policy thread starts")
}

/// The name a telemetry kind is stored and blocked under.
pub fn kind_name(kind: &TelemetryKind) -> &'static str {
    match kind {
        TelemetryKind::Event => "event",
        TelemetryKind::PageView => "page-view",
        TelemetryKind::SessionStart => "session-start",
        TelemetryKind::SessionEnd => "session-end",
        TelemetryKind::SessionHeartbeat => "session-heartbeat",
        TelemetryKind::Interaction => "interaction",
        TelemetryKind::FeatureExposure => "feature-exposure",
        TelemetryKind::Identify => "identify",
        TelemetryKind::Alias => "alias",
        TelemetryKind::Group => "group",
        TelemetryKind::Conversion => "conversion",
        TelemetryKind::Error => "error",
        TelemetryKind::Span => "span",
        TelemetryKind::MetricPoint => "metric-point",
        TelemetryKind::CampaignTouch => "campaign-touch",
        TelemetryKind::CampaignCost => "campaign-cost",
    }
}

/// The name a policy blocks an item by.
///
/// **It has to be the name the head projects**, or a rule that blocks
/// `debug-ping` at the head would not block it here and the saving this module
/// exists for would be silent and partial. `tallyowl_head::project` computes the
/// same name from the same payload; this reads the wire item, which is what a
/// collector has.
pub fn item_name(item: &TelemetryItem) -> String {
    if let Some(event) = &item.event {
        return event.name.clone();
    }
    if let Some(page) = &item.page_view {
        return page.route.clone();
    }
    if let Some(interaction) = &item.interaction {
        return format!("{}:{}", interaction.target, interaction.action);
    }
    if let Some(exposure) = &item.feature_exposure {
        return exposure.feature.clone();
    }
    if let Some(conversion) = &item.conversion {
        return conversion.goal.clone();
    }
    if let Some(error) = &item.error {
        return error.error_type.clone();
    }
    if let Some(span) = &item.span {
        return span.operation.clone();
    }
    if let Some(metric) = &item.metric_point {
        return metric.metric_name.clone();
    }
    if let Some(cost) = &item.campaign_cost {
        return cost.campaign.clone();
    }
    if let Some(touch) = &item.campaign_touch {
        return touch
            .campaign
            .campaign
            .clone()
            .unwrap_or_else(|| "campaign-touch".to_string());
    }
    if let Some(group) = &item.group {
        return group.group_id.clone();
    }
    kind_name(&item.envelope.kind).to_string()
}

pub mod testing {
    use super::*;

    /// A head that answers however a test needs it to.
    pub struct FakeSource {
        answers: Mutex<Vec<Result<Option<CollectionPolicy>, TallyOwlError>>>,
        pub asked: Mutex<Vec<Option<u64>>>,
    }

    impl FakeSource {
        /// Answers are given in order, and the last one repeats.
        pub fn new(
            answers: Vec<Result<Option<CollectionPolicy>, TallyOwlError>>,
        ) -> Arc<FakeSource> {
            Arc::new(FakeSource {
                answers: Mutex::new(answers),
                asked: Mutex::new(Vec::new()),
            })
        }
    }

    impl PolicySource for FakeSource {
        fn fetch_policy(
            &self,
            _source_id: &[u8],
            known_version: Option<u64>,
        ) -> Result<Option<CollectionPolicy>, TallyOwlError> {
            self.asked.lock().unwrap().push(known_version);
            let mut answers = self.answers.lock().unwrap();
            if answers.len() > 1 {
                answers.remove(0)
            } else {
                match answers.first() {
                    None => Ok(None),
                    Some(Ok(policy)) => Ok(policy.clone()),
                    Some(Err(e)) => Err(TallyOwlError::new(e.code, e.message.clone())),
                }
            }
        }
    }

    /// A snapshot with everything set to what a test does not care about.
    pub fn snapshot(version: u64) -> CollectionPolicy {
        CollectionPolicy {
            policy_version: version,
            enabled_kinds: Vec::new(),
            head_sample_rate: 1.0,
            tail_rules: None,
            tail_decision_window_ms: None,
            late_span_grace_ms: None,
            always_keep_expressions: None,
            retention: Vec::new(),
            protected_keys: Vec::new(),
            stamped_properties: Vec::new(),
            max_event_bytes: 64 * 1024,
            max_batch_bytes: 4 * 1024 * 1024,
            max_properties: 256,
            session_max_lifetime_ms: 12 * 3_600_000,
            redact_keys: None,
            blocked_event_names: None,
            blocked_property_keys: None,
            campaign_linking: None,
            kill_switch: None,
        }
    }
}
