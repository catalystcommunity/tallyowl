//! The LinkKeys binding.
//!
//! **LinkKeys owns human authentication. TallyOwl owns the resulting
//! application session and its authorization.** `AGENTS.md` says the first and
//! D7 says the second, and the line between them is this module: everything
//! above it is verified protocol fact from the SDK, and everything below it is
//! TallyOwl's own session and membership records.
//!
//! The SDK is explicit about what it will not do: "SDKs must not own
//! application storage, sessions, database writes, or local user
//! authorization." So this module owns three things the SDK deliberately
//! leaves out.
//!
//! # The identity, generated once
//!
//! A local relying party's identity is the fingerprint of a signing key it
//! generates itself, SSH-host-key style, rather than a domain. It is generated
//! on first use and kept in the control catalog, which is where TallyOwl's
//! other credentials already live and what a snapshot already carries.
//!
//! There is no key continuity in this protocol version: a new identity means a
//! new fingerprint and re-approval at every LinkKeys domain. So this generates
//! one and never rotates it on its own.
//!
//! # The pending login, held between two calls and used once
//!
//! The SDK "cannot enforce single-use itself — replay protection at the app
//! boundary is the app's responsibility." The pending record is therefore
//! removed before the completion is attempted, not after it succeeds. A
//! completion that fails does not leave a record somebody can try again with a
//! different token.
//!
//! # The trusted domains
//!
//! D7: "An installation administrator selects the trusted domains. That
//! administrator also maps each claim to a role. An unsigned claim never maps
//! to a role. A claim from a domain that the installation does not trust never
//! maps to a role."
//!
//! `linkkeys.trustedDomains` is that list, and it is empty by default.
//!
//! **A first sign-in grants no membership.** A person who signs in and belongs
//! to nothing gets a session and sees nothing until an administrator grants
//! them a role. Signing in is not authorization, and a default role would make
//! trusting a domain mean trusting everybody at it.

use std::sync::Arc;

use chrono::Utc;
use linkkeys_local_rp::{
    begin_local_login, complete_local_login, generate_local_rp_identity,
    local_rp_identity_from_bytes, local_rp_identity_to_bytes, BeginLocalLoginConfig,
    CompleteLocalLoginConfig, GenerateLocalRpIdentityConfig, LocalRpKeyMaterial, PendingLogin,
};
use tallyowl_control_api::types::{
    BeginLoginRequest, BeginLoginResponse, CompleteLoginRequest, CompleteLoginResponse, Membership,
    Membership_role as WireRole,
};
use tallyowl_obs::error::TallyOwlError;
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_obs::time::now_ms;
use tallyowl_store::control::Role;
use tallyowl_store::SegmentedStore;

/// What an installation configured for LinkKeys.
#[derive(Debug, Clone)]
pub struct LinkKeysSettings {
    pub enabled: bool,
    /// The domains whose assertions this installation accepts. Empty means
    /// none, which is the default.
    pub trusted_domains: Vec<String>,
    /// Where the browser comes back to. A request that names anything else is
    /// refused, so a caller cannot redirect a login somewhere it chose.
    pub callback_url: String,
    /// The name a person sees at the LinkKeys domain when they approve this
    /// installation.
    pub app_name: String,
    pub session_lifetime_ms: i64,
}

/// The sign-in half of the control service.
pub struct SignIn {
    pub store: Arc<SegmentedStore>,
    pub metrics: Arc<Registry>,
    pub settings: LinkKeysSettings,
}

/// One message for every sign-in failure a caller sees.
///
/// A verification failure, an unknown pending login, and a login that was
/// already completed are one sentence to a caller. Telling them apart would
/// help somebody probing the endpoint more than it helps somebody signing in,
/// and the reason still reaches the metric.
const REFUSAL: &str =
    "That sign-in did not complete. Start again, and if it keeps happening ask the person who runs \
     TallyOwl.";

impl SignIn {
    pub fn declare_metrics(metrics: &Registry) {
        metrics
            .declare(
                "tallyowl_sign_ins_total",
                tallyowl_obs::MetricKind::Counter,
                "Sign-in attempts, by outcome.",
                &[],
            )
            .unwrap_or_else(|e| {
                panic!(
                    "the metric `tallyowl_sign_ins_total` is not a name the registry accepts: {}",
                    e.0
                )
            });
    }

