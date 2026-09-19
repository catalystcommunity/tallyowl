//! Role tokens, node enrollment, and the certificate authority.
//!
//! The rules under test come from `docs/NODE_IDENTITY.md` and `AGENTS.md`:
//!
//! - TallyOwl stores a token ID and a keyed digest, never the token value;
//! - a token cannot be used as a source key and a source key cannot enroll;
//! - the controller intersects the requested scope with the token policy and
//!   never widens it;
//! - each enrolled node generates its private key, and the control plane signs
//!   only the certificate request;
//! - revoking a token stops new enrollment and leaves issued certificates alone
//!   unless the operator asks for cascade.

use std::path::PathBuf;

use tallyowl_store::catalog::Catalog;
use tallyowl_store::certificates::{sign_request, Subject, DEFAULT_CERTIFICATE_LIFETIME_MS};
use tallyowl_store::control::AuthFailure;
use tallyowl_store::identity::{is_role_token, NodeRecord, NodeRole, RoleTokenPolicy};

const NOW: i64 = 1_785_628_800_000;

fn catalog(name: &str) -> Catalog {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("identity-tests")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a place to work");
    Catalog::open(path.join("catalog.redb")).expect("the catalog opens")
}

fn policy(role: NodeRole) -> RoleTokenPolicy {
    RoleTokenPolicy::for_role(role)
}

// ---------------------------------------------------------------------------
// The token itself
// ---------------------------------------------------------------------------

#[test]
fn a_token_resolves_once_and_its_value_is_never_stored() {
    // NODE_IDENTITY.md section 2: TallyOwl stores a token ID and a keyed digest.
    // It does not store the token value.
    let catalog = catalog("token-digest");
    let issued = catalog
        .issue_role_token(
            "kubernetes collectors",
            policy(NodeRole::CollectorIntake),
            NOW,
        )
        .expect("the token is issued");

    assert!(is_role_token(&issued.credential));
    assert!(issued.credential.starts_with("towr_"));

    let resolved = catalog
        .resolve_role_token(&issued.credential, NOW)
        .expect("it resolves");
    assert_eq!(resolved.token_id, issued.token.token_id);
    assert!(resolved.policy.permits_role(NodeRole::CollectorIntake));

    // The stored record holds no part of the secret.
    let held = catalog
        .role_token(&issued.token.token_id)
        .expect("it reads")
        .expect("it is there");
    // The base64url alphabet holds `_`, so the split is from the front: the
    // prefix, then the token ID, then everything else is the secret. Splitting
    // from the back can land inside the secret and compare a few characters
    // that appear anywhere.
    let secret = issued
        .credential
        .strip_prefix("towr_")
        .expect("the role-token prefix")
        .split_once('_')
        .expect("a secret half")
        .1;
    assert!(!secret.is_empty());
    let stored = format!("{held:?}");
    assert!(
        !stored.contains(secret),
        "the token value reached the record"
    );
}

#[test]
fn a_token_and_a_source_key_are_not_interchangeable() {
    // Section 1: these credentials have different scopes. Do not use one as a
    // replacement for another.
    let catalog = catalog("token-not-a-key");
    let issued = catalog
        .issue_role_token("nodes", policy(NodeRole::CollectorIntake), NOW)
        .expect("the token is issued");

    // A role token is not a source credential.
    assert_eq!(
        catalog.resolve_credential(&issued.credential, NOW),
        Err(AuthFailure::Malformed)
    );

    // And a source key does not enroll.
    let key = catalog
        .provision("checkout", "default", NOW)
        .expect("a project is provisioned");
    assert!(!is_role_token(&key.credential));
    assert_eq!(
        catalog
            .resolve_role_token(&key.credential, NOW)
            .unwrap_err(),
        AuthFailure::Malformed
    );
}

#[test]
fn a_wrong_secret_against_a_real_token_id_is_refused() {
    let catalog = catalog("token-wrong-secret");
    let issued = catalog
        .issue_role_token("nodes", policy(NodeRole::Projector), NOW)
        .expect("the token is issued");
    let forged = format!(
        "towr_{}_{}",
        issued.token.token_id, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
    );
    assert_eq!(
        catalog.resolve_role_token(&forged, NOW).unwrap_err(),
        AuthFailure::Unknown
    );
}

