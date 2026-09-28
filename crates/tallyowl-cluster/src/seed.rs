//! Who is in a cell before its controller quorum has said anything.
//!
//! A cell learns its members from its controller quorum. The first start has no
//! quorum yet, so the first voter set comes from configuration, and **every
//! node has to read the same voter set out of it**. A consensus identity is a
//! hash of a node's name ([`crate::raft::node_id`]), so two nodes that name a
//! third differently hold two identities for it, and the group never forms.
//!
//! # The rule
//!
//! 1. A node's **address** is `replication.advertise`, or `replication.listen`
//!    when that is empty. It is the address its peers dial, so it cannot be an
//!    unspecified address such as `0.0.0.0`.
//! 2. A node's **name** is `node.name`, or, when that is empty, `node-` and its
//!    address with every `.` and `:` written as `-`.
//! 3. An entry in `replication.peers` is `address` or `name@address`. A plain
//!    address names its node by rule 2. **A node that sets `node.name` must be
//!    listed by its peers as `name@address`**, or they would derive a different
//!    name for it.
//! 4. An entry whose address or name is this node's own is this node, and is
//!    skipped. One list that names every node therefore works on every node.
//!
//! The members come back sorted by name, so the order does not depend on which
//! node computed them.

use tallyowl_obs::error::{ErrorCode, TallyOwlError};

use crate::topology::Member;

/// What one node reads out of its configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    /// This node's name. Rule 2.
    pub node: String,
    /// The address this node's peers dial. Rule 1.
    pub address: String,
    /// Every first voter, this node included, sorted by name.
    pub members: Vec<Member>,
}

/// What configuration says about this node and its peers.
#[derive(Debug, Clone, Default)]
pub struct SeedSettings<'a> {
    /// `node.name`
    pub name: &'a str,
    /// `replication.listen`
    pub listen: &'a str,
    /// `replication.advertise`
    pub advertise: &'a str,
    /// `replication.peers`
    pub peers: &'a [String],
    /// `cell.region`
    pub region: &'a str,
    /// `node.failureDomain`
    pub domain: &'a str,
}

/// The name rule 2 derives from an address.
pub fn name_for(address: &str) -> String {
    format!("node-{}", address.replace(['.', ':'], "-"))
}

/// Whether an address names no host a peer could dial.
fn is_unspecified(address: &str) -> bool {
    let host = match address.rsplit_once(':') {
        Some((host, _)) => host,
        None => address,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.is_empty() || host == "0.0.0.0" || host == "::" || host == "*"
}

/// Read the first voter set. See the module note for the rule.
pub fn seed(settings: &SeedSettings) -> Result<Seed, TallyOwlError> {
    let advertise = settings.advertise.trim();
    let address = if advertise.is_empty() {
        settings.listen.trim()
    } else {
        advertise
    };

    if !advertise.is_empty() && is_unspecified(advertise) {
        return Err(TallyOwlError::new(
            ErrorCode::FailedPrecondition,
            format!(
                "`replication.advertise` is `{advertise}`, which is not an address another node can dial. Set it to this node's own reachable address, for example `tallyowl-0.tallyowl:5200`."
            ),
        ));
    }

    let own_name = match settings.name.trim() {
        "" => name_for(address),
        name => name.to_string(),
    };

    let mut members: Vec<Member> = Vec::new();
    let mut listed_self = false;
    for (index, entry) in settings.peers.iter().enumerate() {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (name, peer_address) = match entry.split_once('@') {
            Some((name, peer_address)) => (name.trim().to_string(), peer_address.trim()),
            None => (name_for(entry), entry),
        };
        if name.is_empty() || peer_address.is_empty() {
            return Err(TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                format!(
                    "`replication.peers` holds `{entry}`, which is not `address` or `name@address`."
                ),
            ));
        }
        let same_address = peer_address == address;
        let same_name = name == own_name;
        if same_address && !same_name {
            // The peers that read this list will call this node `name`, and it
            // calls itself something else. The group could never form.
            return Err(TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                format!(
                    "`replication.peers` lists this node's address `{address}` under the name `{name}`, and this node's name is `{own_name}`. Write the entry as `{own_name}@{address}` on every node, or clear `node.name` so the name comes from the address."
                ),
            ));
        }
        if same_address || same_name {
            if same_name && !same_address {
                return Err(TallyOwlError::new(
                    ErrorCode::FailedPrecondition,
                    format!(
                        "`replication.peers` lists `{own_name}` at `{peer_address}`, and this node advertises `{address}`. Set `replication.advertise` to the address its peers use for it."
                    ),
                ));
            }
            listed_self = true;
            members.push(
                Member::voter(own_name.clone(), address)
                    .in_region(settings.region)
                    .in_domain(match settings.domain {
                        "" => format!("peer-{index}"),
                        told => told.to_string(),
                    }),
            );
            continue;
        }
        members.push(
            Member::voter(name, peer_address)
                .in_region(settings.region)
                // Without a told domain, each peer counts as its own. Assuming
                // they share one would make placement believe a spread it does
                // not have.
                .in_domain(format!("peer-{index}")),
        );
    }
    if !listed_self {
        members.push(
            Member::voter(own_name.clone(), address)
                .in_region(settings.region)
                .in_domain(settings.domain),
        );
    }

    // A node that has peers is dialled by them, so its address has to be one
    // they can dial. A node alone is never dialled.
    if members.len() > 1 && is_unspecified(address) {
        return Err(TallyOwlError::new(
            ErrorCode::FailedPrecondition,
            format!(
                "This node has peers and would tell them to reach it at `{address}`, which is the address it listens on and not one they can dial. Set `replication.advertise` to this node's own reachable address."
            ),
        ));
    }

    let mut seen = std::collections::BTreeSet::new();
    for member in &members {
        if !seen.insert(member.node.clone()) {
            return Err(TallyOwlError::new(
                ErrorCode::FailedPrecondition,
                format!(
                    "`replication.peers` names `{}` more than once.",
                    member.node
                ),
            ));
        }
    }
    members.sort_by(|a, b| a.node.cmp(&b.node));

    Ok(Seed {
        node: own_name,
        address: address.to_string(),
        members,
    })
}
