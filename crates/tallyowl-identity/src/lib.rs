//! A TallyOwl service's own identity, and the trust it verifies peers against.
//!
//! D62 and `docs/NODE_IDENTITY.md`. A connection between two TallyOwl services
//! that crosses a network uses mutual TLS. Each side shows a node certificate
//! that the installation's authority signed, and verifies the other side
//! against the authorities in `installation.authorities`.
//!
//! # Where a certificate comes from
//!
//! - **A collector** makes a new key in memory at every start, because it holds
//!   no durable state (AGENTS.md). It enrolls with `enrollment.roleToken` at
//!   `head.endpoint`, and it renews at two thirds of the certificate's life.
//! - **A head that signs** issues its own certificate from its signing
//!   authority, with no token, and issues itself a new one at the same point.
//! - **A head that does not sign** enrolls like a collector.
//!
//! None of this blocks a service's start. [`Enrolled`] has no identity until
//! its first certificate arrives, and it tries again on a capped, jittered
//! wait. A collector's intake does not need it: an application reaches intake
//! over a certificate from files. Only the calls to the head wait, and Corndogs
//! holds the batches meanwhile.
//!
//! # Short life is the revocation
//!
//! There is no revocation list. A node that cannot renew loses its identity when
//! its certificate lapses, and [`Enrolled`] then presents nothing rather than a
//! certificate every peer refuses. An operator stops a collector by revoking
//! its role token: a stateless collector re-enrolls, so revoking only its node
//! record lasts until its next start.

mod enrolled;
mod issuer;

use std::path::PathBuf;
use std::sync::Arc;

use tallyowl_config::Config;
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_obs::metrics::{MetricKind, Registry};
use tallyowl_rpc::material::{IdentitySource, Reloadable};
use tallyowl_rpc::trust::{FileTrust, TrustSource};
use tallyowl_rpc::Address;
use tallyowl_store::catalog::Catalog;
use tallyowl_store::certificates::Authority;

pub use enrolled::{Enrolled, EnrollmentTemplate, RenewalHandle};
pub use issuer::{Issuer, LocalIssuer, RemoteIssuer};
pub use tallyowl_store::certificates::{HEAD_ROLE, HEAD_SERVER_NAME};

/// Milliseconds since the epoch. A field so a test moves time.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// The host's clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        tallyowl_obs::time::now_ms()
    }
}

/// A number in `[0, 1)`, for the jitter. A field so a test fixes it.
pub trait Random: Send + Sync {
    fn unit(&self) -> f64;
}

/// The system random source.
pub struct SystemRandom;

