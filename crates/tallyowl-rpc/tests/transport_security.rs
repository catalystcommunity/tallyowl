//! D62: plaintext only where the host is trusted, server-authenticated TLS for
//! applications, mutual TLS between services, and certificates that change
//! while a service runs.
//!
//! Everything here uses real sockets and real certificates. A test about time
//! passes the time in; the only real clock is the one rustls checks a
//! certificate against, so every certificate that is presented in a handshake
//! is valid now.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
    SanType,
};
use tallyowl_obs::error::ErrorCode;
use tallyowl_rpc::material::{
    CertificateSet, IdentitySource, Reload, ReloadDriver, Reloadable, SwappableIdentity,
};
use tallyowl_rpc::tls::{install_crypto_provider, serve_mutual, serve_server_auth, Identity};
use tallyowl_rpc::trust::{FileTrust, StaticTrust, TrustSource};
use tallyowl_rpc::{
    reply, serve_with, Client, Dispatcher, Outcome, Peer, Pipeline, Request, ServerOptions,
};
use time::OffsetDateTime;

const MAX_FRAME: usize = 4 * 1024 * 1024;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn now_ms() -> i64 {
    tallyowl_rpc::material::now_ms()
}

fn at(ms: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(ms / 1000).expect("a time")
}

struct Authority {
    key: KeyPair,
    certificate: rcgen::Certificate,
}

impl Authority {
    fn new(name: &str) -> Authority {
        let mut params = CertificateParams::default();
        let mut subject = DistinguishedName::new();
        subject.push(DnType::CommonName, name);
        params.distinguished_name = subject;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params.not_before = at(now_ms() - 30 * DAY_MS);
        params.not_after = at(now_ms() + 3650 * DAY_MS);
        let key = KeyPair::generate().expect("a key");
        let certificate = params.self_signed(&key).expect("an authority");
        Authority { key, certificate }
    }

    fn der(&self) -> Vec<u8> {
        self.certificate.der().to_vec()
    }

    fn pem(&self) -> String {
        self.certificate.pem()
    }
}

/// One leaf this authority signed, as the pieces each test needs.
struct Leaf {
    identity: Identity,
    certificate_pem: String,
    key_pem: String,
}

fn leaf(
    authority: &Authority,
    name: &str,
    role: Option<&str>,
    not_before_ms: i64,
    not_after_ms: i64,
) -> Leaf {
    let key = KeyPair::generate().expect("a key");
    let mut params = CertificateParams::new(vec![name.to_string()]).expect("params");
    params
        .subject_alt_names
        .push(SanType::DnsName("localhost".try_into().expect("a name")));
    let mut subject = DistinguishedName::new();
    subject.push(DnType::CommonName, name);
    if let Some(role) = role {
        subject.push(DnType::OrganizationalUnitName, role);
    }
    params.distinguished_name = subject;
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.not_before = at(not_before_ms);
    params.not_after = at(not_after_ms);
    let certificate = params
        .signed_by(&key, &authority.certificate, &authority.key)
        .expect("the authority signs it");
    Leaf {
        identity: Identity {
            chain: vec![certificate.der().to_vec(), authority.der()],
            private_key: key.serialize_der(),
            authority: authority.der(),
            expected_server_name: name.to_string(),
        },
        certificate_pem: format!("{}{}", certificate.pem(), authority.pem()),
        key_pem: key.serialize_pem(),
    }
}

fn valid_leaf(authority: &Authority, name: &str, role: Option<&str>) -> Leaf {
    leaf(
        authority,
        name,
        role,
        now_ms() - DAY_MS,
        now_ms() + 30 * DAY_MS,
    )
}

/// A directory under the system temporary directory that is removed when the
/// test ends. These tests write private keys, and a key a test leaves behind is
/// still a key on the host.
struct Scratch(PathBuf);

impl std::ops::Deref for Scratch {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_directory(name: &str) -> Scratch {
    let directory = std::env::temp_dir().join(format!(
        "tallyowl-transport-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    Scratch(directory)
}

/// Write a pair as an operator would: one directory, tls.crt and tls.key.
fn write_pair(directory: &Path, leaf: &Leaf) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(directory.join("tls.crt"), &leaf.certificate_pem).unwrap();
    std::fs::write(directory.join("tls.key"), &leaf.key_pem).unwrap();
}

/// A service that records the peer each request arrived from.
#[derive(Default)]
struct Recorder {
    peers: Mutex<Vec<Peer>>,
}

impl Dispatcher for Recorder {
    fn dispatch(&self, request: &Request) -> Outcome {
        reply("EchoResponse", request.payload.clone())
    }

