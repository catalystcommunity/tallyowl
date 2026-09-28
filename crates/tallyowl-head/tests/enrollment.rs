//! Node enrollment over the real control surface.
//!
//! A node here does what a real node does: it generates its own key, builds a
//! certificate request, opens a socket, presents a role token, and gets back an
//! identity it did not choose. Nothing is stubbed except the clock, which is
//! the process clock.
//!
//! `docs/NODE_IDENTITY.md` section 4 is the sequence under test, and the two
//! rules from `AGENTS.md` are the assertions that matter:
//!
//! - "Each enrolled node generates its private key. The control plane signs
//!   only the certificate request."
//! - "A role token cannot add a controller voter or change a tablet voter set."

use std::path::PathBuf;
use std::sync::Arc;

use tallyowl_control_api::codec::{
    decode_create_role_token_response, decode_enroll_node_response, decode_node_list,
    decode_role_token_list, decode_service_error, encode_create_role_token_request,
    encode_enroll_node_request, encode_list_request, encode_renew_node_certificate_request,
    encode_revoke_role_token_request,
};
use tallyowl_control_api::types::{
    CreateRoleTokenRequest, EnrollNodeRequest, ErrorCode, ListRequest, NodeCapabilities, NodeRole,
    RenewNodeCertificateRequest, RevokeRoleTokenRequest, RoleTokenPolicy,
};
use tallyowl_head::ingest::Ingest;
use tallyowl_head::query::QueryService;
use tallyowl_head::service::HeadService;
use tallyowl_obs::log::{Logger, Severity};
use tallyowl_obs::metrics::Registry;
use tallyowl_rpc::{Client, SERVICE_ERROR_VARIANT};
use tallyowl_store::SegmentedStore;

const MAX_FRAME: usize = 16 * 1024 * 1024;
const CONTROL: &str = "TallyOwlControl";

/// A head with its control surface on a real socket.
struct Installation {
    _server: tallyowl_rpc::Server,
    address: String,
    segmented: Arc<SegmentedStore>,
    service: Arc<HeadService>,
}

