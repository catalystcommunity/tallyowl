//! The `TallyOwlCollector` service, as collector intake serves it.
//!
//! Intake answers `submit-batch`, `fetch-policy`, and `health`. It does not
//! answer `commit-batch`: that operation is the head's side of the same
//! contract, and a collector that answered it would be pretending to be final
//! storage.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tallyowl_collector_api::codec::{
    decode_fetch_policy_request, decode_health_request, decode_submit_batch_request,
    encode_collector_health, encode_fetch_policy_response, encode_service_error,
    encode_submit_batch_response,
};
use tallyowl_collector_api::types::{CollectorHealth, FetchPolicyResponse};
use tallyowl_obs::health::Health;
use tallyowl_obs::log::Logger;
use tallyowl_rpc::{
    error_outcome, malformed, reply, unknown_operation, Dispatcher, Outcome, Request,
};

use crate::forwarder::ForwarderState;
use crate::intake::{to_wire_error, Intake};

pub const SERVICE_NAME: &str = "TallyOwlCollector";

pub struct CollectorService {
    pub intake: Arc<Intake>,
    pub health: Arc<Health>,
    pub logger: Arc<Logger>,
    pub forwarder_state: Arc<ForwarderState>,
    /// The credential this collector accepts. Phase 4 replaces this with the
    /// scoped API key model and per-connection authentication; the shape here
    /// already resolves tenancy from the credential rather than the payload.
    pub credential: String,
    pub roles: Vec<String>,
    /// The collection policy this collector applies, for the health report.
    pub policy: Option<Arc<crate::policy::Held>>,
}

impl Dispatcher for CollectorService {
    fn dispatch(&self, request: &Request) -> Outcome {
        match request.op.as_str() {
            "submit-batch" => self.submit_batch(request),
            "fetch-policy" => self.fetch_policy(request),
            "health" => self.health(request),
            other => unknown_operation(&request.service, other),
        }
    }
}

impl CollectorService {
    fn submit_batch(&self, request: &Request) -> Outcome {
        // The connection's credential decides tenancy. A per-request credential
        // overrides it only because Phase 1 has no connection-level
        // authentication yet; Phase 4 moves this to the connection.
        let credential = request
            .auth
            .clone()
            .unwrap_or_else(|| self.credential.clone());

        // The credential first, and the batch second. The credential travels
        // outside the payload, so nothing has to be decoded to read it, and a
        // caller that has proved nothing does not get a frame of its choosing
        // turned into a value tree several times its size.
        let tenancy = match self.intake.authenticate(&credential) {
            Ok(tenancy) => tenancy,
            Err(e) => {
                self.logger.warning(
                    "Refused a batch.",
                    &[("code", e.code.as_str()), ("reason", &e.message)],
                );
                return error_outcome(encode_service_error(&to_wire_error(&e)));
            }
        };

        let decoded = match decode_submit_batch_request(&request.payload) {
            Ok(decoded) => decoded,
            Err(e) => return malformed(e),
        };

        match self.intake.submit_as(tenancy, decoded) {
            Ok(accepted) => {
                self.logger.info(
                    "Accepted a batch into the durable store.",
                    &[
                        ("accepted", &accepted.response.accepted.to_string()),
                        ("task", &accepted.task_uuid),
                        // The batch identifier, so the two halves of the path
                        // can be correlated. The delivery line carries it and
                        // this one did not, so nothing could tell which
                        // acceptance produced which delivery.
                        ("batch_id", &tallyowl_store_hex(&accepted.response.batch_id)),
                        ("producer", "app-driver"),
                    ],
                );
                reply(
                    "SubmitBatchResponse",
                    encode_submit_batch_response(&accepted.response),
                )
            }
            Err(e) => {
                self.logger.warning(
                    "Refused a batch.",
                    &[("code", e.code.as_str()), ("reason", &e.message)],
                );
                error_outcome(encode_service_error(&to_wire_error(&e)))
            }
        }
    }

    /// What an app driver asks intake for.
    ///
    /// **A collector does not compile a policy and it does not hand out the
    /// head's snapshot either.** The snapshot the head sends is scoped to this
    /// collector's own source, and an application's driver is a different
    /// source with a different scope. What a driver gets is the version this
    /// collector is applying, so it can tell that the rules changed; the rules
    /// themselves reach a browser through `policy-version` on the ingest
    /// contract, which is the operation that exists for it.
    ///
    /// `unchanged` is true with no policy: there is nothing here to apply, not
    /// "apply nothing". The two read the same on the wire only if a caller
    /// stops reading at the first field.
    fn fetch_policy(&self, request: &Request) -> Outcome {
        if let Err(e) = decode_fetch_policy_request(&request.payload) {
            return malformed(e);
        }
        reply(
            "FetchPolicyResponse",
            encode_fetch_policy_response(&FetchPolicyResponse {
                policy: None,
                unchanged: true,
            }),
        )
    }

