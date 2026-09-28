//! A process's identity: enrollment, renewal at two thirds, a head that is
//! away, a revoked token, and the certificates on a real mutual-TLS connection.
//!
//! Time is a field. No test waits for it to pass. The two socket tests use
//! real TLS because the handshake is what they test.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tallyowl_config::loader::{flatten_yaml, Inputs};
use tallyowl_config::Config;
use tallyowl_control_api::codec::{
    decode_enroll_node_request, decode_renew_node_certificate_request, encode_enroll_node_response,
    encode_service_error,
};
use tallyowl_control_api::types::{
    EnrollNodeRequest, EnrollNodeResponse, NodeRole, RenewNodeCertificateRequest, ServiceError,
};
use tallyowl_identity::{
    for_collector, for_head, head_node_name, Clock, Enrolled, EnrollmentTemplate, Issuer,
    LocalIssuer, Random, RemoteIssuer, EXPIRY_GAUGE, FAILURES_COUNTER, HEAD_SERVER_NAME,
};
use tallyowl_obs::error::{ErrorCode, TallyOwlError};
use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_rpc::material::{IdentitySource, StaticIdentity};
use tallyowl_rpc::trust::{StaticTrust, TrustSource};
use tallyowl_rpc::{Dispatcher, Outcome, Peer, PeerIdentity, Request, ServerOptions};
use tallyowl_store::certificates::{generate_authorities, Authority, GeneratedAuthorities};

const NOW: i64 = 1_785_628_800_000;
const HOUR: i64 = 60 * 60_000;

struct FakeClock(AtomicI64);

impl FakeClock {
    fn at(ms: i64) -> Arc<FakeClock> {
        Arc::new(FakeClock(AtomicI64::new(ms)))
    }
    fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
    fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct Fixed(f64);

impl Random for Fixed {
    fn unit(&self) -> f64 {
        self.0
    }
}

fn ders(pem: &str) -> Vec<Vec<u8>> {
    x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes())
        .map(|block| block.expect("PEM").contents)
        .collect()
}

fn authority(generated: &GeneratedAuthorities, max_lifetime_ms: i64) -> Arc<Authority> {
    Arc::new(
        Authority::from_pem(
            &generated.intermediate_chain_pem,
            &generated.intermediate_key_pem,
            &ders(&generated.root_certificate_pem),
            NOW,
            max_lifetime_ms,
        )
        .expect("a usable signer"),
    )
}

/// A head that can be away, and that can refuse everything for good.
struct ScriptedHead {
    signer: LocalIssuer,
    away: AtomicBool,
    refusing: AtomicBool,
    enrollments: AtomicUsize,
    renewals: AtomicUsize,
}

impl ScriptedHead {
    fn new(authority: Arc<Authority>, clock: Arc<FakeClock>) -> Arc<ScriptedHead> {
        Arc::new(ScriptedHead {
            signer: LocalIssuer::new(authority, "node-collector-1".into(), clock),
            away: AtomicBool::new(false),
            refusing: AtomicBool::new(false),
            enrollments: AtomicUsize::new(0),
            renewals: AtomicUsize::new(0),
        })
    }

    fn outcome(&self) -> Result<(), TallyOwlError> {
        if self.away.load(Ordering::SeqCst) {
            return Err(
                TallyOwlError::new(ErrorCode::Unavailable, "the head is away").retryable(true),
            );
        }
        if self.refusing.load(Ordering::SeqCst) {
            return Err(TallyOwlError::new(ErrorCode::PermissionDenied, "revoked").retryable(false));
        }
        Ok(())
    }
}

struct Shared(Arc<ScriptedHead>);

impl Issuer for Shared {
    fn enroll(&self, request: EnrollNodeRequest) -> Result<EnrollNodeResponse, TallyOwlError> {
        self.0.enrollments.fetch_add(1, Ordering::SeqCst);
        self.0.outcome()?;
        self.0.signer.enroll(request)
    }

