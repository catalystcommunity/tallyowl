//! Collection policy: what an installation collects, and what it refuses to.
//!
//! `docs/POLICY.md` section 1 gives the shape and this holds it:
//!
//! ```text
//! installation defaults
//!   → workspace overrides
//!     → project overrides
//!       → environment overrides
//!         → source overrides
//! ```
//!
//! **A narrower level overrides a wider one**, and the head compiles the result
//! into one snapshot with one version number.
//!
//! # Two rules that are not obvious from that picture
//!
//! **Invalid policy never replaces valid policy.** Section 1 says so, and
//! [`Policies::put`] refuses a document that would not compile rather than
//! storing it and failing later. A collector that fetched a broken snapshot
//! would stop collecting, and it would do that everywhere at once.
//!
//! **A refusal is not a narrower override.** A wider level that blocks an event
//! name blocks it, and a narrower level cannot unblock it. Inheritance widens
//! what is set and never widens what is forbidden: an installation that
//! switched off a telemetry kind for a legal reason must not be undone by a
//! project setting. Every other field takes the narrowest value that is set.
//!
//! # Where it is applied, and where it is not
//!
//! It is applied at the head, in [`crate::ingest`], because that is the
//! durability boundary and a rule enforced only at the edge is a rule an edge
//! that skipped it can break.
//!
//! It is **also** applied at the collector. `docs/POLICY.md` section 7
//! distributes the compiled snapshot, [`PolicyService::snapshot_for`] produces
//! it, and `tallyowl_collector::policy` fetches, caches, and applies it, so a
//! blocked event costs no batch, no queue write, and no delivery. The two ends
//! come from one compilation, translated twice: a collector that dropped
//! something the head would have kept would lose data no query could find
//! again.

use std::collections::{BTreeMap, BTreeSet};

use tallyowl_obs::error::TallyOwlError;
use tallyowl_store::row::EventRow;

/// Which level a document sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Installation,
    Workspace,
    Project,
    Environment,
    Source,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Installation => "installation",
            Scope::Workspace => "workspace",
            Scope::Project => "project",
            Scope::Environment => "environment",
            Scope::Source => "source",
        }
    }

    pub fn parse(text: &str) -> Option<Scope> {
        Some(match text {
            "installation" => Scope::Installation,
            "workspace" => Scope::Workspace,
            "project" => Scope::Project,
            "environment" => Scope::Environment,
            "source" => Scope::Source,
            _ => return None,
        })
    }
}

/// One level's settings. An absent field inherits the wider level.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Document {
    pub scope: Option<Scope>,
    /// Which workspace, project, environment, or source this level names.
    /// Empty at the installation level.
    pub scope_id: String,
    pub enabled_kinds: Option<Vec<String>>,
    pub head_sample_rate: Option<f64>,
    pub session_max_lifetime_ms: Option<i64>,
    pub max_event_bytes: Option<u64>,
    pub max_properties: Option<u64>,
    /// Event names this level refuses to collect. Cumulative: see the module
    /// note on why a narrower level cannot unblock one.
    pub blocked_event_names: Vec<String>,
    /// Property keys this level drops before anything is stored.
    pub blocked_property_keys: Vec<String>,
    /// Property keys whose value is replaced rather than dropped, so a query
    /// can still see that the field was present.
    pub redact_property_keys: Vec<String>,
    /// How much of a campaign touch this level keeps. D30.
    pub campaign_linking: Option<Capture>,
    /// Whether attribution reads a person who denied marketing consent. D30.
    pub attribution_needs_consent: Option<bool>,
    /// Stop collecting for this scope entirely.
    pub kill_switch: Option<bool>,
}

/// How much of a campaign touch an installation keeps. D30.
///
/// Consent applies to personal data, and a campaign fact on its own is not
/// personal data. It applies where campaign data **joins an identified end
/// user**, and a session identifier is that point, because it is what links
/// touches over time.
///
/// `Unlinked` exists because D30 requires it: an operator who turns off
/// session-linked campaign data must still be able to measure whether a
/// campaign works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capture {
    /// Keep the touch and its links, which is what attribution needs.
    Linked,
    /// Keep the campaign facts and remove the session, end-user, and anonymous
    /// links. A campaign is still measured and nothing is attributed to a
    /// person.
    Unlinked,
    /// Refuse the touch.
    None,
}