    fn dispatch_from(&self, request: &Request, peer: &Peer) -> Outcome {
        self.peers.lock().unwrap().push(peer.clone());
        self.dispatch(request)
    }
}

impl Recorder {
    fn last(&self) -> Peer {
        self.peers
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a request arrived")
    }
}

fn echo(client: &Client) -> Result<Vec<u8>, tallyowl_obs::error::TallyOwlError> {
    client
        .call("TallyOwlCollector", "echo", vec![7])
        .map(|response| response.payload)
}

/// Authorities that a test can change while a listener runs.
struct Changing(RwLock<Vec<Vec<u8>>>);

impl TrustSource for Changing {
    fn authorities(&self) -> Vec<Vec<u8>> {
        self.0.read().unwrap().clone()
    }
}

// ---------------------------------------------------------------- plaintext

#[test]
fn a_unix_socket_carries_plaintext_and_its_peer_is_local() {
    let directory = temp_directory("unix-plain");
    let address = format!("unix:{}", directory.join("intake.sock").display());
    let recorder = Arc::new(Recorder::default());
    let server = serve_with(
        &address,
        Arc::clone(&recorder) as Arc<dyn Dispatcher>,
        ServerOptions::new(MAX_FRAME),
    )
    .expect("it listens on a unix socket");
    assert_eq!(server.bound().to_string(), address);

    let client = Client::new(&address, MAX_FRAME);
    assert_eq!(echo(&client).unwrap(), vec![7]);
    assert_eq!(recorder.last(), Peer::Local);

    // The pipeline reaches it too.
    let mut pipeline = Pipeline::new(&address, MAX_FRAME, 2);
    let id = pipeline.send("TallyOwlCollector", "echo", vec![3]).unwrap();
    let (answered, response) = pipeline.recv().unwrap().expect("a reply");
    assert_eq!((answered, response.payload), (id, vec![3]));

    let path = directory.join("intake.sock");
    drop(server);
    assert!(!path.exists(), "a stopped listener removes its socket file");
}

#[test]
fn plaintext_on_loopback_is_local_and_anywhere_else_is_unverified() {
    let recorder = Arc::new(Recorder::default());
    let loopback = serve_with(
        "127.0.0.1:0",
        Arc::clone(&recorder) as Arc<dyn Dispatcher>,
        ServerOptions::new(MAX_FRAME),
    )
    .unwrap();
    echo(&Client::new(
        loopback.local_address().to_string(),
        MAX_FRAME,
    ))
    .unwrap();
    assert_eq!(recorder.last(), Peer::Local);

    // Bound on every interface, which only `transport.allowPlaintext` permits.
    // Nothing about the connection is proved.
    let everywhere = serve_with(
        "0.0.0.0:0",
        Arc::clone(&recorder) as Arc<dyn Dispatcher>,
        ServerOptions::new(MAX_FRAME),
    )
    .unwrap();
    let port = everywhere.local_address().port();
    echo(&Client::new(format!("127.0.0.1:{port}"), MAX_FRAME)).unwrap();
    assert_eq!(recorder.last(), Peer::Unverified);
}

#[test]
fn a_dispatcher_written_before_peers_existed_still_serves() {
    let server = serve_with(
        "127.0.0.1:0",
        Arc::new(|request: &Request| reply("EchoResponse", request.payload.clone()))
            as Arc<dyn Dispatcher>,
        ServerOptions::new(MAX_FRAME),
    )
    .unwrap();
    assert_eq!(
        echo(&Client::new(server.local_address().to_string(), MAX_FRAME)).unwrap(),
        vec![7]
    );
}

// ---------------------------------------------------------- server-auth TLS

fn server_auth_listener(
    address: &str,
    directories: &[PathBuf],
) -> (tallyowl_rpc::Server, Arc<CertificateSet>, Arc<Recorder>) {
    install_crypto_provider();
    let set = Arc::new(CertificateSet::load(directories, now_ms()).expect("a valid pair"));
    let recorder = Arc::new(Recorder::default());
    let server = serve_server_auth(
        address,
        Arc::clone(&recorder) as Arc<dyn Dispatcher>,
        ServerOptions::new(MAX_FRAME),
        Arc::clone(&set),
    )
    .expect("it listens");
    (server, set, recorder)
}

#[test]
fn an_application_reaches_the_collector_over_tls_and_shows_no_certificate() {
    let authority = Authority::new("operator authority");
    let directory = temp_directory("server-auth");
    write_pair(
        &directory,
        &valid_leaf(&authority, "collector.internal", None),
    );
    let (server, _, recorder) =
        server_auth_listener("127.0.0.1:0", std::slice::from_ref(&*directory));

    let client = Client::server_auth(
        server.local_address().to_string(),
        MAX_FRAME,
        "collector.internal",
        Some(vec![authority.der()]),
    )
    .unwrap();
    assert_eq!(echo(&client).unwrap(), vec![7]);
    assert_eq!(recorder.last(), Peer::Anonymous);
}

#[test]
fn server_auth_tls_works_over_a_unix_socket() {
    let authority = Authority::new("operator authority");
    let directory = temp_directory("server-auth-unix");
    write_pair(
        &directory,
        &valid_leaf(&authority, "collector.internal", None),
    );
    let address = format!("unix:{}", directory.join("intake.sock").display());
    let (_server, _, recorder) = server_auth_listener(&address, std::slice::from_ref(&directory));

    let client = Client::server_auth(
        &address,
        MAX_FRAME,
        "collector.internal",
        Some(vec![authority.der()]),
    )
    .unwrap();
    assert_eq!(echo(&client).unwrap(), vec![7]);
    assert_eq!(recorder.last(), Peer::Anonymous);
}

#[test]
fn a_collector_certificate_from_an_untrusted_authority_is_refused_for_good() {
    let operator = Authority::new("operator authority");
    let other = Authority::new("some other authority");
    let directory = temp_directory("untrusted");
    write_pair(
        &directory,
        &valid_leaf(&operator, "collector.internal", None),
    );
    let (server, _, recorder) =
        server_auth_listener("127.0.0.1:0", std::slice::from_ref(&*directory));

    let client = Client::server_auth(
        server.local_address().to_string(),
        MAX_FRAME,
        "collector.internal",
        Some(vec![other.der()]),
    )
    .unwrap();
    let failure = echo(&client).expect_err("an untrusted certificate is refused");
    assert!(!failure.retryable, "{}", failure.message);
    assert_eq!(failure.code, ErrorCode::FailedPrecondition);
    assert!(
        failure.message.contains("no trusted authority signed")
            && failure
                .message
                .contains("give this client that authority's certificate"),
        "{}",
        failure.message
    );
    assert!(recorder.peers.lock().unwrap().is_empty());
}

#[test]
fn a_certificate_for_another_name_is_refused_and_the_name_is_given() {
    let authority = Authority::new("operator authority");
    let directory = temp_directory("wrong-name");
    write_pair(
        &directory,
        &valid_leaf(&authority, "collector.internal", None),
    );
    let (server, _, _) = server_auth_listener("127.0.0.1:0", std::slice::from_ref(&*directory));

    let client = Client::server_auth(
        server.local_address().to_string(),
        MAX_FRAME,
        "intake.example.com",
        Some(vec![authority.der()]),
    )
    .unwrap();
    let failure = echo(&client).expect_err("a certificate for another name is refused");
    assert!(!failure.retryable);
    assert!(
        failure
            .message
            .contains("not for the name `intake.example.com`"),
        "{}",
        failure.message
    );
}

#[test]
fn a_tls_client_that_reaches_a_plaintext_listener_is_told_so() {
    let server = serve_with(
        "127.0.0.1:0",
        Arc::new(Recorder::default()) as Arc<dyn Dispatcher>,
        ServerOptions::new(MAX_FRAME),
    )
    .unwrap();
    let authority = Authority::new("operator authority");
    let client = Client::server_auth(
        server.local_address().to_string(),
        MAX_FRAME,
        "collector.internal",
        Some(vec![authority.der()]),
    )
    .unwrap();
    let failure = echo(&client).expect_err("plaintext is not TLS");
    // The listener closes the socket, which a restart also does, so the
    // failure stays retryable. What the operator must check is in the words.
    assert!(
        failure.message.contains("did not answer with TLS")
            || failure.message.contains("serves plaintext only"),
        "{}",
        failure.message
    );
}

#[test]
fn a_rotated_certificate_is_served_without_a_restart() {
    let old = Authority::new("old authority");
    let new = Authority::new("new authority");
    let directory = temp_directory("rotate");
    write_pair(&directory, &valid_leaf(&old, "collector.internal", None));
    let (server, set, _) = server_auth_listener("127.0.0.1:0", std::slice::from_ref(&directory));
    let address = server.local_address().to_string();
    let trusts_new = || {
        Client::server_auth(
            &address,
            MAX_FRAME,
            "collector.internal",
            Some(vec![new.der()]),
        )
        .unwrap()
    };
    assert!(
        echo(&trusts_new()).is_err(),
        "the new certificate is not served yet"
    );

    write_pair(&directory, &valid_leaf(&new, "collector.internal", None));
    assert_eq!(set.reload(now_ms()), Reload::Replaced);
    assert_eq!(
        echo(&trusts_new()).unwrap(),
        vec![7],
        "the next handshake shows the new certificate"
    );
}

#[test]
fn a_broken_file_keeps_the_certificate_in_use_and_says_why() {
    let authority = Authority::new("operator authority");
    let directory = temp_directory("broken");
    write_pair(
        &directory,
        &valid_leaf(&authority, "collector.internal", None),
    );
    let (server, set, _) = server_auth_listener("127.0.0.1:0", std::slice::from_ref(&directory));

    // cert-manager is part way through writing the new pair.
    std::fs::write(
        directory.join("tls.key"),
        "-----BEGIN PRIVATE KEY-----\ntruncated",
    )
    .unwrap();
    match set.reload(now_ms()) {
        Reload::Kept(reason) => assert!(reason.contains("tls.key"), "{reason}"),
        other => panic!("a broken file should keep the set in use, got {other:?}"),
    }
    let client = Client::server_auth(
        server.local_address().to_string(),
        MAX_FRAME,
        "collector.internal",
        Some(vec![authority.der()]),
    )
    .unwrap();
    assert_eq!(
        echo(&client).unwrap(),
        vec![7],
        "the old pair is still served"
    );
}

// ----------------------------------------------------------- the set itself

#[test]
fn the_set_presents_the_newest_pair_that_is_valid_now() {
    let authority = Authority::new("operator authority");
    let now = now_ms();
    let base = temp_directory("newest");
    let older = base.join("older");
    let newer = base.join("newer");
    let future = base.join("future");
    write_pair(
        &older,
        &leaf(
            &authority,
            "older",
            None,
            now - 20 * DAY_MS,
            now + 10 * DAY_MS,
        ),
    );
    write_pair(
        &newer,
        &leaf(
            &authority,
            "newer",
            None,
            now - 2 * DAY_MS,
            now + 60 * DAY_MS,
        ),
    );
    write_pair(
        &future,
        &leaf(
            &authority,
            "future",
            None,
            now + 5 * DAY_MS,
            now + 90 * DAY_MS,
        ),
    );

    let set = CertificateSet::load(&[older.clone(), newer.clone(), future.clone()], now).unwrap();
    assert_eq!(set.current_directory(), newer);
    // The gauge follows the last expiry, not the one in use.
    // Certificate times are whole seconds, and `now` is not.
    assert!((set.seconds_left(now) - 90 * DAY_MS / 1000).abs() <= 1);

    // Six days on, the future pair has started and is the newest valid one.
    // No file changed.
    assert_eq!(set.reload(now + 6 * DAY_MS), Reload::Replaced);
    assert_eq!(set.current_directory(), future);
    assert_eq!(set.reload(now + 6 * DAY_MS), Reload::Unchanged);
}

#[test]
fn a_set_with_no_valid_pair_is_refused_and_each_directory_is_named() {
    let authority = Authority::new("operator authority");
    let now = now_ms();
    let base = temp_directory("none-valid");
    let expired = base.join("expired");
    let early = base.join("early");
    write_pair(
        &expired,
        &leaf(&authority, "a", None, now - 20 * DAY_MS, now - DAY_MS),
    );
    write_pair(
        &early,
        &leaf(&authority, "b", None, now + DAY_MS, now + 20 * DAY_MS),
    );

    let refused = CertificateSet::load(&[expired, early], now).expect_err("nothing valid");
    assert!(
        refused.message.contains("has expired") && refused.message.contains("is not valid yet"),
        "{}",
        refused.message
    );
}

#[test]
fn a_key_that_is_not_the_certificates_key_is_refused() {
    let authority = Authority::new("operator authority");
    let directory = temp_directory("mismatch");
    let first = valid_leaf(&authority, "a", None);
    let second = valid_leaf(&authority, "b", None);
    std::fs::write(directory.join("tls.crt"), &first.certificate_pem).unwrap();
    std::fs::write(directory.join("tls.key"), &second.key_pem).unwrap();
    let refused =
        CertificateSet::load(std::slice::from_ref(&*directory), now_ms()).expect_err("a mismatch");
    assert!(
        refused
            .message
            .contains("is not the key for the certificate"),
        "{}",
        refused.message
    );
}

#[test]
fn a_set_with_no_directory_says_which_setting_to_write() {
    let refused = CertificateSet::load(&[], now_ms()).expect_err("nothing configured");
    assert!(
        refused.message.contains("tls.certificateDirectories"),
        "{}",
        refused.message
    );
}

#[test]
fn the_reload_driver_runs_on_its_interval_and_not_before() {
    struct Counting(Mutex<u32>);
    impl Reloadable for Counting {
        fn reload(&self, _now_ms: i64) -> Reload {
            *self.0.lock().unwrap() += 1;
            Reload::Unchanged
        }
    }
    let counting = Arc::new(Counting(Mutex::new(0)));
    let driver = ReloadDriver::new(
        vec![Arc::clone(&counting) as Arc<dyn Reloadable>],
        Duration::from_secs(30),
    );
    assert!(
        driver.tick(1_000).is_none(),
        "the first tick starts the interval"
    );
    assert!(driver.tick(30_999).is_none());
    assert_eq!(driver.tick(31_000), Some(vec![Reload::Unchanged]));
    assert!(driver.tick(40_000).is_none());
    assert!(driver.tick(61_000).is_some());
    assert_eq!(*counting.0.lock().unwrap(), 2);
}

// ---------------------------------------------------------------- mutual TLS

fn mutual_listener(
    address: &str,
    identity: Arc<dyn IdentitySource>,
    trust: Arc<dyn TrustSource>,
) -> (tallyowl_rpc::Server, Arc<Recorder>) {
    install_crypto_provider();
    let recorder = Arc::new(Recorder::default());
    let server = serve_mutual(
        address,
        Arc::clone(&recorder) as Arc<dyn Dispatcher>,
        ServerOptions::new(MAX_FRAME),
        identity,
        trust,
    )
    .expect("it listens");
    (server, recorder)
}

#[test]
fn a_verified_peer_names_its_node_role_serial_and_expiry() {
    let authority = Authority::new("installation authority");
    let head = valid_leaf(&authority, "node-head", Some("head"));
    let collector = valid_leaf(&authority, "node-collector-7", Some("collector-intake"));
    let trust: Arc<dyn TrustSource> = Arc::new(StaticTrust(vec![authority.der()]));
    let (server, recorder) = mutual_listener(
        "127.0.0.1:0",
        Arc::new(SwappableIdentity::new(Some(head.identity))),
        Arc::clone(&trust),
    );

    let client = Client::mutual(
        server.local_address().to_string(),
        MAX_FRAME,
        "node-head",
        Arc::new(SwappableIdentity::new(Some(collector.identity.clone()))),
        trust,
    );
    assert_eq!(echo(&client).unwrap(), vec![7]);
    let Peer::Verified(identity) = recorder.last() else {
        panic!("a mutual TLS peer is verified, got {:?}", recorder.last());
    };
    assert_eq!(identity.node_id, "node-collector-7");
    assert_eq!(identity.role.as_deref(), Some("collector-intake"));
    assert!(!identity.certificate_serial.is_empty());
    assert!(
        identity
            .certificate_serial
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "{}",
        identity.certificate_serial
    );
    assert!(identity.expires_at_ms > now_ms() + 29 * DAY_MS);
}

#[test]
fn mutual_tls_works_over_a_unix_socket() {
    let authority = Authority::new("installation authority");
    let head = valid_leaf(&authority, "node-head", None);
    let collector = valid_leaf(&authority, "node-collector", None);
    let trust: Arc<dyn TrustSource> = Arc::new(StaticTrust(vec![authority.der()]));
    let directory = temp_directory("mutual-unix");
    let address = format!("unix:{}", directory.join("head.sock").display());
    let (_server, recorder) = mutual_listener(
        &address,
        Arc::new(SwappableIdentity::new(Some(head.identity))),
        Arc::clone(&trust),
    );
    let client = Client::mutual(
        &address,
        MAX_FRAME,
        "node-head",
        Arc::new(SwappableIdentity::new(Some(collector.identity))),
        trust,
    );
    assert_eq!(echo(&client).unwrap(), vec![7]);
    assert!(matches!(recorder.last(), Peer::Verified(_)));
}

#[test]
fn a_peer_from_another_authority_is_refused_for_good() {
    let installation = Authority::new("installation authority");
    let stranger = Authority::new("stranger authority");
    let head = valid_leaf(&installation, "node-head", None);
    let intruder = valid_leaf(&stranger, "node-collector", None);
    let (server, recorder) = mutual_listener(
        "127.0.0.1:0",
        Arc::new(SwappableIdentity::new(Some(head.identity))),
        Arc::new(StaticTrust(vec![installation.der()])),
    );
    // The intruder trusts the installation authority, so it accepts the head.
    // The head does not accept the intruder.
    let client = Client::mutual(
        server.local_address().to_string(),
        MAX_FRAME,
        "node-head",
        Arc::new(SwappableIdentity::new(Some(intruder.identity))),
        Arc::new(StaticTrust(vec![installation.der()])),
    );
    let failure = echo(&client).expect_err("a stranger is refused");
    assert!(!failure.retryable, "{}", failure.message);
    assert!(
        failure
            .message
            .contains("refused this connection during the TLS handshake"),
        "{}",
        failure.message
    );
    assert!(recorder.peers.lock().unwrap().is_empty());
}

#[test]
fn a_node_with_no_identity_yet_waits_rather_than_failing_for_good() {
    let authority = Authority::new("installation authority");
    let head = valid_leaf(&authority, "node-head", None);
    let (server, _) = mutual_listener(
        "127.0.0.1:0",
        Arc::new(SwappableIdentity::new(Some(head.identity))),
        Arc::new(StaticTrust(vec![authority.der()])),
    );
    let client = Client::mutual(
        server.local_address().to_string(),
        MAX_FRAME,
        "node-head",
        Arc::new(SwappableIdentity::new(None)),
        Arc::new(StaticTrust(vec![authority.der()])),
    );
    let failure = echo(&client).expect_err("no identity yet");
    assert!(
        failure.retryable,
        "enrollment can still finish: {}",
        failure.message
    );
    assert!(
        failure.message.contains("no enrolled identity yet"),
        "{}",
        failure.message
    );
}

#[test]
fn a_renewed_identity_and_a_new_authority_are_used_without_a_restart() {
    let old = Authority::new("old installation authority");
    let new = Authority::new("new installation authority");
    let head_identity = Arc::new(SwappableIdentity::new(Some(
        valid_leaf(&old, "node-head", None).identity,
    )));
    let head_trust = Arc::new(Changing(RwLock::new(vec![old.der()])));
    let (server, recorder) = mutual_listener(
        "127.0.0.1:0",
        Arc::clone(&head_identity) as Arc<dyn IdentitySource>,
        Arc::clone(&head_trust) as Arc<dyn TrustSource>,
    );
    let address = server.local_address().to_string();

    // A collector that the new authority enrolled, and that trusts both.
    let collector = Client::mutual(
        &address,
        MAX_FRAME,
        "node-head",
        Arc::new(SwappableIdentity::new(Some(
            valid_leaf(&new, "node-collector", None).identity,
        ))),
        Arc::new(StaticTrust(vec![old.der(), new.der()])),
    );
    assert!(
        echo(&collector).is_err(),
        "the head does not trust the new authority yet"
    );

    // The operator adds the new authority beside the old one.
    head_trust.0.write().unwrap().push(new.der());
    assert_eq!(echo(&collector).unwrap(), vec![7]);

    // The head renews. The next connection carries the new certificate.
    let renewed = valid_leaf(&new, "node-head", None);
    head_identity.replace(renewed.identity);
    collector.disconnect();
    assert_eq!(echo(&collector).unwrap(), vec![7]);
    assert!(matches!(recorder.last(), Peer::Verified(_)));
}

#[test]
fn file_trust_reads_several_authorities_and_follows_a_changed_file() {
    let first = Authority::new("first");
    let second = Authority::new("second");
    let directory = temp_directory("trust");
    let one = directory.join("one.pem");
    let two = directory.join("two.pem");
    std::fs::write(&one, first.pem()).unwrap();
    std::fs::write(&two, second.pem()).unwrap();

    let trust = FileTrust::load(&[one.clone(), two.clone()]).unwrap();
    assert_eq!(trust.authorities(), vec![first.der(), second.der()]);
    assert_eq!(trust.reload(0), Reload::Unchanged);

    let third = Authority::new("third");
    std::fs::write(&two, format!("{}{}", second.pem(), third.pem())).unwrap();
    assert_eq!(trust.reload(0), Reload::Replaced);
    assert_eq!(trust.authorities().len(), 3);

    std::fs::remove_file(&one).unwrap();
    assert!(matches!(trust.reload(0), Reload::Kept(_)));
    assert_eq!(
        trust.authorities().len(),
        3,
        "an unreadable file keeps the set in use"
    );

    let refused = FileTrust::load(&[]).err().expect("nothing configured");
    assert!(
        refused.message.contains("installation.authorities"),
        "{}",
        refused.message
    );
}

#[test]
fn a_pipeline_reaches_a_server_auth_listener_and_keeps_calls_outstanding() {
    let authority = Authority::new("operator authority");
    let directory = temp_directory("pipeline-server-auth");
    write_pair(
        &directory,
        &valid_leaf(&authority, "collector.internal", None),
    );
    let (server, _, recorder) =
        server_auth_listener("127.0.0.1:0", std::slice::from_ref(&*directory));

    let mut pipeline = Pipeline::server_auth(
        server.local_address().to_string(),
        MAX_FRAME,
        4,
        "collector.internal",
        Some(vec![authority.der()]),
    )
    .unwrap();
    let first = pipeline.send("TallyOwlCollector", "echo", vec![1]).unwrap();
    let second = pipeline.send("TallyOwlCollector", "echo", vec![2]).unwrap();
    let mut answered = vec![
        pipeline.recv().unwrap().expect("a reply").0,
        pipeline.recv().unwrap().expect("a reply").0,
    ];
    answered.sort_unstable();
    assert_eq!(answered, vec![first, second]);
    assert_eq!(recorder.last(), Peer::Anonymous);
}

#[test]
fn a_pipeline_refused_by_a_mutual_listener_hears_why_and_stops_retrying() {
    // In TLS 1.3 the refusal arrives after the client finished its handshake,
    // on the first read. The pipeline still reports it as the refusal it is.
    let installation = Authority::new("installation authority");
    let stranger = Authority::new("stranger authority");
    let (server, _) = mutual_listener(
        "127.0.0.1:0",
        Arc::new(SwappableIdentity::new(Some(
            valid_leaf(&installation, "node-head", None).identity,
        ))),
        Arc::new(StaticTrust(vec![installation.der()])),
    );
    let mut pipeline = Pipeline::mutual(
        server.local_address().to_string(),
        MAX_FRAME,
        4,
        "node-head",
        Arc::new(SwappableIdentity::new(Some(
            valid_leaf(&stranger, "node-collector", None).identity,
        ))),
        Arc::new(StaticTrust(vec![installation.der()])),
    );
    // The send may succeed: the frame can leave before the alert arrives.
    let failure = match pipeline.send("TallyOwlCollector", "echo", vec![1]) {
        Err(failure) => failure,
        Ok(_) => pipeline
            .recv()
            .expect_err("the listener refused the client"),
    };
    assert!(!failure.retryable, "{}", failure.message);
    assert!(
        failure
            .message
            .contains("refused this connection during the TLS handshake"),
        "{}",
        failure.message
    );
}

/// A mutual listener that also takes a client with no certificate: how a node
/// with no identity yet reaches `enroll-node`, and how an operator's client
/// that proves itself with a session token reaches the control operations.
fn mutual_listener_allowing_anonymous(
    identity: Arc<dyn IdentitySource>,
    trust: Arc<dyn TrustSource>,
) -> (tallyowl_rpc::Server, Arc<Recorder>) {
    install_crypto_provider();
    let recorder = Arc::new(Recorder::default());
    let server = serve_mutual(
        "127.0.0.1:0",
        Arc::clone(&recorder) as Arc<dyn Dispatcher>,
        ServerOptions::new(MAX_FRAME).allow_anonymous(true),
        identity,
        trust,
    )
    .expect("it listens");
    (server, recorder)
}

#[test]
fn a_mutual_listener_that_allows_anonymous_takes_a_client_with_no_certificate() {
    let authority = Authority::new("installation authority");
    let head = valid_leaf(&authority, "node-head", Some("storage-process"));
    let (server, recorder) = mutual_listener_allowing_anonymous(
        Arc::new(SwappableIdentity::new(Some(head.identity))),
        Arc::new(StaticTrust(vec![authority.der()])),
    );

    // A collector before its first enrollment: it checks the head, and shows
    // nothing of its own.
    let newcomer = Client::server_auth(
        server.local_address().to_string(),
        MAX_FRAME,
        "node-head",
        Some(vec![authority.der()]),
    )
    .unwrap();
    assert_eq!(echo(&newcomer).unwrap(), vec![7]);
    assert_eq!(recorder.last(), Peer::Anonymous);
}

#[test]
fn allowing_anonymous_still_verifies_a_client_that_shows_a_certificate() {
    let installation = Authority::new("installation authority");
    let stranger = Authority::new("stranger authority");
    let head = valid_leaf(&installation, "node-head", None);
    let collector = valid_leaf(&installation, "node-collector-1", Some("collector-intake"));
    let intruder = valid_leaf(&stranger, "node-collector-1", Some("collector-intake"));
    let trust: Arc<dyn TrustSource> = Arc::new(StaticTrust(vec![installation.der()]));
    let (server, recorder) = mutual_listener_allowing_anonymous(
        Arc::new(SwappableIdentity::new(Some(head.identity))),
        Arc::clone(&trust),
    );

    let enrolled = Client::mutual(
        server.local_address().to_string(),
        MAX_FRAME,
        "node-head",
        Arc::new(SwappableIdentity::new(Some(collector.identity))),
        Arc::clone(&trust),
    );
    assert_eq!(echo(&enrolled).unwrap(), vec![7]);
    assert!(
        matches!(recorder.last(), Peer::Verified(ref identity) if identity.node_id == "node-collector-1"),
        "a certificate that verifies is a verified peer, not an anonymous one: {:?}",
        recorder.last()
    );

    // A forged certificate is not quietly treated as "no certificate".
    let before = recorder.peers.lock().unwrap().len();
    let forged = Client::mutual(
        server.local_address().to_string(),
        MAX_FRAME,
        "node-head",
        Arc::new(SwappableIdentity::new(Some(intruder.identity))),
        trust,
    );
    let failure = echo(&forged).expect_err("a certificate from a stranger is refused");
    assert!(!failure.retryable, "{}", failure.message);
    assert_eq!(recorder.peers.lock().unwrap().len(), before);
}

#[test]
fn a_mutual_listener_refuses_a_client_with_no_certificate_by_default() {
    let authority = Authority::new("installation authority");
    let head = valid_leaf(&authority, "node-head", None);
    let (server, recorder) = mutual_listener(
        "127.0.0.1:0",
        Arc::new(SwappableIdentity::new(Some(head.identity))),
        Arc::new(StaticTrust(vec![authority.der()])),
    );
    let newcomer = Client::server_auth(
        server.local_address().to_string(),
        MAX_FRAME,
        "node-head",
        Some(vec![authority.der()]),
    )
    .unwrap();
    assert!(echo(&newcomer).is_err());
    assert!(recorder.peers.lock().unwrap().is_empty());
}

#[test]
fn a_refused_handshake_is_counted_and_a_node_with_no_identity_is_not() {
    // The server counts after the client has already read the refusal, so the
    // test waits on the hook itself, with a bound, rather than on time.
    let installation = Authority::new("installation authority");
    let stranger = Authority::new("stranger authority");
    let head = valid_leaf(&installation, "node-head", None);
    let intruder = valid_leaf(&stranger, "node-collector", None);
    let (heard, hear) = std::sync::mpsc::channel::<()>();
    let heard = Mutex::new(heard);
    install_crypto_provider();
    let server = serve_mutual(
        "127.0.0.1:0",
        Arc::new(Recorder::default()) as Arc<dyn Dispatcher>,
        ServerOptions::new(MAX_FRAME).on_handshake_refused(Arc::new(move || {
            let _ = heard.lock().unwrap().send(());
        })),
        Arc::new(SwappableIdentity::new(Some(head.identity))),
        Arc::new(StaticTrust(vec![installation.der()])),
    )
    .expect("it listens");

    let forged = Client::mutual(
        server.local_address().to_string(),
        MAX_FRAME,
        "node-head",
        Arc::new(SwappableIdentity::new(Some(intruder.identity))),
        Arc::new(StaticTrust(vec![installation.der()])),
    );
    assert!(echo(&forged).is_err());
    hear.recv_timeout(std::time::Duration::from_secs(5))
        .expect("the refused handshake reached the hook");
    assert_eq!(server.stats().handshakes_refused(), 1);

    // A node that has no certificate of its own yet drops the connection, and
    // the peer did nothing wrong, so it is not counted as a refusal.
    let (waiting, _) = mutual_listener(
        "127.0.0.1:0",
        Arc::new(SwappableIdentity::new(None)),
        Arc::new(StaticTrust(vec![installation.der()])),
    );
    let collector = valid_leaf(&installation, "node-collector-2", None);
    let early = Client::mutual(
        waiting.local_address().to_string(),
        MAX_FRAME,
        "node-head",
        Arc::new(SwappableIdentity::new(Some(collector.identity))),
        Arc::new(StaticTrust(vec![installation.der()])),
    );
    assert!(echo(&early).is_err());
    assert_eq!(waiting.stats().handshakes_refused(), 0);
}
