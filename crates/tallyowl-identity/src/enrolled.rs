//! One process's certificate, from its first enrollment to each renewal.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tallyowl_control_api::types::{EnrollNodeRequest, NodeRole, RenewNodeCertificateRequest};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_obs::log::Logger;
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_rpc::material::{IdentitySource, ReloadDriver, ReloadReport, Reloadable};
use tallyowl_rpc::tls::Identity;
use tallyowl_rpc::trust::TrustSource;

use crate::issuer::Issuer;
use crate::{Clock, Random, Security, EXPIRY_GAUGE, FAILURES_COUNTER};

/// The first wait after a failure.
const BACKOFF_FLOOR_MS: i64 = 1_000;
/// The longest wait after a failure. It is short against any certificate
/// lifetime, so an outage costs minutes of retrying and never hours.
const BACKOFF_CEILING_MS: i64 = 5 * 60_000;

/// What this process asks to be.
#[derive(Debug, Clone)]
pub struct EnrollmentTemplate {
    /// The role token, or `None` for a head that signs its own.
    pub token: Option<String>,
    pub role: NodeRole,
    pub cell: Option<String>,
    pub region: Option<String>,
    /// The name this process's certificate is checked against by a peer that
    /// dials it, and the name it expects of the head.
    pub server_name: String,
}

/// The certificate this process holds now. Not `Debug`: it holds the key.
#[derive(Clone)]
struct Held {
    node_id: String,
    identity: Identity,
    expires_at: i64,
    renew_after: i64,
}

#[derive(Default)]
struct State {
    held: Option<Held>,
    /// Failures in a row, for the wait.
    failures: u32,
    /// No attempt before this time.
    next_attempt_ms: i64,
    /// The head refused a renewal for good, so the next attempt enrolls.
    renewal_refused: bool,
}

/// One process's identity.
///
/// It is an [`IdentitySource`] that presents the held certificate while it is
/// valid and nothing after it lapsed, so a peer never sees a certificate it
/// must refuse. [`Enrolled::step`] does one attempt when one is due and says
/// when to call it again; the renewal thread calls it, and a test calls it with
/// its own clock.
pub struct Enrolled {
    issuer: Box<dyn Issuer>,
    template: EnrollmentTemplate,
    clock: Arc<dyn Clock>,
    random: Arc<dyn Random>,
    state: Mutex<State>,
    metrics: Mutex<Option<Arc<Registry>>>,
    logger: Mutex<Option<Arc<Logger>>>,
}

impl Enrolled {
    pub fn new(
        issuer: Box<dyn Issuer>,
        template: EnrollmentTemplate,
        clock: Arc<dyn Clock>,
        random: Arc<dyn Random>,
    ) -> Arc<Enrolled> {
        Arc::new(Enrolled {
            issuer,
            template,
            clock,
            random,
            state: Mutex::new(State::default()),
            metrics: Mutex::new(None),
            logger: Mutex::new(None),
        })
    }

    /// Build one, start its renewal thread, and return what a service needs.
    pub fn start(
        issuer: Box<dyn Issuer>,
        template: EnrollmentTemplate,
        trust: Arc<dyn TrustSource>,
        trust_reloader: Option<Arc<dyn Reloadable>>,
        clock: Arc<dyn Clock>,
        random: Arc<dyn Random>,
    ) -> Security {
        let enrolled = Enrolled::new(issuer, template, clock, random);
        let handle = RenewalHandle::spawn(Arc::clone(&enrolled), trust_reloader);
        (enrolled as Arc<dyn IdentitySource>, trust, handle)
    }

    /// Publish the expiry gauge and the failure counter here.
    pub fn publish_to(&self, metrics: Arc<Registry>) {
        crate::declare_metrics(&metrics);
        *self.metrics.lock().expect("metrics lock") = Some(metrics);
    }