impl Capture {
    pub fn as_str(&self) -> &'static str {
        match self {
            Capture::Linked => "linked",
            Capture::Unlinked => "unlinked",
            Capture::None => "none",
        }
    }

    pub fn parse(text: &str) -> Option<Capture> {
        Some(match text {
            "linked" => Capture::Linked,
            "unlinked" => Capture::Unlinked,
            "none" => Capture::None,
            _ => return None,
        })
    }

    /// The narrower of two settings.
    ///
    /// A narrower level can only keep less, for the same reason it cannot
    /// unblock an event name. `Linked` is the widest and `None` the narrowest,
    /// which is the order the variants are declared in.
    pub fn narrower(self, other: Capture) -> Capture {
        self.max(other)
    }
}

impl Document {
    pub fn at(scope: Scope, scope_id: impl Into<String>) -> Document {
        Document {
            scope: Some(scope),
            scope_id: scope_id.into(),
            ..Document::default()
        }
    }

    /// Whether this document could be compiled.
    ///
    /// It is checked before it is stored. See the module note.
    pub fn check(&self) -> Result<(), TallyOwlError> {
        let Some(scope) = self.scope else {
            return Err(TallyOwlError::invalid_argument(
                "This policy document does not say which level it sets. Name the scope.",
            ));
        };
        if scope != Scope::Installation && self.scope_id.trim().is_empty() {
            return Err(TallyOwlError::invalid_argument(format!(
                "A `{}` policy has to name which one it applies to.",
                scope.as_str()
            )));
        }
        if let Some(rate) = self.head_sample_rate {
            if !(0.0..=1.0).contains(&rate) {
                return Err(TallyOwlError::invalid_argument(format!(
                    "A head sampling rate is between 0 and 1, and this one is {rate}."
                )));
            }
        }
        if self
            .enabled_kinds
            .as_ref()
            .is_some_and(|kinds| kinds.is_empty())
        {
            return Err(TallyOwlError::invalid_argument(
                "This policy enables no telemetry kind at all. Use the kill switch if that is what you mean, so that it reads as deliberate.",
            ));
        }
        if self.session_max_lifetime_ms.is_some_and(|ms| ms <= 0) {
            return Err(TallyOwlError::invalid_argument(
                "A session maximum lifetime has to be more than nothing.",
            ));
        }
        Ok(())
    }
}

/// The compiled result of every level that applies.
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    pub version: u64,
    pub enabled_kinds: BTreeSet<String>,
    pub head_sample_rate: f64,
    pub session_max_lifetime_ms: i64,
    pub max_event_bytes: u64,
    pub max_properties: u64,
    pub blocked_event_names: BTreeSet<String>,
    pub blocked_property_keys: BTreeSet<String>,
    pub redact_property_keys: BTreeSet<String>,
    pub campaign_linking: Capture,
    pub attribution_needs_consent: bool,
    pub kill_switch: bool,
    /// Which levels contributed, widest first. A person reading a compiled
    /// policy needs to know where a value came from, or the only way to find
    /// out is to change one and see what moves.
    pub from_levels: Vec<String>,
}

/// What the campaign-capture setting did to one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Captured {
    /// Nothing changed.
    Kept,
    /// The row is not collected at all.
    Refused,
    /// The identity links came off, and this many.
    Unlinked(usize),
    /// The campaign properties came off, and this many.
    Stripped(usize),
}

/// The campaign properties a policy removes.
///
/// The classified channel and the classifier version go with them. A row that
/// kept its channel and lost its campaign would be a campaign fact with the
/// name filed off rather than a row with no campaign fact on it.
pub const CAMPAIGN_KEYS: &[&str] = &[
    "campaign",
    "campaign_source",
    "campaign_medium",
    "campaign_term",
    "campaign_content",
    "campaign_click_id",
    "campaign_channel",
    "campaign_classifier_version",
    "referrer",
    "referrer_domain",
    "landing_route",
];