    /// Start a sign-in and return the URL to send the browser to.
    pub fn begin(&self, request: BeginLoginRequest) -> Result<BeginLoginResponse, TallyOwlError> {
        self.check_enabled()?;
        self.check_domain(&request.user_domain)?;

        // The callback has to be the one this installation is configured with.
        // Without this check a caller could begin a login that comes back to an
        // address they chose, and the token would arrive there.
        if request.callback_url != self.settings.callback_url {
            self.count("wrong-callback");
            return Err(TallyOwlError::invalid_argument(
                "That is not this installation's sign-in address. Use the one in \
                 `linkkeys.callbackUrl`.",
            ));
        }

        let identity = self.identity()?;
        let now = Utc::now();
        let (redirect, pending) = begin_local_login(BeginLocalLoginConfig::new(
            &identity,
            request.callback_url.clone(),
            request.user_domain.clone(),
            now,
        ))
        .map_err(|e| {
            self.count("begin-failed");
            // A domain that cannot be reached is a moment, so this is
            // retryable; the message names the domain because the person chose
            // it and may have mistyped it.
            TallyOwlError::unavailable(format!(
                "We could not start a sign-in with {}. {e}",
                request.user_domain
            ))
        })?;

        let login_id = tallyowl_store::row::hex(&random_bytes::<16>());
        let expires_at = now_ms() + self.settings.session_lifetime_ms.min(600_000);
        let encoded = serde_json::to_vec(&pending).map_err(|e| {
            TallyOwlError::internal(format!("The pending sign-in could not be recorded: {e}"))
        })?;
        self.store
            .catalog()
            .put_pending_login(&login_id, &encoded, expires_at)
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;

        self.count("begun");
        Ok(BeginLoginResponse {
            redirect_url: redirect.redirect_url,
            login_id,
            expires_at,
        })
    }

    /// Finish a sign-in with what the callback carried.
    pub fn complete(
        &self,
        request: CompleteLoginRequest,
    ) -> Result<CompleteLoginResponse, TallyOwlError> {
        self.check_enabled()?;

        // Taken, not read. The SDK cannot enforce single use, so this does: a
        // completion that fails leaves no record for a second attempt with a
        // different token.
        let held = self
            .store
            .catalog()
            .take_pending_login(&request.login_id, now_ms())
            .map_err(|e| TallyOwlError::internal(e.to_string()))?
            .ok_or_else(|| self.refused("unknown-login"))?;

        let pending: PendingLogin =
            serde_json::from_slice(&held).map_err(|_| self.refused("unreadable-login"))?;
        let identity = self.identity()?;

        let verified = complete_local_login(CompleteLocalLoginConfig::new(
            &identity,
            &pending,
            &request.encrypted_token,
            &request.arrived_url,
            Utc::now(),
        ))
        .map_err(|_| self.refused("verification-failed"))?;

        // The domain is checked again after verification as well as before the
        // redirect. The first check is about where a login was sent; this one
        // is about who came back, and an assertion could name another domain.
        self.check_domain(&verified.user_domain)?;

        // LinkKeys binds every assertion to one account UUID, and that is the
        // subject. The domain travels with it because two domains can issue the
        // same UUID and they are different people.
        let subject = format!("{}@{}", verified.user_id, verified.user_domain);
        let issued = self
            .store
            .catalog()
            .issue_session(
                &subject,
                &verified.user_domain,
                now_ms(),
                self.settings.session_lifetime_ms,
            )
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;

        // Signing in is not authorization. A person who belongs to nothing gets
        // a session and sees nothing until an administrator grants a role, and
        // a default role would make trusting a domain mean trusting everybody
        // at it.
        let memberships = self
            .store
            .catalog()
            .memberships(&subject)
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;

        self.count("completed");
        Ok(CompleteLoginResponse {
            session_token: issued.token,
            subject,
            expires_at: issued.record.expires_at,
            memberships: memberships
                .into_iter()
                .map(|(workspace_id, role)| Membership {
                    workspace_id: workspace_id.to_vec(),
                    role: to_wire_role(role),
                })
                .collect(),
        })
    }