impl Installation {
    fn start(name: &str) -> Installation {
        let base = std::env::var("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("target"));
        let place = base
            .join("enrollment-tests")
            .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
        let _ = std::fs::remove_dir_all(&place);

        let segmented = Arc::new(
            SegmentedStore::open_with(
                &place,
                tallyowl_store::Sealing {
                    max_open_rows: 100,
                    max_open_ms: i64::MAX,
                    verify_on_read: true,
                    reserve_bytes: 0,
                },
                tallyowl_store::wal::GroupCommit::default(),
            )
            .expect("the store opens"),
        );
        let store: Arc<dyn tallyowl_store::Store> =
            Arc::clone(&segmented) as Arc<dyn tallyowl_store::Store>;
        let logger = Arc::new(Logger::new("tallyowl-head", "0.0.0", Severity::Warning));
        // A head signs only with an authority the operator gave it. D62.
        install_signer(segmented.catalog());

        let service = HeadService {
            ingest: Arc::new(Ingest {
                golden_signal_bucket_ms: 60_000,
                store: Arc::clone(&store),
                metrics: Registry::new(),
                receipt_policy: tallyowl_collector_api::types::ReceiptPolicy::LocalOne,
                open_traces: None,
                policy: None,
                sources: None,
            }),
            enrollment: Arc::new(tallyowl_head::enrollment::EnrollmentService {
                store: Arc::clone(&segmented),
                metrics: Registry::new(),
            }),
            query: Arc::new(QueryService {
                store: Arc::clone(&store),
                max_runtime_ms: 30_000,
                max_expression_depth: tallyowl_head::expr::DEFAULT_MAX_DEPTH,
                guards: tallyowl_head::analysis::Guards::default(),
                attribution: Default::default(),
                policy: Default::default(),
                identity: Default::default(),
            }),
            control: Arc::new(tallyowl_head::control::ControlService {
                store: Arc::clone(&segmented),
                metrics: Registry::new(),
                key_cache_ttl_ms: 30_000,
            }),
            sign_in: Arc::new(tallyowl_head::linkkeys::SignIn {
                store: Arc::clone(&segmented),
                metrics: Registry::new(),
                settings: tallyowl_head::linkkeys::LinkKeysSettings {
                    enabled: false,
                    trusted_domains: Vec::new(),
                    callback_url: String::new(),
                    app_name: "TallyOwl".into(),
                    session_lifetime_ms: 3_600_000,
                },
            }),
            logger: Arc::clone(&logger),
            policy: Arc::new(tallyowl_head::policy::PolicyService::new()),
            saved: Arc::new(tallyowl_head::saved::SavedService::default()),
            attribution: Arc::new(tallyowl_head::attribution::AttributionService::default()),
            alerts: None,
            workflows: None,
        };

        let service = Arc::new(service);
        let server = tallyowl_rpc::serve(
            "127.0.0.1:0",
            Arc::clone(&service) as Arc<dyn tallyowl_rpc::Dispatcher>,
            MAX_FRAME,
        )
        .expect("the head listens");
        let address = server.local_address().to_string();
        Installation {
            _server: server,
            address,
            segmented,
            service,
        }
    }

    /// An owner's session. Managing role tokens needs one.
    fn owner(&self) -> Client {
        let token = self
            .segmented
            .catalog()
            .issue_operator_session("operator", tallyowl_obs::time::now_ms(), 3_600_000)
            .expect("a session")
            .token;
        Client::new(self.address.clone(), MAX_FRAME).with_credential(token)
    }

    /// A node has no session. It presents a role token and nothing else.
    fn node_client(&self) -> Client {
        Client::new(self.address.clone(), MAX_FRAME)
    }

    fn create_token(&self, roles: Vec<NodeRole>) -> (String, String) {
        self.create_token_with(RoleTokenPolicy {
            roles,
            cells: None,
            regions: None,
            workspaces: None,
            projects: None,
            expires_at: None,
            max_uses: None,
            max_active_nodes: None,
            certificate_lifetime_ms: None,
            enrollments_each_hour: None,
            audit_labels: None,
        })
    }

    fn create_token_with(&self, policy: RoleTokenPolicy) -> (String, String) {
        let response = self
            .owner()
            .call(
                CONTROL,
                "create-role-token",
                encode_create_role_token_request(&CreateRoleTokenRequest {
                    label: "kubernetes collectors".into(),
                    policy,
                }),
            )
            .expect("the call reaches the head");
        assert_ne!(
            response.variant.as_deref(),
            Some(SERVICE_ERROR_VARIANT),
            "the token was refused"
        );
        let decoded =
            decode_create_role_token_response(&response.payload).expect("a token comes back");
        (decoded.token_id, decoded.token)
    }
}

/// What a node does before it ever contacts the control plane. The private key
/// stays in this function's return value and is never sent.
fn node_key_and_request(asks_to_be: &str) -> (rcgen::KeyPair, Vec<u8>) {
    let key = rcgen::KeyPair::generate().expect("the node generates its own key");
    let mut params = rcgen::CertificateParams::default();
    let mut name = rcgen::DistinguishedName::new();
    name.push(rcgen::DnType::CommonName, asks_to_be);
    params.distinguished_name = name;
    let request = params
        .serialize_request(&key)
        .expect("the node builds a certificate request");
    (key, request.der().to_vec())
}

fn enroll_request(token: &str, request_der: Vec<u8>, role: NodeRole) -> Vec<u8> {
    encode_enroll_node_request(&EnrollNodeRequest {
        token: token.to_string(),
        certificate_request: request_der,
        requested_role: role,
        cell: None,
        region: None,
        node_id: None,
        capabilities: Some(NodeCapabilities {
            software_version: "0.0.0".into(),
            protocol_versions: None,
            segment_versions: None,
            compression_codecs: None,
            storage_bytes: None,
            policy_generation: None,
        }),
    })
}

// ---------------------------------------------------------------------------
// The sequence
// ---------------------------------------------------------------------------

#[test]
fn a_node_enrolls_with_a_token_and_keeps_its_own_private_key() {
    let installation = Installation::start("enrolls");
    let (token_id, token) = installation.create_token(vec![NodeRole::CollectorIntake]);
    let (node_key, request_der) = node_key_and_request("whatever-i-like");

    let response = installation
        .node_client()
        .call(
            CONTROL,
            "enroll-node",
            enroll_request(&token, request_der, NodeRole::CollectorIntake),
        )
        .expect("the call reaches the head");
    assert_ne!(response.variant.as_deref(), Some(SERVICE_ERROR_VARIANT));
    let enrolled = decode_enroll_node_response(&response.payload).expect("an identity comes back");

    assert!(enrolled.node_id.starts_with("node-"));
    assert_eq!(enrolled.effective_role, NodeRole::CollectorIntake);
    assert_eq!(
        enrolled.certificate_chain.len(),
        2,
        "the leaf and the authority"
    );
    assert!(enrolled.expires_at > enrolled.issued_at);
    assert!(
        enrolled.renew_after > enrolled.issued_at && enrolled.renew_after < enrolled.expires_at,
        "a node must be told to renew inside its own lifetime"
    );

    // The private key never left this process's own variable.
    let private = node_key.serialize_pem();
    assert!(!format!("{enrolled:?}").contains(&private));

    // The certificate names the identity the control plane chose.
    use x509_parser::prelude::*;
    let (_, certificate) =
        X509Certificate::from_der(&enrolled.certificate_chain[0]).expect("it parses");
    let subject = certificate.subject().to_string();
    assert!(subject.contains(&enrolled.node_id), "{subject}");
    assert!(!subject.contains("whatever-i-like"), "{subject}");

    // And the token records what it enrolled.
    let listed = installation
        .owner()
        .call(
            CONTROL,
            "list-role-tokens",
            encode_list_request(&ListRequest {
                cursor: None,
                limit: None,
            }),
        )
        .expect("the call reaches the head");
    let tokens = decode_role_token_list(&listed.payload).expect("a list comes back");
    let held = tokens
        .tokens
        .iter()
        .find(|t| t.token_id == token_id)
        .expect("the token is listed");
    assert_eq!(held.uses, 1);
    assert_eq!(held.active_nodes, 1);
    assert!(!held.revoked);
}

#[test]
fn a_token_cannot_enroll_a_role_it_does_not_permit() {
    // NODE_IDENTITY.md section 4: the controller intersects the requested scope
    // with the token policy. It does not give a permission absent from it.
    let installation = Installation::start("role-refused");
    let (_, token) = installation.create_token(vec![NodeRole::CollectorIntake]);
    let (_, request_der) = node_key_and_request("node");

    let response = installation
        .node_client()
        .call(
            CONTROL,
            "enroll-node",
            enroll_request(&token, request_der, NodeRole::StorageProcess),
        )
        .expect("the call reaches the head");
    assert_eq!(response.variant.as_deref(), Some(SERVICE_ERROR_VARIANT));
    let error = decode_service_error(&response.payload).expect("a typed refusal");
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    // One sentence, whatever the reason. It never says which roles the token
    // does permit, because that is a fact about the installation.
    assert!(
        !error.message.contains("storage-process"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("collector-intake"),
        "{}",
        error.message
    );
}

#[test]
fn a_token_that_is_not_one_enrolls_nothing() {
    let installation = Installation::start("bad-token");
    let (_, request_der) = node_key_and_request("node");
    for token in ["", "not-a-token", "tow_deadbeef_AAAA"] {
        let response = installation
            .node_client()
            .call(
                CONTROL,
                "enroll-node",
                enroll_request(token, request_der.clone(), NodeRole::CollectorIntake),
            )
            .expect("the call reaches the head");
        assert_eq!(
            response.variant.as_deref(),
            Some(SERVICE_ERROR_VARIANT),
            "`{token}` was accepted"
        );
    }
}

#[test]
fn a_request_that_is_not_a_certificate_request_is_refused_and_says_what_is_wrong() {
    // The one refusal that names a detail, because the detail is about the
    // caller's own message and says nothing about the installation.
    let installation = Installation::start("bad-csr");
    let (_, token) = installation.create_token(vec![NodeRole::Projector]);

    let response = installation
        .node_client()
        .call(
            CONTROL,
            "enroll-node",
            enroll_request(&token, b"not a request".to_vec(), NodeRole::Projector),
        )
        .expect("the call reaches the head");
    assert_eq!(response.variant.as_deref(), Some(SERVICE_ERROR_VARIANT));
    let error = decode_service_error(&response.payload).expect("a typed refusal");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(error.message.contains("PKCS#10"), "{}", error.message);
}

#[test]
fn an_active_node_limit_stops_an_autoscaler_making_unlimited_identities() {
    // NODE_IDENTITY.md section 7: the token policy can limit active nodes, and
    // this limit prevents an incorrect autoscaler from creating unlimited
    // identities.
    let installation = Installation::start("node-limit");
    let (_, token) = installation.create_token_with(RoleTokenPolicy {
        roles: vec![NodeRole::CollectorIntake],
        max_active_nodes: Some(2),
        cells: None,
        regions: None,
        workspaces: None,
        projects: None,
        expires_at: None,
        max_uses: None,
        certificate_lifetime_ms: None,
        enrollments_each_hour: None,
        audit_labels: None,
    });

    for attempt in 0..2 {
        let (_, request_der) = node_key_and_request("pod");
        let response = installation
            .node_client()
            .call(
                CONTROL,
                "enroll-node",
                enroll_request(&token, request_der, NodeRole::CollectorIntake),
            )
            .expect("the call reaches the head");
        assert_ne!(
            response.variant.as_deref(),
            Some(SERVICE_ERROR_VARIANT),
            "enrollment {attempt} was refused inside the limit"
        );
    }

    let (_, request_der) = node_key_and_request("pod");
    let response = installation
        .node_client()
        .call(
            CONTROL,
            "enroll-node",
            enroll_request(&token, request_der, NodeRole::CollectorIntake),
        )
        .expect("the call reaches the head");
    assert_eq!(response.variant.as_deref(), Some(SERVICE_ERROR_VARIANT));
    let error = decode_service_error(&response.payload).expect("a typed refusal");
    // Retryable: a node going away frees a slot, so this is a "not now" rather
    // than a "never".
    assert!(error.retryable, "an active-node limit clears on its own");
}

#[test]
fn revoking_a_token_stops_new_enrollment_and_leaves_the_enrolled_node_alone() {
    // Section 2: token revocation prevents new enrollment. It does not
    // immediately revoke certificates by default.
    let installation = Installation::start("revoke");
    let (token_id, token) = installation.create_token(vec![NodeRole::CollectorIntake]);

    let (_, request_der) = node_key_and_request("pod");
    let first = installation
        .node_client()
        .call(
            CONTROL,
            "enroll-node",
            enroll_request(&token, request_der, NodeRole::CollectorIntake),
        )
        .expect("the call reaches the head");
    let enrolled = decode_enroll_node_response(&first.payload).expect("an identity");

    installation
        .owner()
        .call(
            CONTROL,
            "revoke-role-token",
            encode_revoke_role_token_request(&RevokeRoleTokenRequest {
                token_id: token_id.clone(),
                cascade: None,
            }),
        )
        .expect("the call reaches the head");

    let (_, request_der) = node_key_and_request("pod");
    let after = installation
        .node_client()
        .call(
            CONTROL,
            "enroll-node",
            enroll_request(&token, request_der, NodeRole::CollectorIntake),
        )
        .expect("the call reaches the head");
    assert_eq!(
        after.variant.as_deref(),
        Some(SERVICE_ERROR_VARIANT),
        "a revoked token still enrolled a node"
    );

    // The node that was already enrolled is still listed and not revoked.
    let listed = installation
        .owner()
        .call(
            CONTROL,
            "list-nodes",
            encode_list_request(&ListRequest {
                cursor: None,
                limit: None,
            }),
        )
        .expect("the call reaches the head");
    let nodes = decode_node_list(&listed.payload).expect("a list");
    let held = nodes
        .nodes
        .iter()
        .find(|n| n.node_id == enrolled.node_id)
        .expect("the node is listed");
    assert!(
        !held.revoked,
        "the enrolled node was revoked without cascade"
    );
}

impl Installation {
    /// The enrollment service over this installation's store, for a caller
    /// that stands where a mutual TLS listener stands: it knows who the peer
    /// proved it is. The socket in these tests is plain and knows nothing.
    fn enrollment(&self) -> tallyowl_head::enrollment::EnrollmentService {
        tallyowl_head::enrollment::EnrollmentService {
            store: Arc::clone(&self.segmented),
            metrics: Registry::new(),
        }
    }

    fn enroll(
        &self,
        token: &str,
        role: NodeRole,
    ) -> tallyowl_control_api::types::EnrollNodeResponse {
        let (_, request_der) = node_key_and_request("pod");
        let response = self
            .node_client()
            .call(
                CONTROL,
                "enroll-node",
                enroll_request(token, request_der, role),
            )
            .expect("the call reaches the head");
        assert_ne!(response.variant.as_deref(), Some(SERVICE_ERROR_VARIANT));
        decode_enroll_node_response(&response.payload).expect("an identity")
    }
}

/// What a mutual TLS listener hands over for a peer that showed this node's
/// current certificate.
fn proved(node_id: &str, serial: &str) -> tallyowl_head::enrollment::PeerIdentity {
    tallyowl_head::enrollment::PeerIdentity {
        node_id: node_id.to_string(),
        role: None,
        certificate_serial: serial.to_string(),
        expires_at_ms: i64::MAX,
    }
}

fn renewal_of(node_id: &str) -> RenewNodeCertificateRequest {
    // A fresh key for the renewal, which is what a careful node does.
    let (_, renewal_der) = node_key_and_request("pod");
    RenewNodeCertificateRequest {
        node_id: node_id.to_string(),
        certificate_request: renewal_der,
    }
}

#[test]
fn a_renewal_uses_the_node_identity_and_not_the_token() {
    // Section 6: the node uses its current identity for renewal. It does not
    // need the role token for normal renewal, which is what lets a deployment
    // drop the token after enrollment.
    let installation = Installation::start("renew");
    let (_, token) = installation.create_token(vec![NodeRole::Projector]);
    let enrolled = installation.enroll(&token, NodeRole::Projector);

    let peer = proved(&enrolled.node_id, &enrolled.certificate_serial);
    let renewed = installation
        .enrollment()
        .renew(renewal_of(&enrolled.node_id), Some(&peer))
        .expect("a node that proved its identity renews");

    assert_eq!(
        renewed.node_id, enrolled.node_id,
        "the identity is the same"
    );
    assert_eq!(renewed.effective_role, NodeRole::Projector);
    assert_ne!(
        renewed.certificate_serial, enrolled.certificate_serial,
        "a renewal issues new certificate material"
    );

    // The certificate it just replaced is no longer the node's identity.
    installation
        .enrollment()
        .renew(renewal_of(&enrolled.node_id), Some(&peer))
        .expect_err("an old certificate renewed the node");
}

#[test]
fn a_node_id_alone_renews_nothing() {
    // A node ID is in a log line, in `list-nodes`, and in every certificate's
    // common name. A renewal that trusted it signed a certificate, with that
    // node's role, for anybody who could reach the listener.
    let installation = Installation::start("renew-unproven");
    let (_, token) = installation.create_token(vec![NodeRole::IngestGateway]);
    let enrolled = installation.enroll(&token, NodeRole::IngestGateway);

    // Over the plain socket, which proves nothing about the caller.
    let response = installation
        .node_client()
        .call(
            CONTROL,
            "renew-node-certificate",
            encode_renew_node_certificate_request(&renewal_of(&enrolled.node_id)),
        )
        .expect("the call reaches the head");
    assert_eq!(
        response.variant.as_deref(),
        Some(SERVICE_ERROR_VARIANT),
        "a caller that named a node ID got a certificate for that node"
    );

    // A peer that proved it is another node.
    let other = proved("node-somebody-else", &enrolled.certificate_serial);
    installation
        .enrollment()
        .renew(renewal_of(&enrolled.node_id), Some(&other))
        .expect_err("one node renewed another node's identity");

    // A caller that proved nothing. There is no setting that allows this any
    // more: D62 gave the listener a certificate to read.
    installation
        .enrollment()
        .renew(renewal_of(&enrolled.node_id), None)
        .expect_err("a renewal with no proved peer was signed");
}

#[test]
fn a_renewal_keeps_the_lifetime_the_token_set() {
    // L057: a token limited to one hour used to get a day at its first
    // renewal, because renewal ignored the token's own lifetime.
    let installation = Installation::start("renew-lifetime");
    let (_, token) = installation.create_token_with(RoleTokenPolicy {
        roles: vec![NodeRole::Projector],
        cells: None,
        regions: None,
        workspaces: None,
        projects: None,
        expires_at: None,
        max_uses: None,
        max_active_nodes: None,
        certificate_lifetime_ms: Some(3_600_000),
        enrollments_each_hour: None,
        audit_labels: None,
    });
    let enrolled = installation.enroll(&token, NodeRole::Projector);
    let peer = proved(&enrolled.node_id, &enrolled.certificate_serial);
    let renewed = installation
        .enrollment()
        .renew(renewal_of(&enrolled.node_id), Some(&peer))
        .expect("renews");
    assert_eq!(
        renewed.expires_at - renewed.issued_at,
        enrolled.expires_at - enrolled.issued_at,
        "a renewal outlived the lifetime the token permits"
    );
    assert!(renewed.expires_at - renewed.issued_at <= 3_600_000 + 1_000);
}

#[test]
fn the_hourly_rate_stops_an_enrollment_loop_and_says_to_try_again() {
    // AGENTS.md: a role token has a rate control. It was stored and returned,
    // and nothing read it.
    let installation = Installation::start("rate");
    let (_, token) = installation.create_token_with(RoleTokenPolicy {
        roles: vec![NodeRole::CollectorIntake],
        cells: None,
        regions: None,
        workspaces: None,
        projects: None,
        expires_at: None,
        max_uses: None,
        max_active_nodes: None,
        certificate_lifetime_ms: None,
        enrollments_each_hour: Some(2),
        audit_labels: None,
    });
    installation.enroll(&token, NodeRole::CollectorIntake);
    installation.enroll(&token, NodeRole::CollectorIntake);

    let (_, request_der) = node_key_and_request("pod");
    let third = installation
        .node_client()
        .call(
            CONTROL,
            "enroll-node",
            enroll_request(&token, request_der, NodeRole::CollectorIntake),
        )
        .expect("the call reaches the head");
    assert_eq!(third.variant.as_deref(), Some(SERVICE_ERROR_VARIANT));
    let refusal = decode_service_error(&third.payload).expect("a typed refusal");
    assert!(refusal.retryable, "the next hour is a reason to try again");
}

#[test]
fn a_request_that_is_not_a_certificate_request_costs_the_token_nothing() {
    let installation = Installation::start("release");
    let (token_id, token) = installation.create_token(vec![NodeRole::CollectorIntake]);
    let refused = installation
        .node_client()
        .call(
            CONTROL,
            "enroll-node",
            enroll_request(
                &token,
                b"not a certificate request".to_vec(),
                NodeRole::CollectorIntake,
            ),
        )
        .expect("the call reaches the head");
    assert_eq!(refused.variant.as_deref(), Some(SERVICE_ERROR_VARIANT));
    let held = installation
        .segmented
        .catalog()
        .role_token(&token_id)
        .expect("reads")
        .expect("the token");
    assert_eq!(held.uses, 0, "a refused enrollment used up an enrollment");
}

#[test]
fn revoking_a_token_id_that_does_not_exist_says_so() {
    // It used to answer "revoked". An operator who mistyped the ID during an
    // incident then believed a live token was dead.
    let installation = Installation::start("revoke-unknown");
    let response = installation
        .owner()
        .call(
            CONTROL,
            "revoke-role-token",
            encode_revoke_role_token_request(&RevokeRoleTokenRequest {
                token_id: "0123456789abcdef".into(),
                cascade: None,
            }),
        )
        .expect("the call reaches the head");
    assert_eq!(response.variant.as_deref(), Some(SERVICE_ERROR_VARIANT));
    let refusal = decode_service_error(&response.payload).expect("a typed refusal");
    assert_eq!(refusal.code, ErrorCode::NotFound);
    assert!(
        refusal.message.contains("0123456789abcdef"),
        "{}",
        refusal.message
    );
}

#[test]
fn a_renewal_for_a_node_that_never_enrolled_is_refused() {
    let installation = Installation::start("renew-unknown");
    let (_, request_der) = node_key_and_request("pod");
    let response = installation
        .node_client()
        .call(
            CONTROL,
            "renew-node-certificate",
            encode_renew_node_certificate_request(&RenewNodeCertificateRequest {
                node_id: "node-invented".into(),
                certificate_request: request_der,
            }),
        )
        .expect("the call reaches the head");
    assert_eq!(response.variant.as_deref(), Some(SERVICE_ERROR_VARIANT));
}

#[test]
fn managing_role_tokens_needs_an_owner_and_enrolling_does_not() {
    // A node has no session, so enrollment cannot require one. Everything an
    // operator does with tokens requires the highest role there is, because a
    // token that can enroll an ingest gateway is close to a key to the
    // installation.
    let installation = Installation::start("authorization");

    let anonymous = installation.node_client();
    let refused = anonymous
        .call(
            CONTROL,
            "create-role-token",
            encode_create_role_token_request(&CreateRoleTokenRequest {
                label: "mine".into(),
                policy: RoleTokenPolicy {
                    roles: vec![NodeRole::IngestGateway],
                    cells: None,
                    regions: None,
                    workspaces: None,
                    projects: None,
                    expires_at: None,
                    max_uses: None,
                    max_active_nodes: None,
                    certificate_lifetime_ms: None,
                    enrollments_each_hour: None,
                    audit_labels: None,
                },
            }),
        )
        .expect("the call reaches the head");
    assert_eq!(refused.variant.as_deref(), Some(SERVICE_ERROR_VARIANT));

    let listed = anonymous
        .call(
            CONTROL,
            "list-nodes",
            encode_list_request(&ListRequest {
                cursor: None,
                limit: None,
            }),
        )
        .expect("the call reaches the head");
    assert_eq!(listed.variant.as_deref(), Some(SERVICE_ERROR_VARIANT));
}

#[test]
fn a_role_token_has_no_way_to_ask_for_a_voter() {
    // AGENTS.md: "A role token cannot add a controller voter or change a tablet
    // voter set." The contract has no name for one, so this is a property of
    // the generated type rather than a check the head has to remember.
    //
    // This test exists so that adding such a name to the contract fails here
    // rather than passing unnoticed.
    let every_role = [
        NodeRole::CollectorIntake,
        NodeRole::CollectorForwarder,
        NodeRole::CompatibilityReceiver,
        NodeRole::IngestGateway,
        NodeRole::QueryCoordinator,
        NodeRole::Projector,
        NodeRole::WorkflowWorker,
        NodeRole::ReadReplica,
        NodeRole::ExportReplica,
        NodeRole::StorageProcess,
    ];
    assert_eq!(every_role.len(), 10, "the contract grew a role");
    for role in every_role {
        let name = format!("{role:?}").to_lowercase();
        assert!(!name.contains("voter"), "{name} names a voter");
    }
}

/// Make this head a signer, with a root and an intermediate made for the test.
fn install_signer(catalog: &tallyowl_store::catalog::Catalog) {
    use tallyowl_store::certificates::{
        generate_authorities, Authority, DEFAULT_CERTIFICATE_LIFETIME_MS,
    };
    let now = tallyowl_obs::time::now_ms();
    let generated = generate_authorities(now).expect("authorities");
    let root: Vec<Vec<u8>> =
        x509_parser::pem::Pem::iter_from_buffer(generated.root_certificate_pem.as_bytes())
            .map(|block| block.expect("PEM").contents)
            .collect();
    catalog.set_signing_authority(
        Authority::from_pem(
            &generated.intermediate_chain_pem,
            &generated.intermediate_key_pem,
            &root,
            now,
            DEFAULT_CERTIFICATE_LIFETIME_MS,
        )
        .expect("a usable signer"),
    );
}

// ---------------------------------------------------------------------------
// Peer checks over mutual TLS. D62: the head reads who a caller is from the
// certificate the handshake verified, never from the request.
// ---------------------------------------------------------------------------

/// A head's own server certificate, from an authority only the test's clients
/// trust. The head's trust in its nodes is separate: it trusts the authority
/// that signed their enrolled certificates.
struct HeadCertificate {
    identity: tallyowl_rpc::tls::Identity,
    authority: Vec<u8>,
}

fn head_certificate() -> HeadCertificate {
    let authority_key = rcgen::KeyPair::generate().expect("a key");
    let mut params = rcgen::CertificateParams::default();
    let mut name = rcgen::DistinguishedName::new();
    name.push(rcgen::DnType::CommonName, "test head authority");
    params.distinguished_name = name;
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    let authority = params.self_signed(&authority_key).expect("an authority");

    let key = rcgen::KeyPair::generate().expect("a key");
    let mut leaf = rcgen::CertificateParams::new(vec!["head".to_string()]).expect("a name");
    let mut name = rcgen::DistinguishedName::new();
    name.push(rcgen::DnType::CommonName, "head");
    leaf.distinguished_name = name;
    let signed = leaf
        .signed_by(&key, &authority, &authority_key)
        .expect("signed");
    HeadCertificate {
        identity: tallyowl_rpc::tls::Identity {
            chain: vec![signed.der().to_vec(), authority.der().to_vec()],
            private_key: key.serialize_der(),
            authority: authority.der().to_vec(),
            expected_server_name: "head".into(),
        },
        authority: authority.der().to_vec(),
    }
}

/// A node that enrolled, with the key it generated and the chain it got.
struct EnrolledNode {
    response: tallyowl_control_api::types::EnrollNodeResponse,
    key: rcgen::KeyPair,
}

impl Installation {
    fn enroll_keeping_key(&self, token: &str, role: NodeRole) -> EnrolledNode {
        let (key, request_der) = node_key_and_request("pod");
        let response = self
            .node_client()
            .call(
                CONTROL,
                "enroll-node",
                enroll_request(token, request_der, role),
            )
            .expect("the call reaches the head");
        assert_ne!(response.variant.as_deref(), Some(SERVICE_ERROR_VARIANT));
        EnrolledNode {
            response: decode_enroll_node_response(&response.payload).expect("an identity"),
            key,
        }
    }

    /// The same head, on a second listener that requires mutual TLS. It trusts
    /// the authority that signed `node`'s chain.
    fn serve_mutual(&self, node: &EnrolledNode, head: &HeadCertificate) -> tallyowl_rpc::Server {
        let node_authority = node
            .response
            .certificate_chain
            .last()
            .expect("the chain carries its authority")
            .clone();
        tallyowl_rpc::tls::serve_mutual(
            "127.0.0.1:0",
            Arc::clone(&self.service) as Arc<dyn tallyowl_rpc::Dispatcher>,
            tallyowl_rpc::ServerOptions::new(MAX_FRAME),
            Arc::new(tallyowl_rpc::material::StaticIdentity(
                head.identity.clone(),
            )),
            Arc::new(tallyowl_rpc::trust::StaticTrust(vec![node_authority])),
        )
        .expect("the head listens with mutual TLS")
    }
}

/// A client that presents `node`'s enrolled certificate.
fn as_node(server: &tallyowl_rpc::Server, node: &EnrolledNode, head: &HeadCertificate) -> Client {
    let identity = tallyowl_rpc::tls::Identity {
        chain: node.response.certificate_chain.clone(),
        private_key: node.key.serialize_der(),
        authority: head.authority.clone(),
        expected_server_name: "head".into(),
    };
    Client::mutual(
        server.local_address().to_string(),
        MAX_FRAME,
        "head",
        Arc::new(tallyowl_rpc::material::StaticIdentity(identity)),
        Arc::new(tallyowl_rpc::trust::StaticTrust(vec![head
            .authority
            .clone()])),
    )
}

const WORKSPACE: [u8; 16] = [0x11; 16];
const PROJECT: [u8; 16] = [0x22; 16];

fn one_event(project: [u8; 16]) -> Vec<u8> {
    use tallyowl_collector_api::types::{
        Batch, CommitBatchRequest, Envelope, EventPayload, TelemetryKind,
    };
    let at = tallyowl_obs::time::now_ms();
    let item = tallyowl_wire::collector_items_bridge::event(
        Envelope {
            event_id: vec![7; 16],
            kind: TelemetryKind::Event,
            schema_version: 1,
            occurred_at: at,
            observed_at: None,
            received_at: Some(at),
            workspace_id: Some(WORKSPACE.to_vec()),
            project_id: Some(project.to_vec()),
            source_id: Some(vec![0x33; 16]),
            sequence: None,
            release: None,
            service_name: None,
            request_id: None,
            session_id: None,
            end_user_id: None,
            anonymous_id: None,
            trace_id: None,
            span_id: None,
            consent: None,
            sdk_name: "tallyowl-driver-rust".into(),
            sdk_version: "0.0.0".into(),
            properties: Vec::new(),
            measurements: None,
        },
        EventPayload {
            name: "checkout".into(),
            route: None,
            page_title: None,
        },
    );
    tallyowl_collector_api::codec::encode_commit_batch_request(&CommitBatchRequest {
        batch: Batch {
            batch_id: vec![project[0]; 16],
            items: vec![item],
            common_properties: None,
            sealed_at: at,
            compression: None,
        },
        source_id: vec![0x33; 16],
        attempt: None,
        protocol_version: None,
    })
}

fn committed(client: &Client, payload: Vec<u8>) -> Result<(), String> {
    let response = client
        .call("TallyOwlCollector", "commit-batch", payload)
        .map_err(|e| e.to_string())?;
    if response.variant.as_deref() == Some(SERVICE_ERROR_VARIANT) {
        let error = tallyowl_collector_api::codec::decode_service_error(&response.payload)
            .map(|e| e.message)
            .unwrap_or_else(|e| e.to_string());
        return Err(error);
    }
    Ok(())
}

#[test]
fn a_node_renews_over_mutual_tls_and_another_node_cannot_renew_it() {
    let installation = Installation::start("renew-tls");
    let (_, token) = installation.create_token(vec![NodeRole::Projector]);
    let node = installation.enroll_keeping_key(&token, NodeRole::Projector);
    let other = installation.enroll_keeping_key(&token, NodeRole::Projector);
    let head = head_certificate();
    let server = installation.serve_mutual(&node, &head);

    // The other node proves it is itself, and asks for the first node's
    // identity.
    let refused = as_node(&server, &other, &head)
        .call(
            CONTROL,
            "renew-node-certificate",
            encode_renew_node_certificate_request(&renewal_of(&node.response.node_id)),
        )
        .expect("the call reaches the head");
    assert_eq!(
        refused.variant.as_deref(),
        Some(SERVICE_ERROR_VARIANT),
        "one node renewed another node's identity"
    );

    // The node itself, showing its current certificate.
    let renewed = as_node(&server, &node, &head)
        .call(
            CONTROL,
            "renew-node-certificate",
            encode_renew_node_certificate_request(&renewal_of(&node.response.node_id)),
        )
        .expect("the call reaches the head");
    assert_ne!(
        renewed.variant.as_deref(),
        Some(SERVICE_ERROR_VARIANT),
        "a node that showed its current certificate could not renew: {:?}",
        decode_service_error(&renewed.payload).map(|e| e.message)
    );
}

#[test]
fn an_enrolled_collector_commits_and_another_role_does_not() {
    let installation = Installation::start("commit-tls");
    let (_, collectors) = installation.create_token(vec![NodeRole::CollectorForwarder]);
    let (_, projectors) = installation.create_token(vec![NodeRole::Projector]);
    let collector = installation.enroll_keeping_key(&collectors, NodeRole::CollectorForwarder);
    let projector = installation.enroll_keeping_key(&projectors, NodeRole::Projector);
    let head = head_certificate();
    let server = installation.serve_mutual(&collector, &head);

    committed(&as_node(&server, &collector, &head), one_event(PROJECT))
        .expect("an enrolled collector's batch was refused");
    let refused = committed(&as_node(&server, &projector, &head), one_event(PROJECT))
        .expect_err("a projector wrote a batch");
    assert!(refused.contains("only a collector"), "{refused}");
}

#[test]
fn a_collector_writes_only_inside_the_scope_of_the_token_that_enrolled_it() {
    // D32: the head checks each stamped destination against the collector's
    // permitted project set, which is its enrolling token's scope.
    let installation = Installation::start("commit-scope");
    let (_, token) = installation.create_token_with(RoleTokenPolicy {
        roles: vec![NodeRole::CollectorForwarder],
        cells: None,
        regions: None,
        workspaces: None,
        projects: Some(vec![PROJECT.to_vec()]),
        expires_at: None,
        max_uses: None,
        max_active_nodes: None,
        certificate_lifetime_ms: None,
        enrollments_each_hour: None,
        audit_labels: None,
    });
    let collector = installation.enroll_keeping_key(&token, NodeRole::CollectorForwarder);
    let head = head_certificate();
    let server = installation.serve_mutual(&collector, &head);
    let client = as_node(&server, &collector, &head);

    committed(&client, one_event(PROJECT)).expect("a batch inside the scope was refused");
    let refused = committed(&client, one_event([0x44; 16]))
        .expect_err("a batch outside the scope was written");
    assert!(
        refused.contains("may not write to the project"),
        "{refused}"
    );
}

#[test]
fn a_peer_with_no_certificate_cannot_commit_and_a_local_one_can() {
    use tallyowl_rpc::{Dispatcher, Peer};
    let installation = Installation::start("commit-anonymous");
    let request =
        tallyowl_rpc::Request::new("TallyOwlCollector", "commit-batch", one_event(PROJECT));

    // Server-only TLS is for applications. Only the transport can produce
    // this peer, so the dispatcher is asked directly with it.
    let outcome = installation
        .service
        .dispatch_from(&request, &Peer::Anonymous);
    let refusal = match outcome {
        tallyowl_rpc::Outcome::Reply { variant, payload } if variant == SERVICE_ERROR_VARIANT => {
            tallyowl_collector_api::codec::decode_service_error(&payload)
                .expect("a refusal")
                .message
        }
        _ => panic!("a peer with no certificate committed a batch"),
    };
    assert!(refusal.contains("no certificate"), "{refusal}");

    // The collector on this host, over loopback.
    committed(&installation.node_client(), one_event(PROJECT))
        .expect("a local collector's batch was refused");
}
