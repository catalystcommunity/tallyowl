//! The Go app driver against the real collector and the real head.
//!
//! `docs/PLAN.md` Phase 2 asks for the Go and TypeScript drivers reaching the
//! Phase 1 collector. The golden vectors prove that the three languages encode
//! the same bytes; this proves that those bytes travel a real socket, through
//! the durable queue, into the head, and come back out of a query.
//!
//! The fake here is the durable queue, which the collector's own unit tests
//! already use. The collector, the head, the store, and both sockets are real.
//!
//! **A missing Go toolchain fails this test.** A suite that quietly does not run
//! reports the same green as one that passed, which is exactly the failure the
//! cross-language work exists to prevent. See `docs/IMPLEMENTATION_LOG.md` L019.

use std::path::{Path, PathBuf};
use std::process::Command;
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
use tallyowl_head::ingest::Ingest;
use tallyowl_head::query::QueryService;
use tallyowl_head::service::HeadService;
use tallyowl_obs::health::Health;
use tallyowl_obs::log::{Logger, Severity};
use tallyowl_obs::metrics::Registry;
use tallyowl_store::{SegmentedStore, Store, TimeBasis as StoreBasis};
use tallyowl_wire::{control as wire, query, Value};

const MAX_FRAME: usize = 16 * 1024 * 1024;
const QUEUE: &str = "tallyowl-delivery";

const BASE_TIME: i64 = 1_785_628_800_000;

