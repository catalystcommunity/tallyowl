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
use tallyowl_store::certificates::{
    generate_authorities, sign_request, Authority, Subject, DEFAULT_CERTIFICATE_LIFETIME_MS,
    HEAD_ROLE, HEAD_SERVER_NAME,
};
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

/// The DER of every certificate in a PEM text.
fn ders(pem: &str) -> Vec<Vec<u8>> {
    x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes())
        .map(|block| block.expect("PEM").contents)
        .collect()
}

/// A generated root and intermediate, with the catalog made a signer.
fn signer(catalog: &Catalog, max_lifetime_ms: i64) -> std::sync::Arc<Authority> {
    let generated = generate_authorities(NOW).expect("authorities");
    let authority = Authority::from_pem(
        &generated.intermediate_chain_pem,
        &generated.intermediate_key_pem,
        &ders(&generated.root_certificate_pem),
        NOW,
        max_lifetime_ms,
    )
    .expect("the intermediate is a usable signer");
    catalog.set_signing_authority(authority);
    catalog.certificate_authority().expect("the catalog signs")
}

fn subject(node_id: &str, role: &str) -> Subject {
    Subject {
        node_id: node_id.into(),
        role: role.into(),
        cell: None,
        region: None,
    }
}

#[test]
fn the_control_plane_signs_a_request_and_never_sees_a_private_key() {
    // AGENTS.md: "Each enrolled node generates its private key. The control
    // plane signs only the certificate request."
    let catalog = catalog("sign-request");
    let authority = signer(&catalog, DEFAULT_CERTIFICATE_LIFETIME_MS);
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

    assert_eq!(
        issued.chain.len(),
        2,
        "the leaf and the intermediate; the root travels in `installation.authorities`"
    );
    assert_eq!(issued.chain[1], authority.certificate_der());
    assert_eq!(issued.expires_at, NOW + DEFAULT_CERTIFICATE_LIFETIME_MS);
    assert_eq!(
        issued.renew_after,
        NOW + DEFAULT_CERTIFICATE_LIFETIME_MS * 2 / 3,
        "D62: a node renews at two thirds of its life"
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
    let authority = signer(&catalog, DEFAULT_CERTIFICATE_LIFETIME_MS);
    let (_, request) = node_request("i-am-the-controller");

    let issued = sign_request(
        &authority,
        &request,
        &subject("node-0002", NodeRole::Projector.as_str()),
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
fn the_recorded_serial_is_the_serial_a_tls_peer_reads() {
    // A renewal is checked against the serial the catalog recorded, and the
    // listener reads the serial out of the certificate. They were two
    // different numbers: a hash of the certificate, and the one rcgen chose.
    use x509_parser::prelude::*;

    let catalog = catalog("serial");
    let authority = signer(&catalog, DEFAULT_CERTIFICATE_LIFETIME_MS);
    for n in 0..32 {
        let (_, request) = node_request("n");
        let issued = sign_request(
            &authority,
            &request,
            &subject(&format!("node-{n}"), "projector"),
            NOW,
            DEFAULT_CERTIFICATE_LIFETIME_MS,
        )
        .expect("signed");
        let (_, certificate) = X509Certificate::from_der(&issued.chain[0]).expect("it parses");
        let read: String = certificate
            .raw_serial()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(read, issued.serial);
    }
}

#[test]
fn a_certificate_names_its_node_for_tls_and_a_head_also_names_the_head_name() {
    use x509_parser::prelude::*;

    let catalog = catalog("names");
    let authority = signer(&catalog, DEFAULT_CERTIFICATE_LIFETIME_MS);
    let names_of = |role: &str| {
        let (_, request) = node_request("n");
        let issued = sign_request(&authority, &request, &subject("node-7", role), NOW, 60_000)
            .expect("signed");
        let (_, certificate) = X509Certificate::from_der(&issued.chain[0]).expect("parses");
        certificate
            .subject_alternative_name()
            .expect("readable")
            .map(|extension| {
                extension
                    .value
                    .general_names
                    .iter()
                    .map(|name| name.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let collector = names_of("collector-forwarder");
    assert!(
        collector.iter().any(|n| n.contains("node-7")),
        "{collector:?}"
    );
    assert!(
        !collector.iter().any(|n| n.contains(HEAD_SERVER_NAME)),
        "{collector:?}"
    );
    let head = names_of(HEAD_ROLE);
    assert!(
        head.iter().any(|n| n.contains(HEAD_SERVER_NAME)),
        "{head:?}"
    );
}

#[test]
fn a_token_cannot_have_more_life_than_the_head_issues() {
    let catalog = catalog("lifetime-cap");
    let hour = 60 * 60_000;
    let authority = signer(&catalog, hour);
    let (_, request) = node_request("n");
    let issued = sign_request(
        &authority,
        &request,
        &subject("node-8", "projector"),
        NOW,
        48 * hour,
    )
    .expect("signed");
    assert_eq!(issued.expires_at, NOW + hour);
    assert_eq!(issued.renew_after, NOW + hour * 2 / 3);
}

#[test]
fn a_certificate_request_that_is_not_one_is_refused() {
    let catalog = catalog("bad-request");
    let authority = signer(&catalog, DEFAULT_CERTIFICATE_LIFETIME_MS);
    let failure = sign_request(
        &authority,
        b"this is not a certificate request",
        &subject("node-0003", NodeRole::Projector.as_str()),
        NOW,
        DEFAULT_CERTIFICATE_LIFETIME_MS,
    )
    .expect_err("it is refused");
    assert!(format!("{failure}").contains("PKCS#10"), "{failure}");
}

#[test]
fn a_head_with_no_signing_certificate_signs_nothing_and_says_which_settings() {
    // It used to make an authority of its own and keep the key in the catalog,
    // so each head of one installation had a different authority. D62.
    let catalog = catalog("no-signer");
    let failure = catalog
        .certificate_authority()
        .expect_err("no authority is made");
    let text = failure.to_string();
    assert!(text.contains("installation.signingCertificate"), "{text}");
    assert!(text.contains("ca create"), "{text}");
}

#[test]
fn a_signing_certificate_that_cannot_sign_is_refused_and_the_setting_is_named() {
    let generated = generate_authorities(NOW).expect("authorities");
    let root = ders(&generated.root_certificate_pem);
    let load = |chain: &str, key: &str, trusted: &[Vec<u8>], now: i64| {
        Authority::from_pem(chain, key, trusted, now, DEFAULT_CERTIFICATE_LIFETIME_MS)
            .expect_err("refused")
            .to_string()
    };

    // A node certificate is not an authority.
    let catalog = catalog("not-a-ca");
    let authority = signer(&catalog, DEFAULT_CERTIFICATE_LIFETIME_MS);
    let key = rcgen::KeyPair::generate().expect("key");
    let request = rcgen::CertificateParams::default()
        .serialize_request(&key)
        .expect("request");
    let leaf = sign_request(
        &authority,
        request.der(),
        &subject("node-9", "projector"),
        NOW,
        DEFAULT_CERTIFICATE_LIFETIME_MS,
    )
    .expect("signed");
    let leaf_pem = format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
        base64_lines(&leaf.chain[0])
    );
    let text = load(&leaf_pem, &key.serialize_pem(), &root, NOW);
    assert!(text.contains("installation.signingCertificate"), "{text}");
    assert!(text.contains("not a certificate authority"), "{text}");

    // Valid once, and not now.
    let text = load(
        &generated.intermediate_chain_pem,
        &generated.intermediate_key_pem,
        &root,
        NOW + 20 * 365 * 24 * 60 * 60_000,
    );
    assert!(text.contains("not valid now"), "{text}");

    // Signed by a root nobody here trusts.
    let foreign = generate_authorities(NOW).expect("another installation");
    let text = load(
        &generated.intermediate_chain_pem,
        &generated.intermediate_key_pem,
        &ders(&foreign.root_certificate_pem),
        NOW,
    );
    assert!(text.contains("installation.authorities"), "{text}");

    // Somebody else's key.
    let text = load(
        &generated.intermediate_chain_pem,
        &foreign.intermediate_key_pem,
        &root,
        NOW,
    );
    assert!(text.contains("installation.signingKey"), "{text}");
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

// ---------------------------------------------------------------------------
// The claim: the use count, the hourly rate, and the revocation in one write
// ---------------------------------------------------------------------------

use tallyowl_store::identity::{EnrollmentRefusal, RATE_WINDOW_MS};

#[test]
fn the_hourly_rate_refuses_inside_the_hour_and_opens_again_after_it() {
    // The clock is the argument, so an hour passes without anybody waiting.
    let catalog = catalog("token-rate");
    let mut limited = policy(NodeRole::CollectorIntake);
    limited.enrollments_each_hour = Some(2);
    let issued = catalog
        .issue_role_token("pods", limited, NOW)
        .expect("issued");
    let token_id = &issued.token.token_id;

    assert_eq!(
        catalog.claim_token_use(token_id, NOW).expect("runs"),
        Ok(())
    );
    assert_eq!(
        catalog.claim_token_use(token_id, NOW + 1).expect("runs"),
        Ok(())
    );
    assert_eq!(
        catalog
            .claim_token_use(token_id, NOW + RATE_WINDOW_MS - 1)
            .expect("runs"),
        Err(EnrollmentRefusal::RateLimited),
        "a third enrollment in one hour was permitted"
    );
    assert_eq!(
        catalog
            .role_token(token_id)
            .expect("reads")
            .expect("there")
            .uses,
        2,
        "a refused claim was counted as a use"
    );
    assert_eq!(
        catalog
            .claim_token_use(token_id, NOW + RATE_WINDOW_MS)
            .expect("runs"),
        Ok(()),
        "the next hour did not open"
    );
}

#[test]
fn a_claim_takes_the_last_use_once_and_a_release_gives_it_back() {
    let catalog = catalog("token-claim");
    let mut once = policy(NodeRole::CollectorIntake);
    once.max_uses = Some(1);
    once.enrollments_each_hour = Some(1);
    let issued = catalog.issue_role_token("pods", once, NOW).expect("issued");
    let token_id = &issued.token.token_id;

    assert_eq!(
        catalog.claim_token_use(token_id, NOW).expect("runs"),
        Ok(())
    );
    assert_eq!(
        catalog.claim_token_use(token_id, NOW).expect("runs"),
        Err(EnrollmentRefusal::UsesExhausted)
    );
    // The enrollment that claimed it failed, so the token keeps its use and its
    // place in the hour.
    catalog.release_token_use(token_id).expect("released");
    assert_eq!(
        catalog.claim_token_use(token_id, NOW).expect("runs"),
        Ok(())
    );
}

#[test]
fn a_revoked_token_gives_no_use_and_a_use_never_undoes_a_revocation() {
    // The defect: an enrollment read the token, a revocation committed, and the
    // enrollment wrote `uses + 1` with `revoked_at: None` on top of it.
    let catalog = catalog("token-revoke-race");
    let issued = catalog
        .issue_role_token("pods", policy(NodeRole::CollectorIntake), NOW)
        .expect("issued");
    let token_id = &issued.token.token_id;

    catalog
        .revoke_role_token(token_id, false, NOW)
        .expect("revoked");
    assert_eq!(
        catalog.claim_token_use(token_id, NOW + 1).expect("runs"),
        Err(EnrollmentRefusal::Credential(AuthFailure::Revoked))
    );
    // Every writer of the record reads it inside its own write.
    catalog
        .record_token_use(token_id, NOW + 2)
        .expect("recorded");
    catalog.release_token_use(token_id).expect("released");
    let held = catalog.role_token(token_id).expect("reads").expect("there");
    assert_eq!(
        held.revoked_at,
        Some(NOW),
        "a later write undid the revocation"
    );
}

#[test]
fn a_renewal_never_writes_over_a_revocation() {
    // `renew` read the node, signed, and wrote its copy back. A cascade
    // revocation in between was overwritten and a new certificate went out.
    let catalog = catalog("node-renew-race");
    let issued = catalog
        .issue_role_token("pods", policy(NodeRole::Projector), NOW)
        .expect("issued");
    let node = NodeRecord {
        node_id: "node-a".into(),
        token_id: issued.token.token_id.clone(),
        role: NodeRole::Projector,
        cell: None,
        region: None,
        certificate_serial: "01".into(),
        enrolled_at: NOW,
        expires_at: NOW + 60_000,
        revoked_at: None,
        software_version: "0.0.0".into(),
    };
    catalog.put_node(&node).expect("stored");

    assert!(catalog
        .renew_node("node-a", "02", NOW + 120_000, NOW + 1)
        .expect("runs"));
    assert_eq!(
        catalog
            .node("node-a")
            .expect("reads")
            .expect("there")
            .certificate_serial,
        "02"
    );

    // The caller still holds the record it read before this.
    catalog
        .revoke_role_token(&issued.token.token_id, true, NOW + 2)
        .expect("revoked");
    assert!(
        !catalog
            .renew_node("node-a", "03", NOW + 180_000, NOW + 3)
            .expect("runs"),
        "a revoked node renewed"
    );
    let held = catalog.node("node-a").expect("reads").expect("there");
    assert_eq!(held.revoked_at, Some(NOW + 2));
    assert_eq!(held.certificate_serial, "02");
    assert!(!catalog
        .renew_node("node-nobody", "04", NOW, NOW)
        .expect("runs"));
}
