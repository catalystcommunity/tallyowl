//! Source identity and tenancy resolution, from D32.
//!
//! An instrumented application does not know about TallyOwl workspaces. It holds
//! a credential, and that credential has project scope. The collector resolves
//! the project and its workspace once, holds the mapping in memory, and stamps
//! it on every envelope.
//!
//! **Never accept tenancy from a payload.** A client value for `workspace_id`,
//! `project_id`, or `source_id` is discarded, not merged and not preferred. A
//! client that could set its own workspace could read another tenant's data by
//! writing into it.
//!
//! Resolve IDs, never names. A workspace name and a project name are display
//! properties and never travel on the ingest path.
//!
//! # Where the answer comes from
//!
//! The head owns the control catalog, so the head is the only process that can
//! say whose key this is. A collector holds no durable state of its own and
//! stores no key. It asks, holds the answer for the time the answer names, and
//! asks again.
//!
//! # The three timings, and why there are three
//!
//! | Timing | What it bounds |
//! | --- | --- |
//! | The cache period the head returns | How long a revocation takes to bite |
//! | The grace period | How long ingest survives a control-plane outage |
//! | The refusal period | How often a wrong credential reaches the head |
//!
//! The grace period is what `docs/DELIVERY.md` section 9 means by "existing
//! collector auth may use a very short safe cache". Without it, a head restart
//! stops every application at once, which is a worse failure than a revocation
//! that takes another minute. With it, a *new* credential still cannot start
//! during the outage, which is the property that makes the grace safe: an
//! outage extends what already worked and grants nothing new.
//!
//! The refusal period exists because a wrong credential must not become a way
//! to make a collector call the head as fast as a client can send batches.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tallyowl_collector_api::types::ResolveKeyResponse;
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_obs::time::now_ms;

/// The tenancy one credential resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tenancy {
    pub workspace_id: [u8; 16],
    pub project_id: [u8; 16],
    pub source_id: [u8; 16],
}

/// Where a resolution comes from. The head implements it over CSIL-RPC; a test
/// implements it in process.
///
/// This is a stand-in for a network hop to another service, not a mock of
/// TallyOwl's storage interface. `AGENTS.md` forbids the second.
pub trait KeyDirectory: Send + Sync {
    fn resolve(&self, credential: &str) -> Result<ResolveKeyResponse, TallyOwlError>;
}

/// How long an answer is held when the head does not say.
const FALLBACK_TTL_MS: i64 = 30_000;

/// How long a refused credential is remembered, so a wrong key cannot turn into
/// a request amplifier.
const REFUSAL_MEMORY_MS: i64 = 5_000;

#[derive(Debug, Clone)]
struct Held {
    tenancy: Tenancy,
    /// When this answer stops being fresh.
    fresh_until: i64,
    /// When the key itself expires, when it does.
    expires_at: Option<i64>,
}

#[derive(Debug, Clone)]
struct Refused {
    message: String,
    until: i64,
}

/// Resolves a credential to a tenancy and remembers the answer.
pub struct TenancyResolver {
    directory: Arc<dyn KeyDirectory>,
    known: RwLock<HashMap<String, Held>>,
    refused: RwLock<HashMap<String, Refused>>,
    /// How long past freshness an answer may still be used when the head cannot
    /// be reached.
    grace_ms: i64,
    /// How many times a stale answer carried a batch through an outage. An
    /// operator needs this number, because it is the size of the window in
    /// which a revocation had not yet taken effect.
    pub served_stale: AtomicU64,
    pub lookups: AtomicU64,
}

impl TenancyResolver {
    pub fn new(directory: Arc<dyn KeyDirectory>, grace_ms: i64) -> TenancyResolver {
        TenancyResolver {
            directory,
            known: RwLock::new(HashMap::new()),
            refused: RwLock::new(HashMap::new()),
            grace_ms,
            served_stale: AtomicU64::new(0),
            lookups: AtomicU64::new(0),
        }
    }

    /// Forget every held answer. An operator revoking a key does not need this,
    /// because the cache expires on its own; a test that must not sleep does.
    pub fn forget_all(&self) {
        self.known.write().expect("tenancy lock").clear();
        self.refused.write().expect("tenancy lock").clear();
    }

    pub fn resolve(&self, credential: &str) -> Result<Tenancy, TallyOwlError> {
        self.resolve_at(credential, now_ms())
    }