    fn renew(
        &self,
        request: RenewNodeCertificateRequest,
        current: Arc<dyn IdentitySource>,
    ) -> Result<EnrollNodeResponse, TallyOwlError> {
        self.0.renewals.fetch_add(1, Ordering::SeqCst);
        assert!(
            current.current().is_some(),
            "a renewal shows the identity it renews"
        );
        self.0.outcome()?;
        self.0.signer.renew(request, current)
    }
}

fn collector_template() -> EnrollmentTemplate {
    EnrollmentTemplate {
        token: Some("towr_token".into()),
        role: NodeRole::CollectorForwarder,
        cell: None,
        region: None,
        server_name: HEAD_SERVER_NAME.into(),
    }
}

/// An identity against a scripted head with a three-hour lifetime.
fn scripted() -> (Arc<Enrolled>, Arc<ScriptedHead>, Arc<FakeClock>) {
    let clock = FakeClock::at(NOW);
    let generated = generate_authorities(NOW).expect("authorities");
    let head = ScriptedHead::new(authority(&generated, 3 * HOUR), Arc::clone(&clock));
    let enrolled = Enrolled::new(
        Box::new(Shared(Arc::clone(&head))),
        collector_template(),
        Arc::clone(&clock) as Arc<dyn Clock>,
        Arc::new(Fixed(0.5)),
    );
    (enrolled, head, clock)
}

#[test]
fn a_head_that_is_away_at_start_does_not_block_and_the_identity_arrives_later() {
    let (enrolled, head, clock) = scripted();
    head.away.store(true, Ordering::SeqCst);

    let next = enrolled.step();
    assert!(
        enrolled.current().is_none(),
        "no identity before the first certificate"
    );
    assert!(next > NOW, "it waits before it tries again");

    // Inside the wait, a step does not call the head.
    assert_eq!(enrolled.step(), next);
    assert_eq!(head.enrollments.load(Ordering::SeqCst), 1);

    head.away.store(false, Ordering::SeqCst);
    clock.set(next);
    enrolled.step();
    assert!(enrolled.current().is_some(), "the identity arrived");
    assert_eq!(head.enrollments.load(Ordering::SeqCst), 2);
}

#[test]
fn a_certificate_is_renewed_at_two_thirds_of_its_life_and_not_before() {
    let (enrolled, head, clock) = scripted();
    let renew_at = enrolled.step();
    assert_eq!(renew_at, NOW + 2 * HOUR, "two thirds of three hours");

    clock.set(NOW + HOUR);
    assert_eq!(enrolled.step(), renew_at);
    assert_eq!(head.renewals.load(Ordering::SeqCst), 0);

    clock.set(renew_at);
    let next = enrolled.step();
    assert_eq!(head.renewals.load(Ordering::SeqCst), 1);
    assert_eq!(
        head.enrollments.load(Ordering::SeqCst),
        1,
        "a renewal is not a new enrollment"
    );
    assert_eq!(next, renew_at + 2 * HOUR);
    assert_eq!(enrolled.seconds_left(renew_at), Some(3 * 3600));
}

#[test]
fn a_certificate_that_lapsed_is_never_shown_and_the_process_enrolls_again() {
    let (enrolled, head, clock) = scripted();
    enrolled.step();
    let first_node = enrolled.node_id();

    // The head is away past the whole last third of the life.
    head.away.store(true, Ordering::SeqCst);
    clock.set(NOW + 2 * HOUR);
    enrolled.step();
    assert!(
        enrolled.current().is_some(),
        "still valid while renewal fails"
    );
    clock.set(NOW + 3 * HOUR + 1);
    assert!(
        enrolled.current().is_none(),
        "a lapsed certificate is not presented"
    );

    head.away.store(false, Ordering::SeqCst);
    clock.advance(10 * 60_000);
    enrolled.step();
    assert!(enrolled.current().is_some());
    assert_eq!(
        head.enrollments.load(Ordering::SeqCst),
        2,
        "it enrolled again rather than renewing"
    );
    assert_eq!(
        enrolled.node_id(),
        first_node,
        "the scripted head names its node the same"
    );
}