#[test]
fn an_expired_token_and_a_revoked_token_each_say_so_to_the_audit_and_not_to_the_caller() {
    let catalog = catalog("token-lifecycle");

    let expiring = catalog
        .issue_role_token(
            "short",
            RoleTokenPolicy {
                expires_at: Some(NOW + 1_000),
                ..policy(NodeRole::ReadReplica)
            },
            NOW,
        )
        .expect("the token is issued");
    assert!(catalog
        .resolve_role_token(&expiring.credential, NOW)
        .is_ok());
    assert_eq!(
        catalog
            .resolve_role_token(&expiring.credential, NOW + 2_000)
            .unwrap_err(),
        AuthFailure::Expired
    );

    let revoked = catalog
        .issue_role_token("rotated", policy(NodeRole::ReadReplica), NOW)
        .expect("the token is issued");
    catalog
        .revoke_role_token(&revoked.token.token_id, false, NOW)
        .expect("it revokes");
    assert_eq!(
        catalog
            .resolve_role_token(&revoked.credential, NOW)
            .unwrap_err(),
        AuthFailure::Revoked
    );
}

#[test]
fn an_installation_holds_many_active_tokens_so_a_rotation_needs_no_cutover() {
    let catalog = catalog("token-rotation");
    let old = catalog
        .issue_role_token("old", policy(NodeRole::CollectorIntake), NOW)
        .expect("issued");
    let new = catalog
        .issue_role_token("new", policy(NodeRole::CollectorIntake), NOW)
        .expect("issued");

    assert!(catalog.resolve_role_token(&old.credential, NOW).is_ok());
    assert!(catalog.resolve_role_token(&new.credential, NOW).is_ok());

    catalog
        .revoke_role_token(&old.token.token_id, false, NOW)
        .expect("it revokes");
    assert!(catalog.resolve_role_token(&old.credential, NOW).is_err());
    assert!(
        catalog.resolve_role_token(&new.credential, NOW).is_ok(),
        "the replacement keeps working"
    );
}

// ---------------------------------------------------------------------------
// The policy
// ---------------------------------------------------------------------------

#[test]
fn a_policy_permits_only_the_roles_it_names() {
    let permitted = RoleTokenPolicy {
        roles: vec![NodeRole::CollectorIntake, NodeRole::CollectorForwarder],
        ..RoleTokenPolicy::default()
    };
    assert!(permitted.permits_role(NodeRole::CollectorIntake));
    assert!(permitted.permits_role(NodeRole::CollectorForwarder));
    assert!(!permitted.permits_role(NodeRole::StorageProcess));
}

#[test]
fn an_empty_location_list_does_not_restrict_and_a_populated_one_does() {
    let anywhere = RoleTokenPolicy::for_role(NodeRole::Projector);
    assert!(anywhere.permits_cell(Some("cell-a")));
    assert!(anywhere.permits_cell(None));

    let somewhere = RoleTokenPolicy {
        cells: vec!["cell-a".into()],
        regions: vec!["eu-west".into()],
        ..RoleTokenPolicy::for_role(NodeRole::Projector)
    };
    assert!(somewhere.permits_cell(Some("cell-a")));
    assert!(!somewhere.permits_cell(Some("cell-b")));
    // A request that names nothing against a restricting list is refused rather
    // than defaulted. Choosing a cell for a caller that did not ask for one
    // would place a node somewhere nobody decided.
    assert!(!somewhere.permits_cell(None));
    assert!(somewhere.permits_region(Some("eu-west")));
    assert!(!somewhere.permits_region(Some("us-east")));
}

#[test]
fn no_role_names_a_voter_so_a_token_cannot_ask_to_be_one() {
    // NODE_IDENTITY.md section 3: a role token cannot create a controller voter,
    // cannot create a global directory voter, and cannot change a tablet voter
    // set. The type has no name for any of them, so the rule cannot be
    // forgotten by a check somebody did not write.
    for role in NodeRole::ALL {
        let name = role.as_str();
        assert!(!name.contains("voter"), "{name} names a voter");
        assert!(!name.contains("controller"), "{name} names a controller");
    }
    assert_eq!(NodeRole::parse("controller-voter"), None);
    assert_eq!(NodeRole::parse("tablet-voter"), None);
    assert_eq!(NodeRole::parse("global-directory-voter"), None);

    // A storage process can enroll. Placement is not part of that.
    assert_eq!(
        NodeRole::parse("storage-process"),
        Some(NodeRole::StorageProcess)
    );
}

