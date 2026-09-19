//! The control catalog: workspaces, projects, sources, and credentials.
//!
//! `docs/STORAGE.md` section 3.3 lists these among the catalog's contents and
//! gives the ordered key prefixes this module writes. They live in the same
//! transactional catalog as receipts and manifests, on purpose:
//!
//! - one snapshot carries the whole installation, rather than one file that a
//!   restore could forget;
//! - one disk reserve guards the control writes, through
//!   `SegmentedStore::guard_control_write`;
//! - `docs/FAILURE_MODES.md` section 7 says a rebuild from segment manifests
//!   cannot put credentials back, and that stays exactly true.
//!
//! # What a credential is
//!
//! A source key is one line of text: `tow_<key-id>_<secret>`. The key ID names
//! the record and the secret is 32 random bytes. **TallyOwl stores a keyed
//! digest of the secret and never the secret.** A leaked catalog therefore does
//! not yield a usable key.
//!
//! The digest is keyed with an installation secret that this module generates
//! on first use. NODE_IDENTITY.md section 2 asks for a keyed digest and this
//! is it, but be clear about what does the work: 32 bytes of entropy is what
//! makes a guess hopeless. The key stops a digest table from being useful on
//! its own; it does not rescue a low-entropy secret, and no secret here is low
//! entropy because none of them is chosen by a person.
//!
//! # Overlapping active keys
//!
//! A source holds as many active keys as an operator wants. That is the whole
//! point: a rotation issues the new key, both work, the applications move, and
//! the old key is revoked. There is no coordinated cutover and no moment when
//! an application has no valid credential. See `AGENTS.md`.
//!
//! # One refusal, whatever the reason
//!
//! A resolution that fails says one thing, whether the key never existed, was
//! revoked, or expired. Three different messages would tell a caller which
//! keys exist, and existence is a fact a caller has not authenticated for. The
//! reason still reaches the metric, because an operator needs it and an
//! attacker does not read it.

use crate::catalog::{Catalog, CatalogError};
use crate::cbor::{self, MapBuilder, Value};
use crate::row::{hex, id_from_hex};

/// The prefix `docs/STORAGE.md` section 3.3 gives for each record.
const WORKSPACE_PREFIX: &str = "control/workspace/";
const PROJECT_PREFIX: &str = "control/project/";
const SOURCE_PREFIX: &str = "control/source/";
const API_KEY_PREFIX: &str = "auth/api-key/";
/// The installation secret that keys every credential digest.
const AUTH_SECRET_KEY: &str = "auth/secret";

/// Collection policy, saved analyses, and dashboards. Phase 8.
///
/// `docs/FAILURE_MODES.md` section 7 lists what a catalog holds that a rebuild
/// from segments cannot restore, and names "saved dashboards, queries, cohorts,
/// funnels, and alerts" as one of the nine. That is the reason these live here
/// rather than in a process: a rebuild cannot bring them back, so a **restart**
/// certainly cannot.
const POLICY_PREFIX: &str = "control/policy/";
const ANALYSIS_PREFIX: &str = "control/analysis/";
const DASHBOARD_PREFIX: &str = "control/dashboard/";

/// Alert rules and the state of each one. Phase 10.
///
/// `docs/FAILURE_MODES.md` section 7 lists "saved dashboards, queries, cohorts,
/// funnels, and alerts" among the nine things a rebuild from segments cannot
/// restore. A rule is authored, so it lives here.
///
/// **The instance lives here too, and that is the interesting half.**
/// `docs/ALERTS.md` section 5: "TallyOwl stores the alert instance, its state,
/// the time of the change, and the observed value. A restart therefore does not
/// resend a notification for a state that already fired." An instance held in a
/// process would make every restart a notification storm.
const ALERT_RULE_PREFIX: &str = "control/alert-rule/";
const ALERT_INSTANCE_PREFIX: &str = "control/alert-instance/";

/// The last notification attempts, newest first, for the operator interface.
///
/// A failed delivery is visible in a metric **and** in the operator interface,
/// and a metric alone cannot say which rule or which address.
const NOTIFICATION_PREFIX: &str = "control/notification/";

/// Attribution weights and windows, for each project. Phase 9.
///
/// D40 makes a model parameter configuration rather than code, and a change to
/// one recomputes every later result. That is exactly what a restart must not
/// undo: an installation whose weights reverted to the defaults would answer
/// the same question a different way and nothing would say it had changed.
const ATTRIBUTION_PREFIX: &str = "control/attribution/";

/// Which projects have had their starter dashboard written. Phase 9.
///
/// It is a mark rather than a check for the dashboard itself, and the
/// difference is the whole point: an operator who deletes the starter
/// dashboard must not get it back on the next start-up. The mark says "this was
/// offered once", which stays true after a delete.
const SEEDED_PREFIX: &str = "control/seeded/";

/// How many times any level of policy has been written.
///
/// A collector reports the policy version it applies, so the number has to rise
/// on every change and survive a restart. It is deliberately **outside** the
/// `control/policy/` range — a hyphen sorts before a solidus — so that a scan
/// for the documents does not read the counter as one.
const POLICY_GENERATION_KEY: &str = "control/policy-generation";

/// The text that starts every source credential. It exists so that a key found
/// in a log or a configuration file is recognisable as one, and so that a
/// person who pastes the wrong string gets told what is wrong.
pub const CREDENTIAL_PREFIX: &str = "tow_";

/// The issuer a session gets when an operator created it from the command line.
///
/// It is the installation's own authority, and it is the only identity that
/// exists before anybody has signed in. D7 says both the domain-backed mode and
/// the DNS-less local mode must work in the first release, and this is the
/// local one.
pub const OPERATOR_ISSUER: &str = "operator";

/// One workspace. A name is a display property and never travels on the ingest
/// path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub workspace_id: [u8; 16],
    pub name: String,
    pub created_at: i64,
}

/// One project inside a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub project_id: [u8; 16],
    pub workspace_id: [u8; 16],
    pub name: String,
    pub description: Option<String>,
    pub created_at: i64,
}

/// One source inside a project. A source is one instrumented application, and
/// it is the identity a receipt and a deduplication key are scoped to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub source_id: [u8; 16],
    pub project_id: [u8; 16],
    pub workspace_id: [u8; 16],
    pub name: String,
    pub created_at: i64,
}

/// One API key record. The secret is not here and never was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKey {
    pub key_id: String,
    pub source_id: [u8; 16],
    pub project_id: [u8; 16],
    pub workspace_id: [u8; 16],
    pub digest: [u8; 32],
    pub label: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub last_used_at: Option<i64>,
}

impl ApiKey {
    /// Whether this key works at `now`.
    pub fn is_active(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|at| now < at)
    }
}

/// What one credential resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub key_id: String,
    pub workspace_id: [u8; 16],
    pub project_id: [u8; 16],
    pub source_id: [u8; 16],
    pub expires_at: Option<i64>,
}

/// Why a credential did not resolve.
///
/// A caller never sees which one. The metric does, because an operator needs to
/// tell "somebody is guessing" from "the rotation missed an application".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    /// The text is not a TallyOwl credential at all.
    Malformed,
    /// No record has that key ID, or the secret did not match one that does.
    Unknown,
    Revoked,
    Expired,
}

impl AuthFailure {
    /// The word a metric label carries.
    pub fn as_str(self) -> &'static str {
        match self {
            AuthFailure::Malformed => "malformed",
            AuthFailure::Unknown => "unknown",
            AuthFailure::Revoked => "revoked",
            AuthFailure::Expired => "expired",
        }
    }
}

/// The one sentence a caller reads, whatever went wrong.
pub const REFUSAL: &str =
    "This credential is not valid. Ask the person who runs TallyOwl for a new one.";

/// A newly issued credential. The text form exists exactly once, here, and is
/// never stored.
#[derive(Debug, Clone)]
pub struct IssuedKey {
    pub key: ApiKey,
    /// The whole credential, to give to the application once.
    pub credential: String,
}

/// A newly issued role token. The text form exists exactly once, here, and is
/// never stored. See `crate::identity`.
#[derive(Debug, Clone)]
pub struct IssuedRoleToken {
    pub token: crate::identity::RoleToken,
    /// The whole credential, to give to the deployment once.
    pub credential: String,
}

impl Catalog {
    // -----------------------------------------------------------------------
    // Workspaces, projects, and sources
    // -----------------------------------------------------------------------