    /// This installation's own relying-party identity, generated on first use.
    fn identity(&self) -> Result<LocalRpKeyMaterial, TallyOwlError> {
        if let Some(bytes) = self
            .store
            .catalog()
            .local_rp_identity()
            .map_err(|e| TallyOwlError::internal(e.to_string()))?
        {
            return local_rp_identity_from_bytes(&bytes).map_err(|e| {
                TallyOwlError::internal(format!(
                    "This installation's sign-in identity could not be read. {e}"
                ))
            });
        }

        let identity = generate_local_rp_identity(GenerateLocalRpIdentityConfig::new(
            self.settings.app_name.clone(),
            Utc::now(),
        ))
        .map_err(|e| {
            TallyOwlError::internal(format!(
                "This installation's sign-in identity could not be made. {e}"
            ))
        })?;
        self.store
            .catalog()
            .put_local_rp_identity(&local_rp_identity_to_bytes(&identity))
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;
        Ok(identity)
    }

    fn check_enabled(&self) -> Result<(), TallyOwlError> {
        if self.settings.enabled {
            return Ok(());
        }
        Err(TallyOwlError::new(
            tallyowl_obs::ErrorCode::FailedPrecondition,
            "This installation does not have LinkKeys sign-in turned on. Turn it on with \
             `linkkeys.enabled` and name the domains it trusts, or ask an operator for a session \
             with `tallyowl-head session create`.",
        )
        .retryable(false))
    }

    fn check_domain(&self, domain: &str) -> Result<(), TallyOwlError> {
        if self
            .settings
            .trusted_domains
            .iter()
            .any(|trusted| trusted == domain)
        {
            return Ok(());
        }
        self.count("untrusted-domain");
        // The message names the domain, because the person chose it and needs
        // to know which one was refused. It does not list the trusted ones: an
        // installation's trust list is not a caller's business.
        Err(TallyOwlError::new(
            tallyowl_obs::ErrorCode::PermissionDenied,
            format!(
                "This installation does not accept sign-ins from {domain}. Ask the person who \
                 runs TallyOwl to add it to `linkkeys.trustedDomains`."
            ),
        )
        .retryable(false))
    }

    fn refused(&self, reason: &str) -> TallyOwlError {
        self.count(reason);
        TallyOwlError::new(tallyowl_obs::ErrorCode::Unauthenticated, REFUSAL).retryable(false)
    }

    fn count(&self, outcome: &str) {
        self.metrics
            .increment("tallyowl_sign_ins_total", &labels(&[("outcome", outcome)]));
    }
}