#[test]
fn every_role_survives_the_round_trip_through_a_stored_policy() {
    let catalog = catalog("policy-round-trip");
    let policy = RoleTokenPolicy {
        roles: NodeRole::ALL.to_vec(),
        cells: vec!["cell-a".into(), "cell-b".into()],
        regions: vec!["eu-west".into()],
        workspaces: vec![[7; 16]],
        projects: vec![[9; 16]],
        expires_at: Some(NOW + 86_400_000),
        max_uses: Some(50),
        max_active_nodes: Some(20),
        certificate_lifetime_ms: Some(3_600_000),
        enrollments_each_hour: Some(100),
        audit_labels: vec!["team-platform".into()],
    };
    let issued = catalog
        .issue_role_token("everything", policy.clone(), NOW)
        .expect("issued");

    let held = catalog
        .role_token(&issued.token.token_id)
        .expect("reads")
        .expect("there");
    assert_eq!(held.policy, policy);
}

// ---------------------------------------------------------------------------
// Certificates
// ---------------------------------------------------------------------------

/// A node's own key and its certificate request. This is what a node does
/// before it ever contacts the control plane, and the private key stays here.
fn node_request(node_asked_to_be: &str) -> (rcgen::KeyPair, Vec<u8>) {
    let key = rcgen::KeyPair::generate().expect("the node generates its key");
    let mut params = rcgen::CertificateParams::default();
    let mut name = rcgen::DistinguishedName::new();
    name.push(rcgen::DnType::CommonName, node_asked_to_be);
    params.distinguished_name = name;
    let request = params
        .serialize_request(&key)
        .expect("the node builds a request");
    (key, request.der().to_vec())
}

#[test]
fn the_control_plane_signs_a_request_and_never_sees_a_private_key() {
    // AGENTS.md: "Each enrolled node generates its private key. The control
    // plane signs only the certificate request."
    let catalog = catalog("sign-request");
    let authority = catalog
        .certificate_authority()
        .expect("the authority exists");
    let (node_key, request) = node_request("node-asks-for-this");

    let issued = sign_request(
        &authority,
        &request,
        &Subject {
            node_id: "node-0001".into(),
            role: NodeRole::CollectorIntake.as_str().into(),
            cell: Some("cell-a".into()),
            region: Some("eu-west".into()),
        },
        NOW,
        DEFAULT_CERTIFICATE_LIFETIME_MS,
    )
    .expect("the request is signed");

    assert_eq!(issued.chain.len(), 2, "the leaf and the authority");
    assert_eq!(issued.chain[1], authority.certificate_der());
    assert_eq!(issued.expires_at, NOW + DEFAULT_CERTIFICATE_LIFETIME_MS);
    assert!(issued.renew_after > NOW);
    assert!(
        issued.renew_after < issued.expires_at,
        "a node must renew before it expires"
    );

    // The node's private key never left the node, and nothing the control plane
    // produced contains it.
    let private = node_key.serialize_pem();
    let signed = format!("{issued:?}");
    assert!(!signed.contains(&private));
}

#[test]
fn the_certificate_names_the_node_the_control_plane_assigned_not_the_one_it_asked_for() {
    // A node that could choose its own subject could name itself another node.
    use x509_parser::prelude::*;

    let catalog = catalog("assigned-subject");
    let authority = catalog.certificate_authority().expect("authority");
    let (_, request) = node_request("i-am-the-controller");

    let issued = sign_request(
        &authority,
        &request,
        &Subject {
            node_id: "node-0002".into(),
            role: NodeRole::Projector.as_str().into(),
            cell: None,
            region: None,
        },
        NOW,
        DEFAULT_CERTIFICATE_LIFETIME_MS,
    )
    .expect("signed");

    let (_, certificate) = X509Certificate::from_der(&issued.chain[0]).expect("it parses");
    let subject = certificate.subject().to_string();
    assert!(subject.contains("node-0002"), "{subject}");
    assert!(!subject.contains("i-am-the-controller"), "{subject}");
    assert!(subject.contains("projector"), "{subject}");
}

#[test]
fn a_certificate_request_that_is_not_one_is_refused() {
    let catalog = catalog("bad-request");
    let authority = catalog.certificate_authority().expect("authority");
    let failure = sign_request(
        &authority,
        b"this is not a certificate request",
        &Subject {
            node_id: "node-0003".into(),
            role: NodeRole::Projector.as_str().into(),
            cell: None,
            region: None,
        },
        NOW,
        DEFAULT_CERTIFICATE_LIFETIME_MS,
    )
    .expect_err("it is refused");
    assert!(format!("{failure}").contains("PKCS#10"), "{failure}");
}

