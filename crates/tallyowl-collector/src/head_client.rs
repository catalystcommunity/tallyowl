//! The forwarder's connection to head ingest.
//!
//! The head returns a committed receipt only after the tablet satisfies its
//! configured receipt policy. The collector completes its Corndogs task only
//! after that receipt. See `docs/DELIVERY.md` section 5.
//!
//! The trait exists so that a delivery test can make the head reject, stall, or
//! drop a connection at a chosen point. The head is TallyOwl's own service, but
//! this seam is a network boundary rather than the storage interface, and
//! `AGENTS.md` forbids mocking the second, not the first.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tallyowl_collector_api::codec::{
    decode_commit_batch_response, decode_resolve_key_response, decode_service_error,
    encode_resolve_key_request,
};
use tallyowl_collector_api::types::{ResolveKeyRequest, ResolveKeyResponse};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_obs::metrics::{labels, MetricKind, Registry};
use tallyowl_rpc::{Client, Response, SERVICE_ERROR_VARIANT};

use crate::forwarder::{CommitReceipt, DeliveryResult};
use crate::tenancy::KeyDirectory;

/// The head, as the forwarder sees it.
pub trait HeadClient: Send + Sync {
    /// Send an encoded `CommitBatchRequest` and read the receipt.
    fn commit_batch(&self, request: Vec<u8>) -> DeliveryResult;
}

const HEAD_CALL_SECONDS: &str = "tallyowl_head_call_seconds";
const HEAD_CALL_FAILURES: &str = "tallyowl_head_call_failures_total";

/// The real head, over CSIL-RPC on TCP.
///
/// One `RemoteHead` is one connection, and a call holds it for its whole round
/// trip. A process that commits batches and resolves credentials builds one for
/// each job, so a slow commit does not hold every intake worker whose key just
/// expired. See `main.rs`.
pub struct RemoteHead {
    client: Client,
    metrics: Option<Arc<Registry>>,
}

impl RemoteHead {
    pub fn new(address: &str, max_frame_bytes: usize) -> RemoteHead {
        RemoteHead {
            client: Client::new(address, max_frame_bytes),
            metrics: None,
        }
    }

    /// The head over mutual TLS, showing this collector's enrolled identity and
    /// verifying the head against the trusted authorities. D62.
    ///
    /// Until the collector has enrolled, the identity is empty, and a call
    /// fails as retryable rather than going out in the clear. The forwarder's
    /// breaker waits, and intake, which needs no identity, keeps accepting.
    pub fn mutual(
        address: &str,
        max_frame_bytes: usize,
        server_name: &str,
        identity: Arc<dyn tallyowl_rpc::material::IdentitySource>,
        trust: Arc<dyn tallyowl_rpc::trust::TrustSource>,
    ) -> RemoteHead {
        RemoteHead {
            client: Client::mutual(address, max_frame_bytes, server_name, identity, trust),
            metrics: None,
        }
    }

    /// How long one answer from the head may take. A head that accepts the
    /// connection and never answers costs one wait of this length.
    pub fn with_call_timeout(self, timeout: Duration) -> RemoteHead {
        RemoteHead {
            client: self.client.with_io_timeout(timeout),
            metrics: self.metrics,
        }
    }

    /// Publish how long each call takes and how many fail.
    pub fn with_metrics(mut self, metrics: Arc<Registry>) -> RemoteHead {
        self.metrics = Some(metrics);
        self
    }

    pub fn declare_metrics(metrics: &Registry) {
        metrics
            .declare(
                HEAD_CALL_SECONDS,
                MetricKind::Histogram,
                "How long one call to the head took, by operation.",
                &[0.001, 0.005, 0.025, 0.1, 0.5, 2.0, 10.0, 30.0],
            )
            .unwrap_or_else(|e| {
                panic!(
                    "the metric `{HEAD_CALL_SECONDS}` is not a name the registry accepts: {}",
                    e.0
                )
            });
        metrics
            .declare(
                HEAD_CALL_FAILURES,
                MetricKind::Counter,
                "Calls to the head that produced no answer, by operation.",
                &[],
            )
            .unwrap_or_else(|e| {
                panic!(
                    "the metric `{HEAD_CALL_FAILURES}` is not a name the registry accepts: {}",
                    e.0
                )
            });
    }

    fn call(&self, op: &str, payload: Vec<u8>) -> Result<Response, TallyOwlError> {
        let started = Instant::now();
        let outcome = self.client.call("TallyOwlCollector", op, payload);
        if let Some(metrics) = &self.metrics {
            metrics.observe(
                HEAD_CALL_SECONDS,
                &labels(&[("op", op)]),
                started.elapsed().as_secs_f64(),
            );
            if outcome.is_err() {
                metrics.increment(HEAD_CALL_FAILURES, &labels(&[("op", op)]));
            }
        }
        outcome
    }

    /// Forget the current connection. The next call opens a fresh one.
    ///
    /// Ordinary operation never needs this: the client reconnects on its own
    /// when a call fails. A test that takes the head away needs it, because an
    /// open socket outlives a listener that stopped accepting.
    pub fn disconnect(&self) {
        self.client.disconnect();
    }
}