#[test]
fn a_revoked_token_stops_renewal_and_the_identity_ends_with_the_certificate() {
    let (enrolled, head, clock) = scripted();
    let metrics = Registry::new();
    enrolled.publish_to(Arc::clone(&metrics));
    enrolled.step();

    head.refusing.store(true, Ordering::SeqCst);
    clock.set(NOW + 2 * HOUR);
    enrolled.step();
    assert_eq!(head.renewals.load(Ordering::SeqCst), 1);

    // A refused renewal goes to enrollment next, which the revoked token also
    // fails, until the certificate lapses.
    for _ in 0..200 {
        let next = enrolled.step();
        clock.set(next.max(clock.now_ms() + 1));
        if clock.now_ms() > NOW + 4 * HOUR {
            break;
        }
    }
    assert_eq!(
        head.renewals.load(Ordering::SeqCst),
        1,
        "a refused renewal is not asked again"
    );
    assert!(head.enrollments.load(Ordering::SeqCst) > 1);
    assert!(
        enrolled.current().is_none(),
        "the identity ended with the certificate"
    );
    assert!(metrics.gauge_value(EXPIRY_GAUGE, &labels(&[])) < 0);
    assert!(metrics.counter_value(FAILURES_COUNTER, &labels(&[("reason", "refused")])) > 1);
}

#[test]
fn the_wait_between_failures_doubles_and_stops_at_five_minutes() {
    let (enrolled, head, clock) = scripted();
    head.away.store(true, Ordering::SeqCst);
    let mut waits = Vec::new();
    for _ in 0..12 {
        let now = clock.now_ms();
        let next = enrolled.step();
        waits.push(next - now);
        clock.set(next);
    }
    // Equal jitter with a fixed 0.5: three quarters of each ceiling.
    assert_eq!(&waits[..4], &[750, 1_500, 3_000, 6_000]);
    assert_eq!(*waits.last().expect("waits"), 5 * 60_000 * 3 / 4);
}

#[test]
fn a_head_names_itself_by_the_clusters_rule() {
    // Plaintext is allowed here only so the configuration needs no files.
    let config =
        config("transport:\n  allowPlaintext: true\nreplication:\n  listen: 10.0.0.5:5200\n");
    assert_eq!(
        head_node_name(&config),
        tallyowl_cluster::seed::name_for("10.0.0.5:5200")
    );
    let config = config_with(
        "transport:\n  allowPlaintext: true\nreplication:\n  listen: 0.0.0.0:5200\n  advertise: tallyowl-1.tallyowl-nodes.prod.svc:5200\n",
    );
    assert_eq!(
        head_node_name(&config),
        tallyowl_cluster::seed::name_for("tallyowl-1.tallyowl-nodes.prod.svc:5200")
    );
    assert_eq!(
        head_node_name(&config_with("node:\n  name: head-a\n")),
        "head-a"
    );
}

#[test]
fn a_heads_own_certificate_names_its_node_for_the_sender_check_and_for_tls() {
    // The cluster compares the common name with the sender, and a peer that
    // dials the head checks the DNS name. Both must be the node name, and the
    // recorded serial must be the one a listener reads.
    use x509_parser::prelude::*;
    let clock = FakeClock::at(NOW);
    let generated = generate_authorities(NOW).expect("authorities");
    let name = "node-10-0-0-5-5200".to_string();
    let enrolled = Enrolled::new(
        Box::new(LocalIssuer::new(
            authority(&generated, 24 * HOUR),
            name.clone(),
            Arc::clone(&clock) as Arc<dyn Clock>,
        )),
        EnrollmentTemplate {
            token: None,
            role: NodeRole::StorageProcess,
            cell: None,
            region: None,
            server_name: name.clone(),
        },
        Arc::clone(&clock) as Arc<dyn Clock>,
        Arc::new(Fixed(0.5)),
    );
    enrolled.step();
    let identity = enrolled.current().expect("issued");
    let leaf = &identity.chain[0];

    let peer = PeerIdentity::from_certificate(leaf).expect("readable");
    assert_eq!(peer.node_id, name, "the common name is the node name");
    assert_eq!(peer.role.as_deref(), Some("storage-process"));

    let (_, certificate) = X509Certificate::from_der(leaf).expect("parses");
    let names: Vec<String> = certificate
        .subject_alternative_name()
        .expect("readable")
        .expect("present")
        .value
        .general_names
        .iter()
        .map(|n| n.to_string())
        .collect();
    assert!(names.iter().any(|n| n.contains(&name)), "{names:?}");
    assert!(
        names.iter().any(|n| n.contains(HEAD_SERVER_NAME)),
        "{names:?}"
    );
    assert_eq!(
        identity.chain.len(),
        2,
        "the leaf and the intermediate, never the root"
    );
}