    /// Write each failed enrollment or renewal to this log, with its reason.
    /// The counter says how often; the log line says why, which is what an
    /// operator needs to act. The waits between attempts grow to five
    /// minutes, so a long outage writes about twelve lines an hour.
    pub fn log_to(&self, logger: Arc<Logger>) {
        *self.logger.lock().expect("logger lock") = Some(logger);
    }

    /// The node ID the head assigned, once there is one.
    pub fn node_id(&self) -> Option<String> {
        self.lock().held.as_ref().map(|held| held.node_id.clone())
    }

    /// Seconds until the held certificate expires, negative once it lapsed, or
    /// `None` before the first certificate.
    pub fn seconds_left(&self, now_ms: i64) -> Option<i64> {
        self.lock()
            .held
            .as_ref()
            .map(|held| (held.expires_at - now_ms).div_euclid(1000))
    }

    /// Do the attempt that is due, if one is, and return when to call again.
    ///
    /// - No certificate, or one that lapsed, or one the head will not renew:
    ///   enroll.
    /// - A certificate past two thirds of its life: renew it.
    /// - Otherwise: nothing, until the renewal point.
    ///
    /// A failure keeps the certificate that is still valid and waits: one
    /// second, doubling to five minutes, with jitter so a fleet that lost its
    /// head does not return to it in the same instant.
    pub fn step(self: &Arc<Self>) -> i64 {
        let now = self.clock.now_ms();
        let (renew, node_id) = {
            let state = self.lock();
            if now < state.next_attempt_ms {
                return state.next_attempt_ms;
            }
            match &state.held {
                Some(held) if now < held.renew_after => {
                    self.publish_expiry(held.expires_at, now);
                    return held.renew_after;
                }
                Some(held) if now < held.expires_at && !state.renewal_refused => {
                    (true, Some(held.node_id.clone()))
                }
                _ => (false, None),
            }
        };

        let attempt = self.attempt(renew, node_id);
        let mut state = self.lock();
        match attempt {
            Ok(held) => {
                self.publish_expiry(held.expires_at, now);
                let next = held.renew_after;
                state.held = Some(held);
                state.failures = 0;
                state.next_attempt_ms = 0;
                state.renewal_refused = false;
                next
            }
            Err(error) => {
                self.count_failure(&error);
                if renew && !error.retryable {
                    state.renewal_refused = true;
                }
                state.failures = state.failures.saturating_add(1);
                let ceiling = BACKOFF_FLOOR_MS
                    .saturating_mul(1i64 << state.failures.saturating_sub(1).min(20))
                    .min(BACKOFF_CEILING_MS);
                // Equal jitter: half the wait is certain and half is random.
                let wait = ceiling / 2 + (self.random.unit() * (ceiling / 2) as f64) as i64;
                state.next_attempt_ms = now + wait.max(1);
                if let Some(held) = &state.held {
                    self.publish_expiry(held.expires_at, now);
                }
                state.next_attempt_ms
            }
        }
    }

    /// One enrollment or renewal, with a key made for it.
    fn attempt(
        self: &Arc<Self>,
        renew: bool,
        node_id: Option<String>,
    ) -> Result<Held, TallyOwlError> {
        let key = rcgen::KeyPair::generate().map_err(|e| {
            TallyOwlError::internal(format!("This process could not make a key: {e}"))
        })?;
        let certificate_request = rcgen::CertificateParams::default()
            .serialize_request(&key)
            .map_err(|e| {
                TallyOwlError::internal(format!(
                    "This process could not build a certificate request: {e}"
                ))
            })?
            .der()
            .to_vec();

        let answer = match (renew, node_id) {
            (true, Some(node_id)) => self.issuer.renew(
                RenewNodeCertificateRequest {
                    node_id,
                    certificate_request,
                },
                Arc::clone(self) as Arc<dyn IdentitySource>,
            )?,
            _ => self.issuer.enroll(EnrollNodeRequest {
                token: self.template.token.clone().unwrap_or_default(),
                certificate_request,
                requested_role: self.template.role.clone(),
                cell: self.template.cell.clone(),
                region: self.template.region.clone(),
                node_id: None,
                capabilities: None,
            })?,
        };

        let authority = answer.certificate_chain.last().cloned().ok_or_else(|| {
            TallyOwlError::internal("The head answered with an empty certificate chain.")
        })?;
        Ok(Held {
            node_id: answer.node_id,
            identity: Identity {
                chain: answer.certificate_chain,
                private_key: key.serialize_der(),
                authority,
                expected_server_name: self.template.server_name.clone(),
            },
            expires_at: answer.expires_at,
            renew_after: answer.renew_after,
        })
    }