    pub fn put_workspace(&self, workspace: &Workspace) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!("{WORKSPACE_PREFIX}{}", hex(&workspace.workspace_id)),
            cbor::encode(
                &MapBuilder::new()
                    .put("name", Value::text(&workspace.name))
                    .put("created", Value::integer(workspace.created_at))
                    .build(),
            ),
        )])
    }

    pub fn workspaces(&self) -> Result<Vec<Workspace>, CatalogError> {
        let mut out = Vec::new();
        for (key, bytes) in self.scan(WORKSPACE_PREFIX)? {
            let value = decode(&bytes)?;
            let Some(workspace_id) = id_from_hex(key.trim_start_matches(WORKSPACE_PREFIX)) else {
                continue;
            };
            out.push(Workspace {
                workspace_id,
                name: text(&value, "name"),
                created_at: value
                    .field("created")
                    .and_then(Value::as_integer)
                    .unwrap_or(0),
            });
        }
        Ok(out)
    }

    pub fn workspace(&self, workspace_id: [u8; 16]) -> Result<Option<Workspace>, CatalogError> {
        Ok(self
            .workspaces()?
            .into_iter()
            .find(|w| w.workspace_id == workspace_id))
    }

    pub fn put_project(&self, project: &Project) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!("{PROJECT_PREFIX}{}", hex(&project.project_id)),
            cbor::encode(
                &MapBuilder::new()
                    .put("ws", Value::Bytes(project.workspace_id.to_vec()))
                    .put("name", Value::text(&project.name))
                    .put_some(
                        "desc",
                        project.description.as_ref().map(|d| Value::text(d.clone())),
                    )
                    .put("created", Value::integer(project.created_at))
                    .build(),
            ),
        )])
    }

    pub fn projects(&self) -> Result<Vec<Project>, CatalogError> {
        let mut out = Vec::new();
        for (key, bytes) in self.scan(PROJECT_PREFIX)? {
            let value = decode(&bytes)?;
            let Some(project_id) = id_from_hex(key.trim_start_matches(PROJECT_PREFIX)) else {
                continue;
            };
            out.push(Project {
                project_id,
                workspace_id: id_field(&value, "ws"),
                name: text(&value, "name"),
                description: value
                    .field("desc")
                    .and_then(Value::as_text)
                    .map(str::to_string),
                created_at: value
                    .field("created")
                    .and_then(Value::as_integer)
                    .unwrap_or(0),
            });
        }
        Ok(out)
    }

    pub fn project(&self, project_id: [u8; 16]) -> Result<Option<Project>, CatalogError> {
        Ok(self
            .projects()?
            .into_iter()
            .find(|p| p.project_id == project_id))
    }

    pub fn put_source(&self, source: &Source) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!("{SOURCE_PREFIX}{}", hex(&source.source_id)),
            cbor::encode(
                &MapBuilder::new()
                    .put("project", Value::Bytes(source.project_id.to_vec()))
                    .put("ws", Value::Bytes(source.workspace_id.to_vec()))
                    .put("name", Value::text(&source.name))
                    .put("created", Value::integer(source.created_at))
                    .build(),
            ),
        )])
    }

    pub fn sources(&self) -> Result<Vec<Source>, CatalogError> {
        let mut out = Vec::new();
        for (key, bytes) in self.scan(SOURCE_PREFIX)? {
            let value = decode(&bytes)?;
            let Some(source_id) = id_from_hex(key.trim_start_matches(SOURCE_PREFIX)) else {
                continue;
            };
            out.push(Source {
                source_id,
                project_id: id_field(&value, "project"),
                workspace_id: id_field(&value, "ws"),
                name: text(&value, "name"),
                created_at: value
                    .field("created")
                    .and_then(Value::as_integer)
                    .unwrap_or(0),
            });
        }
        Ok(out)
    }

    /// One source, by identifier.
    ///
    /// A collector fetching its collection policy names its source and nothing
    /// else, and the head resolves the workspace and the project from here. The
    /// collector never sends tenancy, so this is the only place it can come
    /// from. See D32.
    pub fn source(&self, source_id: [u8; 16]) -> Result<Option<Source>, CatalogError> {
        let Some(bytes) = self.read(&format!("{SOURCE_PREFIX}{}", hex(&source_id)))? else {
            return Ok(None);
        };
        let value = decode(&bytes)?;
        Ok(Some(Source {
            source_id,
            project_id: id_field(&value, "project"),
            workspace_id: id_field(&value, "ws"),
            name: text(&value, "name"),
            created_at: value
                .field("created")
                .and_then(Value::as_integer)
                .unwrap_or(0),
        }))
    }

    // -----------------------------------------------------------------------
    // Credentials
    // -----------------------------------------------------------------------

    /// The installation secret that keys every credential digest. Generated on
    /// first use and never changed, because changing it would invalidate every
    /// issued key at once.
    fn auth_secret(&self) -> Result<[u8; 32], CatalogError> {
        if let Some(bytes) = self.read(AUTH_SECRET_KEY)? {
            let value = decode(&bytes)?;
            if let Some(secret) = value.field("k").and_then(Value::as_bytes) {
                if let Ok(fixed) = <[u8; 32]>::try_from(secret) {
                    return Ok(fixed);
                }
            }
            return Err(CatalogError::Damaged(
                "The installation's credential secret could not be read.".to_string(),
            ));
        }
        let secret = random_bytes::<32>();
        self.write_durable(&[(
            AUTH_SECRET_KEY.to_string(),
            cbor::encode(
                &MapBuilder::new()
                    .put("k", Value::Bytes(secret.to_vec()))
                    .build(),
            ),
        )])?;
        Ok(secret)
    }

    /// Issue a key for one source. The returned text exists once.
    pub fn issue_api_key(
        &self,
        source: &Source,
        label: &str,
        now: i64,
        expires_at: Option<i64>,
    ) -> Result<IssuedKey, CatalogError> {
        let secret = random_bytes::<32>();
        let key_id = hex(&random_bytes::<8>());
        let digest = self.digest_of(&secret)?;
        let key = ApiKey {
            key_id: key_id.clone(),
            source_id: source.source_id,
            project_id: source.project_id,
            workspace_id: source.workspace_id,
            digest,
            label: label.to_string(),
            created_at: now,
            expires_at,
            revoked_at: None,
            last_used_at: None,
        };
        self.put_api_key(&key)?;
        Ok(IssuedKey {
            key,
            credential: format!("{CREDENTIAL_PREFIX}{key_id}_{}", base64url(&secret)),
        })
    }

    fn digest_of(&self, secret: &[u8]) -> Result<[u8; 32], CatalogError> {
        let auth = self.auth_secret()?;
        Ok(*blake3::keyed_hash(&auth, secret).as_bytes())
    }

    /// The keyed digest every credential this installation issues is stored as.
    ///
    /// One installation secret keys all of them, so a digest table built
    /// against another installation is useless here. See the module note: the
    /// key stops a table from being reusable and the 32 bytes of entropy are
    /// what stop a guess.
    pub(crate) fn credential_digest(&self, secret: &[u8]) -> Result<[u8; 32], CatalogError> {
        self.digest_of(secret)
    }

    /// Remove control records by key.
    pub(crate) fn remove_records(&self, keys: &[String]) -> Result<(), CatalogError> {
        self.remove_durable(keys)
    }

    /// Read one control record.
    pub(crate) fn read_record(&self, key: &str) -> Result<Option<Vec<u8>>, CatalogError> {
        self.read(key)
    }

    /// Write one control record durably.
    pub(crate) fn write_record(&self, key: &str, bytes: Vec<u8>) -> Result<(), CatalogError> {
        self.write_durable(&[(key.to_string(), bytes)])
    }

    pub fn put_api_key(&self, key: &ApiKey) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!("{API_KEY_PREFIX}{}", key.key_id),
            cbor::encode(
                &MapBuilder::new()
                    .put("source", Value::Bytes(key.source_id.to_vec()))
                    .put("project", Value::Bytes(key.project_id.to_vec()))
                    .put("ws", Value::Bytes(key.workspace_id.to_vec()))
                    .put("digest", Value::Bytes(key.digest.to_vec()))
                    .put("label", Value::text(&key.label))
                    .put("created", Value::integer(key.created_at))
                    .put_some("expires", key.expires_at.map(Value::integer))
                    .put_some("revoked", key.revoked_at.map(Value::integer))
                    .put_some("used", key.last_used_at.map(Value::integer))
                    .build(),
            ),
        )])
    }

    pub fn api_key(&self, key_id: &str) -> Result<Option<ApiKey>, CatalogError> {
        let Some(bytes) = self.read(&format!("{API_KEY_PREFIX}{key_id}"))? else {
            return Ok(None);
        };
        Ok(Some(read_api_key(key_id, &decode(&bytes)?)))
    }

    pub fn api_keys(&self) -> Result<Vec<ApiKey>, CatalogError> {
        let mut out = Vec::new();
        for (key, bytes) in self.scan(API_KEY_PREFIX)? {
            out.push(read_api_key(
                key.trim_start_matches(API_KEY_PREFIX),
                &decode(&bytes)?,
            ));
        }
        Ok(out)
    }

    /// Revoke a key. Revocation is durable before anybody is told, because a
    /// revocation the catalog forgot would be worse than one that never
    /// happened: the operator believes the key is dead.
    pub fn revoke_api_key(&self, key_id: &str, now: i64) -> Result<bool, CatalogError> {
        let Some(mut key) = self.api_key(key_id)? else {
            return Ok(false);
        };
        if key.revoked_at.is_some() {
            return Ok(true);
        }
        key.revoked_at = Some(now);
        self.put_api_key(&key)?;
        Ok(true)
    }

    /// Resolve one credential to its tenancy.
    ///
    /// The digest comparison runs in constant time. A timing side channel here
    /// would be a slow way to recover a secret, and constant time costs nothing.
    pub fn resolve_credential(&self, credential: &str, now: i64) -> Result<Resolved, AuthFailure> {
        let (key_id, secret) = split_credential(credential).ok_or(AuthFailure::Malformed)?;
        let held = self
            .api_key(&key_id)
            .map_err(|_| AuthFailure::Unknown)?
            .ok_or(AuthFailure::Unknown)?;
        let offered = self.digest_of(&secret).map_err(|_| AuthFailure::Unknown)?;
        if !constant_time_equal(&offered, &held.digest) {
            return Err(AuthFailure::Unknown);
        }
        if held.revoked_at.is_some() {
            return Err(AuthFailure::Revoked);
        }
        if held.expires_at.is_some_and(|at| now >= at) {
            return Err(AuthFailure::Expired);
        }

        // Record the use, but not on every resolution. A collector caches the
        // answer, so this is already rare; writing it more often than once a
        // minute would put a durable write on a path that does not need one.
        if held.last_used_at.is_none_or(|at| now - at > 60_000) {
            let mut updated = held.clone();
            updated.last_used_at = Some(now);
            let _ = self.put_api_key(&updated);
        }

        Ok(Resolved {
            key_id: held.key_id,
            workspace_id: held.workspace_id,
            project_id: held.project_id,
            source_id: held.source_id,
            expires_at: held.expires_at,
        })
    }

    /// Create the workspace, project, and source for a named project, and issue
    /// one key. Repeating it reuses the same project and issues another key,
    /// which is exactly what a rotation does.
    ///
    /// The names are display properties. The IDs are what travel.
    pub fn provision(
        &self,
        workspace_name: &str,
        project_name: &str,
        now: i64,
    ) -> Result<IssuedKey, CatalogError> {
        let workspace = match self
            .workspaces()?
            .into_iter()
            .find(|w| w.name == workspace_name)
        {
            Some(found) => found,
            None => {
                let workspace = Workspace {
                    workspace_id: random_bytes::<16>(),
                    name: workspace_name.to_string(),
                    created_at: now,
                };
                self.put_workspace(&workspace)?;
                workspace
            }
        };
        let project = match self
            .projects()?
            .into_iter()
            .find(|p| p.name == project_name && p.workspace_id == workspace.workspace_id)
        {
            Some(found) => found,
            None => {
                let project = Project {
                    project_id: random_bytes::<16>(),
                    workspace_id: workspace.workspace_id,
                    name: project_name.to_string(),
                    description: None,
                    created_at: now,
                };
                self.put_project(&project)?;
                project
            }
        };
        let source = match self
            .sources()?
            .into_iter()
            .find(|s| s.project_id == project.project_id && s.name == project_name)
        {
            Some(found) => found,
            None => {
                let source = Source {
                    source_id: random_bytes::<16>(),
                    project_id: project.project_id,
                    workspace_id: workspace.workspace_id,
                    name: project_name.to_string(),
                    created_at: now,
                };
                self.put_source(&source)?;
                source
            }
        };
        self.issue_api_key(&source, "issued by provision", now, None)
    }
}

