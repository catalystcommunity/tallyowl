//! Mutual TLS, with certificates a real authority signed.
//!
//! The authority and the leaf certificates here are built by `rcgen` the same
//! way `tallyowl-store` builds them, because this crate cannot depend on the
//! store: the store depends on nothing above it and the head wires the two
//! together. What is under test is the carrier and the handshake, and both are
//! the real ones.
//!
//! The properties that matter:
//!
//! - a peer with an enrolled identity connects and gets typed replies;
//! - a peer with no client certificate is refused by the handshake, so it never
//!   reaches a decoder;
//! - a peer whose certificate a different authority signed is refused;
//! - the framing, the envelopes, and the codecs are unchanged under TLS.

use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
};
use tallyowl_rpc::tls::{install_crypto_provider, peer_node_id, serve_tls, Identity, TlsClient};
use tallyowl_rpc::{reply, unknown_operation, Dispatcher, Request};

const MAX_FRAME: usize = 4 * 1024 * 1024;

/// One installation authority and the leaves it signs.
struct Authority {
    key: KeyPair,
    certificate: rcgen::Certificate,
    der: Vec<u8>,
}

fn authority() -> Authority {
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "TallyOwl installation authority");
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let key = KeyPair::generate().expect("a key");
    let certificate = params.self_signed(&key).expect("an authority");
    let der = certificate.der().to_vec();
    Authority {
        key,
        certificate,
        der,
    }
}

/// One enrolled node: it generates its own key and the authority signs it.
fn enrolled(authority: &Authority, node_id: &str) -> Identity {
    let key = KeyPair::generate().expect("the node generates its own key");
    let mut params = CertificateParams::new(vec![node_id.to_string()]).expect("params");
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, node_id);
    params.distinguished_name = name;
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    let leaf = params
        .signed_by(&key, &authority.certificate, &authority.key)
        .expect("the authority signs it");

    Identity {
        chain: vec![leaf.der().to_vec(), authority.der.clone()],
        private_key: key.serialize_der(),
        authority: authority.der.clone(),
        expected_server_name: node_id.to_string(),
    }
}

fn echo_service() -> Arc<dyn Dispatcher> {
    Arc::new(|request: &Request| match request.op.as_str() {
        "echo" => reply("EchoResponse", request.payload.clone()),
        other => unknown_operation(&request.service, other),
    }) as Arc<dyn Dispatcher>
}

#[test]
fn an_enrolled_peer_reaches_the_service_and_gets_a_typed_reply() {
    install_crypto_provider();
    let authority = authority();
    let head = enrolled(&authority, "node-head");
    let collector = enrolled(&authority, "node-collector");

    let server = serve_tls("127.0.0.1:0", echo_service(), MAX_FRAME, &head).expect("it listens");

    // The client verifies the server against the name its certificate carries.
    let mut identity = collector.clone();
    identity.expected_server_name = "node-head".into();
    let client =
        TlsClient::new(server.local_address().to_string(), MAX_FRAME, identity).expect("a client");

    let response = client
        .call("TallyOwlCollector", "echo", vec![1, 2, 3])
        .expect("the call goes through");
    assert_eq!(response.payload, vec![1, 2, 3]);
    assert_eq!(response.variant.as_deref(), Some("EchoResponse"));

    // The framing and the envelopes are unchanged, so a second call on the same
    // session works exactly as it does without TLS.
    let response = client
        .call("TallyOwlCollector", "echo", vec![9])
        .expect("the second call goes through");
    assert_eq!(response.payload, vec![9]);
}

#[test]
fn a_peer_with_no_certificate_never_reaches_the_service() {
    // This is what "mutual" means. A plain TCP client speaks no TLS at all, so
    // the handshake fails and no frame is ever decoded.
    install_crypto_provider();
    let authority = authority();
    let head = enrolled(&authority, "node-head");

    let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&served);
    let server = serve_tls(
        "127.0.0.1:0",
        Arc::new(move |request: &Request| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            reply("EchoResponse", request.payload.clone())
        }) as Arc<dyn Dispatcher>,
        MAX_FRAME,
        &head,
    )
    .expect("it listens");

    let plain = tallyowl_rpc::Client::new(server.local_address().to_string(), MAX_FRAME);
    let failure = plain
        .call("TallyOwlCollector", "echo", vec![1])
        .expect_err("a plain client cannot speak to a TLS listener");
    assert_eq!(failure.code, tallyowl_obs::error::ErrorCode::Unavailable);
    assert_eq!(
        served.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a request reached the service without an identity"
    );
}