    fn publish_expiry(&self, expires_at: i64, now: i64) {
        if let Some(metrics) = self.metrics.lock().expect("metrics lock").as_ref() {
            metrics.set_gauge(
                EXPIRY_GAUGE,
                &labels(&[]),
                (expires_at - now).div_euclid(1000),
            );
        }
    }

    fn count_failure(&self, error: &TallyOwlError) {
        let reason = match error.code {
            ErrorCode::Unavailable => "unreachable",
            ErrorCode::PermissionDenied | ErrorCode::Unauthenticated => "refused",
            ErrorCode::FailedPrecondition => "handshake",
            ErrorCode::InvalidArgument => "invalid",
            _ => "other",
        };
        if let Some(metrics) = self.metrics.lock().expect("metrics lock").as_ref() {
            metrics.increment(FAILURES_COUNTER, &labels(&[("reason", reason)]));
        }
        if let Some(logger) = self.logger.lock().expect("logger lock").as_ref() {
            logger.warning(
                "This node could not get a certificate. It tries again after a wait.",
                &[("reason", reason), ("detail", &error.message)],
            );
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl IdentitySource for Enrolled {
    fn current(&self) -> Option<Identity> {
        let now = self.clock.now_ms();
        self.lock()
            .held
            .as_ref()
            .filter(|held| now < held.expires_at)
            .map(|held| held.identity.clone())
    }
}

/// The renewal thread, and the reloader for the trust it verifies with.
pub struct RenewalHandle {
    enrolled: Arc<Enrolled>,
    trust_reloader: Option<Arc<dyn Reloadable>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RenewalHandle {
    fn spawn(
        enrolled: Arc<Enrolled>,
        trust_reloader: Option<Arc<dyn Reloadable>>,
    ) -> RenewalHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let worker = Arc::clone(&enrolled);
        let stopping = Arc::clone(&stop);
        // A host that will not start the thread leaves a process with no
        // identity, which every mutual-TLS call then reports.
        let thread = std::thread::Builder::new()
            .name("tallyowl-identity".into())
            .spawn(move || {
                while !stopping.load(Ordering::Relaxed) {
                    let due = worker.step();
                    let wait = (due - worker.clock.now_ms()).clamp(1, 1_000);
                    std::thread::sleep(Duration::from_millis(wait as u64));
                }
            })
            .ok();
        RenewalHandle {
            enrolled,
            trust_reloader,
            stop,
            thread,
        }
    }

    /// The identity this handle renews.
    pub fn enrolled(&self) -> &Arc<Enrolled> {
        &self.enrolled
    }

    /// Put the trust on a service's reload driver, so a new authority in
    /// `installation.authorities` takes effect with no restart.
    pub fn trust_reloader(&self) -> Option<Arc<dyn Reloadable>> {
        self.trust_reloader.clone()
    }

    /// Read `installation.authorities` again every `interval`, on its own
    /// thread, until this handle stops. `report` hears each change and each
    /// file that could not be used. A service with no file-based trust starts
    /// nothing.
    pub fn watch_trust(
        &self,
        interval: Duration,
        report: ReloadReport,
    ) -> std::io::Result<Option<JoinHandle<()>>> {
        let Some(reloader) = self.trust_reloader.clone() else {
            return Ok(None);
        };
        ReloadDriver::new(vec![reloader], interval)
            .spawn(Arc::clone(&self.stop), report)
            .map(Some)
    }

    /// Stop renewing. The certificate held now stays valid until it expires.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RenewalHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