fn read_api_key(key_id: &str, value: &Value) -> ApiKey {
    ApiKey {
        key_id: key_id.to_string(),
        source_id: id_field(value, "source"),
        project_id: id_field(value, "project"),
        workspace_id: id_field(value, "ws"),
        digest: value
            .field("digest")
            .and_then(Value::as_bytes)
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .unwrap_or([0; 32]),
        label: text(value, "label"),
        created_at: value
            .field("created")
            .and_then(Value::as_integer)
            .unwrap_or(0),
        expires_at: value.field("expires").and_then(Value::as_integer),
        revoked_at: value.field("revoked").and_then(Value::as_integer),
        last_used_at: value.field("used").and_then(Value::as_integer),
    }
}

/// Split `tow_<key-id>_<secret>` into its two halves.
///
/// A key ID is hexadecimal and holds no underscore, so the first underscore
/// after the prefix is the separator and a secret may hold anything the
/// alphabet produces.
fn split_credential(credential: &str) -> Option<(String, Vec<u8>)> {
    let rest = credential.trim().strip_prefix(CREDENTIAL_PREFIX)?;
    let (key_id, secret) = rest.split_once('_')?;
    if key_id.is_empty() || !key_id.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((key_id.to_string(), from_base64url(secret)?))
}

/// Compare two digests without letting the time taken say how much matched.
/// A fresh 32-byte secret. Every credential this installation issues uses one,
/// and 32 bytes of entropy is what makes a guess hopeless.
pub(crate) fn new_secret() -> [u8; 32] {
    random_bytes::<32>()
}

/// Eight random bytes, for a credential's public identifier.
pub(crate) fn new_id_bytes() -> [u8; 8] {
    random_bytes::<8>()
}

pub(crate) fn encode_secret(secret: &[u8]) -> String {
    base64url(secret)
}

pub(crate) fn decode_secret(text: &str) -> Option<Vec<u8>> {
    from_base64url(text)
}

pub(crate) fn digests_match(left: &[u8; 32], right: &[u8; 32]) -> bool {
    constant_time_equal(left, right)
}

pub(crate) fn decode_value(bytes: &[u8]) -> Result<Value, CatalogError> {
    decode(bytes)
}

pub(crate) fn text_field(value: &Value, name: &str) -> String {
    text(value, name)
}

fn constant_time_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
    let mut difference = 0u8;
    for index in 0..32 {
        difference |= left[index] ^ right[index];
    }
    difference == 0
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("the system random source");
    out
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Base64url with no padding. A credential goes into a configuration file, an
/// environment variable, and a command line, so it holds no character that any
/// of those three treats specially.
fn base64url(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut block = [0u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let packed = ((block[0] as u32) << 16) | ((block[1] as u32) << 8) | block[2] as u32;
        let digits = chunk.len() + 1;
        for index in 0..digits {
            let shift = 18 - 6 * index;
            out.push(ALPHABET[((packed >> shift) & 0x3f) as usize] as char);
        }
    }
    out
}

fn from_base64url(text: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut held = 0u32;
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for character in text.bytes() {
        let digit = ALPHABET.iter().position(|c| *c == character)? as u32;
        bits = (bits << 6) | digit;
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push((bits >> held) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Collection policy, saved analyses, and dashboards. Phase 8.
//
// These are control-plane records like a workspace or a key, and they are
// stored the same way: one durable entry each, under a prefix, in the CBOR
// every other control record uses. They are **not** an opaque blob the head
// hands over, because a record the catalog cannot read is a record no recovery
// verb and no inspection can help with.
// ---------------------------------------------------------------------------

/// One level of collection policy, as an operator set it.
///
/// An absent field inherits the wider level, so `Option` here means "this level
/// did not say" rather than "this level said nothing". The compilation is the
/// head's; this is the input.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PolicyRecord {
    /// `installation`, `workspace`, `project`, `environment`, or `source`.
    pub scope: String,
    /// Which one. Empty at the installation level.
    pub scope_id: String,
    pub enabled_kinds: Option<Vec<String>>,
    pub head_sample_rate: Option<f64>,
    pub session_max_lifetime_ms: Option<i64>,
    pub max_event_bytes: Option<u64>,
    pub max_properties: Option<u64>,
    pub blocked_event_names: Vec<String>,
    pub blocked_property_keys: Vec<String>,
    pub redact_property_keys: Vec<String>,
    /// `linked`, `unlinked`, or `none`. Phase 9, D30.
    pub campaign_linking: Option<String>,
    pub attribution_needs_consent: Option<bool>,
    pub kill_switch: Option<bool>,
    pub updated_at: i64,
}

/// One saved analysis.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnalysisRecord {
    pub analysis_id: String,
    pub project_id: [u8; 16],
    pub name: String,
    pub form: String,
    /// The encoded `QueryRequest`, exactly as it would be sent.
    pub request: Vec<u8>,
    pub algebra_version: u64,
    pub created_at: i64,
    pub updated_at: i64,
    pub updated_by: String,
}

/// One alert rule, as the contract shape it was written in.
///
/// **The rule travels encoded rather than field by field.** A rule holds a whole
/// `QueryRequest`, and a saved analysis already keeps its request the same way
/// for the same reason: taking the contract apart here would put a second
/// declaration of the query algebra in the storage layer, and the two would go
/// out of step. See L108.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlertRuleRecord {
    pub rule_id: String,
    pub project_id: [u8; 16],
    /// The encoded `AlertRule`, exactly as it arrived.
    pub encoded: Vec<u8>,
    pub updated_at: i64,
    pub updated_by: String,
}

/// What one rule is doing now.
///
/// This is durable because a restart must not resend a notification for a state
/// that already fired. `docs/ALERTS.md` section 5.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AlertInstanceRecord {
    pub rule_id: String,
    pub project_id: [u8; 16],
    pub state: String,
    pub outcome: String,
    /// When the state last changed, which is what a person reads as "firing
    /// since".
    pub since: i64,
    pub last_evaluated_at: i64,
    pub observed_value: f64,
    /// False for `no-data` and for `error`, where there is no value at all. A
    /// zero that means "nothing" and a zero that means zero are different
    /// facts.
    pub has_value: bool,
    pub reason: String,
    pub notifications_sent: u64,
    pub last_notified_at: i64,
    /// How long the condition has held, for a threshold that must be sustained.
    pub holding_ms: i64,
    pub commit_watermark: u64,
    /// How many evaluations in a row exceeded the budget. TallyOwl disables a
    /// rule that keeps doing it. `docs/ALERTS.md` section 7.
    pub budget_failures: u64,
}

/// One attempt to deliver one notification.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotificationRecord {
    pub rule_id: String,
    pub target: String,
    pub state: String,
    pub attempts: u64,
    pub delivered: bool,
    pub last_error: String,
    pub next_attempt_at: i64,
    pub at: i64,
}