fn strip_campaign(row: &mut EventRow) -> usize {
    let mut removed = 0;
    for key in CAMPAIGN_KEYS {
        if row.properties.remove(*key).is_some() {
            removed += 1;
        }
    }
    removed
}

/// What a redacted value becomes.
///
/// The key stays so that a query can still see that the field was present,
/// which is the difference between redacting and blocking. `AGENTS.md`: "Never
/// record secrets, credentials, request bodies, claim values, or raw personal
/// data by default."
pub const REDACTED: &str = "(redacted)";

/// The defaults an installation starts with.
///
/// `docs/POLICY.md` section 6 holds the numeric defaults. These are the ones a
/// project that has never been configured gets, and every one of them can be
/// narrowed.
impl Default for Compiled {
    fn default() -> Compiled {
        Compiled {
            version: 0,
            enabled_kinds: BTreeSet::new(),
            head_sample_rate: 1.0,
            session_max_lifetime_ms: 12 * 3_600_000,
            max_event_bytes: 64 * 1024,
            max_properties: 256,
            blocked_event_names: BTreeSet::new(),
            blocked_property_keys: BTreeSet::new(),
            redact_property_keys: BTreeSet::new(),
            // A campaign fact tied to nobody is not personal data, and D30
            // captures it by default. An installation that must collect less
            // says so; TallyOwl is not the policy authority for an application.
            campaign_linking: Capture::Linked,
            // Silence is not a denial. An application that never sent a consent
            // state has not said no on its person's behalf, and the stricter
            // reading is the operator's choice to make.
            attribution_needs_consent: false,
            kill_switch: false,
            from_levels: Vec::new(),
        }
    }
}

impl Compiled {
    /// Whether one row survives this policy.
    ///
    /// An empty `enabled_kinds` means every kind, because a project that has
    /// never listed one has not said "none" — it has said nothing.
    pub fn keeps(&self, row: &EventRow) -> bool {
        if self.kill_switch {
            return false;
        }
        if !self.enabled_kinds.is_empty() && !self.enabled_kinds.contains(&row.kind) {
            return false;
        }
        !self.blocked_event_names.contains(&row.name)
    }

    /// What the campaign-capture setting did to one row.
    #[must_use]
    pub fn capture(&self, row: &mut EventRow) -> Captured {
        let is_touch = row.kind == "campaign-touch";
        match self.campaign_linking {
            Capture::Linked => Captured::Kept,
            Capture::None => {
                if is_touch {
                    return Captured::Refused;
                }
                let removed = strip_campaign(row);
                if removed > 0 {
                    Captured::Stripped(removed)
                } else {
                    Captured::Kept
                }
            }
            Capture::Unlinked => {
                if is_touch {
                    // The campaign facts stay and the person goes. D30: an
                    // operator who turns off session-linked campaign data still
                    // measures whether a campaign works.
                    let mut removed = 0;
                    if row.session_id.take().is_some() {
                        removed += 1;
                    }
                    for key in [crate::identity::END_USER_ID, crate::identity::ANONYMOUS_ID] {
                        if row.properties.remove(key).is_some() {
                            removed += 1;
                        }
                    }
                    return if removed > 0 {
                        Captured::Unlinked(removed)
                    } else {
                        Captured::Kept
                    };
                }
                // Every other kind keeps its links and loses the campaign. A
                // page view is somebody moving around the application and its
                // session is what a funnel correlates on; the campaign fact on
                // it is the join this setting exists to break, and it lives on
                // the unlinked touch instead.
                let removed = strip_campaign(row);
                if removed > 0 {
                    Captured::Stripped(removed)
                } else {
                    Captured::Kept
                }
            }
        }
    }

    /// Apply the property rules to one row, in place.
    ///
    /// A blocked key is removed and a redacted one keeps its name. Both are
    /// counted by the caller: `AGENTS.md` requires a refused property to be
    /// counted, because a silent drop is how a person discovers a policy by
    /// noticing a gap.
    pub fn apply_properties(&self, row: &mut EventRow) -> (usize, usize) {
        let mut blocked = 0;
        let mut redacted = 0;
        row.properties.retain(|key, _| {
            let keep = !self.blocked_property_keys.contains(key);
            if !keep {
                blocked += 1;
            }
            keep
        });
        for key in &self.redact_property_keys {
            if let Some(held) = row.properties.get_mut(key) {
                held.0 = tallyowl_store::row::PropertyValue::Text(REDACTED.to_string());
                redacted += 1;
            }
        }
        (blocked, redacted)
    }
}