/// The Go program, and where to find a Go that can run it.
///
/// The shared toolchain bundle is the ordinary source on a workstation here.
/// A Go on the path wins, so a developer with their own install is unaffected.
fn go_program() -> PathBuf {
    if let Ok(bundle) = std::env::var("CATALYST_TOOLS") {
        let candidate = PathBuf::from(bundle).join("go/bin/go");
        if candidate.is_file() {
            return candidate;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = PathBuf::from(home).join(".local/catalyst-tools/go/bin/go");
        if candidate.is_file() {
            return candidate;
        }
    }
    let found = Command::new("sh")
        .args(["-c", "command -v go"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    match found {
        Some(path) => PathBuf::from(path),
        None => panic!(
            "Go is not installed, and this test proves the Go driver reaches the collector. \
             Install the shared toolchains with `bash tools/install-transport-toolchains.sh`, \
             or put your own Go on the path."
        ),
    }
}

fn driver_package() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/driver-go")
        .canonicalize()
        .expect("the Go driver package is in this repository")
}

fn temporary_directory(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("go-driver")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

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
    head: tallyowl_rpc::Server,
    forwarder: Arc<Forwarder>,
    store: Arc<dyn Store>,
    segmented: Arc<SegmentedStore>,
    credential: String,
    project_id: [u8; 16],
}

impl Installation {
    fn start(name: &str) -> Installation {
        let directory = temporary_directory(name);
        let logger = Arc::new(Logger::new("test", "0.0.0", Severity::Error));

        let segmented = open_store(&directory);
        let store: Arc<dyn Store> = Arc::clone(&segmented) as Arc<dyn Store>;
        let issued = segmented
            .catalog()
            .provision("default", "go-driver", tallyowl_obs::time::now_ms())
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

        let head_client = Arc::new(RemoteHead::new(
            &head.local_address().to_string(),
            MAX_FRAME,
        ));

        let queue = FakeQueue::new();
        let health = Health::new();
        health.declare(SWEEP_CHECK, "not yet");
        health.declare(DURABLE_STORE_CHECK, "not yet");
        let collector_metrics = Registry::new();
        Intake::declare_metrics(&collector_metrics);
        Forwarder::declare_metrics(&collector_metrics);

        let intake = Arc::new(Intake {
            queue: Arc::clone(&queue) as Arc<dyn DurableQueue>,
            queue_name: QUEUE.into(),
            tenancy: Arc::new(TenancyResolver::new(
                Arc::clone(&head_client) as Arc<dyn KeyDirectory>,
                60_000,
            )),
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
            stamped: vec![("region".into(), "home".into())],
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
            head,
            forwarder,
            store,
            segmented,
            credential: issued.credential,
            project_id: issued.key.project_id,
        }
    }

    fn deliver_all(&self) -> usize {
        let mut delivered = 0;
        while self.forwarder.deliver_one() {
            delivered += 1;
        }
        delivered
    }
}

/// What the Go program reported it sent.
struct Sent {
    accepted: u64,
    durable_copies: u64,
    event_ids: Vec<[u8; 16]>,
}

fn run_go_driver(address: &str, credential: &str, at: i64) -> Sent {
    let output = Command::new(go_program())
        .args([
            "run",
            "./cmd/send-events",
            address,
            credential,
            &at.to_string(),
        ])
        .current_dir(driver_package())
        .env("GOFLAGS", "-mod=mod")
        .output()
        .expect("the Go driver runs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the Go driver failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // The program writes one line of JSON. A full parser would buy nothing for
    // four known fields.
    let number = |key: &str| -> u64 {
        let start = stdout
            .find(&format!("\"{key}\":"))
            .unwrap_or_else(|| panic!("the Go driver did not report `{key}`: {stdout}"))
            + key.len()
            + 3;
        stdout[start..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or_else(|_| panic!("`{key}` was not a number: {stdout}"))
    };

    let ids_start = stdout.find("\"event_ids\":[").expect("event ids") + 13;
    let ids_end = stdout[ids_start..].find(']').expect("event ids end") + ids_start;
    let event_ids = stdout[ids_start..ids_end]
        .split(',')
        .map(|part| part.trim().trim_matches('"'))
        .filter(|part| !part.is_empty())
        .map(|hex| {
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hexadecimal"))
                .collect();
            bytes.try_into().expect("a 16-byte identifier")
        })
        .collect();

    Sent {
        accepted: number("accepted"),
        durable_copies: number("durable_copies"),
        event_ids,
    }
}

#[test]
fn the_go_driver_reaches_the_collector_and_its_events_survive_a_query() {
    let installation = Installation::start("go-round-trip");
    let address = installation.collector.local_address().to_string();

    let sent = run_go_driver(&address, &installation.credential, BASE_TIME);
    assert_eq!(sent.accepted, 4, "the collector accepted every item");
    // The receipt names the durability it reached. A caller must never have to
    // assume which one it got.
    assert_eq!(sent.durable_copies, 1);

    // The collector acknowledged after the durable queue took the batch. The
    // head has not seen it yet, and that is the contract rather than a delay.
    assert_eq!(installation.store.row_count(), 0);
    assert_eq!(installation.deliver_all(), 1);
    assert_eq!(installation.store.row_count(), 4);

    // Every item arrived as the kind it says it is. A page view that projected
    // as an event is the failure the shape change in L017 removed.
    let mut kinds: Vec<String> = installation
        .store
        .scan(
            installation.project_id,
            BASE_TIME - 1,
            BASE_TIME + 1_000,
            StoreBasis::OccurredAt,
        )
        .expect("scan")
        .rows
        .into_iter()
        .map(|row| row.kind)
        .collect();
    kinds.sort();
    assert_eq!(kinds, vec!["conversion", "error", "event", "page-view"]);

    // The exact lookup finds each identifier the Go driver reported.
    for event_id in &sent.event_ids {
        assert!(
            installation
                .store
                .lookup_event(*event_id)
                .expect("lookup")
                .is_some(),
            "an event the Go driver sent is not in the store"
        );
    }
}

#[test]
fn a_go_typed_property_keeps_its_type_through_the_whole_path() {
    // The distinction TypeScript and Go could not carry at all before L017. It
    // has to survive the driver, the wire, the queue, the head, and the store.
    let installation = Installation::start("go-types");
    let address = installation.collector.local_address().to_string();
    run_go_driver(&address, &installation.credential, BASE_TIME);
    installation.deliver_all();

    let rows = installation
        .store
        .scan(
            installation.project_id,
            BASE_TIME - 1,
            BASE_TIME + 1_000,
            StoreBasis::OccurredAt,
        )
        .expect("scan")
        .rows;

    let page_view = rows
        .iter()
        .find(|row| row.kind == "page-view")
        .expect("the page view");
    assert_eq!(
        page_view.properties["attempts"].0,
        tallyowl_store::PropertyValue::Unsigned(2),
        "an unsigned property arrived as something else"
    );

    let conversion = rows
        .iter()
        .find(|row| row.kind == "conversion")
        .expect("the conversion");
    // Money keeps its exact digits. 19.99 through a float is 19.989999999999998,
    // and a revenue total built from that disagrees with the customer's own
    // records.
    assert_eq!(
        conversion.properties["value"].0,
        tallyowl_store::PropertyValue::Decimal("19.99".into())
    );
    assert_eq!(conversion.name, "purchase");

    // The collector stamped its own property, and the driver stamped its own.
    let event = rows
        .iter()
        .find(|row| row.kind == "event")
        .expect("the event");
    assert_eq!(event.properties["region"].1, "collector");
    assert_eq!(event.properties["service"].1, "driver");
    assert_eq!(event.properties["plan"].1, "client");
}

#[test]
fn a_query_returns_what_the_go_driver_sent() {
    let installation = Installation::start("go-query");
    let address = installation.collector.local_address().to_string();
    let sent = run_go_driver(&address, &installation.credential, BASE_TIME);
    installation.deliver_all();

    let request = encode_query_request(&query::trend(
        1,
        query::events(
            &installation.project_id,
            TimeRange {
                range_start: BASE_TIME - 60_000,
                range_end: BASE_TIME + 60_000,
                basis: TimeBasis::OccurredAt,
                timezone: None,
            },
        ),
        60_000,
        "events",
    ));

    let client =
        tallyowl_rpc::Client::new(installation.head.local_address().to_string(), MAX_FRAME)
            .with_credential(
                installation
                    .segmented
                    .catalog()
                    .issue_operator_session("operator", tallyowl_obs::time::now_ms(), 3_600_000)
                    .expect("a session")
                    .token,
            );
    let response = client
        .call("TallyOwlControl", "run-query", request)
        .expect("the query runs");
    let decoded = decode_query_response(&response.payload).expect("the answer decodes");

    let total: u64 = decoded
        .rows
        .iter()
        .filter_map(|row| match row.values.get(1).map(wire::read) {
            Some(Ok(Value::Unsigned(count))) => Some(count),
            _ => None,
        })
        .sum();
    assert_eq!(total, sent.accepted);
    // Correctness is the default, and a result says whether it is exact.
    assert!(decoded.metadata.complete);
    assert!(decoded.metadata.exactness[0].exact);
}