fn to_wire_role(role: Role) -> WireRole {
    match role {
        Role::Viewer => WireRole::Viewer,
        Role::Admin => WireRole::Admin,
        Role::Owner => WireRole::Owner,
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("the system random source");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(name: &str) -> std::path::PathBuf {
        let base = std::env::var("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target"));
        let path = base
            .join("linkkeys-tests")
            .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    fn sign_in(name: &str, enabled: bool, trusted: &[&str]) -> SignIn {
        let metrics = Registry::new();
        SignIn::declare_metrics(&metrics);
        SignIn {
            store: Arc::new(SegmentedStore::open(directory(name)).expect("open")),
            metrics,
            settings: LinkKeysSettings {
                enabled,
                trusted_domains: trusted.iter().map(|d| d.to_string()).collect(),
                callback_url: "https://tallyowl.example/sign-in/callback".into(),
                app_name: "TallyOwl".into(),
                session_lifetime_ms: 3_600_000,
            },
        }
    }

    fn begin_request(domain: &str) -> BeginLoginRequest {
        BeginLoginRequest {
            user_domain: domain.into(),
            callback_url: "https://tallyowl.example/sign-in/callback".into(),
        }
    }

    #[test]
    fn sign_in_is_off_until_an_operator_turns_it_on() {
        let service = sign_in("off", false, &["example.com"]);
        let failure = service.begin(begin_request("example.com")).unwrap_err();
        assert_eq!(failure.code, tallyowl_obs::ErrorCode::FailedPrecondition);
        assert!(failure.message.contains("linkkeys.enabled"));
        // And it names the way in that does work today.
        assert!(failure.message.contains("session create"));
    }

    #[test]
    fn a_domain_this_installation_does_not_trust_is_refused_before_anything_is_reached() {
        // D7: a claim from a domain the installation does not trust never maps
        // to a role. This refuses before a network call, so an untrusted domain
        // cannot even be used to make this installation dial it.
        let service = sign_in("untrusted", true, &["example.com"]);
        let failure = service
            .begin(begin_request("attacker.example"))
            .unwrap_err();
        assert_eq!(failure.code, tallyowl_obs::ErrorCode::PermissionDenied);
        assert!(failure.message.contains("attacker.example"));
        // The trusted list is not a caller's business.
        assert!(!failure.message.contains("example.com,"));
    }

    #[test]
    fn a_callback_this_installation_did_not_configure_is_refused() {
        // Without this a caller could begin a login that comes back to an
        // address they chose, and the token would arrive there.
        let service = sign_in("callback", true, &["example.com"]);
        let mut request = begin_request("example.com");
        request.callback_url = "https://attacker.example/steal".into();
        let failure = service.begin(request).unwrap_err();
        assert_eq!(failure.code, tallyowl_obs::ErrorCode::InvalidArgument);
        assert!(failure.message.contains("linkkeys.callbackUrl"));
    }

    #[test]
    fn the_installation_identity_is_generated_once_and_held() {
        let service = sign_in("identity", true, &["example.com"]);
        let first = service.identity().expect("an identity");
        let second = service.identity().expect("the same identity");
        // There is no key continuity in this protocol version: a new identity
        // means re-approval at every domain, so it must not change on its own.
        assert_eq!(
            local_rp_identity_to_bytes(&first),
            local_rp_identity_to_bytes(&second)
        );
    }

    #[test]
    fn an_identity_survives_a_restart() {
        let path = directory("identity-restart");
        let settings = LinkKeysSettings {
            enabled: true,
            trusted_domains: vec!["example.com".into()],
            callback_url: "https://tallyowl.example/sign-in/callback".into(),
            app_name: "TallyOwl".into(),
            session_lifetime_ms: 3_600_000,
        };
        let first = {
            let service = SignIn {
                store: Arc::new(SegmentedStore::open(&path).unwrap()),
                metrics: Registry::new(),
                settings: settings.clone(),
            };
            local_rp_identity_to_bytes(&service.identity().unwrap())
        };
        let service = SignIn {
            store: Arc::new(SegmentedStore::open(&path).unwrap()),
            metrics: Registry::new(),
            settings,
        };
        assert_eq!(
            local_rp_identity_to_bytes(&service.identity().unwrap()),
            first
        );
    }

    #[test]
    fn a_completion_for_a_login_nobody_began_is_refused_and_says_nothing_useful() {
        let service = sign_in("unknown-login", true, &["example.com"]);
        let failure = service
            .complete(CompleteLoginRequest {
                login_id: "0011223344556677".into(),
                encrypted_token: "anything".into(),
                arrived_url: "https://tallyowl.example/sign-in/callback".into(),
            })
            .unwrap_err();
        assert_eq!(failure.code, tallyowl_obs::ErrorCode::Unauthenticated);
        assert_eq!(failure.message, REFUSAL);
    }

    #[test]
    fn a_pending_login_is_used_once() {
        // The SDK owns no storage and cannot enforce single use itself, so this
        // does: the record is taken before the completion is attempted, so a
        // completion that fails leaves nothing to try again with.
        let service = sign_in("single-use", true, &["example.com"]);
        service
            .store
            .catalog()
            .put_pending_login("abcd", b"not a pending login", now_ms() + 60_000)
            .unwrap();

        let request = || CompleteLoginRequest {
            login_id: "abcd".into(),
            encrypted_token: "t".into(),
            arrived_url: "https://tallyowl.example/sign-in/callback".into(),
        };
        assert!(service.complete(request()).is_err());
        assert!(service.complete(request()).is_err());
        assert_eq!(
            service
                .store
                .catalog()
                .take_pending_login("abcd", now_ms())
                .unwrap(),
            None,
            "the first attempt took it"
        );
    }

    #[test]
    fn a_pending_login_that_expired_is_gone() {
        let service = sign_in("expired-login", true, &["example.com"]);
        service
            .store
            .catalog()
            .put_pending_login("abcd", b"x", 1_000)
            .unwrap();
        assert_eq!(
            service
                .store
                .catalog()
                .take_pending_login("abcd", 1_000)
                .unwrap(),
            None
        );
    }
}