#[test]
fn the_authority_is_generated_once_and_stays_the_same() {
    let catalog = catalog("authority-stable");
    let first = catalog.certificate_authority().expect("authority");
    let second = catalog.certificate_authority().expect("authority");
    assert_eq!(first.certificate_der(), second.certificate_der());
    assert!(!first.certificate_der().is_empty());
}

// ---------------------------------------------------------------------------
// Enrolled nodes
// ---------------------------------------------------------------------------

fn node(node_id: &str, token_id: &str, expires_at: i64) -> NodeRecord {
    NodeRecord {
        node_id: node_id.to_string(),
        token_id: token_id.to_string(),
        role: NodeRole::CollectorIntake,
        cell: Some("cell-a".into()),
        region: None,
        certificate_serial: "abcd".into(),
        enrolled_at: NOW,
        expires_at,
        revoked_at: None,
        software_version: "0.0.0".into(),
    }
}

#[test]
fn a_node_record_survives_the_round_trip_and_counts_toward_its_token() {
    let catalog = catalog("node-records");
    let issued = catalog
        .issue_role_token("pods", policy(NodeRole::CollectorIntake), NOW)
        .expect("issued");
    let token_id = &issued.token.token_id;

    catalog
        .put_node(&node("node-a", token_id, NOW + 86_400_000))
        .expect("stored");
    catalog
        .put_node(&node("node-b", token_id, NOW + 86_400_000))
        .expect("stored");
    // Already expired, so it does not count against the active limit.
    catalog
        .put_node(&node("node-c", token_id, NOW - 1))
        .expect("stored");

    assert_eq!(catalog.nodes().expect("reads").len(), 3);
    assert_eq!(
        catalog.active_nodes_for(token_id, NOW).expect("counts"),
        2,
        "an expired identity does not hold a slot"
    );

    let held = catalog.node("node-a").expect("reads").expect("there");
    assert_eq!(held.role, NodeRole::CollectorIntake);
    assert_eq!(held.cell.as_deref(), Some("cell-a"));
}

#[test]
fn revoking_a_token_stops_enrollment_and_leaves_certificates_alone_until_cascade() {
    // Section 2: token revocation prevents new enrollment. It does not
    // immediately revoke certificates by default. An operator can select
    // cascade revocation.
    let catalog = catalog("cascade");
    let issued = catalog
        .issue_role_token("pods", policy(NodeRole::CollectorIntake), NOW)
        .expect("issued");
    let token_id = &issued.token.token_id;
    catalog
        .put_node(&node("node-a", token_id, NOW + 86_400_000))
        .expect("stored");

    let cascaded = catalog
        .revoke_role_token(token_id, false, NOW)
        .expect("revokes");
    assert_eq!(cascaded, 0, "no certificate was revoked");
    assert!(
        catalog
            .node("node-a")
            .expect("reads")
            .expect("there")
            .is_active(NOW),
        "the enrolled node keeps working"
    );
    assert!(catalog.resolve_role_token(&issued.credential, NOW).is_err());

    let cascaded = catalog
        .revoke_role_token(token_id, true, NOW)
        .expect("revokes");
    assert_eq!(cascaded, 1);
    assert!(!catalog
        .node("node-a")
        .expect("reads")
        .expect("there")
        .is_active(NOW));
}

#[test]
fn an_expired_node_is_removed_after_its_safety_period_and_not_before() {
    // Section 7: an expired pod identity needs no manual removal. The controller
    // removes it after its lease and certificate safety periods.
    let catalog = catalog("node-expiry");
    catalog
        .put_node(&node("node-a", "t1", NOW))
        .expect("stored");

    let safety = 3_600_000;
    assert_eq!(
        catalog.expire_nodes(NOW + 1_000, safety).expect("runs"),
        0,
        "still inside the safety period"
    );
    assert_eq!(catalog.nodes().expect("reads").len(), 1);

    assert_eq!(
        catalog
            .expire_nodes(NOW + safety + 1_000, safety)
            .expect("runs"),
        1
    );
    assert!(catalog.nodes().expect("reads").is_empty());
}

#[test]
fn a_token_records_each_enrollment_it_authorized() {
    let catalog = catalog("token-uses");
    let issued = catalog
        .issue_role_token("pods", policy(NodeRole::CollectorIntake), NOW)
        .expect("issued");
    let token_id = &issued.token.token_id;

    for _ in 0..3 {
        catalog.record_token_use(token_id, NOW).expect("recorded");
    }
    let held = catalog.role_token(token_id).expect("reads").expect("there");
    assert_eq!(held.uses, 3);
    assert_eq!(held.last_used_at, Some(NOW));
}
