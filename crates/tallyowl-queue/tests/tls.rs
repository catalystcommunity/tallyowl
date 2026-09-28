//! The hop to the durable store over TLS (D62).
//!
//! Corndogs speaks CSIL-RPC, the same framing `tallyowl-rpc` serves, so the
//! durable store here is a `tallyowl-rpc` listener with server-only TLS that
//! answers the one operation the test calls. The certificates are made here and
//! removed when the test ends.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use corndogs::{
    encode_get_queue_and_state_counts_response, GetQueueAndStateCountsResponse, QueueAndStateCounts,
};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
use tallyowl_queue::{CorndogsQueue, DurableQueue, QueueOptions, QueueTls};
use tallyowl_rpc::material::CertificateSet;
use tallyowl_rpc::{Dispatcher, Request, ServerOptions};

/// A directory removed when the test ends. It holds a private key.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(name: &str) -> Scratch {
    let path = std::env::temp_dir().join(format!(
        "tallyowl-queue-{name}-{}-{}",
        std::process::id(),
        tallyowl_obs::time::now_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    Scratch(path)
}

/// An authority, and a store certificate for `corndogs.test` that it signed,
/// written as a listener reads them. Returns the authority's PEM file.
fn store_certificates(directory: &Scratch) -> PathBuf {
    let mut authority = CertificateParams::default();
    authority
        .distinguished_name
        .push(DnType::CommonName, "installation authority");
    authority.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let authority_key = KeyPair::generate().unwrap();
    let authority = authority.self_signed(&authority_key).unwrap();

    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["corndogs.test".to_string()])
        .unwrap()
        .signed_by(&key, &authority, &authority_key)
        .unwrap();
    let pair = directory.0.join("store");
    std::fs::create_dir_all(&pair).unwrap();
    std::fs::write(pair.join("tls.crt"), leaf.pem()).unwrap();
    std::fs::write(pair.join("tls.key"), key.serialize_pem()).unwrap();
    let ca = directory.0.join("ca.crt");
    std::fs::write(&ca, authority.pem()).unwrap();
    ca
}

/// A durable store on server-only TLS that answers one count.
fn store(directory: &Scratch) -> tallyowl_rpc::Server {
    tallyowl_rpc::tls::install_crypto_provider();
    let certificates = Arc::new(
        CertificateSet::load(
            &[directory.0.join("store")],
            tallyowl_rpc::material::now_ms(),
        )
        .unwrap(),
    );
    let answer = Arc::new(|request: &Request| {
        assert_eq!(request.op, "GetQueueAndStateCounts");
        let mut held = HashMap::new();
        held.insert(
            "delivery".to_string(),
            QueueAndStateCounts {
                queue: "delivery".to_string(),
                count: 3,
                state_counts: HashMap::from([("queued".to_string(), 3)]),
            },
        );
        tallyowl_rpc::reply(
            "GetQueueAndStateCountsResponse",
            encode_get_queue_and_state_counts_response(&GetQueueAndStateCountsResponse {
                queue_and_state_counts: held,
            }),
        )
    }) as Arc<dyn Dispatcher>;
    tallyowl_rpc::tls::serve_server_auth(
        "127.0.0.1:0",
        answer,
        ServerOptions::new(1024 * 1024),
        certificates,
    )
    .unwrap()
}

fn options(tls: QueueTls) -> QueueOptions {
    QueueOptions {
        connections: 1,
        call_timeout: Duration::from_secs(5),
        metrics: None,
        tls: Some(tls),
    }
}

#[test]
fn a_queue_reaches_a_durable_store_over_tls_and_checks_its_name() {
    let directory = scratch("tls");
    let ca = store_certificates(&directory);
    let server = store(&directory);

    let queue = CorndogsQueue::connect_with(
        &server.local_address().to_string(),
        options(QueueTls {
            ca_file: Some(ca),
            server_name: Some("corndogs.test".to_string()),
        }),
    )
    .expect("the store proves its name");
    assert!(queue.secured());
    let counts = queue.counts().expect("a count over TLS");
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].in_state("queued"), 3);
}

#[test]
fn a_durable_store_signed_by_an_authority_the_queue_does_not_trust_is_refused() {
    let directory = scratch("untrusted");
    store_certificates(&directory);
    let server = store(&directory);
    // Another authority entirely.
    let other = scratch("other");
    let other_ca = store_certificates(&other);

    let refused = CorndogsQueue::connect_with(
        &server.local_address().to_string(),
        options(QueueTls {
            ca_file: Some(other_ca),
            server_name: Some("corndogs.test".to_string()),
        }),
    );
    let Err(refused) = refused else {
        panic!("a store the queue cannot verify was accepted");
    };
    assert!(refused.message.contains("over TLS"), "{}", refused.message);
    // RUNBOOK_INCIDENT.md section 8.5 tells an operator to look for this.
    assert!(
        refused.message.contains("invalid peer certificate"),
        "{}",
        refused.message
    );
}

#[test]
fn a_durable_store_that_shows_another_name_is_refused() {
    let directory = scratch("name");
    let ca = store_certificates(&directory);
    let server = store(&directory);
    let refused = CorndogsQueue::connect_with(
        &server.local_address().to_string(),
        options(QueueTls {
            ca_file: Some(ca),
            server_name: Some("somebody-else.test".to_string()),
        }),
    );
    let Err(refused) = refused else {
        panic!("a certificate for another name was accepted");
    };
    // RUNBOOK_INCIDENT.md section 8.5 tells an operator to look for this.
    assert!(
        refused.message.contains("not valid for name"),
        "{}",
        refused.message
    );
}

#[test]
fn a_durable_store_that_serves_plaintext_is_refused_and_says_so() {
    // A Corndogs with no certificate, reached by a service that expects TLS.
    let answer =
        Arc::new(|_: &Request| tallyowl_rpc::reply("x", Vec::new())) as Arc<dyn Dispatcher>;
    let server =
        tallyowl_rpc::serve_with("127.0.0.1:0", answer, ServerOptions::new(1024 * 1024)).unwrap();
    let directory = scratch("plain");
    let ca = store_certificates(&directory);
    let refused = CorndogsQueue::connect_with(
        &server.local_address().to_string(),
        options(QueueTls {
            ca_file: Some(ca),
            server_name: Some("corndogs.test".to_string()),
        }),
    );
    let Err(refused) = refused else {
        panic!("a plaintext store was accepted as TLS");
    };
    assert!(refused.message.contains("over TLS"), "{}", refused.message);
    // RUNBOOK_INCIDENT.md section 8.5 tells an operator to look for this.
    assert!(
        refused.message.contains("tls handshake"),
        "{}",
        refused.message
    );
}