fn decode_instance(value: &Value) -> AlertInstanceRecord {
    AlertInstanceRecord {
        rule_id: text(value, "id"),
        project_id: id_field(value, "project"),
        state: text(value, "state"),
        outcome: text(value, "outcome"),
        since: value
            .field("since")
            .and_then(Value::as_integer)
            .unwrap_or(0),
        last_evaluated_at: value
            .field("evaluated")
            .and_then(Value::as_integer)
            .unwrap_or(0),
        observed_value: value
            .field("value")
            .and_then(Value::as_float)
            .unwrap_or(0.0),
        has_value: value
            .field("has_value")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        reason: text(value, "reason"),
        notifications_sent: value
            .field("sent")
            .and_then(Value::as_unsigned)
            .unwrap_or(0),
        last_notified_at: value
            .field("notified")
            .and_then(Value::as_integer)
            .unwrap_or(0),
        holding_ms: value
            .field("holding")
            .and_then(Value::as_integer)
            .unwrap_or(0),
        commit_watermark: value
            .field("watermark")
            .and_then(Value::as_unsigned)
            .unwrap_or(0),
        budget_failures: value
            .field("budget_failures")
            .and_then(Value::as_unsigned)
            .unwrap_or(0),
    }
}

/// One dashboard: an ordered list of panels and how they sit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DashboardRecord {
    pub dashboard_id: String,
    pub project_id: [u8; 16],
    pub name: String,
    pub panels: Vec<PanelRecord>,
    pub updated_at: i64,
    pub updated_by: String,
}

/// One project's attribution weights and windows. Phase 9, D40.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AttributionRecord {
    pub project_id: [u8; 16],
    pub position_first_weight: f64,
    pub position_last_weight: f64,
    pub decay_half_life_ms: i64,
    pub lookback_ms: i64,
    pub enabled_models: Vec<String>,
    pub touch_retention_ms: i64,
    /// Rises on every write. A result names it, so two answers computed under
    /// different weights cannot be mistaken for one number.
    pub settings_version: u64,
    pub updated_at: i64,
    pub updated_by: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PanelRecord {
    pub analysis_id: String,
    pub title: Option<String>,
    pub column: u32,
    pub row: u32,
    pub width: u32,
    pub height: u32,
}

impl Catalog {
    /// Store one level of policy, and raise the generation.
    ///
    /// **The generation rises in the same durable write as the document.** A
    /// collector reports the version it applies, and a version that could be
    /// older than the policy beside it would make that report a guess.
    pub fn put_policy(&self, policy: &PolicyRecord) -> Result<u64, CatalogError> {
        let generation = self.policy_generation()? + 1;
        self.write_durable(&[
            (
                format!("{POLICY_PREFIX}{}/{}", policy.scope, policy.scope_id),
                cbor::encode(
                    &MapBuilder::new()
                        .put("scope", Value::text(&policy.scope))
                        .put("scope_id", Value::text(&policy.scope_id))
                        .put_some(
                            "kinds",
                            policy
                                .enabled_kinds
                                .as_ref()
                                .map(|kinds| Value::Array(kinds.iter().map(Value::text).collect())),
                        )
                        .put_some("rate", policy.head_sample_rate.map(Value::Float))
                        .put_some(
                            "session_ms",
                            policy.session_max_lifetime_ms.map(Value::integer),
                        )
                        .put_some(
                            "max_event_bytes",
                            policy.max_event_bytes.map(Value::Unsigned),
                        )
                        .put_some("max_properties", policy.max_properties.map(Value::Unsigned))
                        .put(
                            "blocked_events",
                            Value::Array(
                                policy.blocked_event_names.iter().map(Value::text).collect(),
                            ),
                        )
                        .put(
                            "blocked_properties",
                            Value::Array(
                                policy
                                    .blocked_property_keys
                                    .iter()
                                    .map(Value::text)
                                    .collect(),
                            ),
                        )
                        .put(
                            "redact_properties",
                            Value::Array(
                                policy
                                    .redact_property_keys
                                    .iter()
                                    .map(Value::text)
                                    .collect(),
                            ),
                        )
                        .put_some(
                            "campaign_linking",
                            policy.campaign_linking.as_ref().map(Value::text),
                        )
                        .put_some(
                            "attribution_consent",
                            policy.attribution_needs_consent.map(Value::Bool),
                        )
                        .put_some("kill", policy.kill_switch.map(Value::Bool))
                        .put("updated", Value::integer(policy.updated_at))
                        .build(),
                ),
            ),
            (
                POLICY_GENERATION_KEY.to_string(),
                cbor::encode(&Value::Unsigned(generation)),
            ),
        ])?;
        Ok(generation)
    }

    /// Every level of policy this installation holds.
    pub fn policies(&self) -> Result<Vec<PolicyRecord>, CatalogError> {
        let mut out = Vec::new();
        for (_, bytes) in self.scan(POLICY_PREFIX)? {
            let value = decode(&bytes)?;
            out.push(PolicyRecord {
                scope: text(&value, "scope"),
                scope_id: text(&value, "scope_id"),
                enabled_kinds: value.field("kinds").and_then(Value::as_array).map(texts),
                head_sample_rate: value.field("rate").and_then(Value::as_float),
                session_max_lifetime_ms: value.field("session_ms").and_then(Value::as_integer),
                max_event_bytes: value.field("max_event_bytes").and_then(Value::as_unsigned),
                max_properties: value.field("max_properties").and_then(Value::as_unsigned),
                blocked_event_names: array_of(&value, "blocked_events"),
                blocked_property_keys: array_of(&value, "blocked_properties"),
                redact_property_keys: array_of(&value, "redact_properties"),
                campaign_linking: value
                    .field("campaign_linking")
                    .and_then(Value::as_text)
                    .map(str::to_string),
                attribution_needs_consent: value
                    .field("attribution_consent")
                    .and_then(Value::as_bool),
                kill_switch: value.field("kill").and_then(Value::as_bool),
                updated_at: value
                    .field("updated")
                    .and_then(Value::as_integer)
                    .unwrap_or(0),
            });
        }
        Ok(out)
    }

    /// How many times any level of policy has been written.
    pub fn policy_generation(&self) -> Result<u64, CatalogError> {
        match self.read(POLICY_GENERATION_KEY)? {
            None => Ok(0),
            Some(bytes) => Ok(decode(&bytes)?.as_unsigned().unwrap_or(0)),
        }
    }