#[test]
fn a_process_that_crosses_no_network_needs_no_identity() {
    let clock = FakeClock::at(NOW) as Arc<dyn Clock>;
    let home = config_with("");
    assert!(for_collector(&home, Arc::clone(&clock))
        .expect("home")
        .is_none());
    assert!(for_head(&home, Arc::clone(&clock)).expect("home").is_none());
    let socket = config_with("head:\n  endpoint: unix:/run/tallyowl/head.sock\n");
    assert!(for_collector(&socket, clock).expect("unix").is_none());
}

#[test]
fn a_collector_on_a_network_starts_at_once_with_no_identity_until_a_certificate_arrives() {
    // Configuration check refuses a collector on a network with no authorities
    // or no role token, and names them. With both, the collector starts, and
    // its identity is empty until the head answers: it never blocks the start.
    let directory = scratch("collector-start");
    let generated = generate_authorities(tallyowl_obs::time::now_ms()).expect("authorities");
    let root = directory.join("root.crt");
    std::fs::write(&root, &generated.root_certificate_pem).expect("written");
    let token = directory.join("role.token");
    std::fs::write(&token, "towr_token\n").expect("written");
    let config = config_with(&format!(
        "head:\n  endpoint: head.invalid:5110\ninstallation:\n  authorities:\n    - {}\nenrollment:\n  roleToken: file:{}\n",
        root.display(),
        token.display()
    ));

    let (identity, trust, handle) =
        for_collector(&config, Arc::new(tallyowl_identity::SystemClock))
            .expect("the settings are complete")
            .expect("a network hop needs an identity");
    assert!(
        identity.current().is_none(),
        "no certificate yet, and nothing waited for one"
    );
    assert_eq!(trust.authorities(), ders(&generated.root_certificate_pem));
    assert!(
        handle.trust_reloader().is_some(),
        "a new authority file can be read without a restart"
    );
    drop(handle);
}

