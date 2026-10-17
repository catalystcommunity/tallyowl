//! The LinkKeys binding.
//!
//! **LinkKeys owns human authentication. TallyOwl owns the resulting
//! application session and its authorization.** `AGENTS.md` says the first and
//! D7 says the second, and the line between them is this module: everything
//! above it is verified protocol fact from the SDK, and everything below it is
//! TallyOwl's own session and membership records.
//!
//! # Two modes
//!
//! D7 makes the domain-backed relying party the default and keeps the local
//! one as the fallback, and both work.
//!
//! - `domain`: the installation belongs to a home domain whose LinkKeys server
//!   has its RP function on. That server is the relying party in the protocol.
//!   It signs the request and decrypts the token, and the head calls it with an
//!   API key over pinned TLS (`linkkeys_rp.rs`). A person's own domain sees the
//!   home domain, which it can check in DNS like any other, so a sign-in needs
//!   no approval there.
//! - `local`: the installation has no domain, and its identity is a key it
//!   generates itself. Each LinkKeys domain a person signs in from must approve
//!   that key first.
//!
//! The SDK is explicit about what it will not do: "SDKs must not own
//! application storage, sessions, database writes, or local user
//! authorization." So this module owns three things the SDK deliberately
//! leaves out.
//!
//! # The local identity, generated once
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
//! boundary is the app's responsibility", and the RP server does not track
//! which nonces were already redeemed either. The pending record is therefore
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

use chrono::{DateTime, Duration, Utc};
use liblinkkeys::generated::types::{
    IdentityAssertion, RpDecryptRequest, RpSignRequest, RpVerifyRequest,
};
use liblinkkeys::generated::{
    decode_rp_decrypt_response, decode_rp_sign_response, decode_rp_verify_response,
    encode_rp_decrypt_request, encode_rp_sign_request, encode_rp_verify_request,
};
use linkkeys_local_rp::browser::{
    build_browser_endpoint, resolve_browser_base, BROWSER_ROUTE_AUTHORIZE,
};
use linkkeys_local_rp::{
    begin_local_login, complete_local_login, generate_local_rp_identity,
    local_rp_identity_from_bytes, local_rp_identity_to_bytes, BeginLocalLoginConfig,
    CompleteLocalLoginConfig, DnsResolver, GenerateLocalRpIdentityConfig, LocalRpKeyMaterial,
    PendingLogin,
};
use tallyowl_control_api::types::{
    BeginLoginRequest, BeginLoginResponse, CompleteLoginRequest, CompleteLoginResponse, Membership,
    Membership_role as WireRole,
};
use tallyowl_obs::error::TallyOwlError;
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_obs::time::now_ms;
use tallyowl_obs::Logger;
use tallyowl_store::control::Role;
use tallyowl_store::SegmentedStore;

use crate::linkkeys_rp::RpCaller;

/// How far ahead of this clock an assertion's issue time may be. The IDP and
/// the head have different clocks, and the LinkKeys SDKs allow the same.
const CLOCK_SKEW: Duration = Duration::minutes(5);

/// Who the relying party is. See "Two modes" above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A LinkKeys RP server of the installation's home domain. The default.
    Domain,
    /// An identity this installation generates itself. The fallback.
    Local,
}

impl Mode {
    /// The value of `linkkeys.mode`. The configuration loader refuses any
    /// other, so an unknown value here is a defect.
    pub fn parse(text: &str) -> Option<Mode> {
        match text {
            "domain" => Some(Mode::Domain),
            "local" => Some(Mode::Local),
            _ => None,
        }
    }
}

/// What an installation configured for LinkKeys.
#[derive(Debug, Clone)]
pub struct LinkKeysSettings {
    pub enabled: bool,
    pub mode: Mode,
    /// The home domain of the RP server, in `domain` mode. An assertion names
    /// the relying party it was issued to, and an assertion issued to another
    /// one is refused.
    pub rp_domain: String,
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
    /// The RP server, in `domain` mode. `None` in `local` mode.
    pub rp: Option<Arc<dyn RpCaller>>,
    /// Where a domain's browser address is looked up. `None` is the system
    /// resolver.
    pub dns: Option<Arc<dyn DnsResolver>>,
    /// A refusal tells the caller one sentence, so the cause goes here.
    pub logger: Arc<Logger>,
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