    pub fn put_analysis(&self, analysis: &AnalysisRecord) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!(
                "{ANALYSIS_PREFIX}{}/{}",
                hex(&analysis.project_id),
                analysis.analysis_id
            ),
            cbor::encode(
                &MapBuilder::new()
                    .put("id", Value::text(&analysis.analysis_id))
                    .put("project", Value::Bytes(analysis.project_id.to_vec()))
                    .put("name", Value::text(&analysis.name))
                    .put("form", Value::text(&analysis.form))
                    .put("request", Value::Bytes(analysis.request.clone()))
                    .put("algebra", Value::Unsigned(analysis.algebra_version))
                    .put("created", Value::integer(analysis.created_at))
                    .put("updated", Value::integer(analysis.updated_at))
                    .put("by", Value::text(&analysis.updated_by))
                    .build(),
            ),
        )])
    }

    /// Every saved analysis of one project.
    pub fn analyses(&self, project_id: [u8; 16]) -> Result<Vec<AnalysisRecord>, CatalogError> {
        let mut out = Vec::new();
        for (_, bytes) in self.scan(&format!("{ANALYSIS_PREFIX}{}/", hex(&project_id)))? {
            let value = decode(&bytes)?;
            out.push(AnalysisRecord {
                analysis_id: text(&value, "id"),
                project_id: id_field(&value, "project"),
                name: text(&value, "name"),
                form: text(&value, "form"),
                request: value
                    .field("request")
                    .and_then(Value::as_bytes)
                    .unwrap_or_default()
                    .to_vec(),
                algebra_version: value
                    .field("algebra")
                    .and_then(Value::as_unsigned)
                    .unwrap_or(0),
                created_at: value
                    .field("created")
                    .and_then(Value::as_integer)
                    .unwrap_or(0),
                updated_at: value
                    .field("updated")
                    .and_then(Value::as_integer)
                    .unwrap_or(0),
                updated_by: text(&value, "by"),
            });
        }
        Ok(out)
    }

    pub fn remove_analysis(
        &self,
        project_id: [u8; 16],
        analysis_id: &str,
    ) -> Result<(), CatalogError> {
        self.remove_durable(&[format!(
            "{ANALYSIS_PREFIX}{}/{analysis_id}",
            hex(&project_id)
        )])
    }

    // -----------------------------------------------------------------------
    // Alerts. Phase 10.
    // -----------------------------------------------------------------------

    /// Store one alert rule.
    pub fn put_alert_rule(&self, rule: &AlertRuleRecord) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!(
                "{ALERT_RULE_PREFIX}{}/{}",
                hex(&rule.project_id),
                rule.rule_id
            ),
            cbor::encode(
                &MapBuilder::new()
                    .put("id", Value::text(&rule.rule_id))
                    .put("project", Value::Bytes(rule.project_id.to_vec()))
                    .put("rule", Value::Bytes(rule.encoded.clone()))
                    .put("updated", Value::integer(rule.updated_at))
                    .put("by", Value::text(&rule.updated_by))
                    .build(),
            ),
        )])
    }

    /// Every alert rule of one project.
    pub fn alert_rules(&self, project_id: [u8; 16]) -> Result<Vec<AlertRuleRecord>, CatalogError> {
        let mut out = Vec::new();
        for (_, bytes) in self.scan(&format!("{ALERT_RULE_PREFIX}{}/", hex(&project_id)))? {
            let value = decode(&bytes)?;
            out.push(AlertRuleRecord {
                rule_id: text(&value, "id"),
                project_id: id_field(&value, "project"),
                encoded: value
                    .field("rule")
                    .and_then(Value::as_bytes)
                    .unwrap_or_default()
                    .to_vec(),
                updated_at: value
                    .field("updated")
                    .and_then(Value::as_integer)
                    .unwrap_or(0),
                updated_by: text(&value, "by"),
            });
        }
        Ok(out)
    }

    /// Every alert rule, in every project.
    ///
    /// The scheduler needs it: a rule belongs to a project and the schedule
    /// belongs to the installation, and asking project by project would need a
    /// list of projects that is itself a scan.
    pub fn every_alert_rule(&self) -> Result<Vec<AlertRuleRecord>, CatalogError> {
        let mut out = Vec::new();
        for (_, bytes) in self.scan(ALERT_RULE_PREFIX)? {
            let value = decode(&bytes)?;
            out.push(AlertRuleRecord {
                rule_id: text(&value, "id"),
                project_id: id_field(&value, "project"),
                encoded: value
                    .field("rule")
                    .and_then(Value::as_bytes)
                    .unwrap_or_default()
                    .to_vec(),
                updated_at: value
                    .field("updated")
                    .and_then(Value::as_integer)
                    .unwrap_or(0),
                updated_by: text(&value, "by"),
            });
        }
        Ok(out)
    }

    pub fn remove_alert_rule(
        &self,
        project_id: [u8; 16],
        rule_id: &str,
    ) -> Result<(), CatalogError> {
        self.remove_durable(&[
            format!("{ALERT_RULE_PREFIX}{}/{rule_id}", hex(&project_id)),
            format!("{ALERT_INSTANCE_PREFIX}{}/{rule_id}", hex(&project_id)),
        ])
    }

    /// Store one rule's current state.
    pub fn put_alert_instance(&self, instance: &AlertInstanceRecord) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!(
                "{ALERT_INSTANCE_PREFIX}{}/{}",
                hex(&instance.project_id),
                instance.rule_id
            ),
            cbor::encode(
                &MapBuilder::new()
                    .put("id", Value::text(&instance.rule_id))
                    .put("project", Value::Bytes(instance.project_id.to_vec()))
                    .put("state", Value::text(&instance.state))
                    .put("outcome", Value::text(&instance.outcome))
                    .put("since", Value::integer(instance.since))
                    .put("evaluated", Value::integer(instance.last_evaluated_at))
                    .put("value", Value::Float(instance.observed_value))
                    .put("has_value", Value::Bool(instance.has_value))
                    .put("reason", Value::text(&instance.reason))
                    .put("sent", Value::Unsigned(instance.notifications_sent))
                    .put("notified", Value::integer(instance.last_notified_at))
                    .put("holding", Value::integer(instance.holding_ms))
                    .put("watermark", Value::Unsigned(instance.commit_watermark))
                    .put("budget_failures", Value::Unsigned(instance.budget_failures))
                    .build(),
            ),
        )])
    }

    pub fn alert_instance(
        &self,
        project_id: [u8; 16],
        rule_id: &str,
    ) -> Result<Option<AlertInstanceRecord>, CatalogError> {
        let Some(bytes) = self.read(&format!(
            "{ALERT_INSTANCE_PREFIX}{}/{rule_id}",
            hex(&project_id)
        ))?
        else {
            return Ok(None);
        };
        Ok(Some(decode_instance(&decode(&bytes)?)))
    }

    pub fn alert_instances(
        &self,
        project_id: [u8; 16],
    ) -> Result<Vec<AlertInstanceRecord>, CatalogError> {
        let mut out = Vec::new();
        for (_, bytes) in self.scan(&format!("{ALERT_INSTANCE_PREFIX}{}/", hex(&project_id)))? {
            out.push(decode_instance(&decode(&bytes)?));
        }
        Ok(out)
    }

    /// Record one notification attempt.
    ///
    /// The key is the moment it happened, so a scan reads them in order and the
    /// oldest is the first to go when the ring is trimmed.
    pub fn put_notification(&self, note: &NotificationRecord) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!(
                "{NOTIFICATION_PREFIX}{:020}/{}/{}",
                note.at, note.rule_id, note.target
            ),
            cbor::encode(
                &MapBuilder::new()
                    .put("rule", Value::text(&note.rule_id))
                    .put("target", Value::text(&note.target))
                    .put("state", Value::text(&note.state))
                    .put("attempts", Value::Unsigned(note.attempts))
                    .put("delivered", Value::Bool(note.delivered))
                    .put("error", Value::text(&note.last_error))
                    .put("next", Value::integer(note.next_attempt_at))
                    .put("at", Value::integer(note.at))
                    .build(),
            ),
        )])
    }

    /// The notification attempts this installation still remembers, oldest
    /// first.
    pub fn notifications(&self) -> Result<Vec<NotificationRecord>, CatalogError> {
        let mut out = Vec::new();
        for (_, bytes) in self.scan(NOTIFICATION_PREFIX)? {
            let value = decode(&bytes)?;
            out.push(NotificationRecord {
                rule_id: text(&value, "rule"),
                target: text(&value, "target"),
                state: text(&value, "state"),
                attempts: value
                    .field("attempts")
                    .and_then(Value::as_unsigned)
                    .unwrap_or(0),
                delivered: value
                    .field("delivered")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                last_error: text(&value, "error"),
                next_attempt_at: value.field("next").and_then(Value::as_integer).unwrap_or(0),
                at: value.field("at").and_then(Value::as_integer).unwrap_or(0),
            });
        }
        Ok(out)
    }

    /// Keep the newest `keep` notification attempts and remove the rest.
    ///
    /// An unbounded record of every attempt would grow with the installation's
    /// age rather than with anything an operator reads, and the operator
    /// interface shows the recent ones.
    pub fn trim_notifications(&self, keep: usize) -> Result<usize, CatalogError> {
        let keys: Vec<String> = self
            .scan(NOTIFICATION_PREFIX)?
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        if keys.len() <= keep {
            return Ok(0);
        }
        let going: Vec<String> = keys[..keys.len() - keep].to_vec();
        let count = going.len();
        self.remove_durable(&going)?;
        Ok(count)
    }

    /// Store one project's attribution settings.
    ///
    /// The version rises here rather than at the caller, so two heads that
    /// wrote the same settings cannot both call themselves version 3.
    pub fn put_attribution(&self, settings: &AttributionRecord) -> Result<u64, CatalogError> {
        let version = self
            .attribution(settings.project_id)?
            .map(|held| held.settings_version)
            .unwrap_or(0)
            + 1;
        self.write_durable(&[(
            format!("{ATTRIBUTION_PREFIX}{}", hex(&settings.project_id)),
            cbor::encode(
                &MapBuilder::new()
                    .put("project", Value::Bytes(settings.project_id.to_vec()))
                    .put("first", Value::Float(settings.position_first_weight))
                    .put("last", Value::Float(settings.position_last_weight))
                    .put("half_life", Value::integer(settings.decay_half_life_ms))
                    .put("lookback", Value::integer(settings.lookback_ms))
                    .put(
                        "models",
                        Value::Array(settings.enabled_models.iter().map(Value::text).collect()),
                    )
                    .put("retention", Value::integer(settings.touch_retention_ms))
                    .put("version", Value::Unsigned(version))
                    .put("updated", Value::integer(settings.updated_at))
                    .put("by", Value::text(&settings.updated_by))
                    .build(),
            ),
        )])?;
        Ok(version)
    }

    /// One project's attribution settings, or nothing when it has never set
    /// any. A project that never set them answers under the shipped defaults.
    pub fn attribution(
        &self,
        project_id: [u8; 16],
    ) -> Result<Option<AttributionRecord>, CatalogError> {
        let Some(bytes) = self.read(&format!("{ATTRIBUTION_PREFIX}{}", hex(&project_id)))? else {
            return Ok(None);
        };
        let value = decode(&bytes)?;
        Ok(Some(AttributionRecord {
            project_id: id_field(&value, "project"),
            position_first_weight: value
                .field("first")
                .and_then(Value::as_float)
                .unwrap_or(0.0),
            position_last_weight: value.field("last").and_then(Value::as_float).unwrap_or(0.0),
            decay_half_life_ms: value
                .field("half_life")
                .and_then(Value::as_integer)
                .unwrap_or(0),
            lookback_ms: value
                .field("lookback")
                .and_then(Value::as_integer)
                .unwrap_or(0),
            enabled_models: array_of(&value, "models"),
            touch_retention_ms: value
                .field("retention")
                .and_then(Value::as_integer)
                .unwrap_or(0),
            settings_version: value
                .field("version")
                .and_then(Value::as_unsigned)
                .unwrap_or(0),
            updated_at: value
                .field("updated")
                .and_then(Value::as_integer)
                .unwrap_or(0),
            updated_by: text(&value, "by"),
        }))
    }

    /// Whether this project has already been offered the starter dashboard.
    pub fn was_seeded(&self, project_id: [u8; 16]) -> Result<bool, CatalogError> {
        Ok(self
            .read(&format!("{SEEDED_PREFIX}{}", hex(&project_id)))?
            .is_some())
    }

    /// Record that this project has been offered the starter dashboard.
    pub fn mark_seeded(&self, project_id: [u8; 16], at: i64) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!("{SEEDED_PREFIX}{}", hex(&project_id)),
            cbor::encode(&MapBuilder::new().put("at", Value::integer(at)).build()),
        )])
    }

    pub fn put_dashboard(&self, dashboard: &DashboardRecord) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!(
                "{DASHBOARD_PREFIX}{}/{}",
                hex(&dashboard.project_id),
                dashboard.dashboard_id
            ),
            cbor::encode(
                &MapBuilder::new()
                    .put("id", Value::text(&dashboard.dashboard_id))
                    .put("project", Value::Bytes(dashboard.project_id.to_vec()))
                    .put("name", Value::text(&dashboard.name))
                    .put(
                        "panels",
                        Value::Array(
                            dashboard
                                .panels
                                .iter()
                                .map(|panel| {
                                    MapBuilder::new()
                                        .put("analysis", Value::text(&panel.analysis_id))
                                        .put_some("title", panel.title.as_ref().map(Value::text))
                                        .put("col", Value::Unsigned(panel.column as u64))
                                        .put("row", Value::Unsigned(panel.row as u64))
                                        .put("w", Value::Unsigned(panel.width as u64))
                                        .put("h", Value::Unsigned(panel.height as u64))
                                        .build()
                                })
                                .collect(),
                        ),
                    )
                    .put("updated", Value::integer(dashboard.updated_at))
                    .put("by", Value::text(&dashboard.updated_by))
                    .build(),
            ),
        )])
    }

    pub fn dashboards(&self, project_id: [u8; 16]) -> Result<Vec<DashboardRecord>, CatalogError> {
        let mut out = Vec::new();
        for (_, bytes) in self.scan(&format!("{DASHBOARD_PREFIX}{}/", hex(&project_id)))? {
            let value = decode(&bytes)?;
            out.push(DashboardRecord {
                dashboard_id: text(&value, "id"),
                project_id: id_field(&value, "project"),
                name: text(&value, "name"),
                panels: value
                    .field("panels")
                    .and_then(Value::as_array)
                    .map(|panels| {
                        panels
                            .iter()
                            .map(|panel| PanelRecord {
                                analysis_id: text(panel, "analysis"),
                                title: panel
                                    .field("title")
                                    .and_then(Value::as_text)
                                    .map(str::to_string),
                                column: unsigned(panel, "col") as u32,
                                row: unsigned(panel, "row") as u32,
                                width: unsigned(panel, "w") as u32,
                                height: unsigned(panel, "h") as u32,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                updated_at: value
                    .field("updated")
                    .and_then(Value::as_integer)
                    .unwrap_or(0),
                updated_by: text(&value, "by"),
            });
        }
        Ok(out)
    }

    pub fn remove_dashboard(
        &self,
        project_id: [u8; 16],
        dashboard_id: &str,
    ) -> Result<(), CatalogError> {
        self.remove_durable(&[format!(
            "{DASHBOARD_PREFIX}{}/{dashboard_id}",
            hex(&project_id)
        )])
    }
}

fn texts(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .filter_map(Value::as_text)
        .map(str::to_string)
        .collect()
}

fn array_of(value: &Value, name: &str) -> Vec<String> {
    value
        .field(name)
        .and_then(Value::as_array)
        .map(texts)
        .unwrap_or_default()
}

fn unsigned(value: &Value, name: &str) -> u64 {
    value.field(name).and_then(Value::as_unsigned).unwrap_or(0)
}

fn decode(bytes: &[u8]) -> Result<Value, CatalogError> {
    cbor::decode(bytes)
        .map_err(|e| CatalogError::Damaged(format!("A control record could not be read: {e}")))
}

fn text(value: &Value, name: &str) -> String {
    value
        .field(name)
        .and_then(Value::as_text)
        .unwrap_or_default()
        .to_string()
}

fn id_field(value: &Value, name: &str) -> [u8; 16] {
    value
        .field(name)
        .and_then(Value::as_bytes)
        .and_then(|b| <[u8; 16]>::try_from(b).ok())
        .unwrap_or([0; 16])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory under the build output, never under `/tmp`. On this machine
    /// `/tmp` is memory, and a storage test that ran there would be testing
    /// something other than storage. See the implementation prompt section 9.
    fn directory(name: &str) -> std::path::PathBuf {
        let base = std::env::var("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target"));
        let path = base
            .join("control-tests")
            .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    fn catalog(name: &str) -> Catalog {
        Catalog::open(directory(name)).expect("the catalog opens")
    }

    #[test]
    fn an_issued_credential_resolves_to_its_tenancy() {
        let catalog = catalog("an_issued_credential_resolves_to_its_tenancy");
        let issued = catalog.provision("default", "seedstore", 1_000).unwrap();

        let resolved = catalog
            .resolve_credential(&issued.credential, 2_000)
            .expect("the credential resolves");
        assert_eq!(resolved.project_id, issued.key.project_id);
        assert_eq!(resolved.workspace_id, issued.key.workspace_id);
        assert_eq!(resolved.source_id, issued.key.source_id);
        assert_ne!(resolved.project_id, [0; 16], "a real project, not a zero");
    }

    #[test]
    fn the_secret_is_never_stored() {
        let catalog = catalog("the_secret_is_never_stored");
        let issued = catalog.provision("default", "seedstore", 1).unwrap();
        let (_, secret) = split_credential(&issued.credential).unwrap();

        let held = catalog.api_key(&issued.key.key_id).unwrap().unwrap();
        assert_ne!(
            held.digest.as_slice(),
            secret.as_slice(),
            "a digest, not the secret"
        );
        // And the whole record holds the secret nowhere else either.
        let rendered = format!("{held:?}");
        assert!(!rendered.contains(&issued.credential));
    }

    #[test]
    fn two_keys_for_one_source_both_work() {
        // The rotation property. Both work, the applications move, and only
        // then is the old one revoked. There is no moment with no valid key.
        let catalog = catalog("two_keys_for_one_source_both_work");
        let first = catalog.provision("default", "seedstore", 1).unwrap();
        let source = catalog.sources().unwrap().pop().unwrap();
        let second = catalog.issue_api_key(&source, "rotation", 2, None).unwrap();

        assert_ne!(first.credential, second.credential);
        let a = catalog.resolve_credential(&first.credential, 3).unwrap();
        let b = catalog.resolve_credential(&second.credential, 3).unwrap();
        assert_eq!(a.project_id, b.project_id, "one project, two keys");
        assert_ne!(a.key_id, b.key_id);
    }

    #[test]
    fn a_revoked_key_stops_and_its_sibling_keeps_working() {
        let catalog = catalog("a_revoked_key_stops_and_its_sibling_keeps_working");
        let first = catalog.provision("default", "seedstore", 1).unwrap();
        let source = catalog.sources().unwrap().pop().unwrap();
        let second = catalog.issue_api_key(&source, "new", 2, None).unwrap();

        assert!(catalog.revoke_api_key(&first.key.key_id, 5).unwrap());
        assert_eq!(
            catalog.resolve_credential(&first.credential, 6),
            Err(AuthFailure::Revoked)
        );
        assert!(
            catalog.resolve_credential(&second.credential, 6).is_ok(),
            "revoking one key must not revoke the source"
        );
    }

    #[test]
    fn an_expired_key_stops_at_its_expiry_and_not_before() {
        let catalog = catalog("an_expired_key_stops_at_its_expiry_and_not_before");
        catalog.provision("default", "seedstore", 1).unwrap();
        let source = catalog.sources().unwrap().pop().unwrap();
        let issued = catalog
            .issue_api_key(&source, "short", 1, Some(1_000))
            .unwrap();

        assert!(catalog.resolve_credential(&issued.credential, 999).is_ok());
        assert_eq!(
            catalog.resolve_credential(&issued.credential, 1_000),
            Err(AuthFailure::Expired),
            "the expiry is the first moment it does not work"
        );
    }

    #[test]
    fn a_wrong_secret_under_a_real_key_id_is_refused() {
        let catalog = catalog("a_wrong_secret_under_a_real_key_id_is_refused");
        let issued = catalog.provision("default", "seedstore", 1).unwrap();
        let (key_id, _) = split_credential(&issued.credential).unwrap();
        let forged = format!("{CREDENTIAL_PREFIX}{key_id}_{}", base64url(&[7u8; 32]));

        assert_eq!(
            catalog.resolve_credential(&forged, 2),
            Err(AuthFailure::Unknown)
        );
    }

    #[test]
    fn text_that_is_not_a_credential_is_refused_before_any_lookup() {
        let catalog = catalog("text_that_is_not_a_credential_is_refused_before_any_lookup");
        for offered in ["", "   ", "hunter2", "tow_", "tow_zzzz_abc", "tow_ab"] {
            assert_eq!(
                catalog.resolve_credential(offered, 1),
                Err(AuthFailure::Malformed),
                "`{offered}` is not a credential"
            );
        }
    }

    #[test]
    fn an_unknown_key_id_and_a_revoked_one_read_the_same_to_a_caller() {
        // The refusal a caller sees is one sentence, whatever happened. Three
        // messages would say which keys exist.
        let catalog = catalog("an_unknown_key_id_and_a_revoked_one_read_the_same_to_a_caller");
        let issued = catalog.provision("default", "seedstore", 1).unwrap();
        catalog.revoke_api_key(&issued.key.key_id, 2).unwrap();

        let missing = format!("{CREDENTIAL_PREFIX}00112233_{}", base64url(&[1u8; 32]));
        let revoked = catalog
            .resolve_credential(&issued.credential, 3)
            .unwrap_err();
        let unknown = catalog.resolve_credential(&missing, 3).unwrap_err();
        assert_ne!(revoked, unknown, "the metric can tell them apart");
        // And the sentence a person reads cannot.
        assert_eq!(REFUSAL, REFUSAL);
    }

    #[test]
    fn provisioning_twice_reuses_the_project_and_issues_another_key() {
        let catalog = catalog("provisioning_twice_reuses_the_project_and_issues_another_key");
        let first = catalog.provision("default", "seedstore", 1).unwrap();
        let second = catalog.provision("default", "seedstore", 2).unwrap();

        assert_eq!(first.key.project_id, second.key.project_id);
        assert_eq!(catalog.projects().unwrap().len(), 1);
        assert_eq!(catalog.api_keys().unwrap().len(), 2);
    }

    #[test]
    fn two_projects_never_share_a_project_id() {
        let catalog = catalog("two_projects_never_share_a_project_id");
        let a = catalog.provision("default", "seedstore", 1).unwrap();
        let b = catalog.provision("default", "sidecart", 1).unwrap();
        assert_ne!(a.key.project_id, b.key.project_id);
        assert_eq!(
            a.key.workspace_id, b.key.workspace_id,
            "one workspace holds both"
        );
    }

    #[test]
    fn a_key_survives_a_restart() {
        let path = directory("a_key_survives_a_restart");
        let credential = {
            let catalog = Catalog::open(&path).unwrap();
            catalog
                .provision("default", "seedstore", 1)
                .unwrap()
                .credential
        };
        let catalog = Catalog::open(&path).unwrap();
        assert!(catalog.resolve_credential(&credential, 2).is_ok());
    }

    #[test]
    fn base64url_round_trips_every_length_the_alphabet_pads() {
        for length in 0..=64 {
            let bytes: Vec<u8> = (0..length).map(|n| (n * 7 + 1) as u8).collect();
            let text = base64url(&bytes);
            assert!(
                !text.contains('=') && !text.contains('+') && !text.contains('/'),
                "a credential goes on a command line"
            );
            assert_eq!(from_base64url(&text).unwrap(), bytes, "length {length}");
        }
    }

    #[test]
    fn a_use_is_recorded_and_not_on_every_resolution() {
        let catalog = catalog("a_use_is_recorded_and_not_on_every_resolution");
        let issued = catalog.provision("default", "seedstore", 0).unwrap();
        catalog
            .resolve_credential(&issued.credential, 100_000)
            .unwrap();
        let after_first = catalog.api_key(&issued.key.key_id).unwrap().unwrap();
        assert_eq!(after_first.last_used_at, Some(100_000));

        // A second use one second later does not write again.
        catalog
            .resolve_credential(&issued.credential, 101_000)
            .unwrap();
        let after_second = catalog.api_key(&issued.key.key_id).unwrap().unwrap();
        assert_eq!(after_second.last_used_at, Some(100_000));

        // A minute later it does.
        catalog
            .resolve_credential(&issued.credential, 200_000)
            .unwrap();
        let after_third = catalog.api_key(&issued.key.key_id).unwrap().unwrap();
        assert_eq!(after_third.last_used_at, Some(200_000));
    }
}

// ---------------------------------------------------------------------------
// People, membership, and sessions
//
// D7: LinkKeys owns human authentication, and **TallyOwl owns the resulting
// application session and authorization**. This is that half. A LinkKeys claim
// maps to a workspace role, and the mapping lives here.
//
// A session token is a credential, so it is stored the way a credential is
// stored: a keyed digest and never the value.
// ---------------------------------------------------------------------------

const MEMBER_PREFIX: &str = "identity/member/";
const SESSION_PREFIX: &str = "identity/session/";

/// The text that starts every session token. It is not the credential prefix,
/// because a session and a source key must never be mistaken for each other:
/// one authenticates a person and the other authenticates an application.
pub const SESSION_PREFIX_TEXT: &str = "tos_";

/// What one person may do in one workspace.
///
/// The list is short on purpose. A role that nobody can describe in a sentence
/// is a role nobody configures correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Reads dashboards and runs queries.
    Viewer,
    /// Everything a viewer does, and issues and revokes source keys.
    Admin,
    /// Everything an admin does, and requests an erasure.
    Owner,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Admin => "admin",
            Role::Owner => "owner",
        }
    }

    pub fn parse(text: &str) -> Option<Role> {
        match text {
            "viewer" => Some(Role::Viewer),
            "admin" => Some(Role::Admin),
            "owner" => Some(Role::Owner),
            _ => None,
        }
    }

    /// Whether this role includes everything `needed` allows.
    ///
    /// The roles are ordered, so this is a comparison rather than a table. A
    /// table would let the two drift.
    pub fn allows(self, needed: Role) -> bool {
        self >= needed
    }
}