#[test]
fn a_collector_enrolls_and_renews_with_a_real_head_over_tls() {
    // The enrollment client, end to end: a server-authenticated call that
    // checks the head's name, then a renewal over mutual TLS.
    tallyowl_rpc::tls::install_crypto_provider();
    let clock = FakeClock::at(tallyowl_obs::time::now_ms());
    let generated = generate_authorities(clock.now_ms()).expect("authorities");
    let signer = Arc::new(
        Authority::from_pem(
            &generated.intermediate_chain_pem,
            &generated.intermediate_key_pem,
            &ders(&generated.root_certificate_pem),
            clock.now_ms(),
            3 * HOUR,
        )
        .expect("signer"),
    );
    let trust: Arc<dyn TrustSource> = Arc::new(StaticTrust(ders(&generated.root_certificate_pem)));

    // The head's own certificate, in the directory form a listener reads.
    let head_identity = Enrolled::new(
        Box::new(LocalIssuer::new(
            Arc::clone(&signer),
            "node-head".into(),
            Arc::clone(&clock) as Arc<dyn Clock>,
        )),
        EnrollmentTemplate {
            token: None,
            role: NodeRole::StorageProcess,
            cell: None,
            region: None,
            server_name: "node-head".into(),
        },
        Arc::clone(&clock) as Arc<dyn Clock>,
        Arc::new(Fixed(0.5)),
    );
    head_identity.step();
    let held = head_identity
        .current()
        .expect("the head holds a certificate");

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(TestHead {
        signer: LocalIssuer::new(
            signer,
            "node-collector-1".into(),
            Arc::clone(&clock) as Arc<dyn Clock>,
        ),
        seen: Arc::clone(&seen),
    });

    // Enrollment: the head shows its certificate, and the collector shows none.
    let directory = scratch("head-tls");
    std::fs::create_dir_all(&directory).expect("made");
    write_pair(&directory, &held);
    let certificates = Arc::new(
        tallyowl_rpc::material::CertificateSet::load(&[directory], clock.now_ms()).expect("loads"),
    );
    let enroll_server = tallyowl_rpc::tls::serve_server_auth(
        "127.0.0.1:0",
        Arc::clone(&dispatcher) as Arc<dyn Dispatcher>,
        ServerOptions::new(1024 * 1024),
        certificates,
    )
    .expect("listens");
    let collector = Enrolled::new(
        Box::new(
            RemoteIssuer::new(
                &enroll_server.local_address().to_string(),
                Arc::clone(&trust),
            )
            .expect("issuer"),
        ),
        collector_template(),
        Arc::clone(&clock) as Arc<dyn Clock>,
        Arc::new(Fixed(0.5)),
    );
    let renew_at = collector.step();
    assert!(collector.current().is_some(), "enrolled over TLS");
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        ["enroll from anonymous"]
    );

    // Renewal: both sides show a certificate.
    let renew_server = tallyowl_rpc::tls::serve_mutual(
        "127.0.0.1:0",
        dispatcher,
        ServerOptions::new(1024 * 1024),
        Arc::new(StaticIdentity(held)),
        trust,
    )
    .expect("listens");
    // The collector renews the certificate it enrolled with, now showing it.
    drop(enroll_server);
    let renewal = RemoteIssuer::new(
        &renew_server.local_address().to_string(),
        Arc::new(StaticTrust(ders(&generated.root_certificate_pem))),
    )
    .expect("issuer")
    .renew(
        RenewNodeCertificateRequest {
            node_id: collector.node_id().expect("enrolled"),
            certificate_request: fresh_request(),
        },
        Arc::clone(&collector) as Arc<dyn IdentitySource>,
    )
    .expect("renewed over mutual TLS");
    assert!(renewal.expires_at > 0);
    assert_eq!(
        seen.lock().expect("seen").last().map(String::as_str),
        Some("renew from node-collector-1"),
        "the head saw the collector's verified node ID"
    );
    assert!(renew_at > clock.now_ms());
}

#[test]
fn a_collector_that_does_not_trust_the_heads_authority_does_not_enroll() {
    tallyowl_rpc::tls::install_crypto_provider();
    let clock = FakeClock::at(tallyowl_obs::time::now_ms());
    let generated = generate_authorities(clock.now_ms()).expect("authorities");
    let signer = Arc::new(
        Authority::from_pem(
            &generated.intermediate_chain_pem,
            &generated.intermediate_key_pem,
            &ders(&generated.root_certificate_pem),
            clock.now_ms(),
            3 * HOUR,
        )
        .expect("signer"),
    );
    let head_identity = Enrolled::new(
        Box::new(LocalIssuer::new(
            Arc::clone(&signer),
            "node-head".into(),
            Arc::clone(&clock) as Arc<dyn Clock>,
        )),
        EnrollmentTemplate {
            token: None,
            role: NodeRole::StorageProcess,
            cell: None,
            region: None,
            server_name: "node-head".into(),
        },
        Arc::clone(&clock) as Arc<dyn Clock>,
        Arc::new(Fixed(0.5)),
    );
    head_identity.step();
    let directory = scratch("head-foreign");
    std::fs::create_dir_all(&directory).expect("made");
    write_pair(&directory, &head_identity.current().expect("held"));
    let server = tallyowl_rpc::tls::serve_server_auth(
        "127.0.0.1:0",
        Arc::new(TestHead {
            signer: LocalIssuer::new(
                signer,
                "node-collector-1".into(),
                Arc::clone(&clock) as Arc<dyn Clock>,
            ),
            seen: Arc::new(Mutex::new(Vec::new())),
        }),
        ServerOptions::new(1024 * 1024),
        Arc::new(
            tallyowl_rpc::material::CertificateSet::load(&[directory], clock.now_ms())
                .expect("loads"),
        ),
    )
    .expect("listens");

    let foreign = generate_authorities(clock.now_ms()).expect("another installation");
    let failure = RemoteIssuer::new(
        &server.local_address().to_string(),
        Arc::new(StaticTrust(ders(&foreign.root_certificate_pem))),
    )
    .expect("issuer")
    .enroll(EnrollNodeRequest {
        token: "towr_token".into(),
        certificate_request: fresh_request(),
        requested_role: NodeRole::CollectorForwarder,
        cell: None,
        region: None,
        node_id: None,
        capabilities: None,
    })
    .expect_err("an untrusted head gets no role token");
    assert!(!failure.retryable, "{failure:?}");
}

