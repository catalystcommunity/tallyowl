//! The walking skeleton, end to end.
//!
//! An event sent by the driver reaches the head through the durable store, and a
//! query returns it. That is the Phase 1 exit criterion, and this is the test
//! that holds it.
//!
//! Every boundary that matters is real:
//!
//! - the driver reaches collector intake over a real socket, speaking CSIL-RPC;
//! - the collector puts the batch in a durable store before it acknowledges;
//! - the forwarder claims it and reaches the head over a second real socket;
//! - the head projects, commits to a real directory, and answers a real query.
//!
//! The one stand-in is the durable queue, and it stands in for Corndogs rather
//! than for TallyOwl's own storage. `AGENTS.md` forbids mocking the storage
//! interface, and this does not: the store under the head is the real
//! `SegmentedStore` writing real files.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tallyowl_collector::durable::testing::FakeQueue;
use tallyowl_collector::durable::DurableQueue;
use tallyowl_collector::forwarder::{Forwarder, ForwarderState, DURABLE_STORE_CHECK, SWEEP_CHECK};
use tallyowl_collector::head_client::HeadClient;
use tallyowl_collector::head_client::RemoteHead;
use tallyowl_collector::intake::{Intake, Limits};
use tallyowl_collector::service::CollectorService;
use tallyowl_collector::tenancy::{KeyDirectory, TenancyResolver};

use tallyowl_collector_api::types::ReceiptPolicy;
use tallyowl_control_api::codec::{decode_query_response, encode_query_request};
use tallyowl_control_api::types::{TimeBasis, TimeRange};
use tallyowl_driver_rust::{Capture, Driver, Settings};
use tallyowl_head::ingest::Ingest;
use tallyowl_head::query::QueryService;
use tallyowl_head::service::HeadService;
use tallyowl_obs::health::Health;
use tallyowl_obs::log::{Logger, Severity};
use tallyowl_obs::metrics::Registry;
use tallyowl_store::{SegmentedStore, Store};
use tallyowl_wire::{control as wire, query, Value};

const MAX_FRAME: usize = 16 * 1024 * 1024;
const QUEUE: &str = "tallyowl-delivery";

