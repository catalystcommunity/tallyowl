//! How this node's peers reach each other, and what a peer must prove.
//!
//! D62: node-to-node traffic is mutual TLS with enrolled certificates. A node
//! proves itself with its own certificate and checks a peer's against the
//! installation's authorities. Plaintext is legal only on a loopback or unix
//! address, or when an operator set `transport.allowPlaintext`.
//!
//! Every client this crate opens to a peer is made here, so one setting decides
//! the transport for consensus, forwarded proposals, fan-out reads, and segment
//! copies alike.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tallyowl_obs::error::TallyOwlError;
use tallyowl_rpc::material::IdentitySource;
use tallyowl_rpc::trust::TrustSource;
use tallyowl_rpc::{Address, Client, Peer};

/// The transport one node uses to its peers.
#[derive(Clone, Default)]
pub struct PeerSecurity {
    mutual: Option<(Arc<dyn IdentitySource>, Arc<dyn TrustSource>)>,
    allow_plaintext: bool,
    /// Peer node names by the address they are dialled at. A peer's certificate
    /// carries its node name, so the name is what a client verifies against.
    names: Arc<RwLock<HashMap<String, String>>>,
}

impl PeerSecurity {
    /// No TLS. Legal only where [`PeerSecurity::check_listen`] says so.
    pub fn plaintext(allow_plaintext: bool) -> PeerSecurity {
        PeerSecurity {
            allow_plaintext,
            ..PeerSecurity::default()
        }
    }

    /// Mutual TLS with this node's identity, trusting these authorities.
    pub fn mutual(
        identity: Arc<dyn IdentitySource>,
        trust: Arc<dyn TrustSource>,
        allow_plaintext: bool,
    ) -> PeerSecurity {
        PeerSecurity {
            mutual: Some((identity, trust)),
            allow_plaintext,
            names: Arc::default(),
        }
    }

    pub fn is_mutual(&self) -> bool {
        self.mutual.is_some()
    }

    pub fn identity(&self) -> Option<Arc<dyn IdentitySource>> {
        self.mutual
            .as_ref()
            .map(|(identity, _)| Arc::clone(identity))
    }

    pub fn trust(&self) -> Option<Arc<dyn TrustSource>> {
        self.mutual.as_ref().map(|(_, trust)| Arc::clone(trust))
    }

    /// Whether a plaintext peer on a network address is accepted. Only an
    /// operator who set `transport.allowPlaintext` gets one.
    pub fn allows_plaintext(&self) -> bool {
        self.allow_plaintext
    }

    /// Keep the peer names another setting already learned.
    pub fn adopt_names(&self, from: &PeerSecurity) {
        if Arc::ptr_eq(&self.names, &from.names) {
            return;
        }
        let learned = from.names.read().expect("peer names").clone();
        self.names.write().expect("peer names").extend(learned);
    }

    /// Learn which node answers at one address.
    pub fn remember(&self, address: &str, node: &str) {
        self.names
            .write()
            .expect("peer names")
            .insert(address.to_string(), node.to_string());
    }

    /// The name a client verifies the peer at `address` against: its node name
    /// when this node knows it, and otherwise the host in the address.
    pub fn server_name(&self, address: &str) -> String {
        if let Some(name) = self.names.read().expect("peer names").get(address) {
            return name.clone();
        }
        match Address::parse(address) {
            Ok(parsed) => parsed.host().unwrap_or("localhost").to_string(),
            Err(_) => address.to_string(),
        }
    }

    /// A client to one peer, with the connect and I/O deadlines the caller
    /// chose for its kind of call.
    pub fn client(
        &self,
        address: &str,
        max_frame_bytes: usize,
        connect_timeout: Duration,
        io_timeout: Duration,
    ) -> Client {
        let client = match &self.mutual {
            Some((identity, trust)) => Client::mutual(
                address.to_string(),
                max_frame_bytes,
                &self.server_name(address),
                Arc::clone(identity),
                Arc::clone(trust),
            ),
            None => Client::new(address.to_string(), max_frame_bytes),
        };
        client
            .with_connect_timeout(connect_timeout)
            .with_io_timeout(io_timeout)
    }

    /// Refuse a plaintext replication listener where D62 does not permit one.
    pub fn check_listen(&self, listen: &str) -> Result<(), TallyOwlError> {
        if self.is_mutual() || self.allow_plaintext {
            return Ok(());
        }
        let address = Address::parse(listen)?;
        if address.plaintext_permitted() {
            return Ok(());
        }
        Err(TallyOwlError::invalid_argument(format!(
            "`replication.listen` is {listen}, which is not a loopback or unix address, and this node has no certificate for it. Give the node `installation.authorities` and an enrolled identity, bind replication to a loopback or `unix:` address, or set `transport.allowPlaintext: true` if something else protects this network."
        )))
    }

    /// Whether a peer may speak to this node's replication service at all.
    ///
    /// A mutual TLS listener only ever produces `Verified`, so this matters on
    /// a plaintext listener: `Local` is a loopback or unix peer, and
    /// `Unverified` is a network peer the operator allowed by setting.
    pub fn admits(&self, peer: &Peer) -> Result<(), String> {
        match peer {
            Peer::Verified(_) | Peer::Local => Ok(()),
            Peer::Unverified if self.allow_plaintext => Ok(()),
            Peer::Unverified => Err(
                "This node takes replication traffic only from a peer that proved its identity."
                    .to_string(),
            ),
            Peer::Anonymous => Err(
                "A peer with no certificate cannot speak to the replication service.".to_string(),
            ),
        }
    }

    /// Whether the node that sent a consensus message is the node that the
    /// connection proved. A name in a message is a claim; a certificate is
    /// proof.
    pub fn check_sender(&self, peer: &Peer, sender: &str) -> Result<(), String> {
        self.admits(peer)?;
        match peer {
            Peer::Verified(identity) if identity.node_id != sender => Err(format!(
                "The connection proved it is `{}`, and the message says it is from `{sender}`, so it was not taken.",
                identity.node_id
            )),
            _ => Ok(()),
        }
    }
}