/// Every stored policy document, and the compiler over them.
#[derive(Debug, Clone, Default)]
pub struct Policies {
    /// Scope and scope identifier to the document.
    documents: BTreeMap<(Scope, String), Document>,
    /// How many times any level has been written. It is the catalog's number
    /// when there is a catalog, so that it survives a restart: a collector
    /// compares the version it applies against this one.
    pub version: u64,
}

impl Policies {
    pub fn new() -> Policies {
        Policies::default()
    }

    /// Store one level's document.
    ///
    /// It compiles first. Invalid policy never replaces valid policy.
    pub fn put(&mut self, document: Document) -> Result<u64, TallyOwlError> {
        document.check()?;
        let scope = document.scope.expect("checked");
        self.documents
            .insert((scope, document.scope_id.clone()), document);
        self.version += 1;
        Ok(self.version)
    }

    pub fn get(&self, scope: Scope, scope_id: &str) -> Option<&Document> {
        self.documents.get(&(scope, scope_id.to_string()))
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Compile the policy that applies to one source.
    ///
    /// Each named level is applied in turn, widest first, so the narrowest one
    /// that sets a field wins it.
    pub fn compile(
        &self,
        workspace_id: &str,
        project_id: &str,
        environment: &str,
        source_id: &str,
    ) -> Compiled {
        let mut out = Compiled {
            version: self.version,
            ..Compiled::default()
        };
        for (scope, id) in [
            (Scope::Installation, ""),
            (Scope::Workspace, workspace_id),
            (Scope::Project, project_id),
            (Scope::Environment, environment),
            (Scope::Source, source_id),
        ] {
            let Some(document) = self.documents.get(&(scope, id.to_string())) else {
                continue;
            };
            out.from_levels.push(match id {
                "" => scope.as_str().to_string(),
                named => format!("{}:{named}", scope.as_str()),
            });

            if let Some(kinds) = &document.enabled_kinds {
                out.enabled_kinds = kinds.iter().cloned().collect();
            }
            if let Some(rate) = document.head_sample_rate {
                out.head_sample_rate = rate;
            }
            if let Some(ms) = document.session_max_lifetime_ms {
                out.session_max_lifetime_ms = ms;
            }
            if let Some(bytes) = document.max_event_bytes {
                out.max_event_bytes = bytes;
            }
            if let Some(count) = document.max_properties {
                out.max_properties = count;
            }
            if let Some(capture) = document.campaign_linking {
                // Narrower only. A workspace that switched off session-linked
                // campaign data for a legal reason must not be undone by a
                // project setting.
                out.campaign_linking = out.campaign_linking.narrower(capture);
            }
            if let Some(needs) = document.attribution_needs_consent {
                // The same rule: a level that required consent required it.
                out.attribution_needs_consent = out.attribution_needs_consent || needs;
            }
            if let Some(stopped) = document.kill_switch {
                // A kill switch set anywhere holds. A narrower level cannot
                // switch collection back on, for the same reason it cannot
                // unblock an event name.
                out.kill_switch = out.kill_switch || stopped;
            }
            // Refusals accumulate. See the module note.
            out.blocked_event_names
                .extend(document.blocked_event_names.iter().cloned());
            out.blocked_property_keys
                .extend(document.blocked_property_keys.iter().cloned());
            out.redact_property_keys
                .extend(document.redact_property_keys.iter().cloned());
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The wire surface
// ---------------------------------------------------------------------------

/// The head's collection policy.
///
/// **It is durable.** Every level is written to the control catalog, beside the
/// workspaces, projects, and keys, and it is read back at start-up.
/// `docs/FAILURE_MODES.md` section 7 lists saved control state among the nine
/// things a catalog rebuild cannot restore, which is the same reason a restart
/// must not lose it. See L112.
///
/// The working copy stays in memory, because the ingest path asks for a
/// compiled policy for every row and a durable read for every row would put the
/// catalog on the hot path. The two cannot disagree: a write goes to the
/// catalog **first** and reaches memory only when that returns, so a failure to
/// make a policy durable is a failure to apply it.
pub struct PolicyService {
    store: Option<std::sync::Arc<tallyowl_store::SegmentedStore>>,
    held: std::sync::Mutex<Policies>,
    /// The compiled result for each project, so the ingest path does not
    /// recompile five levels for every row. Emptied on every write.
    compiled: std::sync::Mutex<BTreeMap<[u8; 16], std::sync::Arc<Compiled>>>,
}

impl Default for PolicyService {
    fn default() -> PolicyService {
        PolicyService::new()
    }
}

impl PolicyService {
    /// A policy set that nothing outlives.
    ///
    /// For a test that is not about durability, and for a head that has no
    /// catalog to keep one in.
    pub fn new() -> PolicyService {
        PolicyService {
            store: None,
            held: std::sync::Mutex::new(Policies::new()),
            compiled: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// The installation's policy, read back from its catalog.
    ///
    /// A stored record that will not compile is **skipped and named** rather
    /// than refused. A head that would not start because one stored document
    /// was unreadable is a head an upgrade could brick, and the other levels
    /// are still valid policy. The caller logs what did not apply.
    pub fn open(
        store: std::sync::Arc<tallyowl_store::SegmentedStore>,
    ) -> (PolicyService, Vec<String>) {
        let mut policies = Policies::new();
        let mut refused = Vec::new();
        match store.catalog().policies() {
            Err(e) => refused.push(format!(
                "The stored collection policy could not be read: {e}"
            )),
            Ok(records) => {
                for record in records {
                    let applied = from_record(&record).and_then(|document| {
                        policies.put(document)?;
                        Ok(())
                    });
                    if let Err(e) = applied {
                        refused.push(format!(
                            "The stored `{}` policy for `{}` was not applied: {}",
                            record.scope, record.scope_id, e.message
                        ));
                    }
                }
            }
        }
        // The version is the catalog's rather than a count of what loaded. A
        // collector compares it against the version it holds, so it has to be
        // the same number across a restart even when a record was skipped.
        policies.version = store.catalog().policy_generation().unwrap_or(0);
        (
            PolicyService {
                store: Some(store),
                held: std::sync::Mutex::new(policies),
                compiled: std::sync::Mutex::new(BTreeMap::new()),
            },
            refused,
        )
    }

    /// The version a collector reports as the one it applies.
    pub fn version(&self) -> u64 {
        self.held.lock().expect("policies").version
    }

    /// Store one level, and answer with what the whole set now compiles to for
    /// that level's own scope.
    pub fn put(
        &self,
        document: &tallyowl_control_api::types::PolicyDocument,
    ) -> Result<tallyowl_control_api::types::CompiledPolicy, TallyOwlError> {
        let taken = from_wire_document(document)?;
        // It compiles before it is stored, and that has to hold on the durable
        // copy as much as on the working one. Invalid policy never replaces
        // valid policy.
        taken.check()?;
        let scope_id = taken.scope_id.clone();
        let scope = taken.scope.unwrap_or(Scope::Installation);
        let mut held = self.held.lock().expect("policies");

        // Durable first. A policy that applied and did not survive would be one
        // an operator believes is set.
        if let Some(store) = &self.store {
            let generation = store
                .catalog()
                .put_policy(&to_record(&taken, tallyowl_obs::time::now_ms()))
                .map_err(|e| {
                    TallyOwlError::unavailable(format!(
                        "This policy was not stored, so it was not applied either: {e}"
                    ))
                })?;
            held.put(taken)?;
            held.version = generation;
        } else {
            held.put(taken)?;
        }
        self.compiled.lock().expect("compiled").clear();

        Ok(to_wire_compiled(&match scope {
            Scope::Installation => held.compile("", "", "", ""),
            Scope::Workspace => held.compile(&scope_id, "", "", ""),
            Scope::Project => held.compile("", &scope_id, "", ""),
            Scope::Environment => held.compile("", "", &scope_id, ""),
            Scope::Source => held.compile("", "", "", &scope_id),
        }))
    }

    /// What applies to one source.
    pub fn compiled(
        &self,
        request: &tallyowl_control_api::types::PolicyRequest,
    ) -> tallyowl_control_api::types::CompiledPolicy {
        let text = |bytes: &Option<Vec<u8>>| {
            bytes
                .as_deref()
                .map(tallyowl_store::row::hex)
                .unwrap_or_default()
        };
        let held = self.held.lock().expect("policies");
        to_wire_compiled(&held.compile(
            &text(&request.workspace_id),
            &text(&request.project_id),
            request.environment.as_deref().unwrap_or_default(),
            &text(&request.source_id),
        ))
    }

    /// The snapshot one collector applies. `docs/POLICY.md` section 7.
    ///
    /// It is the same compiled policy that [`Self::compiled`] returns, in the
    /// collector's shape. **The two must never disagree**: the head enforces
    /// this policy at commit and the collector enforces it at intake, and a
    /// collector that dropped something the head would have kept would lose
    /// data no query could find again. One compilation, translated twice.
    ///
    /// The `max_batch_bytes` a collector holds is its own operator
    /// configuration rather than a policy value, so the field carries the
    /// snapshot's event limit for the batch as well. A collector that already
    /// has a configured batch limit keeps the smaller of the two, which is what
    /// [`Compiled`] means by a narrower level.
    pub fn snapshot_for(
        &self,
        source: &tallyowl_store::control::Source,
    ) -> tallyowl_collector_api::types::CollectionPolicy {
        let held = self.held.lock().expect("policies");
        let compiled = held.compile(
            &tallyowl_store::row::hex(&source.workspace_id),
            &tallyowl_store::row::hex(&source.project_id),
            "",
            &tallyowl_store::row::hex(&source.source_id),
        );
        to_wire_snapshot(&compiled)
    }

    /// What applies to one project, for the ingest path.
    ///
    /// Cached, because this is asked for every row of every batch.
    pub fn for_project(&self, project_id: [u8; 16]) -> std::sync::Arc<Compiled> {
        if let Some(held) = self.compiled.lock().expect("compiled").get(&project_id) {
            return std::sync::Arc::clone(held);
        }
        let compiled = std::sync::Arc::new(self.held.lock().expect("policies").compile(
            "",
            &tallyowl_store::row::hex(&project_id),
            "",
            "",
        ));
        self.compiled
            .lock()
            .expect("compiled")
            .insert(project_id, std::sync::Arc::clone(&compiled));
        compiled
    }
}

/// One level, in the shape the catalog holds.
fn to_record(document: &Document, at: i64) -> tallyowl_store::control::PolicyRecord {
    tallyowl_store::control::PolicyRecord {
        scope: document
            .scope
            .unwrap_or(Scope::Installation)
            .as_str()
            .to_string(),
        scope_id: document.scope_id.clone(),
        enabled_kinds: document.enabled_kinds.clone(),
        head_sample_rate: document.head_sample_rate,
        session_max_lifetime_ms: document.session_max_lifetime_ms,
        max_event_bytes: document.max_event_bytes,
        max_properties: document.max_properties,
        blocked_event_names: document.blocked_event_names.clone(),
        blocked_property_keys: document.blocked_property_keys.clone(),
        redact_property_keys: document.redact_property_keys.clone(),
        campaign_linking: document.campaign_linking.map(|c| c.as_str().to_string()),
        attribution_needs_consent: document.attribution_needs_consent,
        kill_switch: document.kill_switch,
        updated_at: at,
    }
}

fn from_record(record: &tallyowl_store::control::PolicyRecord) -> Result<Document, TallyOwlError> {
    let scope = Scope::parse(&record.scope).ok_or_else(|| {
        TallyOwlError::invalid_argument(format!(
            "`{}` is not a policy level this build knows.",
            record.scope
        ))
    })?;
    Ok(Document {
        scope: Some(scope),
        scope_id: record.scope_id.clone(),
        enabled_kinds: record.enabled_kinds.clone(),
        head_sample_rate: record.head_sample_rate,
        session_max_lifetime_ms: record.session_max_lifetime_ms,
        max_event_bytes: record.max_event_bytes,
        max_properties: record.max_properties,
        blocked_event_names: record.blocked_event_names.clone(),
        blocked_property_keys: record.blocked_property_keys.clone(),
        redact_property_keys: record.redact_property_keys.clone(),
        campaign_linking: record.campaign_linking.as_deref().and_then(Capture::parse),
        attribution_needs_consent: record.attribution_needs_consent,
        kill_switch: record.kill_switch,
    })
}

fn from_wire_document(
    document: &tallyowl_control_api::types::PolicyDocument,
) -> Result<Document, TallyOwlError> {
    use tallyowl_control_api::types::PolicyScope as Wire;
    Ok(Document {
        scope: Some(match document.scope {
            Wire::Installation => Scope::Installation,
            Wire::Workspace => Scope::Workspace,
            Wire::Project => Scope::Project,
            Wire::Environment => Scope::Environment,
            Wire::Source => Scope::Source,
        }),
        scope_id: document.scope_id.clone().unwrap_or_default(),
        enabled_kinds: document
            .enabled_kinds
            .as_ref()
            .map(|kinds| kinds.iter().map(kind_name).map(str::to_string).collect()),
        head_sample_rate: document.head_sample_rate,
        session_max_lifetime_ms: document.session_max_lifetime_ms,
        max_event_bytes: document.max_event_bytes,
        max_properties: document.max_properties,
        blocked_event_names: document.blocked_event_names.clone().unwrap_or_default(),
        blocked_property_keys: document.blocked_property_keys.clone().unwrap_or_default(),
        redact_property_keys: document.redact_property_keys.clone().unwrap_or_default(),
        campaign_linking: document.campaign_linking.as_ref().map(from_wire_capture),
        attribution_needs_consent: document.attribution_needs_consent,
        kill_switch: document.kill_switch,
    })
}

fn to_wire_compiled(compiled: &Compiled) -> tallyowl_control_api::types::CompiledPolicy {
    tallyowl_control_api::types::CompiledPolicy {
        policy_version: compiled.version,
        enabled_kinds: compiled
            .enabled_kinds
            .iter()
            .filter_map(|name| kind_of(name))
            .collect(),
        head_sample_rate: compiled.head_sample_rate,
        session_max_lifetime_ms: compiled.session_max_lifetime_ms,
        max_event_bytes: compiled.max_event_bytes,
        max_properties: compiled.max_properties,
        blocked_event_names: compiled.blocked_event_names.iter().cloned().collect(),
        blocked_property_keys: compiled.blocked_property_keys.iter().cloned().collect(),
        redact_property_keys: compiled.redact_property_keys.iter().cloned().collect(),
        campaign_linking: to_wire_capture(compiled.campaign_linking),
        attribution_needs_consent: compiled.attribution_needs_consent,
        kill_switch: compiled.kill_switch,
        from_levels: compiled.from_levels.clone(),
    }
}

/// The compiled policy, in the shape a collector applies.
fn to_wire_snapshot(compiled: &Compiled) -> tallyowl_collector_api::types::CollectionPolicy {
    use tallyowl_collector_api::types as wire;
    wire::CollectionPolicy {
        policy_version: compiled.version,
        enabled_kinds: compiled
            .enabled_kinds
            .iter()
            .filter_map(|name| collector_kind_of(name))
            .collect(),
        head_sample_rate: compiled.head_sample_rate,
        // A tail rule and an always-keep expression are the head's, because
        // tail sampling happens after commit. See D35. A collector that carried
        // them could not act on them.
        tail_rules: None,
        tail_decision_window_ms: None,
        late_span_grace_ms: None,
        always_keep_expressions: None,
        // Retention is the store's. A collector holds nothing durable of its
        // own, so a retention class it carried would apply to nothing.
        retention: Vec::new(),
        protected_keys: tallyowl_wire::scrub::PROTECTED_KEYS
            .iter()
            .map(|key| wire::ProtectedKey {
                key: key.to_string(),
                origin: wire::PropertyOrigin::Collector,
            })
            .collect(),
        stamped_properties: Vec::new(),
        max_event_bytes: compiled.max_event_bytes,
        // The batch limit is the collector's own configuration. This carries the
        // event limit so a snapshot never widens a configured batch limit; the
        // collector keeps the smaller of the two.
        max_batch_bytes: compiled.max_event_bytes,
        max_properties: compiled.max_properties,
        session_max_lifetime_ms: compiled.session_max_lifetime_ms,
        redact_keys: (!compiled.redact_property_keys.is_empty())
            .then(|| compiled.redact_property_keys.iter().cloned().collect()),
        blocked_event_names: (!compiled.blocked_event_names.is_empty())
            .then(|| compiled.blocked_event_names.iter().cloned().collect()),
        blocked_property_keys: (!compiled.blocked_property_keys.is_empty())
            .then(|| compiled.blocked_property_keys.iter().cloned().collect()),
        campaign_linking: Some(match compiled.campaign_linking {
            Capture::Linked => wire::CampaignLinking::Linked,
            Capture::Unlinked => wire::CampaignLinking::Unlinked,
            Capture::None => wire::CampaignLinking::None,
        }),
        kill_switch: Some(compiled.kill_switch),
    }
}

fn collector_kind_of(name: &str) -> Option<tallyowl_collector_api::types::TelemetryKind> {
    use tallyowl_collector_api::types::TelemetryKind as K;
    Some(match name {
        "event" => K::Event,
        "page-view" => K::PageView,
        "session-start" => K::SessionStart,
        "session-end" => K::SessionEnd,
        "session-heartbeat" => K::SessionHeartbeat,
        "interaction" => K::Interaction,
        "feature-exposure" => K::FeatureExposure,
        "identify" => K::Identify,
        "alias" => K::Alias,
        "group" => K::Group,
        "conversion" => K::Conversion,
        "error" => K::Error,
        "span" => K::Span,
        "metric-point" => K::MetricPoint,
        "campaign-touch" => K::CampaignTouch,
        "campaign-cost" => K::CampaignCost,
        _ => return None,
    })
}

fn to_wire_capture(capture: Capture) -> tallyowl_control_api::types::CampaignLinking {
    use tallyowl_control_api::types::CampaignLinking as W;
    match capture {
        Capture::Linked => W::Linked,
        Capture::Unlinked => W::Unlinked,
        Capture::None => W::None,
    }
}

fn from_wire_capture(capture: &tallyowl_control_api::types::CampaignLinking) -> Capture {
    use tallyowl_control_api::types::CampaignLinking as W;
    match capture {
        W::Linked => Capture::Linked,
        W::Unlinked => Capture::Unlinked,
        W::None => Capture::None,
    }
}

fn kind_name(kind: &tallyowl_control_api::types::TelemetryKind) -> &'static str {
    use tallyowl_control_api::types::TelemetryKind as K;
    match kind {
        K::Event => "event",
        K::PageView => "page-view",
        K::SessionStart => "session-start",
        K::SessionEnd => "session-end",
        K::SessionHeartbeat => "session-heartbeat",
        K::Interaction => "interaction",
        K::FeatureExposure => "feature-exposure",
        K::Identify => "identify",
        K::Alias => "alias",
        K::Group => "group",
        K::Conversion => "conversion",
        K::Error => "error",
        K::Span => "span",
        K::MetricPoint => "metric-point",
        K::CampaignTouch => "campaign-touch",
        K::CampaignCost => "campaign-cost",
    }
}

fn kind_of(name: &str) -> Option<tallyowl_control_api::types::TelemetryKind> {
    use tallyowl_control_api::types::TelemetryKind as K;
    Some(match name {
        "event" => K::Event,
        "page-view" => K::PageView,
        "session-start" => K::SessionStart,
        "session-end" => K::SessionEnd,
        "session-heartbeat" => K::SessionHeartbeat,
        "interaction" => K::Interaction,
        "feature-exposure" => K::FeatureExposure,
        "identify" => K::Identify,
        "alias" => K::Alias,
        "group" => K::Group,
        "conversion" => K::Conversion,
        "error" => K::Error,
        "span" => K::Span,
        "metric-point" => K::MetricPoint,
        "campaign-touch" => K::CampaignTouch,
        "campaign-cost" => K::CampaignCost,
        _ => return None,
    })
}