impl Random for SystemRandom {
    fn unit(&self) -> f64 {
        let mut bytes = [0u8; 8];
        if getrandom::fill(&mut bytes).is_err() {
            return 0.5;
        }
        (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// What a service needs for its mutual-TLS hops: the identity it shows, the
/// authorities it trusts, and the renewal that keeps the identity current.
pub type Security = (Arc<dyn IdentitySource>, Arc<dyn TrustSource>, RenewalHandle);

/// The certificate-expiry gauge, in seconds. Negative once it lapsed; absent
/// while the service has no certificate yet.
pub const EXPIRY_GAUGE: &str = "tallyowl_node_certificate_expiry_seconds";

/// Enrollment and renewal failures, labelled by a bounded reason.
pub const FAILURES_COUNTER: &str = "tallyowl_enrollment_failures_total";

/// Declare this crate's instruments. Idempotent.
pub fn declare_metrics(metrics: &Registry) {
    for (name, kind, help) in [
        (
            EXPIRY_GAUGE,
            MetricKind::Gauge,
            "Seconds until this process's node certificate expires. Negative once it lapsed. A node renews at two thirds of the lifetime, so a value below one third of `enrollment.certificateLifetimeHours` means renewal is failing.",
        ),
        (
            FAILURES_COUNTER,
            MetricKind::Counter,
            "Enrollment and renewal attempts that failed, by reason: unreachable, refused, handshake, invalid, or other.",
        ),
    ] {
        metrics.declare(name, kind, help, &[]).unwrap_or_else(|e| {
            panic!("the metric `{name}` is not a name the registry accepts: {}", e.0)
        });
    }
}

/// Read and check the signing authority, when this head is a signer.
///
/// `Ok(None)` when `installation.signingCertificate` is empty. Validation has
/// already made sure the key and the authorities are set with it.
pub fn load_signer(config: &Config, now_ms: i64) -> Result<Option<Authority>, TallyOwlError> {
    let certificate_path = config.text("installation.signingCertificate");
    if certificate_path.is_empty() {
        return Ok(None);
    }
    let chain = read_text(certificate_path, "installation.signingCertificate")?;
    let key = config
        .secret("installation.signingKey")
        .map_err(|e| TallyOwlError::new(ErrorCode::FailedPrecondition, e.message))?;
    let authorities = trust_from(config)?.authorities();
    let hours = config.integer("enrollment.certificateLifetimeHours");
    Authority::from_pem(
        &chain,
        key.expose(),
        &authorities,
        now_ms,
        hours.saturating_mul(60 * 60_000),
    )
    .map(Some)
    .map_err(|e| TallyOwlError::new(ErrorCode::FailedPrecondition, e.to_string()))
}

/// Make the catalog a signer when the configuration names a signing authority.
/// Returns whether it did. A head calls this once, after it opens its store.
pub fn install_signer(
    config: &Config,
    catalog: &Catalog,
    now_ms: i64,
) -> Result<bool, TallyOwlError> {
    match load_signer(config, now_ms)? {
        Some(authority) => {
            catalog.set_signing_authority(authority);
            Ok(true)
        }
        None => Ok(false),
    }
}

/// The name a head's own certificate carries, and the name its peers give it.
///
/// It is the cluster's rule (`tallyowl_cluster::seed`): `node.name`, or `node-`
/// followed by the node's address with every `.` and `:` written as `-`. The
/// address is `replication.advertise`, then `replication.listen`, then, for a
/// head with no replication, `head.listen`.
pub fn head_node_name(config: &Config) -> String {
    let named = config.text("node.name");
    if !named.is_empty() {
        return named.to_string();
    }
    let address = ["replication.advertise", "replication.listen", "head.listen"]
        .into_iter()
        .map(|path| config.text(path))
        .find(|value| !value.is_empty())
        .unwrap_or("head");
    format!("node-{}", address.replace(['.', ':'], "-"))
}

/// A head's identity and trust, or `None` when none of its hops crosses a
/// network.
///
/// A signing head issues its own certificate. A head that does not sign
/// enrolls with `enrollment.roleToken` at `head.endpoint`. The renewal runs on
/// its own thread from the returned handle; the identity is `None` until the
/// first certificate exists.
pub fn for_head(config: &Config, clock: Arc<dyn Clock>) -> Result<Option<Security>, TallyOwlError> {
    let mut hops = vec![("head.listen", config.text("head.listen"))];
    for path in ["replication.listen", "replication.advertise"] {
        hops.push((path, config.text(path)));
    }
    if !needs_identity(config, &hops)? {
        return Ok(None);
    }
    let trust = trust_from(config)?;
    let node = head_node_name(config);
    let template = EnrollmentTemplate {
        token: None,
        role: tallyowl_control_api::types::NodeRole::StorageProcess,
        cell: non_empty(config.text("cell.id")),
        region: non_empty(config.text("cell.region")),
        server_name: node.clone(),
    };
    let issuer: Box<dyn Issuer> = match load_signer(config, clock.now_ms())? {
        Some(authority) => Box::new(LocalIssuer::new(
            Arc::new(authority),
            node,
            Arc::clone(&clock),
        )),
        None => {
            let token = role_token(config, "a head that does not sign")?;
            return Ok(Some(start(
                config,
                EnrollmentTemplate {
                    token: Some(token),
                    ..template
                },
                trust,
                clock,
            )?));
        }
    };
    Ok(Some(Enrolled::start(
        issuer,
        template,
        Arc::clone(&trust) as Arc<dyn TrustSource>,
        Some(trust as Arc<dyn Reloadable>),
        clock,
        Arc::new(SystemRandom),
    )))
}

/// A collector's identity and trust, or `None` when it reaches the head on a
/// loopback address or a unix socket.
pub fn for_collector(
    config: &Config,
    clock: Arc<dyn Clock>,
) -> Result<Option<Security>, TallyOwlError> {
    if !needs_identity(config, &[("head.endpoint", config.text("head.endpoint"))])? {
        return Ok(None);
    }
    let trust = trust_from(config)?;
    let token = role_token(config, "a collector that reaches the head over a network")?;
    let roles = config.list("collector.roles");
    let role = if roles.iter().any(|r| r == "forwarder") {
        tallyowl_control_api::types::NodeRole::CollectorForwarder
    } else if roles.iter().any(|r| r == "intake") {
        tallyowl_control_api::types::NodeRole::CollectorIntake
    } else {
        tallyowl_control_api::types::NodeRole::CompatibilityReceiver
    };
    let template = EnrollmentTemplate {
        token: Some(token),
        role,
        cell: non_empty(config.text("cell.id")),
        region: non_empty(config.text("cell.region")),
        server_name: HEAD_SERVER_NAME.to_string(),
    };
    start(config, template, trust, clock).map(Some)
}

fn start(
    config: &Config,
    template: EnrollmentTemplate,
    trust: Arc<FileTrust>,
    clock: Arc<dyn Clock>,
) -> Result<Security, TallyOwlError> {
    let issuer = RemoteIssuer::new(
        config.text("head.endpoint"),
        Arc::clone(&trust) as Arc<dyn TrustSource>,
    )?;
    Ok(Enrolled::start(
        Box::new(issuer),
        template,
        Arc::clone(&trust) as Arc<dyn TrustSource>,
        Some(trust as Arc<dyn Reloadable>),
        clock,
        Arc::new(SystemRandom),
    ))
}

/// Whether any of these addresses crosses a network, and no setting permits
/// plaintext there.
fn needs_identity(config: &Config, hops: &[(&str, &str)]) -> Result<bool, TallyOwlError> {
    if allows_plaintext(config) {
        return Ok(false);
    }
    for (path, value) in hops {
        if value.is_empty() {
            continue;
        }
        let address = Address::parse(value).map_err(|e| {
            TallyOwlError::new(
                ErrorCode::InvalidArgument,
                format!("The setting `{path}` is not an address: {}", e.message),
            )
        })?;
        if !address.plaintext_permitted() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `transport.allowPlaintext`: the operator accepted plaintext on a network.
fn allows_plaintext(config: &Config) -> bool {
    config.boolean("transport.allowPlaintext")
}

fn trust_from(config: &Config) -> Result<Arc<FileTrust>, TallyOwlError> {
    let files: Vec<PathBuf> = config
        .list("installation.authorities")
        .into_iter()
        .map(PathBuf::from)
        .collect();
    if files.is_empty() {
        return Err(TallyOwlError::new(
            ErrorCode::FailedPrecondition,
            "This process reaches another TallyOwl service over a network, and `installation.authorities` is empty, so it cannot verify that service. Add the root authority's certificate, for example `/etc/tallyowl/root.crt`. `tallyowl-head ca create` makes one.",
        ));
    }
    FileTrust::load(&files).map(Arc::new)
}

fn role_token(config: &Config, who: &str) -> Result<String, TallyOwlError> {
    if config.text("enrollment.roleToken").is_empty() {
        return Err(TallyOwlError::new(
            ErrorCode::FailedPrecondition,
            format!("This is {who}, and `enrollment.roleToken` is not set, so it cannot get a certificate. Ask the operator for a role token, and set `enrollment.roleToken` to `file:<path>` or `env:<name>`."),
        ));
    }
    config
        .secret("enrollment.roleToken")
        .map(|secret| secret.expose().trim().to_string())
        .map_err(|e| TallyOwlError::new(ErrorCode::FailedPrecondition, e.message))
}

fn read_text(path: &str, setting: &str) -> Result<String, TallyOwlError> {
    std::fs::read_to_string(path).map_err(|e| {
        TallyOwlError::new(
            ErrorCode::FailedPrecondition,
            format!("The file `{path}` that `{setting}` names could not be read: {e}"),
        )
    })
}

fn non_empty(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_string())
}