fn temporary_directory(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("round-trip")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// One home installation, in one process, over real sockets.
/// Open the data directory, waiting for a previous installation to release it.
///
/// One process owns one data directory, and the catalog takes an exclusive lock
/// to hold that. A test that restarts an installation is the same situation a
/// container restart produces: the replacement can start before the old one has
/// finished exiting, so the open waits a bounded time rather than failing.
fn open_store(directory: &Path) -> Arc<SegmentedStore> {
    Arc::new(
        SegmentedStore::open_waiting(directory, Duration::from_secs(5)).expect("the store opens"),
    )
}

struct Installation {
    collector: tallyowl_rpc::Server,
    head: Option<tallyowl_rpc::Server>,
    forwarder: Arc<Forwarder>,
    queue: Arc<FakeQueue>,
    store: Arc<dyn Store>,
    /// The connection both collector roles share with the head. A test that
    /// takes the head away has to take this with it, because a socket that is
    /// already open outlives the listener.
    head_client: Arc<RemoteHead>,
    /// The store as its own type, so a test can reach the control catalog.
    segmented: Arc<SegmentedStore>,
    /// Intake's resolver, so a revocation test can end the cache period without
    /// sleeping through it.
    tenancy: Arc<TenancyResolver>,
    /// The credential the head issued for this installation's project, and the
    /// project it resolves to. An application holds the first and never learns
    /// the second.
    credential: String,
    project_id: [u8; 16],
}

impl Installation {
    fn start(name: &str) -> Installation {
        Installation::start_in(temporary_directory(name))
    }

    /// Start against an existing directory, so a test can restart the head and
    /// prove that committed data survived.
    fn start_in(directory: PathBuf) -> Installation {
        let logger = Arc::new(Logger::new("test", "0.0.0", Severity::Error));

        // ---- the head, on its own socket ----
        let segmented = open_store(&directory);
        let store: Arc<dyn Store> = Arc::clone(&segmented) as Arc<dyn Store>;
        // The head issues the credential, because the head owns the control
        // catalog. A restart reuses the project and issues another key, which
        // is what a rotation does.
        let issued = segmented
            .catalog()
            .provision("default", "round-trip", tallyowl_obs::time::now_ms())
            .expect("the head provisions a project");
        let head_metrics = Registry::new();
        Ingest::declare_metrics(&head_metrics);
        let head = tallyowl_rpc::serve(
            "127.0.0.1:0",
            Arc::new(HeadService {
                ingest: Arc::new(Ingest {
                    golden_signal_bucket_ms: 60_000,
                    store: Arc::clone(&store),
                    metrics: head_metrics,
                    receipt_policy: ReceiptPolicy::LocalOne,
                    open_traces: None,
                    policy: None,
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
            }),
            MAX_FRAME,
        )
        .expect("the head listens");

        // ---- the durable store between them ----
        let queue = FakeQueue::new();

        // ---- the collector: intake on a socket, forwarder driven by the test ----
        let health = Health::new();
        health.declare(SWEEP_CHECK, "not yet");
        health.declare(DURABLE_STORE_CHECK, "not yet");
        let collector_metrics = Registry::new();
        Intake::declare_metrics(&collector_metrics);
        Forwarder::declare_metrics(&collector_metrics);

        // One client to the head: the forwarder commits through it and intake
        // resolves credentials through it.
        let head_client = Arc::new(RemoteHead::new(
            &head.local_address().to_string(),
            MAX_FRAME,
        ));

        let tenancy = Arc::new(TenancyResolver::new(
            Arc::clone(&head_client) as Arc<dyn KeyDirectory>,
            60_000,
        ));
        let intake = Arc::new(Intake {
            queue: Arc::clone(&queue) as Arc<dyn DurableQueue>,
            queue_name: QUEUE.into(),
            tenancy: Arc::clone(&tenancy),
            limits: Limits {
                max_batch_bytes: 512 * 1024,
                max_event_bytes: 64 * 1024,
                max_properties: 128,
            },
            durable_copies: 1,
            series: std::sync::Arc::new(tallyowl_collector::series::SeriesLedger::new(
                tallyowl_collector::series::SeriesBudget::default(),
            )),
            metrics: Arc::clone(&collector_metrics),
            stamped: vec![
                ("cell".into(), "home".into()),
                ("region".into(), "home".into()),
            ],
            policy: None,
        });

        let forwarder_state = ForwarderState::new();
        let collector = tallyowl_rpc::serve(
            "127.0.0.1:0",
            Arc::new(CollectorService {
                intake,
                health: Arc::clone(&health),
                logger: Arc::clone(&logger),
                forwarder_state: Arc::clone(&forwarder_state),
                credential: issued.credential.clone(),
                roles: vec!["intake".into(), "forwarder".into()],
                policy: None,
            }),
            MAX_FRAME,
        )
        .expect("intake listens");

        let forwarder = Arc::new(Forwarder {
            queue: Arc::clone(&queue) as Arc<dyn DurableQueue>,
            queue_name: QUEUE.into(),
            quarantine_queue: "tallyowl-quarantine".into(),
            // The real client, over the real socket the head is on.
            head: Arc::clone(&head_client) as Arc<dyn HeadClient>,
            health,
            metrics: collector_metrics,
            logger,
            state: forwarder_state,
            sweep_staleness_ms: 3_000,
            max_payload_bytes: 16 * 1024 * 1024,
            max_delivery_age_ms: 24 * 60 * 60 * 1000,
        });

        Installation {
            collector,
            head: Some(head),
            forwarder,
            queue,
            store,
            head_client,
            segmented,
            tenancy,
            credential: issued.credential,
            project_id: issued.key.project_id,
        }
    }

    fn driver(&self) -> Driver {
        let mut settings =
            Settings::new(self.collector.local_address().to_string(), &self.credential);
        // The test decides when a batch is sent, so a linger would only add
        // waiting to every case.
        settings.linger = Duration::from_millis(0);
        Driver::new(settings)
    }

    /// Deliver everything waiting. Returns how many batches moved.
    fn head_address(&self) -> String {
        self.head
            .as_ref()
            .expect("the head is running")
            .local_address()
            .to_string()
    }

    fn deliver_all(&self) -> usize {
        let mut delivered = 0;
        while self.forwarder.deliver_one() {
            delivered += 1;
        }
        delivered
    }

    /// The head's control catalog, for a test that rotates or revokes a key.
    fn catalog(&self) -> &tallyowl_store::catalog::Catalog {
        self.segmented.catalog()
    }

    /// Take the head away, the way a process kill does.
    ///
    /// Stopping the listener alone is not enough any more. Intake resolves a
    /// credential over the same connection the forwarder commits on, so that
    /// connection is already open by the time a batch is flushed, and a
    /// listener that stops accepting does not close it. A head that died closes
    /// both, so the test closes both.
    fn stop_head(&mut self) {
        if let Some(head) = self.head.take() {
            head.stop();
        }
        self.head_client.disconnect();
    }

    /// A client that is signed in as an operator.
    ///
    /// Every control operation checks authorization, so a query needs a
    /// session. The credential travels on the connection, which is how the
    /// head tells a person from an application.
    fn query_client(&self) -> tallyowl_rpc::Client {
        tallyowl_rpc::Client::new(self.head_address(), MAX_FRAME)
            .with_credential(self.session_token())
    }

    /// A client with no session at all.
    fn anonymous_client(&self) -> tallyowl_rpc::Client {
        tallyowl_rpc::Client::new(self.head_address(), MAX_FRAME)
    }

    fn session_token(&self) -> String {
        self.segmented
            .catalog()
            .issue_operator_session("operator", tallyowl_obs::time::now_ms(), 3_600_000)
            .expect("a session")
            .token
    }
}

/// The trend query a dashboard runs, for one project.
fn trend_request(project_id: &[u8], range: (i64, i64), bucket_ms: i64) -> Vec<u8> {
    encode_query_request(&query::trend(
        1,
        query::events(
            project_id,
            TimeRange {
                range_start: range.0,
                range_end: range.1,
                basis: TimeBasis::OccurredAt,
                timezone: None,
            },
        ),
        bucket_ms,
        "events",
    ))
}

// ---------------------------------------------------------------------------
// The exit criterion
// ---------------------------------------------------------------------------

#[test]
fn an_event_travels_from_the_driver_to_the_head_and_a_query_returns_it() {
    let installation = Installation::start("round-trip");
    let driver = installation.driver();

    let at = tallyowl_obs::time::now_ms();
    driver
        .capture(Capture::event("checkout-started").at(at))
        .expect("the driver buffers the event");

    // The receipt means the durable store holds the batch, and nothing further.
    let receipt = driver
        .flush()
        .expect("the collector accepts")
        .expect("one batch went");
    assert_eq!(receipt.accepted, 1);
    assert_eq!(receipt.durable_copies, 1);
    assert!(receipt.rejected.is_empty());
    assert_eq!(installation.queue.depth(), 1, "it is in the durable store");

    // Nothing has reached the head yet. The forwarder is what moves it.
    assert_eq!(installation.store.commit_watermark(), 0);

    assert_eq!(installation.deliver_all(), 1);
    assert_eq!(installation.store.commit_watermark(), 1);
    assert_eq!(installation.queue.completed().len(), 1);

    // And a query returns it.
    let project = installation.project_id;
    let response = installation
        .query_client()
        .call(
            "TallyOwlControl",
            "run-query",
            trend_request(&project, (at - 60_000, at + 60_000), 60_000),
        )
        .expect("the query runs");
    let decoded = decode_query_response(&response.payload).expect("the answer decodes");

    assert_eq!(decoded.columns, vec!["bucket", "events"]);
    assert_eq!(decoded.rows.len(), 1);
    assert_eq!(
        wire::read(&decoded.rows[0].values[1]).unwrap(),
        Value::Unsigned(1)
    );
    assert!(decoded.metadata.complete);
    assert_eq!(decoded.metadata.commit_watermark, 1);
    assert!(decoded.metadata.exactness[0].exact);
}

#[test]
fn the_collector_stamps_tenancy_that_the_application_never_sent() {
    // The app never sends a workspace or project ID, and the query still finds
    // its data under one. That is the whole of D32, proved end to end.
    let installation = Installation::start("tenancy");
    let driver = installation.driver();
    let at = tallyowl_obs::time::now_ms();

    driver.capture(Capture::event("page-view").at(at)).unwrap();
    driver.flush().unwrap();
    installation.deliver_all();

    let rows = installation
        .store
        .scan(
            installation.project_id,
            at - 60_000,
            at + 60_000,
            tallyowl_store::TimeBasis::OccurredAt,
        )
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 1);
    assert_ne!(rows[0].workspace_id, [0; 16]);
    assert_ne!(rows[0].project_id, [0; 16]);
    // The collector stamped its own properties, and an operator can tell.
    assert_eq!(rows[0].properties["region"].1, "collector");
}

#[test]
fn many_events_in_one_batch_all_arrive() {
    let installation = Installation::start("many");
    let driver = installation.driver();
    let at = tallyowl_obs::time::now_ms();

    for n in 0..250 {
        driver
            .capture(Capture::event("checkout-started").at(at + n))
            .unwrap();
    }
    let receipt = driver.flush().unwrap().unwrap();
    assert_eq!(receipt.accepted, 250);

    installation.deliver_all();
    assert_eq!(installation.store.row_count(), 250);
}

#[test]
fn a_committed_event_survives_a_restart_of_the_head() {
    // "Accepted data survives abrupt process and host restart." The process
    // here restarts by dropping the whole installation and opening the same
    // directory again, with no clean shutdown of the store in between.
    let directory = temporary_directory("restart");
    let at = tallyowl_obs::time::now_ms();
    {
        let installation = Installation::start_in(directory.clone());
        let driver = installation.driver();
        driver
            .capture(Capture::event("checkout-started").at(at))
            .unwrap();
        driver.flush().unwrap();
        installation.deliver_all();
        assert_eq!(installation.store.commit_watermark(), 1);
    }

    let installation = Installation::start_in(directory);
    let project = installation.project_id;
    let response = installation
        .query_client()
        .call(
            "TallyOwlControl",
            "run-query",
            trend_request(&project, (at - 60_000, at + 60_000), 60_000),
        )
        .unwrap();
    let decoded = decode_query_response(&response.payload).unwrap();
    assert_eq!(
        wire::read(&decoded.rows[0].values[1]).unwrap(),
        Value::Unsigned(1)
    );
}

#[test]
fn a_batch_delivered_twice_counts_once() {
    // At-least-once delivery with logically idempotent ingestion. The forwarder
    // delivers, the completion is lost, the sweep returns the task, and the
    // head deduplicates the batch ID.
    let installation = Installation::start("duplicate");
    let driver = installation.driver();
    let at = tallyowl_obs::time::now_ms();

    driver
        .capture(Capture::event("checkout-started").at(at))
        .unwrap();
    driver.flush().unwrap();

    // Claim it and deliver it by hand, then put it back as a lost completion
    // would leave it.
    let claimed = installation.queue.claim(QUEUE, 30).unwrap().unwrap();
    installation.queue.release_claimed();
    let _ = claimed;

    assert_eq!(installation.deliver_all(), 1);
    let after_first = installation.store.commit_watermark();

    // Deliver the same batch again.
    installation
        .queue
        .submit(QUEUE, claimed.payload, 0)
        .unwrap();
    assert_eq!(installation.deliver_all(), 1);

    assert_eq!(
        installation.store.commit_watermark(),
        after_first,
        "the second delivery did not commit again"
    );
    assert_eq!(installation.store.row_count(), 1, "one logical event");
}

#[test]
fn an_application_never_learns_the_head_address() {
    // The driver is configured with a collector address only. If it could reach
    // the head at all, this test would have had to name it.
    let installation = Installation::start("addresses");
    let driver = installation.driver();
    assert_eq!(
        driver.settings().collector_address,
        installation.collector.local_address().to_string()
    );
    assert_ne!(
        driver.settings().collector_address,
        installation.head_address()
    );
}

#[test]
fn a_query_for_another_project_returns_nothing() {
    let installation = Installation::start("isolation");
    let driver = installation.driver();
    let at = tallyowl_obs::time::now_ms();
    driver.capture(Capture::event("secret").at(at)).unwrap();
    driver.flush().unwrap();
    installation.deliver_all();

    let response = installation
        .query_client()
        .call(
            "TallyOwlControl",
            "run-query",
            trend_request(&[0xaa; 16], (at - 60_000, at + 60_000), 60_000),
        )
        .unwrap();
    // A project this session holds no role in is refused rather than answered
    // with nothing. An empty answer would say the project exists and is empty,
    // and existence is a fact a caller has not authenticated for.
    assert_eq!(
        response.variant.as_deref(),
        Some(tallyowl_rpc::SERVICE_ERROR_VARIANT),
        "another project is refused, not answered"
    );
}

#[test]
fn a_collector_that_cannot_reach_the_durable_store_refuses_rather_than_acknowledging() {
    let installation = Installation::start("refuse");
    let driver = installation.driver();

    installation.queue.refuse(true);
    driver.capture(Capture::event("checkout-started")).unwrap();
    let failure = driver.flush().expect_err("the batch is refused");

    assert_eq!(failure.code, tallyowl_obs::ErrorCode::Unavailable);
    assert!(failure.retryable);
    assert_eq!(installation.store.row_count(), 0);
}

#[test]
fn a_head_that_is_down_holds_the_batch_in_the_durable_store() {
    // "Collector loses head connectivity: Corndogs retains tasks; workers retry
    // with jitter." The data must still be there when the head comes back.
    let mut installation = Installation::start("head-down");
    let driver = installation.driver();
    let at = tallyowl_obs::time::now_ms();

    driver
        .capture(Capture::event("checkout-started").at(at))
        .unwrap();
    driver.flush().unwrap();

    // The head stops answering.
    installation.stop_head();

    installation.forwarder.deliver_one();
    assert_eq!(installation.store.row_count(), 0, "nothing committed");
    assert_eq!(
        installation.queue.parked_count(),
        1,
        "the batch waits for another attempt rather than disappearing"
    );

    // The sweep is what returns it for that attempt.
    installation.forwarder.sweep_once();
    assert_eq!(installation.queue.depth(), 1);
}

// ---------------------------------------------------------------------------
// Key rotation and revocation
//
// Phase 4 failure work. A rotation must never leave an application without a
// working key, and a revocation must actually stop the application it names and
// nothing else.
// ---------------------------------------------------------------------------

#[test]
fn a_rotation_leaves_no_moment_with_no_working_key() {
    let installation = Installation::start("rotation");
    let source = installation
        .catalog()
        .sources()
        .unwrap()
        .pop()
        .expect("provisioning made one source");

    // The operator issues the second key. Both work now, which is the whole
    // point: the applications move at their own pace.
    let second = installation
        .catalog()
        .issue_api_key(&source, "rotation", tallyowl_obs::time::now_ms(), None)
        .expect("a second key");

    for credential in [&installation.credential, &second.credential] {
        let mut settings = Settings::new(
            installation.collector.local_address().to_string(),
            credential,
        );
        settings.linger = Duration::from_millis(0);
        let driver = Driver::new(settings);
        driver.capture(Capture::event("checkout-started")).unwrap();
        driver
            .flush()
            .unwrap_or_else(|e| panic!("both keys work during a rotation: {}", e.message));
    }

    // Both applications wrote to one project, because both keys name one
    // source. A rotation that split the data in two would be worse than an
    // outage, because nobody would notice.
    installation.deliver_all();
    assert_eq!(installation.store.row_count(), 2);
    let rows = installation
        .store
        .scan(
            installation.project_id,
            0,
            tallyowl_obs::time::now_ms() + 1,
            tallyowl_store::TimeBasis::ReceivedAt,
        )
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 2, "one project holds both");
    assert_eq!(rows[0].source_id, rows[1].source_id, "one source");
}

#[test]
fn a_revoked_key_stops_and_its_replacement_carries_on() {
    let installation = Installation::start("revocation");
    let source = installation.catalog().sources().unwrap().pop().unwrap();
    let replacement = installation
        .catalog()
        .issue_api_key(&source, "replacement", tallyowl_obs::time::now_ms(), None)
        .expect("a second key");

    let driver_for = |credential: &str| {
        let mut settings = Settings::new(
            installation.collector.local_address().to_string(),
            credential,
        );
        settings.linger = Duration::from_millis(0);
        Driver::new(settings)
    };

    // Both work before the revocation.
    let old = driver_for(&installation.credential);
    old.capture(Capture::event("before")).unwrap();
    old.flush().unwrap();

    installation
        .catalog()
        .revoke_api_key(
            &installation
                .catalog()
                .api_keys()
                .unwrap()
                .into_iter()
                .find(|k| k.label != "replacement")
                .unwrap()
                .key_id,
            tallyowl_obs::time::now_ms(),
        )
        .unwrap();
    // End the cache period rather than sleeping through it. A revocation takes
    // effect at the end of that period, and how long the period is belongs to
    // the resolver's own tests rather than to this one.
    installation.tenancy.forget_all();

    let refused = driver_for(&installation.credential);
    refused.capture(Capture::event("after")).unwrap();
    let failure = refused.flush().expect_err("the revoked key is refused");
    assert_eq!(failure.code, tallyowl_obs::ErrorCode::Unauthenticated);
    assert!(!failure.retryable, "no retry fixes a revoked key");
    assert!(
        !failure.message.contains(&installation.credential),
        "a refusal never repeats the credential"
    );

    // The replacement is untouched. Revoking one key does not revoke a source.
    let new = driver_for(&replacement.credential);
    new.capture(Capture::event("after")).unwrap();
    new.flush().expect("the replacement still works");

    installation.deliver_all();
    assert_eq!(
        installation.store.row_count(),
        2,
        "the refused batch reached no storage"
    );
}

#[test]
fn a_credential_this_installation_never_issued_reaches_no_project() {
    let installation = Installation::start("unknown-key");
    let mut settings = Settings::new(
        installation.collector.local_address().to_string(),
        "tow_0011223344556677_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    );
    settings.linger = Duration::from_millis(0);
    let driver = Driver::new(settings);
    driver.capture(Capture::event("checkout-started")).unwrap();

    let failure = driver.flush().expect_err("an unknown key is refused");
    assert_eq!(failure.code, tallyowl_obs::ErrorCode::Unauthenticated);
    assert_eq!(installation.store.row_count(), 0);
    assert_eq!(
        installation.queue.depth(),
        0,
        "a refused batch never reaches the durable store"
    );
}

// ---------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------

#[test]
fn a_query_with_no_session_is_refused() {
    // D8 makes the workspace the isolation boundary, and a query names a
    // project. Without this check a caller who guessed a project ID would read
    // another tenant's data.
    let installation = Installation::start("no-session");
    let response = installation
        .anonymous_client()
        .call(
            "TallyOwlControl",
            "run-query",
            trend_request(
                &installation.project_id,
                (0, tallyowl_obs::time::now_ms() + 1),
                60_000,
            ),
        )
        .expect("the call reaches the head");
    assert_eq!(
        response.variant.as_deref(),
        Some(tallyowl_rpc::SERVICE_ERROR_VARIANT),
        "an unauthenticated query is refused rather than answered"
    );
}

#[test]
fn a_source_key_is_not_a_way_into_the_control_plane() {
    // NODE_IDENTITY.md section 1: a source key authorizes telemetry for a
    // project, and a session authenticates a person. Do not use one credential
    // as a replacement for another.
    let installation = Installation::start("key-not-a-session");
    let response = tallyowl_rpc::Client::new(installation.head_address(), MAX_FRAME)
        .with_credential(installation.credential.clone())
        .call(
            "TallyOwlControl",
            "run-query",
            trend_request(
                &installation.project_id,
                (0, tallyowl_obs::time::now_ms() + 1),
                60_000,
            ),
        )
        .expect("the call reaches the head");
    assert_eq!(
        response.variant.as_deref(),
        Some(tallyowl_rpc::SERVICE_ERROR_VARIANT)
    );
}

#[test]
fn a_person_with_no_membership_reads_nothing_and_is_told_so() {
    let installation = Installation::start("no-membership");
    // A real session for somebody who belongs to no workspace.
    let outsider = installation
        .catalog()
        .issue_session(
            "outsider",
            "operator",
            tallyowl_obs::time::now_ms(),
            3_600_000,
        )
        .expect("a session")
        .token;

    let response = tallyowl_rpc::Client::new(installation.head_address(), MAX_FRAME)
        .with_credential(outsider)
        .call(
            "TallyOwlControl",
            "run-query",
            trend_request(
                &installation.project_id,
                (0, tallyowl_obs::time::now_ms() + 1),
                60_000,
            ),
        )
        .expect("the call reaches the head");
    assert_eq!(
        response.variant.as_deref(),
        Some(tallyowl_rpc::SERVICE_ERROR_VARIANT)
    );
}

#[test]
fn a_signed_in_operator_sees_only_the_workspaces_they_belong_to() {
    let installation = Installation::start("list-workspaces");
    // A second workspace, with a second tenant's project in it.
    installation
        .catalog()
        .provision(
            "another-tenant",
            "their-project",
            tallyowl_obs::time::now_ms(),
        )
        .expect("a second workspace");

    // A person who belongs to the first workspace only.
    let workspaces = installation.catalog().workspaces().unwrap();
    let theirs = workspaces
        .iter()
        .find(|w| w.name == "default")
        .expect("the first workspace");
    installation
        .catalog()
        .put_member(&tallyowl_store::control::Member {
            subject: "one-tenant".into(),
            workspace_id: theirs.workspace_id,
            role: tallyowl_store::control::Role::Viewer,
            display_name: "One Tenant".into(),
            added_at: tallyowl_obs::time::now_ms(),
        })
        .expect("the membership is recorded");
    let token = installation
        .catalog()
        .issue_session(
            "one-tenant",
            "operator",
            tallyowl_obs::time::now_ms(),
            3_600_000,
        )
        .expect("a session")
        .token;
    let listed = tallyowl_rpc::Client::new(installation.head_address(), MAX_FRAME)
        .with_credential(token)
        .call(
            "TallyOwlControl",
            "list-workspaces",
            tallyowl_control_api::codec::encode_list_request(
                &tallyowl_control_api::types::ListRequest {
                    cursor: None,
                    limit: None,
                },
            ),
        )
        .expect("the call reaches the head");
    let decoded =
        tallyowl_control_api::codec::decode_workspace_list(&listed.payload).expect("it decodes");
    assert_eq!(decoded.workspaces.len(), 1, "one workspace, not both");
    assert_eq!(decoded.workspaces[0].name, "default");
}