#[test]
fn a_certificate_from_another_authority_is_refused() {
    // A node enrolled somewhere else holds a perfectly valid certificate. It is
    // not valid here, and the handshake is where that is decided.
    install_crypto_provider();
    let ours = authority();
    let theirs = authority();
    let head = enrolled(&ours, "node-head");
    let stranger = enrolled(&theirs, "node-stranger");

    let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&served);
    let server = serve_tls(
        "127.0.0.1:0",
        Arc::new(move |request: &Request| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            reply("EchoResponse", request.payload.clone())
        }) as Arc<dyn Dispatcher>,
        MAX_FRAME,
        &head,
    )
    .expect("it listens");

    // The stranger trusts our authority for the server, so it gets past its own
    // verification and is refused by ours.
    let mut identity = stranger.clone();
    identity.authority = ours.der.clone();
    identity.expected_server_name = "node-head".into();
    let client =
        TlsClient::new(server.local_address().to_string(), MAX_FRAME, identity).expect("a client");

    assert!(
        client.call("TallyOwlCollector", "echo", vec![1]).is_err(),
        "a certificate from another authority was accepted"
    );
    assert_eq!(
        served.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a request reached the service on a refused identity"
    );
}

#[test]
fn a_client_that_expects_a_different_peer_refuses_the_server() {
    // The other direction. A client that reaches an address it did not mean to
    // reach must not proceed just because the certificate is well signed.
    install_crypto_provider();
    let authority = authority();
    let head = enrolled(&authority, "node-head");
    let collector = enrolled(&authority, "node-collector");

    let server = serve_tls("127.0.0.1:0", echo_service(), MAX_FRAME, &head).expect("it listens");

    let mut identity = collector.clone();
    identity.expected_server_name = "node-somebody-else".into();
    let client =
        TlsClient::new(server.local_address().to_string(), MAX_FRAME, identity).expect("a client");
    assert!(
        client.call("TallyOwlCollector", "echo", vec![1]).is_err(),
        "the client accepted a peer it was not looking for"
    );
}

#[test]
fn a_certificate_names_the_node_a_service_can_read_from_it() {
    // A service that wants to know who it is talking to reads the identity the
    // control plane assigned, not one the peer chose.
    let authority = authority();
    let node = enrolled(&authority, "node-0007");
    assert_eq!(peer_node_id(&node.chain[0]).as_deref(), Some("node-0007"));
    assert_eq!(peer_node_id(b"not a certificate"), None);
}

#[test]
fn a_secured_connection_answers_a_fast_call_while_a_slow_one_is_still_running() {
    // This is the L058 defect, stated as a test. A TLS connection used to serve
    // one request at a time, so the fast call waited for the slow one and a
    // measurement of the replicated write path would have measured the carrier.
    use std::time::{Duration, Instant};

    install_crypto_provider();
    let authority = authority();
    let head = enrolled(&authority, "node-head");
    let peer = enrolled(&authority, "node-peer");

    let server = serve_tls(
        "127.0.0.1:0",
        Arc::new(|request: &Request| {
            if request.payload == b"slow".to_vec() {
                std::thread::sleep(Duration::from_millis(300));
            }
            reply("EchoResponse", request.payload.clone())
        }) as Arc<dyn Dispatcher>,
        MAX_FRAME,
        &head,
    )
    .expect("it listens");

    let mut identity = peer.clone();
    identity.expected_server_name = "node-head".into();
    let mut pipeline =
        tallyowl_rpc::Pipeline::secure(server.local_address().to_string(), MAX_FRAME, 4, identity)
            .expect("a pipeline");

    let slow = pipeline.send("S", "echo", b"slow".to_vec()).expect("send");
    let fast = pipeline.send("S", "echo", b"f".to_vec()).expect("send");

    let started = Instant::now();
    let (first, _) = pipeline.recv().expect("recv").expect("a reply");
    assert_eq!(first, fast, "the fast call is answered first");
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "the fast call waited behind the slow one: {:?}",
        started.elapsed()
    );

    let (second, _) = pipeline.recv().expect("recv").expect("a reply");
    assert_eq!(second, slow);
}

#[test]
fn many_calls_at_once_on_one_secured_session_each_get_their_own_reply() {
    // Two directions of one TLS session run at the same time here. If a record
    // reached the socket out of order, or a plaintext read raced a write, the
    // peer would close the session rather than answer. Every reply arriving,
    // and arriving with the right payload, is what proves the split is sound.
    install_crypto_provider();
    let authority = authority();
    let head = enrolled(&authority, "node-head");
    let peer = enrolled(&authority, "node-peer");

    let server = serve_tls("127.0.0.1:0", echo_service(), MAX_FRAME, &head).expect("it listens");

    let mut identity = peer.clone();
    identity.expected_server_name = "node-head".into();
    let mut pipeline =
        tallyowl_rpc::Pipeline::secure(server.local_address().to_string(), MAX_FRAME, 16, identity)
            .expect("a pipeline");

    // Payloads big enough to span several TLS records, so record ordering is
    // actually exercised rather than fitting in one each time.
    let mut expected = std::collections::HashMap::new();
    for n in 0..16u8 {
        let payload = vec![n; 40 * 1024];
        let id = pipeline.send("S", "echo", payload.clone()).expect("send");
        expected.insert(id, payload);
    }
    while let Some((id, response)) = pipeline.recv().expect("recv") {
        let want = expected.remove(&id).expect("a reply for a call we made");
        assert_eq!(
            response.payload, want,
            "reply {id} carried another call's body"
        );
    }
    assert!(expected.is_empty(), "every call was answered");
}