/// A head that signs what it is asked, and records who asked.
struct TestHead {
    signer: LocalIssuer,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Dispatcher for TestHead {
    fn dispatch(&self, request: &Request) -> Outcome {
        self.dispatch_from(request, &Peer::Local)
    }

    fn dispatch_from(&self, request: &Request, peer: &Peer) -> Outcome {
        let who = match peer {
            Peer::Verified(identity) => identity.node_id.clone(),
            Peer::Anonymous => "anonymous".into(),
            _ => "someone unproven".into(),
        };
        let answer = match request.op.as_str() {
            "enroll-node" => {
                self.seen
                    .lock()
                    .expect("seen")
                    .push(format!("enroll from {who}"));
                self.signer
                    .enroll(decode_enroll_node_request(&request.payload).expect("readable"))
            }
            "renew-node-certificate" => {
                self.seen
                    .lock()
                    .expect("seen")
                    .push(format!("renew from {who}"));
                if !matches!(peer, Peer::Verified(_)) {
                    Err(TallyOwlError::new(
                        ErrorCode::PermissionDenied,
                        "show a certificate",
                    ))
                } else {
                    self.signer.renew(
                        decode_renew_node_certificate_request(&request.payload).expect("readable"),
                        Arc::new(StaticIdentity(unused_identity())),
                    )
                }
            }
            other => panic!("the test head has no `{other}`"),
        };
        match answer {
            Ok(response) => {
                tallyowl_rpc::reply("EnrollNodeResponse", encode_enroll_node_response(&response))
            }
            Err(error) => tallyowl_rpc::error_outcome(encode_service_error(&ServiceError {
                code: tallyowl_control_api::types::ErrorCode::PermissionDenied,
                message: error.message,
                retryable: false,
                detail: None,
            })),
        }
    }
}

fn unused_identity() -> tallyowl_rpc::tls::Identity {
    tallyowl_rpc::tls::Identity {
        chain: Vec::new(),
        private_key: Vec::new(),
        authority: Vec::new(),
        expected_server_name: String::new(),
    }
}

fn fresh_request() -> Vec<u8> {
    let key = rcgen::KeyPair::generate().expect("key");
    rcgen::CertificateParams::default()
        .serialize_request(&key)
        .expect("request")
        .der()
        .to_vec()
}

/// Write an identity as the `tls.crt` and `tls.key` a listener reads.
fn write_pair(directory: &std::path::Path, identity: &tallyowl_rpc::tls::Identity) {
    let pem = |label: &str, der: &[u8]| {
        format!(
            "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
            base64_lines(der)
        )
    };
    let chain: String = identity
        .chain
        .iter()
        .map(|der| pem("CERTIFICATE", der))
        .collect();
    std::fs::write(directory.join("tls.crt"), chain).expect("written");
    std::fs::write(
        directory.join("tls.key"),
        pem("PRIVATE KEY", &identity.private_key),
    )
    .expect("written");
}

fn scratch(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("identity-crate-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a place to work");
    path
}

fn config(yaml: &str) -> Config {
    config_with(yaml)
}

fn config_with(yaml: &str) -> Config {
    let file: BTreeMap<String, String> = flatten_yaml(yaml).expect("YAML");
    Config::from_inputs(&Inputs {
        file,
        ..Inputs::default()
    })
    .unwrap_or_else(|errors| panic!("the configuration is valid: {errors:?}"))
}

/// Base64 in 64-character lines, for a PEM block a test writes itself.
fn base64_lines(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out.as_bytes()
        .chunks(64)
        .map(|line| std::str::from_utf8(line).expect("ascii"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_collector_enrolls_and_renews_on_the_one_port_the_head_serves() {
    // The production shape (D62): the head's ingest listener is mutual TLS and
    // also takes a client with no certificate. A collector with no identity
    // enrolls there as an anonymous peer, then renews on the same port as a
    // verified one.
    tallyowl_rpc::tls::install_crypto_provider();
    let clock = FakeClock::at(tallyowl_obs::time::now_ms());
    let generated = generate_authorities(clock.now_ms()).expect("authorities");
    let signer = Arc::new(
        Authority::from_pem(
            &generated.intermediate_chain_pem,
            &generated.intermediate_key_pem,
            &ders(&generated.root_certificate_pem),
            clock.now_ms(),
            3 * HOUR,
        )
        .expect("signer"),
    );
    let trust: Arc<dyn TrustSource> = Arc::new(StaticTrust(ders(&generated.root_certificate_pem)));
    let head_identity = Enrolled::new(
        Box::new(LocalIssuer::new(
            Arc::clone(&signer),
            "node-head".into(),
            Arc::clone(&clock) as Arc<dyn Clock>,
        )),
        EnrollmentTemplate {
            token: None,
            role: NodeRole::StorageProcess,
            cell: None,
            region: None,
            server_name: "node-head".into(),
        },
        Arc::clone(&clock) as Arc<dyn Clock>,
        Arc::new(Fixed(0.5)),
    );
    head_identity.step();
    let held = head_identity
        .current()
        .expect("the head holds a certificate");

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(TestHead {
        signer: LocalIssuer::new(
            signer,
            "node-collector-1".into(),
            Arc::clone(&clock) as Arc<dyn Clock>,
        ),
        seen: Arc::clone(&seen),
    });
    let head = tallyowl_rpc::tls::serve_mutual(
        "127.0.0.1:0",
        dispatcher as Arc<dyn Dispatcher>,
        ServerOptions::new(1024 * 1024).allow_anonymous(true),
        Arc::new(StaticIdentity(held)),
        Arc::clone(&trust),
    )
    .expect("listens");
    let address = head.local_address().to_string();

    let collector = Enrolled::new(
        Box::new(RemoteIssuer::new(&address, Arc::clone(&trust)).expect("issuer")),
        collector_template(),
        Arc::clone(&clock) as Arc<dyn Clock>,
        Arc::new(Fixed(0.5)),
    );
    collector.step();
    assert!(collector.current().is_some(), "enrolled on the mutual port");

    RemoteIssuer::new(&address, trust)
        .expect("issuer")
        .renew(
            RenewNodeCertificateRequest {
                node_id: collector.node_id().expect("enrolled"),
                certificate_request: fresh_request(),
            },
            Arc::clone(&collector) as Arc<dyn IdentitySource>,
        )
        .expect("renewed on the same port");
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        ["enroll from anonymous", "renew from node-collector-1"],
        "the same port saw an anonymous enrollment and a verified renewal"
    );
}

#[test]
fn a_failed_enrollment_says_why_in_the_log() {
    // The counter says how often. The line says why, which is what an operator
    // acts on.
    use tallyowl_obs::log::{CaptureSink, Logger, Severity};
    let (enrolled, head, _clock) = scripted();
    let captured = Arc::new(CaptureSink::default());
    enrolled.log_to(Arc::new(
        Logger::new("tallyowl-collector", "test", Severity::Info)
            .with_sink(Arc::clone(&captured) as Arc<dyn tallyowl_obs::log::Sink>),
    ));
    head.away.store(true, Ordering::SeqCst);

    enrolled.step();
    let lines = captured.lines();
    assert_eq!(lines.len(), 1, "one failed attempt, one line: {lines:?}");
    assert!(
        lines[0].contains("could not get a certificate"),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].contains("unreachable"),
        "the reason travels: {}",
        lines[0]
    );
}