        let (redirect_url, encoded) = match self.settings.mode {
            Mode::Domain => self.begin_domain(&request)?,
            Mode::Local => self.begin_local(&request)?,
        };

        let login_id = tallyowl_store::row::hex(&random_bytes::<16>());
        let expires_at = now_ms() + self.settings.session_lifetime_ms.min(600_000);
        self.store
            .catalog()
            .put_pending_login(&login_id, &encoded, expires_at)
            .map_err(|e| TallyOwlError::internal(e.to_string()))?;

        self.count("begun");
        Ok(BeginLoginResponse {
            redirect_url,
            login_id,
            expires_at,
        })
    }

    /// Ask the RP server to sign a request, and send the browser to the
    /// person's own domain with it.
    fn begin_domain(
        &self,
        request: &BeginLoginRequest,
    ) -> Result<(String, Vec<u8>), TallyOwlError> {
        let identity = liblinkkeys::identity_input::parse_identity_input(&request.user_domain)
            .map_err(|_| {
                self.count("begin-failed");
                TallyOwlError::invalid_argument(format!(
                    "{} is not a LinkKeys domain or login. Type a domain such as example.com, or a \
                     login such as alice@example.com.",
                    request.user_domain
                ))
            })?;
        let rp = self.rp()?;

        // The nonce comes back inside the assertion the person's domain signs,
        // and that is what ties the assertion to this sign-in.
        let nonce = tallyowl_store::row::hex(&random_bytes::<16>());
        let signed = rp
            .call(
                "sign-request",
                encode_rp_sign_request(&RpSignRequest {
                    callback_url: request.callback_url.clone(),
                    nonce: nonce.clone(),
                    // TallyOwl needs the account and its domain, which every
                    // assertion carries. It asks for no claim, so a person is
                    // not asked to release one.
                    requested_claims: None,
                    authentication_requirements: None,
                    flow_context: None,
                }),
            )
            .and_then(|reply| {
                decode_rp_sign_response(&reply)
                    .map_err(|e| format!("The Rp/sign-request reply did not decode: {e}"))
            })
            .map_err(|cause| {
                self.count("begin-failed");
                self.logger.warning(
                    "The LinkKeys RP server did not sign a request.",
                    &[("cause", &cause)],
                );
                TallyOwlError::unavailable(
                    "We could not start a sign-in, because this installation's LinkKeys server did \
                     not answer. Try again, and if it keeps happening ask the person who runs \
                     TallyOwl.",
                )
            })?;

        // The domain names who the person is. Its `_linkkeys_apis` record names
        // where their browser signs in, and that can be another host. A domain
        // with no such record serves the page itself.
        let dns: &dyn DnsResolver = match &self.dns {
            Some(dns) => dns.as_ref(),
            None => linkkeys_local_rp::default_dns_resolver(),
        };
        let base = resolve_browser_base(dns, &identity.domain)
            .unwrap_or_else(|_| format!("https://{}", identity.domain));
        let address =
            build_browser_endpoint(&base, BROWSER_ROUTE_AUTHORIZE, &signed.signed_request)
                .map_err(|e| {
                    self.count("begin-failed");
                    TallyOwlError::unavailable(format!(
                        "We could not start a sign-in with {}. {e}",
                        identity.domain
                    ))
                })?;
        let mut address = url::Url::parse(&address).map_err(|e| {
            TallyOwlError::internal(format!("The sign-in address is not a URL: {e}"))
        })?;
        if let Some(username) = &identity.username {
            address.query_pairs_mut().append_pair("username", username);
        }

        let pending = serde_json::json!({
            "nonce": nonce,
            "domain": identity.domain,
            "callbackUrl": request.callback_url,
        });
        Ok((address.into(), pending.to_string().into_bytes()))
    }

    /// Sign a request with this installation's own key.
    fn begin_local(&self, request: &BeginLoginRequest) -> Result<(String, Vec<u8>), TallyOwlError> {
        let identity = self.identity()?;
        let (redirect, pending) = begin_local_login(BeginLocalLoginConfig::new(
            &identity,
            request.callback_url.clone(),
            request.user_domain.clone(),
            Utc::now(),
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
        let encoded = serde_json::to_vec(&pending).map_err(|e| {
            TallyOwlError::internal(format!("The pending sign-in could not be recorded: {e}"))
        })?;
        Ok((redirect.redirect_url, encoded))
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

        let (user_id, user_domain) = match self.settings.mode {
            Mode::Domain => self.complete_domain(&held, &request)?,
            Mode::Local => self.complete_local(&held, &request)?,
        };

        // The domain is checked again after verification as well as before the
        // redirect. The first check is about where a login was sent; this one
        // is about who came back, and an assertion could name another domain.
        self.check_domain(&user_domain)?;

        // LinkKeys binds every assertion to one account UUID, and that is the
        // subject. The domain travels with it because two domains can issue the
        // same UUID and they are different people.
        let subject = format!("{user_id}@{user_domain}");
        let issued = self
            .store
            .catalog()
            .issue_session(
                &subject,
                &user_domain,
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

    /// Have the RP server decrypt and verify what came back, and check that it
    /// answers this sign-in.
    fn complete_domain(
        &self,
        held: &[u8],
        request: &CompleteLoginRequest,
    ) -> Result<(String, String), TallyOwlError> {
        let pending: serde_json::Value =
            serde_json::from_slice(held).map_err(|_| self.refused("unreadable-login"))?;
        let field = |name: &str| {
            pending
                .get(name)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let (Some(nonce), Some(domain), Some(callback_url)) =
            (field("nonce"), field("domain"), field("callbackUrl"))
        else {
            // A login begun in the other mode, before the mode changed.
            return Err(self.refused("unreadable-login"));
        };

        if !same_address(&request.arrived_url, &callback_url) {
            return Err(self.refused("wrong-callback"));
        }
        let rp = self.rp()?;

        let decrypted = rp
            .call(
                "decrypt-token",
                encode_rp_decrypt_request(&RpDecryptRequest {
                    encrypted_token: request.encrypted_token.clone(),
                }),
            )
            .and_then(|reply| {
                decode_rp_decrypt_response(&reply)
                    .map_err(|e| format!("The Rp/decrypt-token reply did not decode: {e}"))
            })
            .map_err(|cause| self.refused_because("decrypt-failed", &cause))?;

        // The expected domain is the one the person typed, never one read out
        // of the token. That is what pins the result to who they said they are.
        let verified = rp
            .call(
                "verify-assertion",
                encode_rp_verify_request(&RpVerifyRequest {
                    signed_assertion: decrypted.signed_assertion,
                    expected_domain: domain.clone(),
                }),
            )
            .and_then(|reply| {
                decode_rp_verify_response(&reply)
                    .map_err(|e| format!("The Rp/verify-assertion reply did not decode: {e}"))
            })
            .map_err(|cause| self.refused_because("verification-failed", &cause))?;
        if !verified.verified {
            return Err(self.refused_because(
                "verification-failed",
                "The RP server answered that the assertion did not verify.",
            ));
        }

        let assertion = verified.assertion;
        if let Err((outcome, cause)) = check_assertion(
            &assertion,
            &domain,
            &self.settings.rp_domain,
            &nonce,
            Utc::now(),
        ) {
            return Err(self.refused_because(outcome, &cause));
        }
        Ok((assertion.user_id, assertion.domain.to_ascii_lowercase()))
    }

    /// Verify what came back with this installation's own key.
    fn complete_local(
        &self,
        held: &[u8],
        request: &CompleteLoginRequest,
    ) -> Result<(String, String), TallyOwlError> {
        let pending: PendingLogin =
            serde_json::from_slice(held).map_err(|_| self.refused("unreadable-login"))?;
        let identity = self.identity()?;

        let verified = complete_local_login(CompleteLocalLoginConfig::new(
            &identity,
            &pending,
            &request.encrypted_token,
            &request.arrived_url,
            Utc::now(),
        ))
        .map_err(|e| self.refused_because("verification-failed", &e.to_string()))?;
        Ok((verified.user_id, verified.user_domain))
    }

    fn rp(&self) -> Result<&Arc<dyn RpCaller>, TallyOwlError> {
        self.rp.as_ref().ok_or_else(|| {
            TallyOwlError::new(
                tallyowl_obs::ErrorCode::FailedPrecondition,
                "This installation signs in through a LinkKeys RP server and has none configured. \
                 Set `linkkeys.rp.address`, `linkkeys.rp.fingerprints`, and `linkkeys.rp.apiKey`, \
                 or set `linkkeys.mode` to `local`.",
            )
            .retryable(false)
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
        let asked = domain_of(domain);
        if self
            .settings
            .trusted_domains
            .iter()
            .any(|trusted| trusted.trim().eq_ignore_ascii_case(&asked))
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

    /// A refusal whose cause an operator needs. The caller still gets one
    /// sentence; the cause goes to the log, where the person who runs TallyOwl
    /// reads it.
    fn refused_because(&self, reason: &str, cause: &str) -> TallyOwlError {
        self.logger.warning(
            "A sign-in was refused.",
            &[("outcome", reason), ("cause", cause)],
        );
        self.refused(reason)
    }

    fn count(&self, outcome: &str) {
        self.metrics
            .increment("tallyowl_sign_ins_total", &labels(&[("outcome", outcome)]));
    }
}

/// The checks the RP server leaves to the application, in the order the
/// LinkKeys SDKs make them. `verify-assertion` proves that a key of `domain`
/// signed the assertion and that it has not expired; it does not know which
/// sign-in it answers.
fn check_assertion(
    assertion: &IdentityAssertion,
    domain: &str,
    rp_domain: &str,
    nonce: &str,
    now: DateTime<Utc>,
) -> Result<(), (&'static str, String)> {
    if !assertion.domain.eq_ignore_ascii_case(domain) {
        return Err((
            "domain-mismatch",
            format!(
                "The assertion is for {} and the sign-in was for {domain}.",
                assertion.domain
            ),
        ));
    }
    // An assertion issued to another relying party is somebody else's sign-in,
    // replayed here.
    if !assertion.audience.eq_ignore_ascii_case(rp_domain.trim()) {
        return Err((
            "audience-mismatch",
            format!(
                "The assertion was issued to {} and this installation's RP domain is {rp_domain}. \
                 Check `linkkeys.rp.domain`.",
                assertion.audience
            ),
        ));
    }
    if assertion.nonce != nonce {
        return Err((
            "nonce-mismatch",
            "The assertion answers another sign-in.".to_string(),
        ));
    }
    let time = |text: &str| DateTime::parse_from_rfc3339(text).map(|t| t.with_timezone(&Utc));
    match (time(&assertion.issued_at), time(&assertion.expires_at)) {
        (Ok(issued), Ok(expires))
            if issued <= now + CLOCK_SKEW && expires >= now && expires >= issued =>
        {
            Ok(())
        }
        _ => Err((
            "invalid-assertion-time",
            format!(
                "The assertion was issued at {} and expires at {}, and it is now {}.",
                assertion.issued_at,
                assertion.expires_at,
                now.to_rfc3339()
            ),
        )),
    }
}

/// Whether the browser came back to the configured callback: the same scheme,
/// host, port, and path. The query differs, because it carries the token.
fn same_address(arrived: &str, configured: &str) -> bool {
    match (url::Url::parse(arrived), url::Url::parse(configured)) {
        (Ok(arrived), Ok(configured)) => {
            arrived.scheme() == configured.scheme()
                && arrived.host_str() == configured.host_str()
                && arrived.port_or_known_default() == configured.port_or_known_default()
                && arrived.path() == configured.path()
        }
        _ => false,
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

/// The domain in what a person typed: `example.com`, or `alice@example.com`
/// with a username. From LinkKeys 0.16 the SDK accepts both, sends the username
/// to the identity domain, and gives back the domain in lowercase. A domain is
/// trusted or not whatever its case and whoever is named in front of it.
fn domain_of(input: &str) -> String {
    let input = input.trim();
    input
        .rsplit_once('@')
        .map_or(input, |(_, domain)| domain)
        .to_ascii_lowercase()
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

    fn settings(enabled: bool, mode: Mode, trusted: &[&str]) -> LinkKeysSettings {
        LinkKeysSettings {
            enabled,
            mode,
            rp_domain: "home.example".into(),
            trusted_domains: trusted.iter().map(|d| d.to_string()).collect(),
            callback_url: "https://tallyowl.example/sign-in/callback".into(),
            app_name: "TallyOwl".into(),
            session_lifetime_ms: 3_600_000,
        }
    }

    fn quiet() -> Arc<Logger> {
        Arc::new(Logger::new("test", "0", tallyowl_obs::Severity::Error))
    }

    /// The local mode, which needs no RP server.
    fn sign_in(name: &str, enabled: bool, trusted: &[&str]) -> SignIn {
        let metrics = Registry::new();
        SignIn::declare_metrics(&metrics);
        SignIn {
            store: Arc::new(SegmentedStore::open(directory(name)).expect("open")),
            metrics,
            settings: settings(enabled, Mode::Local, trusted),
            rp: None,
            dns: None,
            logger: quiet(),
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
    fn a_trusted_domain_is_trusted_with_a_username_and_in_any_case() {
        // LinkKeys 0.16 accepts `alice@example.com` and returns the domain in
        // lowercase, so the check after verification sees `example.com` even
        // when the operator wrote `Example.com`.
        let service = sign_in("domain-forms", true, &["Example.com"]);
        for accepted in [
            "example.com",
            "EXAMPLE.com",
            "alice@example.com",
            " alice@Example.COM ",
        ] {
            assert!(service.check_domain(accepted).is_ok(), "{accepted}");
        }
        for refused in [
            "attacker.example",
            "example.com@attacker.example",
            "example.com.attacker.example",
        ] {
            let failure = service.check_domain(refused).unwrap_err();
            assert_eq!(
                failure.code,
                tallyowl_obs::ErrorCode::PermissionDenied,
                "{refused}"
            );
        }
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
        let settings = settings(true, Mode::Local, &["example.com"]);
        let first = {
            let service = SignIn {
                store: Arc::new(SegmentedStore::open(&path).unwrap()),
                metrics: Registry::new(),
                settings: settings.clone(),
                rp: None,
                dns: None,
                logger: quiet(),
            };
            local_rp_identity_to_bytes(&service.identity().unwrap())
        };
        let service = SignIn {
            store: Arc::new(SegmentedStore::open(&path).unwrap()),
            metrics: Registry::new(),
            settings,
            rp: None,
            dns: None,
            logger: quiet(),
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

    // ---- Domain mode -------------------------------------------------------
    //
    // The RP server is answered in process: each test says what each `Rp`
    // operation replies, and reads what the head sent. The pinned TLS under it
    // is `linkkeys-rpc-client`'s, and LinkKeys tests that.

    use liblinkkeys::generated::types::{RpDecryptResponse, RpSignResponse, RpVerifyResponse};
    use liblinkkeys::generated::{
        decode_rp_decrypt_request, decode_rp_sign_request, decode_rp_verify_request,
        encode_rp_decrypt_response, encode_rp_sign_response, encode_rp_verify_response,
    };
    use linkkeys_local_rp::dns::DnsLookupError;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeRp {
        replies: Mutex<HashMap<String, Result<Vec<u8>, String>>>,
        calls: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl FakeRp {
        fn reply(&self, op: &str, reply: Result<Vec<u8>, String>) {
            self.replies.lock().unwrap().insert(op.into(), reply);
        }

        fn sent(&self, op: &str) -> Vec<Vec<u8>> {
            let calls = self.calls.lock().unwrap();
            calls
                .iter()
                .filter(|(o, _)| o == op)
                .map(|(_, p)| p.clone())
                .collect()
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl RpCaller for FakeRp {
        fn call(&self, op: &str, payload: Vec<u8>) -> Result<Vec<u8>, String> {
            self.calls.lock().unwrap().push((op.into(), payload));
            self.replies
                .lock()
                .unwrap()
                .get(op)
                .cloned()
                .unwrap_or_else(|| Err(format!("no reply for {op}")))
        }
    }

    struct FakeDns(HashMap<String, Vec<String>>);

    impl DnsResolver for FakeDns {
        fn txt_lookup(&self, name: &str) -> Result<Vec<String>, DnsLookupError> {
            self.0
                .get(name)
                .cloned()
                .ok_or_else(|| DnsLookupError::Lookup(format!("no record for {name}")))
        }
    }

    fn domain_sign_in(name: &str, records: &[(&str, &str)]) -> (SignIn, Arc<FakeRp>) {
        let metrics = Registry::new();
        SignIn::declare_metrics(&metrics);
        let rp = Arc::new(FakeRp::default());
        rp.reply(
            "sign-request",
            Ok(encode_rp_sign_response(&RpSignResponse {
                signed_request: "SIGNED".into(),
            })),
        );
        rp.reply(
            "decrypt-token",
            Ok(encode_rp_decrypt_response(&RpDecryptResponse {
                signed_assertion: "ASSERTION".into(),
            })),
        );
        let dns = FakeDns(
            records
                .iter()
                .map(|(name, txt)| (name.to_string(), vec![txt.to_string()]))
                .collect(),
        );
        let service = SignIn {
            store: Arc::new(SegmentedStore::open(directory(name)).expect("open")),
            metrics,
            settings: settings(true, Mode::Domain, &["example.com"]),
            rp: Some(rp.clone() as Arc<dyn RpCaller>),
            dns: Some(Arc::new(dns)),
            logger: quiet(),
        };
        (service, rp)
    }

    /// An assertion that answers the sign-in `rp` was asked to sign.
    fn answering(rp: &FakeRp) -> IdentityAssertion {
        let sent = decode_rp_sign_request(&rp.sent("sign-request")[0]).unwrap();
        let now = Utc::now();
        IdentityAssertion {
            user_id: "0b9f3c1e-6a4d-4b8e-9f1a-2c3d4e5f6a7b".into(),
            domain: "example.com".into(),
            audience: "home.example".into(),
            nonce: sent.nonce,
            issued_at: (now - Duration::minutes(1)).to_rfc3339(),
            expires_at: (now + Duration::minutes(5)).to_rfc3339(),
            authorized_claims: vec![],
            display_name: None,
        }
    }

    fn verifies(rp: &FakeRp, assertion: IdentityAssertion, verified: bool) {
        rp.reply(
            "verify-assertion",
            Ok(encode_rp_verify_response(&RpVerifyResponse {
                assertion,
                verified,
            })),
        );
    }

    fn callback(login_id: &str) -> CompleteLoginRequest {
        CompleteLoginRequest {
            login_id: login_id.into(),
            encrypted_token: "TOKEN".into(),
            arrived_url: "https://tallyowl.example/sign-in/callback?encrypted_token=TOKEN".into(),
        }
    }

    #[test]
    fn a_domain_sign_in_goes_to_the_browser_address_of_the_persons_domain() {
        let (service, rp) = domain_sign_in(
            "domain-begin",
            &[(
                "_linkkeys_apis.example.com",
                "v=lk1 tcp=lk.example.com:4987 https=login.example.com/lk",
            )],
        );
        let begun = service.begin(begin_request("alice@Example.com")).unwrap();
        assert_eq!(
            begun.redirect_url,
            "https://login.example.com/lk/auth/authorize?signed_request=SIGNED&username=alice"
        );

        let sent = decode_rp_sign_request(&rp.sent("sign-request")[0]).unwrap();
        assert_eq!(
            sent.callback_url,
            "https://tallyowl.example/sign-in/callback"
        );
        assert_eq!(sent.nonce.len(), 32);
        // No claim is asked for, so nobody is asked to release one.
        assert!(sent.requested_claims.is_none());
    }

    #[test]
    fn a_domain_with_no_browser_record_serves_the_page_itself() {
        let (service, _) = domain_sign_in("domain-fallback", &[]);
        let begun = service.begin(begin_request("example.com")).unwrap();
        assert_eq!(
            begun.redirect_url,
            "https://example.com/auth/authorize?signed_request=SIGNED"
        );
    }

    #[test]
    fn a_domain_sign_in_from_an_untrusted_domain_never_reaches_the_rp_server() {
        let (service, rp) = domain_sign_in("domain-untrusted", &[]);
        let failure = service
            .begin(begin_request("attacker.example"))
            .unwrap_err();
        assert_eq!(failure.code, tallyowl_obs::ErrorCode::PermissionDenied);
        assert_eq!(rp.call_count(), 0);
    }

    #[test]
    fn an_rp_server_that_does_not_answer_is_a_moment_and_says_nothing_of_itself() {
        let (service, rp) = domain_sign_in("domain-rp-down", &[]);
        rp.reply(
            "sign-request",
            Err("Rp/sign-request failed: forbidden".into()),
        );
        let failure = service.begin(begin_request("example.com")).unwrap_err();
        assert_eq!(failure.code, tallyowl_obs::ErrorCode::Unavailable);
        assert!(!failure.message.contains("forbidden"));
    }

    #[test]
    fn a_verified_assertion_that_answers_this_sign_in_makes_a_session() {
        let (service, rp) = domain_sign_in("domain-complete", &[]);
        let begun = service.begin(begin_request("example.com")).unwrap();
        verifies(&rp, answering(&rp), true);

        let done = service.complete(callback(&begun.login_id)).unwrap();
        assert_eq!(
            done.subject,
            "0b9f3c1e-6a4d-4b8e-9f1a-2c3d4e5f6a7b@example.com"
        );
        assert!(done.session_token.starts_with("tos_"));
        // Signing in is not authorization.
        assert!(done.memberships.is_empty());

        let decrypt = decode_rp_decrypt_request(&rp.sent("decrypt-token")[0]).unwrap();
        assert_eq!(decrypt.encrypted_token, "TOKEN");
        // The domain the person typed, never one read out of the token.
        let verify = decode_rp_verify_request(&rp.sent("verify-assertion")[0]).unwrap();
        assert_eq!(verify.expected_domain, "example.com");
        assert_eq!(verify.signed_assertion, "ASSERTION");

        // Used once.
        let again = service.complete(callback(&begun.login_id)).unwrap_err();
        assert_eq!(again.message, REFUSAL);
    }

    #[test]
    fn an_assertion_that_does_not_answer_this_sign_in_is_refused() {
        type Change = fn(&mut IdentityAssertion);
        let cases: [(&str, Change); 7] = [
            ("another nonce", |a| a.nonce = "0".repeat(32)),
            ("another relying party", |a| {
                a.audience = "attacker.example".into()
            }),
            ("another domain", |a| a.domain = "attacker.example".into()),
            ("expired", |a| {
                a.expires_at = (Utc::now() - Duration::minutes(1)).to_rfc3339()
            }),
            ("issued in the future", |a| {
                a.issued_at = (Utc::now() + Duration::minutes(30)).to_rfc3339()
            }),
            ("expires before issue", |a| {
                a.issued_at = (Utc::now() + Duration::minutes(2)).to_rfc3339();
                a.expires_at = (Utc::now() + Duration::minutes(1)).to_rfc3339();
            }),
            ("unreadable time", |a| a.expires_at = "soon".into()),
        ];
        for (index, (case, change)) in cases.into_iter().enumerate() {
            let (service, rp) = domain_sign_in(&format!("domain-refused-{index}"), &[]);
            let begun = service.begin(begin_request("example.com")).unwrap();
            let mut assertion = answering(&rp);
            change(&mut assertion);
            verifies(&rp, assertion, true);
            let failure = service.complete(callback(&begun.login_id)).unwrap_err();
            assert_eq!(
                failure.code,
                tallyowl_obs::ErrorCode::Unauthenticated,
                "{case}"
            );
            assert_eq!(failure.message, REFUSAL, "{case}");
        }
    }

    #[test]
    fn an_assertion_the_rp_server_did_not_verify_is_refused() {
        let (service, rp) = domain_sign_in("domain-unverified", &[]);
        let begun = service.begin(begin_request("example.com")).unwrap();
        verifies(&rp, answering(&rp), false);
        let failure = service.complete(callback(&begun.login_id)).unwrap_err();
        assert_eq!(failure.message, REFUSAL);
    }

    #[test]
    fn a_callback_that_arrived_somewhere_else_is_refused_before_the_rp_server_is_asked() {
        let (service, rp) = domain_sign_in("domain-arrived", &[]);
        let begun = service.begin(begin_request("example.com")).unwrap();
        let mut request = callback(&begun.login_id);
        request.arrived_url = "https://attacker.example/sign-in/callback?encrypted_token=T".into();
        let failure = service.complete(request).unwrap_err();
        assert_eq!(failure.message, REFUSAL);
        assert!(rp.sent("decrypt-token").is_empty());
    }

    #[test]
    fn a_domain_installation_with_no_rp_server_says_what_to_set() {
        let (mut service, _) = domain_sign_in("domain-no-rp", &[]);
        service.rp = None;
        let failure = service.begin(begin_request("example.com")).unwrap_err();
        assert_eq!(failure.code, tallyowl_obs::ErrorCode::FailedPrecondition);
        assert!(failure.message.contains("linkkeys.rp.address"));
    }
}