    pub fn resolve_at(&self, credential: &str, now: i64) -> Result<Tenancy, TallyOwlError> {
        if credential.trim().is_empty() {
            return Err(TallyOwlError::new(
                ErrorCode::Unauthenticated,
                "This connection sent no credential. Configure `collector.apiKey` for the application that sends this data.",
            ).retryable(false));
        }

        // A held answer that is still fresh, and whose key has not expired
        // while it was held. The expiry is checked here as well as at the head,
        // because a key that expires inside the cache period must stop at its
        // expiry rather than at the end of the period.
        if let Some(held) = self.known.read().expect("tenancy lock").get(credential) {
            if now < held.fresh_until && held.expires_at.is_none_or(|at| now < at) {
                return Ok(held.tenancy);
            }
        }

        // A refusal we already have. A wrong credential retried in a loop must
        // not become a way to make this collector call the head in a loop.
        if let Some(refused) = self.refused.read().expect("tenancy lock").get(credential) {
            if now < refused.until {
                return Err(TallyOwlError::new(
                    ErrorCode::Unauthenticated,
                    refused.message.clone(),
                )
                .retryable(false));
            }
        }

        self.lookups.fetch_add(1, Ordering::Relaxed);
        match self.directory.resolve(credential) {
            Ok(answer) => {
                let tenancy = to_tenancy(&answer)?;
                // A head that answers with a key that has already expired is a
                // head defect. The collector still refuses it, because the
                // collector is what stamps tenancy and an expired key must not
                // reach a project through a fault somewhere else.
                if answer.expires_at.is_some_and(|at| now >= at) {
                    self.known.write().expect("tenancy lock").remove(credential);
                    return Err(TallyOwlError::new(
                        ErrorCode::Unauthenticated,
                        "This credential is not valid. Ask the person who runs TallyOwl for a new one.",
                    )
                    .retryable(false));
                }
                let ttl = if answer.cache_ttl_ms > 0 {
                    answer.cache_ttl_ms
                } else {
                    FALLBACK_TTL_MS
                };
                self.known.write().expect("tenancy lock").insert(
                    credential.to_string(),
                    Held {
                        tenancy,
                        fresh_until: now + ttl,
                        expires_at: answer.expires_at,
                    },
                );
                self.refused
                    .write()
                    .expect("tenancy lock")
                    .remove(credential);
                Ok(tenancy)
            }
            // The head said no. Drop the held answer at once: a refusal is the
            // one outcome that must not wait for a period to end.
            Err(e) if e.code == ErrorCode::Unauthenticated || !e.retryable => {
                self.known.write().expect("tenancy lock").remove(credential);
                self.refused.write().expect("tenancy lock").insert(
                    credential.to_string(),
                    Refused {
                        message: e.message.clone(),
                        until: now + REFUSAL_MEMORY_MS,
                    },
                );
                Err(e.retryable(false))
            }
            // The head could not be reached. An answer that already worked may
            // carry on for the grace period. Nothing new starts working.
            Err(e) => {
                let held = self
                    .known
                    .read()
                    .expect("tenancy lock")
                    .get(credential)
                    .cloned();
                match held {
                    Some(held)
                        if now < held.fresh_until + self.grace_ms
                            && held.expires_at.is_none_or(|at| now < at) =>
                    {
                        self.served_stale.fetch_add(1, Ordering::Relaxed);
                        Ok(held.tenancy)
                    }
                    _ => Err(TallyOwlError::unavailable(format!(
                        "We cannot check this application's key right now, so this batch was not accepted. Send it again. {}",
                        e.message
                    ))),
                }
            }
        }
    }

    /// How many credentials this collector holds an answer for.
    pub fn resolved_count(&self) -> usize {
        self.known.read().expect("tenancy lock").len()
    }
}

fn to_tenancy(answer: &ResolveKeyResponse) -> Result<Tenancy, TallyOwlError> {
    let read = |bytes: &[u8], what: &str| -> Result<[u8; 16], TallyOwlError> {
        bytes.try_into().map_err(|_| {
            TallyOwlError::internal(format!(
                "The head answered with a {what} that is not 16 bytes."
            ))
        })
    };
    Ok(Tenancy {
        workspace_id: read(&answer.workspace_id, "workspace")?,
        project_id: read(&answer.project_id, "project")?,
        source_id: read(&answer.source_id, "source")?,
    })
}

pub mod testing {
    //! A directory a test drives.

    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Inner {
        keys: HashMap<String, ResolveKeyResponse>,
        unreachable: bool,
    }

    #[derive(Default)]
    pub struct FakeDirectory {
        inner: Mutex<Inner>,
        pub calls: AtomicU64,
    }

    impl FakeDirectory {
        pub fn new() -> Arc<FakeDirectory> {
            Arc::new(FakeDirectory::default())
        }