    fn health(&self, request: &Request) -> Outcome {
        if let Err(e) = decode_health_request(&request.payload) {
            return malformed(e);
        }
        let report = self.health.report();
        let last_sweep = self.forwarder_state.last_sweep_ms.load(Ordering::Relaxed);
        // The depth is the queue's own answer, copied here at each sweep. The
        // age is this process's lower bound: a restart loses the age and keeps
        // the depth, and a depth with no age says so rather than guessing.
        let queue_depth = self.forwarder_state.queue_depth.load(Ordering::Relaxed);
        let oldest_at = self
            .forwarder_state
            .oldest_waiting_at
            .load(Ordering::Relaxed);
        let health = CollectorHealth {
            role: primary_role(&self.roles),
            ready: report.ready,
            queue_depth: queue_depth.max(0) as u64,
            oldest_task_age_ms: if queue_depth > 0 && oldest_at > 0 {
                (tallyowl_obs::time::now_ms() - oldest_at).max(0)
            } else {
                0
            },
            quarantine_count: self.forwarder_state.quarantined.load(Ordering::Relaxed),
            // The version this collector is judging batches against. Zero means
            // it holds none, which is what a collector that has never fetched
            // one honestly reports.
            applied_policy_version: self.policy.as_ref().map(|held| held.version()).unwrap_or(0),
            // How long since the head last answered a policy fetch.
            //
            // **It never fails readiness**, and that is deliberate: a collector
            // that stopped accepting telemetry when it lost the head would turn
            // a control-plane outage into a data-plane one, and the last good
            // policy is still in force. This is the number an operator alerts
            // on instead. Absent means this collector has never fetched one,
            // which is not the same as a stale one.
            policy_age_ms: self.policy.as_ref().and_then(|held| {
                let at = held.fetched_at();
                (at > 0).then(|| tallyowl_obs::time::now_ms() - at)
            }),
            last_sweep_at: (last_sweep > 0).then_some(last_sweep),
        };
        reply("CollectorHealth", encode_collector_health(&health))
    }
}

/// Which role this process reports. A process can run several; the report names
/// the one that decides its readiness, and the forwarder decides it whenever it
/// is present, because the sweep is the check that fails silently.
fn primary_role(roles: &[String]) -> tallyowl_collector_api::types::CollectorHealth_role {
    use tallyowl_collector_api::types::CollectorHealth_role as Role;
    if roles.iter().any(|r| r == "forwarder") {
        Role::Forwarder
    } else if roles.iter().any(|r| r == "intake") {
        Role::Intake
    } else {
        Role::CompatibilityReceiver
    }
}

/// Hexadecimal, for a log line that has to be correlated with another one.
fn tallyowl_store_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable::testing::FakeQueue;
    use crate::durable::DurableQueue;
    use crate::intake::Limits;
    use crate::tenancy::testing::FakeDirectory;
    use crate::tenancy::{KeyDirectory, TenancyResolver};
    use tallyowl_collector_api::codec::decode_service_error;
    use tallyowl_obs::log::Severity;
    use tallyowl_obs::metrics::{labels, Registry};

    fn service(directory: Arc<FakeDirectory>, metrics: Arc<Registry>) -> CollectorService {
        Intake::declare_metrics(&metrics);
        CollectorService {
            intake: Arc::new(Intake {
                queue: FakeQueue::new() as Arc<dyn DurableQueue>,
                queue_name: "tallyowl-delivery".into(),
                tenancy: Arc::new(TenancyResolver::new(
                    directory as Arc<dyn KeyDirectory>,
                    60_000,
                )),
                limits: Limits {
                    max_batch_bytes: 512 * 1024,
                    max_event_bytes: 64 * 1024,
                    max_properties: 128,
                },
                durable_copies: 1,
                series: Arc::new(crate::series::SeriesLedger::new(
                    crate::series::SeriesBudget::default(),
                )),
                metrics,
                stamped: Vec::new(),
                policy: None,
            }),
            health: Health::new(),
            logger: Arc::new(Logger::new(
                "collector-service-test",
                "0.0.0",
                Severity::Error,
            )),
            forwarder_state: ForwarderState::new(),
            credential: String::new(),
            roles: vec!["intake".into()],
            policy: None,
        }
    }

    fn submit(credential: &str, payload: Vec<u8>) -> Request {
        let mut request = Request::new(SERVICE_NAME, "submit-batch", payload);
        request.auth = Some(credential.to_string());
        request
    }

    #[test]
    fn a_caller_with_no_valid_key_is_refused_before_its_batch_is_decoded() {
        // The body here cannot be decoded. A caller that has proved nothing is
        // told about its key and not about its body, which shows the body was
        // never looked at.
        let directory = FakeDirectory::new();
        let metrics = Registry::new();
        let service = service(Arc::clone(&directory), Arc::clone(&metrics));

        let outcome = service.dispatch(&submit("wrong", vec![0xff; 64]));
        let Outcome::Reply { variant, payload } = outcome else {
            panic!("a wrong key is a typed refusal, not a transport failure");
        };
        assert_eq!(variant, "ServiceError");
        let refusal = decode_service_error(&payload).expect("a service error");
        assert_eq!(
            refusal.code,
            tallyowl_collector_api::types::ErrorCode::Unauthenticated
        );
        assert_eq!(
            metrics.counter_value(
                "tallyowl_batches_refused_total",
                &labels(&[("reason", "unauthenticated")])
            ),
            1
        );
    }

    #[test]
    fn a_caller_with_a_valid_key_still_hears_that_its_batch_could_not_be_read() {
        let directory = FakeDirectory::new();
        directory.add("key-a", None);
        let service = service(directory, Registry::new());

        let outcome = service.dispatch(&submit("key-a", vec![0xff; 64]));
        assert!(
            matches!(outcome, Outcome::Transport(..)),
            "an authenticated caller gets the decode failure it got before"
        );
    }
}