impl HeadClient for RemoteHead {
    fn commit_batch(&self, request: Vec<u8>) -> DeliveryResult {
        let response = self.call("commit-batch", request)?;

        // An application error rides back with transport status 0 and the
        // variant `ServiceError`. Reading the variant is what tells a permanent
        // rejection from an unreachable head.
        if response.variant.as_deref() == Some(SERVICE_ERROR_VARIANT) {
            let error = decode_service_error(&response.payload).map_err(|e| {
                TallyOwlError::internal(format!(
                    "The head returned an error we could not read: {e}"
                ))
            })?;
            return Err(
                TallyOwlError::new(from_wire_code(&error.code), error.message)
                    .retryable(error.retryable),
            );
        }

        let receipt = decode_commit_batch_response(&response.payload).map_err(|e| {
            TallyOwlError::internal(format!(
                "The head returned a receipt we could not read: {e}"
            ))
        })?;
        Ok(CommitReceipt {
            accepted: receipt.accepted,
            committed_at: receipt.committed_at,
            commit_watermark: receipt.commit_watermark,
            deduplicated: receipt.deduplicated.unwrap_or(false),
        })
    }
}

/// The head is also the control plane, so it is also where a credential
/// resolves. One connection carries both, because a CSIL-RPC request names its
/// own operation and the head serves them on one listener.
impl KeyDirectory for RemoteHead {
    fn resolve(&self, credential: &str) -> Result<ResolveKeyResponse, TallyOwlError> {
        let response = self.call(
            "resolve-key",
            encode_resolve_key_request(&ResolveKeyRequest {
                credential: credential.to_string(),
            }),
        )?;
        if response.variant.as_deref() == Some(SERVICE_ERROR_VARIANT) {
            let error = decode_service_error(&response.payload).map_err(|e| {
                TallyOwlError::internal(format!(
                    "The head returned an error we could not read: {e}"
                ))
            })?;
            return Err(
                TallyOwlError::new(from_wire_code(&error.code), error.message)
                    .retryable(error.retryable),
            );
        }
        decode_resolve_key_response(&response.payload).map_err(|e| {
            TallyOwlError::internal(format!(
                "The head returned an answer we could not read: {e}"
            ))
        })
    }
}

/// The head is where a collection policy comes from as well, over the same
/// connection and the same service. `docs/POLICY.md` section 7.
impl crate::policy::PolicySource for RemoteHead {
    fn fetch_policy(
        &self,
        source_id: &[u8],
        known_version: Option<u64>,
    ) -> Result<Option<tallyowl_collector_api::types::CollectionPolicy>, TallyOwlError> {
        use tallyowl_collector_api::codec::{
            decode_fetch_policy_response, encode_fetch_policy_request,
        };
        use tallyowl_collector_api::types::FetchPolicyRequest;

        let response = self.call(
            "fetch-policy",
            encode_fetch_policy_request(&FetchPolicyRequest {
                source_id: source_id.to_vec(),
                known_version,
            }),
        )?;
        if response.variant.as_deref() == Some(SERVICE_ERROR_VARIANT) {
            let error = decode_service_error(&response.payload).map_err(|e| {
                TallyOwlError::internal(format!(
                    "The head returned an error we could not read: {e}"
                ))
            })?;
            return Err(
                TallyOwlError::new(from_wire_code(&error.code), error.message)
                    .retryable(error.retryable),
            );
        }
        let answer = decode_fetch_policy_response(&response.payload).map_err(|e| {
            TallyOwlError::internal(format!(
                "The head returned a collection policy we could not read: {e}"
            ))
        })?;
        // `unchanged` and an absent policy both mean the held version is
        // current. They are read together rather than either alone, because a
        // head that said "unchanged" and sent a policy anyway would otherwise
        // have its policy quietly ignored.
        if answer.unchanged || answer.policy.is_none() {
            return Ok(None);
        }
        Ok(answer.policy)
    }
}

fn from_wire_code(code: &tallyowl_collector_api::types::ErrorCode) -> ErrorCode {
    use tallyowl_collector_api::types::ErrorCode as Wire;
    match code {
        Wire::InvalidArgument => ErrorCode::InvalidArgument,
        Wire::Unauthenticated => ErrorCode::Unauthenticated,
        Wire::PermissionDenied => ErrorCode::PermissionDenied,
        Wire::NotFound => ErrorCode::NotFound,
        Wire::AlreadyExists => ErrorCode::AlreadyExists,
        Wire::ResourceExhausted => ErrorCode::ResourceExhausted,
        Wire::FailedPrecondition => ErrorCode::FailedPrecondition,
        Wire::Unavailable => ErrorCode::Unavailable,
        Wire::SchemaUnsupported => ErrorCode::SchemaUnsupported,
        Wire::BudgetExceeded => ErrorCode::BudgetExceeded,
        Wire::IncompleteResult => ErrorCode::IncompleteResult,
        Wire::Internal => ErrorCode::Internal,
    }
}

pub mod testing {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    /// A head that answers however a test needs it to.
    #[derive(Default)]
    pub struct FakeHead {
        pub calls: AtomicU64,
        outcome: Mutex<Outcome>,
        pub seen: Mutex<Vec<Vec<u8>>>,
    }

    #[derive(Clone, Default)]
    pub enum Outcome {
        #[default]
        Commit,
        Retryable(String),
        Permanent(String),
    }

    impl FakeHead {
        pub fn new() -> Arc<FakeHead> {
            Arc::new(FakeHead::default())
        }

        pub fn set(&self, outcome: Outcome) {
            *self.outcome.lock().unwrap() = outcome;
        }

        pub fn call_count(&self) -> u64 {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl HeadClient for FakeHead {
        fn commit_batch(&self, request: Vec<u8>) -> DeliveryResult {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.seen.lock().unwrap().push(request);
            match self.outcome.lock().unwrap().clone() {
                Outcome::Commit => Ok(CommitReceipt {
                    accepted: 1,
                    committed_at: 1,
                    commit_watermark: 1,
                    deduplicated: false,
                }),
                Outcome::Retryable(m) => Err(TallyOwlError::unavailable(m)),
                Outcome::Permanent(m) => {
                    Err(TallyOwlError::new(ErrorCode::InvalidArgument, m).retryable(false))
                }
            }
        }
    }
}