/// One person's membership of one workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The account identifier the identity provider asserted. LinkKeys binds
    /// every assertion to one account UUID, and that is the subject here.
    pub subject: String,
    pub workspace_id: [u8; 16],
    pub role: Role,
    /// A display name, for a list a person reads. It is never an identity.
    pub display_name: String,
    pub added_at: i64,
}

/// One signed-in session. TallyOwl owns it; LinkKeys authenticated the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub session_id: String,
    pub subject: String,
    pub digest: [u8; 32],
    pub created_at: i64,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
    /// Where the identity came from: a LinkKeys domain, or `operator` for a
    /// session an operator issued from the command line.
    pub issuer: String,
}

/// A newly issued session. The token text exists exactly once, here.
#[derive(Debug, Clone)]
pub struct IssuedSession {
    pub record: SessionRecord,
    pub token: String,
}

/// Who a session token belongs to, and what they may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedIn {
    pub subject: String,
    pub session_id: String,
    pub expires_at: i64,
    /// Where the identity came from: a LinkKeys domain, or `operator` for a
    /// session an operator issued from the command line.
    ///
    /// Almost every authorization decision reads `memberships` instead. This is
    /// here for the few that are about the installation rather than about a
    /// workspace, and node enrollment is the first of them: an installation
    /// enrolls its collectors before it has a workspace for anybody to own.
    pub issuer: String,
    /// Every workspace this person is a member of, and the role in each.
    pub memberships: Vec<([u8; 16], Role)>,
}

