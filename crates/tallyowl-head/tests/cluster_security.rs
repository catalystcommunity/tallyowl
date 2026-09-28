//! D62 at the start of a cluster node: a replication listener that would carry
//! every row in plaintext across a network is refused before it opens.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tallyowl_config::{Config, Inputs};
use tallyowl_obs::log::{Logger, Severity};
use tallyowl_store::SegmentedStore;

fn place(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let place = base
        .join("cluster-security-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&place);
    place
}

fn inputs(listen: &str, data: &std::path::Path) -> Inputs {
    let mut file = BTreeMap::new();
    file.insert("replication.listen".to_string(), listen.to_string());
    file.insert("head.dataDir".to_string(), data.display().to_string());
    Inputs {
        file,
        environment: BTreeMap::new(),
        arguments: Vec::new(),
    }
}

/// The first layer: `config check` refuses the configuration and names what
/// to set. A head never starts with it.
#[test]
fn a_plaintext_replication_listener_on_a_network_address_is_refused_by_config_check() {
    let data = place("refused");
    let Err(errors) = Config::from_inputs(&inputs("10.99.0.1:5200", &data)) else {
        panic!("a plaintext replication listener on a network address was accepted");
    };
    let message = errors
        .iter()
        .map(|e| e.message.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        message.contains("`replication.listen`") && message.contains("loopback"),
        "the refusal does not name the setting and the fix: {message}"
    );
}

/// The second layer: a caller that reaches `cluster::start` with a
/// configuration check allowed but no certificate to use is refused before a
/// socket opens. The operator allowed plaintext in the configuration, and the
/// security this process built says otherwise, so the stricter one wins.
#[test]
fn start_refuses_a_plaintext_listener_the_security_it_was_given_does_not_allow() {
    let data = place("start");
    let mut given = inputs("10.99.0.1:5200", &data);
    given
        .file
        .insert("transport.allowPlaintext".to_string(), "true".to_string());
    let config = Config::from_inputs(&given).unwrap_or_else(|errors| {
        panic!(
            "the configuration was refused: {:?}",
            errors.iter().map(|e| e.message.clone()).collect::<Vec<_>>()
        )
    });
    let store = Arc::new(SegmentedStore::open(data.join("store")).expect("the store opens"));
    let logger = Logger::new("tallyowl-head", "0.0.0", Severity::Error);

    let Err(refused) = tallyowl_head::cluster::start(
        &config,
        store,
        &logger,
        tallyowl_head::cluster::Security::default(),
    ) else {
        panic!("a plaintext replication listener on a network address started");
    };
    let message = refused.to_string();
    assert!(
        message.contains("`replication.listen`") && message.contains("loopback"),
        "the refusal does not name the setting and the fix: {message}"
    );
}