        /// Give a credential a tenancy. The three IDs are derived from the
        /// credential's position so a test can name one and get three distinct
        /// values.
        pub fn add(&self, credential: &str, expires_at: Option<i64>) {
            let mut inner = self.inner.lock().unwrap();
            let count = inner.keys.len() as u8 + 1;
            inner.keys.insert(
                credential.to_string(),
                ResolveKeyResponse {
                    key_id: format!("key-{count}"),
                    workspace_id: vec![count; 16],
                    project_id: vec![count.wrapping_add(100); 16],
                    source_id: vec![count.wrapping_add(200); 16],
                    cache_ttl_ms: 30_000,
                    expires_at,
                },
            );
        }

        pub fn set_cache_ttl(&self, credential: &str, ttl_ms: i64) {
            if let Some(entry) = self.inner.lock().unwrap().keys.get_mut(credential) {
                entry.cache_ttl_ms = ttl_ms;
            }
        }

        pub fn revoke(&self, credential: &str) {
            self.inner.lock().unwrap().keys.remove(credential);
        }

        pub fn set_unreachable(&self, unreachable: bool) {
            self.inner.lock().unwrap().unreachable = unreachable;
        }

        pub fn call_count(&self) -> u64 {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl KeyDirectory for FakeDirectory {
        fn resolve(&self, credential: &str) -> Result<ResolveKeyResponse, TallyOwlError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let inner = self.inner.lock().unwrap();
            if inner.unreachable {
                return Err(TallyOwlError::unavailable("We could not reach the head."));
            }
            inner.keys.get(credential).cloned().ok_or_else(|| {
                TallyOwlError::new(
                    ErrorCode::Unauthenticated,
                    "This credential is not valid. Ask the person who runs TallyOwl for a new one.",
                )
                .retryable(false)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeDirectory;
    use super::*;

    const GRACE: i64 = 60_000;

    fn resolver(directory: &Arc<FakeDirectory>) -> TenancyResolver {
        TenancyResolver::new(Arc::clone(directory) as Arc<dyn KeyDirectory>, GRACE)
    }

    #[test]
    fn one_credential_resolves_once_and_the_answer_is_held() {
        let directory = FakeDirectory::new();
        directory.add("key-a", None);
        let resolver = resolver(&directory);

        let first = resolver.resolve_at("key-a", 1_000).unwrap();
        let second = resolver.resolve_at("key-a", 2_000).unwrap();
        assert_eq!(first, second);
        assert_eq!(directory.call_count(), 1, "the head is asked once");
    }

    #[test]
    fn the_answer_is_asked_for_again_when_its_period_ends() {
        let directory = FakeDirectory::new();
        directory.add("key-a", None);
        directory.set_cache_ttl("key-a", 1_000);
        let resolver = resolver(&directory);

        resolver.resolve_at("key-a", 0).unwrap();
        resolver.resolve_at("key-a", 999).unwrap();
        assert_eq!(directory.call_count(), 1);
        resolver.resolve_at("key-a", 1_000).unwrap();
        assert_eq!(directory.call_count(), 2, "the period ended");
    }

    #[test]
    fn two_credentials_reach_two_projects() {
        let directory = FakeDirectory::new();
        directory.add("key-a", None);
        directory.add("key-b", None);
        let resolver = resolver(&directory);

        let a = resolver.resolve_at("key-a", 1).unwrap();
        let b = resolver.resolve_at("key-b", 1).unwrap();
        assert_ne!(a.project_id, b.project_id);
        assert_ne!(a.workspace_id, b.workspace_id);
    }

    #[test]
    fn the_three_identifiers_of_one_credential_are_distinct() {
        let directory = FakeDirectory::new();
        directory.add("key-a", None);
        let tenancy = resolver(&directory).resolve_at("key-a", 1).unwrap();
        assert_ne!(tenancy.workspace_id, tenancy.project_id);
        assert_ne!(tenancy.project_id, tenancy.source_id);
    }

    #[test]
    fn a_connection_with_no_credential_is_refused_without_asking_the_head() {
        let directory = FakeDirectory::new();
        let resolver = resolver(&directory);
        for empty in ["", "   "] {
            let failure = resolver.resolve_at(empty, 1).unwrap_err();
            assert_eq!(failure.code, ErrorCode::Unauthenticated);
            assert!(failure.message.contains("collector.apiKey"));
        }
        assert_eq!(directory.call_count(), 0);
    }

    #[test]
    fn a_revoked_credential_fails_at_the_end_of_its_period_and_the_message_holds_no_key() {
        let directory = FakeDirectory::new();
        directory.add("key-a", None);
        directory.set_cache_ttl("key-a", 1_000);
        let resolver = resolver(&directory);

        assert!(resolver.resolve_at("key-a", 0).is_ok());
        directory.revoke("key-a");
        // Inside the period the held answer still works. That is the whole cost
        // of caching and it is why the period is short.
        assert!(resolver.resolve_at("key-a", 500).is_ok());

        let failure = resolver.resolve_at("key-a", 1_000).unwrap_err();
        assert_eq!(failure.code, ErrorCode::Unauthenticated);
        assert!(!failure.retryable, "no retry fixes a revoked key");
        assert!(
            !failure.message.contains("key-a"),
            "no credential in a message"
        );
    }

    #[test]
    fn a_refused_credential_is_refused_from_memory_rather_than_from_the_head() {
        // A wrong key retried in a loop must not turn into a request amplifier
        // against the head.
        let directory = FakeDirectory::new();
        let resolver = resolver(&directory);

        for at in [0, 1, 2, 3, 4_999] {
            assert!(resolver.resolve_at("wrong", at).is_err());
        }
        assert_eq!(directory.call_count(), 1);
        assert!(resolver.resolve_at("wrong", 5_000).is_err());
        assert_eq!(directory.call_count(), 2, "the memory expires");
    }

    #[test]
    fn a_head_outage_extends_what_already_worked_and_starts_nothing_new() {
        let directory = FakeDirectory::new();
        directory.add("known", None);
        directory.add("also-known", None);
        directory.set_cache_ttl("known", 1_000);
        let resolver = resolver(&directory);

        resolver.resolve_at("known", 0).unwrap();
        directory.set_unreachable(true);

        // Past its period, inside the grace period: it carries on.
        assert!(resolver.resolve_at("known", 5_000).is_ok());
        assert_eq!(resolver.served_stale.load(Ordering::Relaxed), 1);

        // A credential this collector never resolved gets nothing, however
        // valid it is. An outage must not become a way in.
        let failure = resolver.resolve_at("also-known", 5_000).unwrap_err();
        assert_eq!(failure.code, ErrorCode::Unavailable);
        assert!(failure.retryable, "the head can come back");

        // Past the grace period, the known credential stops too.
        assert!(resolver.resolve_at("known", 1_000 + GRACE).is_err());
    }

    #[test]
    fn a_key_that_expires_inside_its_cache_period_stops_at_its_expiry() {
        // The head said the answer was good for 30 seconds and also said the
        // key expires in 5. The shorter one wins, or a key would outlive its
        // own expiry by the length of the cache period.
        //
        // The directory here keeps answering after the expiry, which is what a
        // head defect would look like. The collector refuses anyway, because
        // the collector is what stamps tenancy.
        let directory = FakeDirectory::new();
        directory.add("key-a", Some(5_000));
        let resolver = resolver(&directory);

        assert!(resolver.resolve_at("key-a", 0).is_ok());
        assert!(resolver.resolve_at("key-a", 4_999).is_ok());
        let failure = resolver.resolve_at("key-a", 5_000).unwrap_err();
        assert_eq!(failure.code, ErrorCode::Unauthenticated);
    }

    #[test]
    fn an_expired_key_is_not_carried_through_an_outage() {
        let directory = FakeDirectory::new();
        directory.add("key-a", Some(5_000));
        let resolver = resolver(&directory);
        resolver.resolve_at("key-a", 0).unwrap();
        directory.set_unreachable(true);

        // The grace period extends an answer, never an expiry.
        assert!(resolver.resolve_at("key-a", 6_000).is_err());
        assert_eq!(resolver.served_stale.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn forgetting_makes_the_next_batch_ask_again() {
        let directory = FakeDirectory::new();
        directory.add("key-a", None);
        let resolver = resolver(&directory);
        resolver.resolve_at("key-a", 0).unwrap();
        resolver.forget_all();
        resolver.resolve_at("key-a", 1).unwrap();
        assert_eq!(directory.call_count(), 2);
        assert_eq!(resolver.resolved_count(), 1);
    }

    #[test]
    fn a_head_that_answers_with_a_short_identifier_is_a_fault_rather_than_a_tenancy() {
        struct ShortAnswer;
        impl KeyDirectory for ShortAnswer {
            fn resolve(&self, _: &str) -> Result<ResolveKeyResponse, TallyOwlError> {
                Ok(ResolveKeyResponse {
                    key_id: "k".into(),
                    workspace_id: vec![1; 16],
                    project_id: vec![2; 4],
                    source_id: vec![3; 16],
                    cache_ttl_ms: 1_000,
                    expires_at: None,
                })
            }
        }
        let resolver = TenancyResolver::new(Arc::new(ShortAnswer), GRACE);
        let failure = resolver.resolve_at("key-a", 1).unwrap_err();
        assert_eq!(failure.code, ErrorCode::Internal);
    }
}