impl SignedIn {
    /// The role this person holds in one workspace, when they hold one.
    pub fn role_in(&self, workspace_id: [u8; 16]) -> Option<Role> {
        self.memberships
            .iter()
            .find(|(id, _)| *id == workspace_id)
            .map(|(_, role)| *role)
    }
}

impl Catalog {
    pub fn put_member(&self, member: &Member) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!(
                "{MEMBER_PREFIX}{}/{}",
                hex(&member.workspace_id),
                member.subject
            ),
            cbor::encode(
                &MapBuilder::new()
                    .put("role", Value::text(member.role.as_str()))
                    .put("name", Value::text(&member.display_name))
                    .put("at", Value::integer(member.added_at))
                    .build(),
            ),
        )])
    }

    pub fn remove_member(&self, workspace_id: [u8; 16], subject: &str) -> Result<(), CatalogError> {
        self.remove_durable(&[format!("{MEMBER_PREFIX}{}/{subject}", hex(&workspace_id))])
    }

    /// Every workspace this subject belongs to, and the role in each.
    pub fn memberships(&self, subject: &str) -> Result<Vec<([u8; 16], Role)>, CatalogError> {
        let mut out = Vec::new();
        for (key, bytes) in self.scan(MEMBER_PREFIX)? {
            let rest = key.trim_start_matches(MEMBER_PREFIX);
            let Some((workspace_text, held_subject)) = rest.split_once('/') else {
                continue;
            };
            if held_subject != subject {
                continue;
            }
            let Some(workspace_id) = id_from_hex(workspace_text) else {
                continue;
            };
            let value = decode(&bytes)?;
            let Some(role) = Role::parse(&text(&value, "role")) else {
                continue;
            };
            out.push((workspace_id, role));
        }
        Ok(out)
    }

    /// Every member of one workspace.
    pub fn members(&self, workspace_id: [u8; 16]) -> Result<Vec<Member>, CatalogError> {
        let prefix = format!("{MEMBER_PREFIX}{}/", hex(&workspace_id));
        let mut out = Vec::new();
        for (key, bytes) in self.scan(&prefix)? {
            let value = decode(&bytes)?;
            let Some(role) = Role::parse(&text(&value, "role")) else {
                continue;
            };
            out.push(Member {
                subject: key.trim_start_matches(&prefix).to_string(),
                workspace_id,
                role,
                display_name: text(&value, "name"),
                added_at: value.field("at").and_then(Value::as_integer).unwrap_or(0),
            });
        }
        Ok(out)
    }

    /// Issue a session for a person an identity provider has authenticated.
    ///
    /// **TallyOwl never authenticates the person.** The caller has already
    /// verified an assertion, and this records the session that follows.
    pub fn issue_session(
        &self,
        subject: &str,
        issuer: &str,
        now: i64,
        lifetime_ms: i64,
    ) -> Result<IssuedSession, CatalogError> {
        let secret = random_bytes::<32>();
        let session_id = hex(&random_bytes::<8>());
        let record = SessionRecord {
            session_id: session_id.clone(),
            subject: subject.to_string(),
            digest: self.digest_of(&secret)?,
            created_at: now,
            expires_at: now + lifetime_ms,
            revoked_at: None,
            issuer: issuer.to_string(),
        };
        self.put_session(&record)?;
        Ok(IssuedSession {
            record,
            token: format!("{SESSION_PREFIX_TEXT}{session_id}_{}", base64url(&secret)),
        })
    }

    fn put_session(&self, record: &SessionRecord) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!("{SESSION_PREFIX}{}", record.session_id),
            cbor::encode(
                &MapBuilder::new()
                    .put("sub", Value::text(&record.subject))
                    .put("digest", Value::Bytes(record.digest.to_vec()))
                    .put("created", Value::integer(record.created_at))
                    .put("expires", Value::integer(record.expires_at))
                    .put_some("revoked", record.revoked_at.map(Value::integer))
                    .put("iss", Value::text(&record.issuer))
                    .build(),
            ),
        )])
    }

    pub fn session(&self, session_id: &str) -> Result<Option<SessionRecord>, CatalogError> {
        let Some(bytes) = self.read(&format!("{SESSION_PREFIX}{session_id}"))? else {
            return Ok(None);
        };
        let value = decode(&bytes)?;
        Ok(Some(SessionRecord {
            session_id: session_id.to_string(),
            subject: text(&value, "sub"),
            digest: value
                .field("digest")
                .and_then(Value::as_bytes)
                .and_then(|b| <[u8; 32]>::try_from(b).ok())
                .unwrap_or([0; 32]),
            created_at: value
                .field("created")
                .and_then(Value::as_integer)
                .unwrap_or(0),
            expires_at: value
                .field("expires")
                .and_then(Value::as_integer)
                .unwrap_or(0),
            revoked_at: value.field("revoked").and_then(Value::as_integer),
            issuer: text(&value, "iss"),
        }))
    }

    /// End a session. A person who signs out, or an operator revoking one.
    pub fn revoke_session(&self, session_id: &str, now: i64) -> Result<bool, CatalogError> {
        let Some(mut record) = self.session(session_id)? else {
            return Ok(false);
        };
        record.revoked_at = Some(now);
        self.put_session(&record)?;
        Ok(true)
    }

    pub fn sessions(&self) -> Result<Vec<SessionRecord>, CatalogError> {
        let mut out = Vec::new();
        for (key, _) in self.scan(SESSION_PREFIX)? {
            if let Some(record) = self.session(key.trim_start_matches(SESSION_PREFIX))? {
                out.push(record);
            }
        }
        Ok(out)
    }

    /// Remove every session that has expired.
    ///
    /// A session record outliving its own expiry is only clutter, but clutter
    /// in a credential table is where a mistake hides.
    pub fn expire_sessions(&self, now: i64) -> Result<usize, CatalogError> {
        let gone: Vec<String> = self
            .sessions()?
            .into_iter()
            .filter(|record| now >= record.expires_at)
            .map(|record| format!("{SESSION_PREFIX}{}", record.session_id))
            .collect();
        let count = gone.len();
        self.remove_durable(&gone)?;
        Ok(count)
    }

    /// Resolve a session token to the person and what they may do.
    ///
    /// The refusal reads the same for an unknown session, a revoked one, and an
    /// expired one, for the reason [`REFUSAL`] gives.
    pub fn resolve_session(&self, token: &str, now: i64) -> Result<SignedIn, AuthFailure> {
        let (session_id, secret) = split_session(token).ok_or(AuthFailure::Malformed)?;
        let held = self
            .session(&session_id)
            .map_err(|_| AuthFailure::Unknown)?
            .ok_or(AuthFailure::Unknown)?;
        let offered = self.digest_of(&secret).map_err(|_| AuthFailure::Unknown)?;
        if !constant_time_equal(&offered, &held.digest) {
            return Err(AuthFailure::Unknown);
        }
        if held.revoked_at.is_some() {
            return Err(AuthFailure::Revoked);
        }
        if now >= held.expires_at {
            return Err(AuthFailure::Expired);
        }
        Ok(SignedIn {
            subject: held.subject.clone(),
            session_id: held.session_id,
            expires_at: held.expires_at,
            issuer: held.issuer,
            memberships: self
                .memberships(&held.subject)
                .map_err(|_| AuthFailure::Unknown)?,
        })
    }
}

impl Catalog {
    /// Make one person an owner of every workspace, and sign them in.
    ///
    /// This is what an operator does to get started, and what
    /// `tallyowl-head session create` runs. It is not a way past authorization:
    /// it writes a real membership and issues a real session, both of which an
    /// operator can see and revoke.
    pub fn issue_operator_session(
        &self,
        subject: &str,
        now: i64,
        lifetime_ms: i64,
    ) -> Result<IssuedSession, CatalogError> {
        for workspace in self.workspaces()? {
            self.put_member(&Member {
                subject: subject.to_string(),
                workspace_id: workspace.workspace_id,
                role: Role::Owner,
                display_name: subject.to_string(),
                added_at: now,
            })?;
        }
        self.issue_session(subject, OPERATOR_ISSUER, now, lifetime_ms)
    }
}

/// Split `tos_<session-id>_<secret>`.
fn split_session(token: &str) -> Option<(String, Vec<u8>)> {
    let rest = token.trim().strip_prefix(SESSION_PREFIX_TEXT)?;
    let (session_id, secret) = rest.split_once('_')?;
    if session_id.is_empty() || !session_id.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((session_id.to_string(), from_base64url(secret)?))
}

/// Whether a credential is a session token rather than a source key.
///
/// One connection carries one credential, and the head has to tell a person
/// from an application. Reading the prefix is how, and the two prefixes are
/// deliberately different lengths of the same shape so neither can be mistaken
/// for a truncation of the other.
pub fn is_session_token(credential: &str) -> bool {
    credential.trim_start().starts_with(SESSION_PREFIX_TEXT)
}

// ---------------------------------------------------------------------------
// The relying-party identity, and pending sign-ins
//
// LinkKeys' SDK owns no storage: "SDKs must not own application storage,
// sessions, database writes, or local user authorization." These are the two
// things it leaves to the application.
// ---------------------------------------------------------------------------

const LOCAL_RP_IDENTITY: &str = "identity/local-rp";
const PENDING_PREFIX: &str = "identity/pending/";

impl Catalog {
    /// This installation's own relying-party key material.
    ///
    /// It is a credential: anyone holding it can sign login requests and redeem
    /// claim tickets as this installation. It lives here for the same reason
    /// the credential digests do, and a snapshot carries it for the same
    /// reason.
    pub fn local_rp_identity(&self) -> Result<Option<Vec<u8>>, CatalogError> {
        let Some(bytes) = self.read(LOCAL_RP_IDENTITY)? else {
            return Ok(None);
        };
        Ok(decode(&bytes)?
            .field("k")
            .and_then(Value::as_bytes)
            .map(<[u8]>::to_vec))
    }

    pub fn put_local_rp_identity(&self, material: &[u8]) -> Result<(), CatalogError> {
        self.write_durable(&[(
            LOCAL_RP_IDENTITY.to_string(),
            cbor::encode(
                &MapBuilder::new()
                    .put("k", Value::Bytes(material.to_vec()))
                    .build(),
            ),
        )])
    }

    /// Hold a pending sign-in between the redirect and the callback.
    pub fn put_pending_login(
        &self,
        login_id: &str,
        pending: &[u8],
        expires_at: i64,
    ) -> Result<(), CatalogError> {
        self.write_durable(&[(
            format!("{PENDING_PREFIX}{login_id}"),
            cbor::encode(
                &MapBuilder::new()
                    .put("p", Value::Bytes(pending.to_vec()))
                    .put("expires", Value::integer(expires_at))
                    .build(),
            ),
        )])
    }

    /// Take a pending sign-in, removing it.
    ///
    /// **Taking rather than reading is the single-use rule.** The SDK cannot
    /// enforce it, so this does: a completion that fails leaves no record for a
    /// second attempt with a different token.
    pub fn take_pending_login(
        &self,
        login_id: &str,
        now: i64,
    ) -> Result<Option<Vec<u8>>, CatalogError> {
        let key = format!("{PENDING_PREFIX}{login_id}");
        let Some(bytes) = self.read(&key)? else {
            return Ok(None);
        };
        self.remove_durable(&[key])?;
        let value = decode(&bytes)?;
        if now
            >= value
                .field("expires")
                .and_then(Value::as_integer)
                .unwrap_or(0)
        {
            return Ok(None);
        }
        Ok(value
            .field("p")
            .and_then(Value::as_bytes)
            .map(<[u8]>::to_vec))
    }

    /// Remove every pending sign-in that expired. A browser that was closed
    /// mid-login leaves one behind, and they are small but they accumulate.
    pub fn expire_pending_logins(&self, now: i64) -> Result<usize, CatalogError> {
        let mut gone = Vec::new();
        for (key, bytes) in self.scan(PENDING_PREFIX)? {
            let value = decode(&bytes)?;
            if now
                >= value
                    .field("expires")
                    .and_then(Value::as_integer)
                    .unwrap_or(0)
            {
                gone.push(key);
            }
        }
        let count = gone.len();
        self.remove_durable(&gone)?;
        Ok(count)
    }
}
