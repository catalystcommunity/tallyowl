//! The reference application, against a real installation.
//!
//! `docs/TESTBED.md` says why this exists: unit tests prove that one component
//! obeys its contract, and they do not prove that a complete product produces
//! correct analytics. The simulator writes a ledger **before** it sends
//! anything, and these tests run the equivalent query and compare. A mismatch is
//! a failure, which makes analytics correctness a test result rather than a
//! judgment.
//!
//! This is the build-order step 1 of section 13: the backend, the ledger, and
//! generic events. Later phases add the marketing site, errors, traces, metrics,
//! identity, and attribution.
//!
//! The fake here is the durable queue. The collector, the head, the store, both
//! sockets, the app driver, and the backend are real.

use std::collections::BTreeMap;
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
use tallyowl_head::ingest::Ingest;
use tallyowl_head::query::QueryService;
use tallyowl_head::service::HeadService;
use tallyowl_obs::health::Health;
use tallyowl_obs::log::{Logger, Severity};
use tallyowl_obs::metrics::Registry;
use tallyowl_store::{PropertyValue, SegmentedStore, Store, TimeBasis};

const MAX_FRAME: usize = 16 * 1024 * 1024;
const QUEUE: &str = "tallyowl-delivery";
/// The virtual clock's origin. A fixed value, so a failure reproduces.
const START_AT: i64 = 1_785_628_800_000;

/// How far a scenario's own range reaches past its origin.
///
/// The Phase 8 identity journey brings a person back on later days, so a range
/// of a couple of hours would leave those visits outside every assertion and a
/// retention matrix would have nothing to count. The ledger states its own
/// range; this is the bound every query here uses so that the two agree.
const RANGE_BEFORE: i64 = 7_200_000;
/// Twenty days, because the Phase 9 marketing journey spans fourteen: three
/// touches one decay half-life apart and a purchase at the end. A range that
/// stopped at ten would leave every conversion outside it and every attribution
/// assertion would compare two empty answers and pass.
const RANGE_AFTER: i64 = 20 * 86_400_000;

fn go_program() -> PathBuf {
    for candidate in [
        std::env::var("CATALYST_TOOLS")
            .ok()
            .map(|b| PathBuf::from(b).join("go/bin/go")),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/catalyst-tools/go/bin/go")),
    ]
    .into_iter()
    .flatten()
    {
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
            "Go is not installed, and the reference application is written in it. \
             Install the shared toolchains with `bash tools/install-transport-toolchains.sh`."
        ),
    }
}

fn testbed_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testbed")
        .canonicalize()
        .expect("the reference application is in this repository")
}

fn temporary_directory(name: &str) -> PathBuf {
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    let path = base
        .join("testbed")
        .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a place to work");
    // The reference application runs with its own working directory, so a
    // relative path here would put the ledger somewhere neither side expects.
    path.canonicalize().expect("an absolute path")
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
    _head: tallyowl_rpc::Server,
    forwarder: Arc<Forwarder>,
    store: Arc<dyn Store>,
    /// The tablet group, when this installation is replicated. Held so that it
    /// outlives the store that writes through it.
    _registry: Option<Arc<tallyowl_cluster::groups::GroupRegistry>>,
    /// One credential for each application in the scenario, and the project
    /// each one resolves to. The head issued both; the applications know only
    /// the credentials.
    credentials: Vec<String>,
    projects: Vec<[u8; 16]>,
    /// The head's own query executor, so an analysis runs through the same path
    /// a dashboard uses rather than through a second one written for a test.
    query: Arc<QueryService>,
    /// The head's own store, for the control state an alert rule lives in.
    segmented: Arc<SegmentedStore>,
}

impl Installation {
    fn start(directory: &Path) -> Installation {
        Installation::start_with(directory, false)
    }

    /// The same installation, with the head's store behind a real tablet group.
    ///
    /// Phase 7's last exit criterion is that the reference application runs
    /// **unchanged** against a replicated installation. Nothing below this line
    /// differs between the two: the same collector, the same head, the same
    /// query service, and the same scenario. Only the store the head commits
    /// through is different, which is the whole claim.
    fn start_replicated(directory: &Path) -> Installation {
        Installation::start_with(directory, true)
    }

    fn start_with(directory: &Path, replicate: bool) -> Installation {
        let logger = Arc::new(Logger::new("testbed", "0.0.0", Severity::Error));
        let segmented = open_store(&directory.join("data"));
        let local: Arc<dyn Store> = Arc::clone(&segmented) as Arc<dyn Store>;
        let (store, registry) = if replicate {
            let registry = tallyowl_cluster::groups::GroupRegistry::new(
                "testbed",
                "127.0.0.1:0",
                Some(directory.join("consensus")),
            )
            .expect("a group registry");
            let group = tallyowl_cluster::groups::GroupKey::Tablet("t0".into());
            let members = vec![tallyowl_cluster::topology::Member::voter(
                "testbed",
                "127.0.0.1:1",
            )];
            registry
                .start(
                    group.clone(),
                    Arc::new(tallyowl_cluster::raft::machine::TabletMachine::new(
                        Arc::clone(&local),
                    )),
                    members.clone(),
                    0,
                )
                .expect("the tablet group starts");
            registry.bootstrap(&group, &members).expect("one voter");
            registry
                .await_leader(&group, Duration::from_secs(20))
                .expect("one voter elects itself");
            let replicated: Arc<dyn Store> =
                Arc::new(tallyowl_cluster::replicated::ReplicatedStore::new(
                    Arc::clone(&registry),
                    "t0",
                    Arc::clone(&local),
                    tallyowl_cluster::topology::ReceiptPolicy::LocalOne,
                    "home",
                ));
            (replicated, Some(registry))
        } else {
            (Arc::clone(&local), None)
        };

        // Two applications share one installation, which is what proves tenant
        // isolation with real traffic rather than only with a negative unit
        // test. Each holds its own key and neither knows its project.
        let now = tallyowl_obs::time::now_ms();
        let issued: Vec<_> = ["seedstore", "sidecart"]
            .iter()
            .map(|name| {
                segmented
                    .catalog()
                    .provision("default", name, now)
                    .expect("the head provisions a project")
            })
            .collect();

        // One query executor, held by the head and by this test, so an analysis
        // a test runs is the one a dashboard would get.
        let query = Arc::new(QueryService {
            store: Arc::clone(&store),
            max_runtime_ms: 30_000,
            max_expression_depth: tallyowl_head::expr::DEFAULT_MAX_DEPTH,
            guards: tallyowl_head::analysis::Guards::default(),
            attribution: Default::default(),
            policy: Default::default(),
            identity: Default::default(),
        });

        let head_metrics = Registry::new();
        Ingest::declare_metrics(&head_metrics);
        let head = tallyowl_rpc::serve(
            "127.0.0.1:0",
            Arc::new(HeadService {
                query: Arc::clone(&query),
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
        let metrics = Registry::new();
        Intake::declare_metrics(&metrics);
        Forwarder::declare_metrics(&metrics);

        // Intake resolves a credential against the head's control catalog and
        // holds the answer for a short time. A credential the catalog does not
        // hold reaches no project.
        let intake = Arc::new(Intake {
            queue: Arc::clone(&queue) as Arc<dyn DurableQueue>,
            queue_name: QUEUE.into(),
            tenancy: Arc::new(TenancyResolver::new(
                Arc::clone(&head_client) as Arc<dyn KeyDirectory>,
                60_000,
            )),
            limits: Limits {
                max_batch_bytes: 4 * 1024 * 1024,
                max_event_bytes: 64 * 1024,
                max_properties: 128,
            },
            durable_copies: 1,
            series: std::sync::Arc::new(tallyowl_collector::series::SeriesLedger::new(
                tallyowl_collector::series::SeriesBudget::default(),
            )),
            metrics: Arc::clone(&metrics),
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
                // Each application presents its own key on its own request, so
                // the connection default is never used here.
                credential: String::new(),
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
            metrics,
            logger,
            state: forwarder_state,
            sweep_staleness_ms: 3_000,
            max_payload_bytes: 16 * 1024 * 1024,
            max_delivery_age_ms: 24 * 60 * 60 * 1000,
        });

        Installation {
            collector,
            _head: head,
            forwarder,
            store,
            credentials: issued.iter().map(|i| i.credential.clone()).collect(),
            projects: issued.iter().map(|i| i.key.project_id).collect(),
            _registry: registry,
            query,
            segmented,
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

/// The expected result for one application, read back from the ledger.
#[derive(Debug, Default)]
struct LedgerProject {
    credential: String,
    total_events: usize,
    count_by_kind: BTreeMap<String, usize>,
    count_by_name: BTreeMap<String, usize>,
    count_by_minute: BTreeMap<i64, usize>,
    conversion_value: String,
    conversion_count: usize,
    event_ids: Vec<[u8; 16]>,
    duplicate_event_ids: Vec<[u8; 16]>,
    errors_by_defect: BTreeMap<String, usize>,
    traces: BTreeMap<String, TraceShape>,
    metrics: BTreeMap<String, MetricShape>,
    /// Phase 8. What the identity journey must produce.
    anonymous_by_end_user: BTreeMap<String, Vec<String>>,
    funnel: Vec<(String, usize)>,
    retention: RetentionShape,
    timeline_by_end_user: BTreeMap<String, usize>,
    /// Phase 9. What every attribution model must credit.
    attribution: AttributionShape,
}

/// What the marketing journey must produce, for every model. Phase 9.
///
/// The credited values are exact decimal text, never floats. A credited value
/// that arrived through a float would disagree with the revenue it was divided
/// from, which is the whole thing this assertion exists to catch.
#[derive(Debug, Default, PartialEq, Eq)]
struct AttributionShape {
    goal: String,
    lookback_ms: i64,
    conversions: usize,
    order_repeats: usize,
    /// Model name to campaign to credited value.
    by_model: BTreeMap<String, BTreeMap<String, String>>,
    cost_by_campaign: BTreeMap<String, String>,
    touches_by_channel: BTreeMap<String, usize>,
}

/// The cohort-by-period matrix the scenario expects. Phase 8.
#[derive(Debug, Default, PartialEq, Eq)]
struct RetentionShape {
    period: String,
    periods: usize,
    cohort_size: usize,
    returned_by_period: Vec<usize>,
}

/// What one metric name must hold after the scenario runs.
///
/// The ledger predicts the value because the application aggregated it in
/// process. A collector merge, a series budget, or a projection that lost a
/// label would each break one of these numbers.
#[derive(Debug, Default, PartialEq)]
struct MetricShape {
    kind: String,
    series: usize,
    total: f64,
    sum: f64,
}

/// The exact parent and child shape of one trace, as the simulator wrote it.
#[derive(Debug, Default, PartialEq, Eq)]
struct TraceShape {
    spans: usize,
    max_depth: usize,
    errors: usize,
}

/// Read the ledger the simulator wrote.
///
/// A field this reader cannot find is a failure rather than a default, because
/// a silently empty expectation would pass against an empty store.
fn read_ledger(path: &Path) -> Vec<LedgerProject> {
    let text = std::fs::read_to_string(path).expect("the simulator wrote a ledger");
    let value: serde_json::Value = serde_json::from_str(&text).expect("the ledger is readable");
    let projects = value["projects"]
        .as_array()
        .expect("the ledger names its projects");
    assert!(!projects.is_empty(), "the ledger holds no project");

    projects
        .iter()
        .map(|project| {
            let counts = |key: &str| -> BTreeMap<String, usize> {
                project[key]
                    .as_object()
                    .expect("a count map")
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_u64().expect("a count") as usize))
                    .collect()
            };
            let ids = |key: &str| -> Vec<[u8; 16]> {
                project[key]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .filter_map(|v| id_from_hex(v.as_str()?))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            LedgerProject {
                metrics: project["metrics"]
                    .as_object()
                    .map(|map| {
                        map.iter()
                            .map(|(k, v)| {
                                (
                                    k.clone(),
                                    MetricShape {
                                        kind: v["kind"].as_str().unwrap_or("").to_string(),
                                        series: v["series"].as_u64().unwrap_or(0) as usize,
                                        total: v["total"].as_f64().unwrap_or(0.0),
                                        sum: v["sum"].as_f64().unwrap_or(0.0),
                                    },
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                errors_by_defect: project["errors_by_defect"]
                    .as_object()
                    .map(|map| {
                        map.iter()
                            .map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0) as usize))
                            .collect()
                    })
                    .unwrap_or_default(),
                traces: project["traces"]
                    .as_object()
                    .map(|map| {
                        map.iter()
                            .map(|(k, v)| {
                                (
                                    k.clone(),
                                    TraceShape {
                                        spans: v["spans"].as_u64().unwrap_or(0) as usize,
                                        max_depth: v["max_depth"].as_u64().unwrap_or(0) as usize,
                                        errors: v["errors"].as_u64().unwrap_or(0) as usize,
                                    },
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                credential: project["credential"]
                    .as_str()
                    .expect("a credential")
                    .to_string(),
                total_events: project["total_events"].as_u64().expect("a total") as usize,
                count_by_kind: counts("count_by_kind"),
                count_by_name: counts("count_by_name"),
                count_by_minute: counts("count_by_minute")
                    .into_iter()
                    .map(|(k, v)| (k.parse().expect("a bucket start"), v))
                    .collect(),
                conversion_value: project["conversion_value"]
                    .as_str()
                    .expect("a revenue total")
                    .to_string(),
                conversion_count: project["conversion_count"].as_u64().unwrap_or(0) as usize,
                event_ids: ids("event_ids"),
                duplicate_event_ids: ids("duplicate_event_ids"),
                anonymous_by_end_user: project["identity"]["anonymous_by_end_user"]
                    .as_object()
                    .map(|map| {
                        map.iter()
                            .map(|(k, v)| {
                                (
                                    k.clone(),
                                    v.as_array()
                                        .map(|list| {
                                            list.iter()
                                                .filter_map(|x| x.as_str().map(str::to_string))
                                                .collect()
                                        })
                                        .unwrap_or_default(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                funnel: project["funnel"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .map(|step| {
                                (
                                    step["name"].as_str().unwrap_or("").to_string(),
                                    step["reached"].as_u64().unwrap_or(0) as usize,
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                retention: RetentionShape {
                    period: project["retention"]["period"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                    periods: project["retention"]["periods"].as_u64().unwrap_or(0) as usize,
                    cohort_size: project["retention"]["cohort_size"].as_u64().unwrap_or(0) as usize,
                    returned_by_period: project["retention"]["returned_by_period"]
                        .as_array()
                        .map(|list| {
                            list.iter()
                                .map(|v| v.as_u64().unwrap_or(0) as usize)
                                .collect()
                        })
                        .unwrap_or_default(),
                },
                timeline_by_end_user: project["timeline_by_end_user"]
                    .as_object()
                    .map(|map| {
                        map.iter()
                            .map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0) as usize))
                            .collect()
                    })
                    .unwrap_or_default(),
                attribution: AttributionShape {
                    goal: project["attribution"]["goal"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                    lookback_ms: project["attribution"]["lookback_ms"].as_i64().unwrap_or(0),
                    conversions: project["attribution"]["conversions"].as_u64().unwrap_or(0)
                        as usize,
                    order_repeats: project["attribution"]["order_repeats"]
                        .as_u64()
                        .unwrap_or(0) as usize,
                    by_model: project["attribution"]["by_model"]
                        .as_object()
                        .map(|map| {
                            map.iter()
                                .map(|(model, credits)| {
                                    (
                                        model.clone(),
                                        credits
                                            .as_object()
                                            .map(|held| {
                                                held.iter()
                                                    .map(|(campaign, value)| {
                                                        (
                                                            campaign.clone(),
                                                            value
                                                                .as_str()
                                                                .unwrap_or("")
                                                                .to_string(),
                                                        )
                                                    })
                                                    .collect()
                                            })
                                            .unwrap_or_default(),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    cost_by_campaign: project["attribution"]["cost_by_campaign"]
                        .as_object()
                        .map(|map| {
                            map.iter()
                                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                                .collect()
                        })
                        .unwrap_or_default(),
                    touches_by_channel: project["attribution"]["touches_by_channel"]
                        .as_object()
                        .map(|map| {
                            map.iter()
                                .map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0) as usize))
                                .collect()
                        })
                        .unwrap_or_default(),
                },
            }
        })
        .collect()
}

fn id_from_hex(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..32)
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect();
    bytes?.try_into().ok()
}

/// Run the reference application against a live collector.
fn run_scenario(collector_address: &str, ledger_path: &Path, credentials: &[String]) -> String {
    // The credentials come from the installation, because TallyOwl issues them.
    // An application never chooses its own key.
    let mut arguments = vec![
        "run".to_string(),
        "./cmd/run-scenario".to_string(),
        collector_address.to_string(),
        ledger_path.to_str().expect("a path").to_string(),
        START_AT.to_string(),
    ];
    arguments.extend(credentials.iter().cloned());
    let output = Command::new(go_program())
        .args(&arguments)
        .current_dir(testbed_directory())
        .env("GOFLAGS", "-mod=mod")
        .output()
        .expect("the reference application runs");
    assert!(
        output.status.success(),
        "the reference application failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// The project one credential resolves to, as this installation issued it.
fn project_id(installation: &Installation, credential: &str) -> [u8; 16] {
    let index = installation
        .credentials
        .iter()
        .position(|held| held == credential)
        .expect("the ledger names a credential this installation issued");
    installation.projects[index]
}

#[test]
fn the_reference_application_produces_events_that_match_the_ledger() {
    let workspace = temporary_directory("ledger");
    let installation = Installation::start(&workspace);
    let ledger_path = workspace.join("ledger.json");

    let report = run_scenario(
        &installation.collector.local_address().to_string(),
        &ledger_path,
        &installation.credentials,
    );
    assert!(report.contains("seedstore-fast"), "{report}");
    assert!(installation.deliver_all() > 0, "nothing reached the head");

    for expected in read_ledger(&ledger_path) {
        let project = project_id(&installation, &expected.credential);
        let rows = installation
            .store
            .scan(
                project,
                START_AT - RANGE_BEFORE,
                START_AT + RANGE_AFTER,
                TimeBasis::OccurredAt,
            )
            .expect("scan")
            .rows;

        // Every logical event is present once. The scenario deliberately sends
        // some identifiers twice, and a primary query counts the logical event
        // rather than the physical row. See DELIVERY.md section 6.
        // A derived rollup is TallyOwl's own output rather than something the
        // application produced, so a ledger of what the application sent leaves
        // it out. See `tallyowl_head::rollup`.
        let mut logical: BTreeMap<[u8; 16], usize> = BTreeMap::new();
        for row in rows
            .iter()
            .filter(|row| !row.properties.contains_key("derived"))
        {
            *logical.entry(row.event_id).or_insert(0) += 1;
        }
        assert_eq!(
            logical.len(),
            expected.total_events,
            "{}: the ledger expects {} logical events and the store holds {}",
            expected.credential,
            expected.total_events,
            logical.len()
        );

        // The breakdown by kind. Every telemetry kind the scenario produced has
        // to arrive as the kind it says it is.
        let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
        let mut counted: BTreeMap<[u8; 16], ()> = BTreeMap::new();
        for row in rows
            .iter()
            .filter(|row| !row.properties.contains_key("derived"))
        {
            if counted.insert(row.event_id, ()).is_none() {
                *by_kind.entry(row.kind.clone()).or_insert(0) += 1;
            }
        }
        assert_eq!(
            by_kind, expected.count_by_kind,
            "{}: the breakdown by kind does not match the ledger",
            expected.credential
        );

        // The breakdown by name, which is what a dashboard lists.
        let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
        counted.clear();
        for row in rows
            .iter()
            .filter(|row| !row.properties.contains_key("derived"))
        {
            if counted.insert(row.event_id, ()).is_none() {
                *by_name.entry(row.name.clone()).or_insert(0) += 1;
            }
        }
        assert_eq!(
            by_name, expected.count_by_name,
            "{}: the breakdown by name does not match the ledger",
            expected.credential
        );

        // The trend a dashboard draws, through the head's own query path rather
        // than by counting rows here. A count is over logical events, so the
        // deliberate duplicates in the scenario must not raise a bucket.
        let trend = installation
            .store
            .trend(
                project,
                START_AT - RANGE_BEFORE,
                START_AT + RANGE_AFTER,
                TimeBasis::OccurredAt,
                60_000,
                None,
            )
            .expect("the trend runs");
        // The store's trend counts every row in the project, and the head
        // derives golden signals from the spans it commits. The ledger records
        // what the application produced, so the derived rows come out of the
        // comparison rather than out of the store. See `tallyowl_head::rollup`.
        let mut derived_by_minute: BTreeMap<i64, usize> = BTreeMap::new();
        let mut derived_total = 0usize;
        let mut counted_derived: BTreeMap<[u8; 16], ()> = BTreeMap::new();
        for row in rows
            .iter()
            .filter(|row| row.properties.contains_key("derived"))
        {
            if counted_derived.insert(row.event_id, ()).is_none() {
                derived_total += 1;
                let bucket = row.occurred_at - row.occurred_at.rem_euclid(60_000);
                *derived_by_minute.entry(bucket).or_insert(0) += 1;
            }
        }
        let buckets: BTreeMap<i64, usize> = trend
            .buckets
            .iter()
            .map(|(bucket, count)| {
                (
                    *bucket,
                    *count as usize - derived_by_minute.get(bucket).copied().unwrap_or(0),
                )
            })
            .filter(|(_, count)| *count > 0)
            .collect();
        assert_eq!(
            buckets, expected.count_by_minute,
            "{}: the trend by minute does not match the ledger",
            expected.credential
        );
        assert_eq!(trend.total as usize - derived_total, expected.total_events);
        assert!(
            !trend.incomplete,
            "the answer says it could not see everything"
        );
    }
}

#[test]
fn the_reference_application_revenue_is_exact() {
    // The ledger uses exact decimal text for money and never a float
    // comparison. 19.99 through a float is 19.989999999999998, and a revenue
    // total built from that disagrees with the customer's own records.
    let workspace = temporary_directory("revenue");
    let installation = Installation::start(&workspace);
    let ledger_path = workspace.join("ledger.json");
    run_scenario(
        &installation.collector.local_address().to_string(),
        &ledger_path,
        &installation.credentials,
    );
    installation.deliver_all();

    for expected in read_ledger(&ledger_path) {
        if expected.conversion_count == 0 {
            continue;
        }
        let rows = installation
            .store
            .scan(
                project_id(&installation, &expected.credential),
                START_AT - RANGE_BEFORE,
                START_AT + RANGE_AFTER,
                TimeBasis::OccurredAt,
            )
            .expect("scan")
            .rows;

        // Two folds, and they are different folds.
        //
        // **By event identifier**, because a duplicate delivery gives one
        // logical event however many physical rows exist. See DELIVERY.md
        // section 6.
        //
        // **By goal and order**, because a checkout that retried, a webhook
        // that arrived twice, and a refreshed receipt page are separate
        // logical events that name one order. Both rows are stored and both
        // are real; the revenue is one. Phase 9. Counting the rows here would
        // make an idempotent order read as two sales.
        let mut seen: BTreeMap<[u8; 16], ()> = BTreeMap::new();
        let mut orders: BTreeMap<(String, String), ()> = BTreeMap::new();
        let mut total_cents: i128 = 0;
        let mut conversions = 0;
        for row in rows.iter().filter(|r| r.kind == "conversion") {
            if seen.insert(row.event_id, ()).is_some() {
                continue;
            }
            if let Some((PropertyValue::Text(order_id), _)) = row.properties.get("order_id") {
                if orders
                    .insert((row.name.clone(), order_id.clone()), ())
                    .is_some()
                {
                    continue;
                }
            }
            conversions += 1;
            let PropertyValue::Decimal(text) = &row.properties["value"].0 else {
                panic!("a conversion value arrived as something other than an exact number");
            };
            let (whole, fraction) = text.split_once('.').unwrap_or((text.as_str(), ""));
            let scaled = format!("{whole}{fraction:0<2}");
            total_cents += scaled.parse::<i128>().expect("an exact amount");
        }
        assert_eq!(conversions, expected.conversion_count);

        let expected_cents = {
            let (whole, fraction) = expected
                .conversion_value
                .split_once('.')
                .unwrap_or((expected.conversion_value.as_str(), ""));
            format!("{whole}{fraction:0<2}")
                .parse::<i128>()
                .expect("an exact amount")
        };
        assert_eq!(
            total_cents, expected_cents,
            "{}: the revenue total does not match the ledger",
            expected.credential
        );
    }
}

#[test]
fn one_application_cannot_see_another_applications_events() {
    // D8 asks the reference application to prove tenant isolation with real
    // traffic rather than only with a negative unit test. The two applications
    // overlap in time, so isolation has to come from the credential.
    let workspace = temporary_directory("isolation");
    let installation = Installation::start(&workspace);
    let ledger_path = workspace.join("ledger.json");
    run_scenario(
        &installation.collector.local_address().to_string(),
        &ledger_path,
        &installation.credentials,
    );
    installation.deliver_all();

    let projects = read_ledger(&ledger_path);
    assert_eq!(projects.len(), 2, "the scenario runs two applications");
    assert_ne!(
        project_id(&installation, &projects[0].credential),
        project_id(&installation, &projects[1].credential),
        "two credentials resolved to one project"
    );

    for (index, expected) in projects.iter().enumerate() {
        let other = &projects[1 - index];
        let rows = installation
            .store
            .scan(
                project_id(&installation, &expected.credential),
                START_AT - RANGE_BEFORE,
                START_AT + RANGE_AFTER,
                TimeBasis::OccurredAt,
            )
            .expect("scan")
            .rows;
        for row in &rows {
            assert!(
                !other.event_ids.contains(&row.event_id),
                "{}: an event from {} appeared in this project",
                expected.credential,
                other.credential
            );
        }
    }
}

#[test]
fn an_exact_lookup_finds_every_event_the_ledger_names() {
    // The promise D20 makes: a value that is unique on every row stays exactly
    // retrievable. The ledger names each identifier, so this is the exact
    // lookup rather than a scan that happens to find them.
    let workspace = temporary_directory("lookup");
    let installation = Installation::start(&workspace);
    let ledger_path = workspace.join("ledger.json");
    run_scenario(
        &installation.collector.local_address().to_string(),
        &ledger_path,
        &installation.credentials,
    );
    installation.deliver_all();

    let mut looked_up = 0;
    for expected in read_ledger(&ledger_path) {
        assert!(!expected.event_ids.is_empty(), "the ledger names no events");
        for event_id in &expected.event_ids {
            let found = installation
                .store
                .lookup_event(*event_id)
                .expect("lookup")
                .unwrap_or_else(|| {
                    panic!(
                        "{}: the ledger names an event the store does not hold",
                        expected.credential
                    )
                });
            assert_eq!(found.event_id, *event_id);
            looked_up += 1;
        }
    }
    assert!(
        looked_up > 20,
        "the scenario produced too little to prove anything"
    );
}

#[test]
fn a_duplicate_delivery_gives_one_logical_event() {
    // The scenario sends some identifiers twice on purpose. DELIVERY.md section
    // 6 says a primary query counts the logical event, whatever the physical
    // rows, and the ledger already expects one.
    let workspace = temporary_directory("duplicates");
    let installation = Installation::start(&workspace);
    let ledger_path = workspace.join("ledger.json");
    run_scenario(
        &installation.collector.local_address().to_string(),
        &ledger_path,
        &installation.credentials,
    );
    installation.deliver_all();

    let projects = read_ledger(&ledger_path);
    let with_duplicates = projects
        .iter()
        .find(|p| !p.duplicate_event_ids.is_empty())
        .expect("the fast scenario sends duplicates");

    for event_id in &with_duplicates.duplicate_event_ids {
        assert!(
            with_duplicates.event_ids.contains(event_id),
            "a duplicate names an identifier the ledger never counted"
        );
        assert!(
            installation
                .store
                .lookup_event(*event_id)
                .expect("lookup")
                .is_some(),
            "a duplicated event is missing entirely"
        );
    }
}

// ---------------------------------------------------------------------------
// Phase 5: errors and traces, against the ledger
// ---------------------------------------------------------------------------

#[test]
fn the_reference_applications_errors_group_the_way_the_ledger_says() {
    // The ledger does not predict a fingerprint. D39 says the projector
    // computes it and a producer never controls its group, so a ledger that
    // guessed the digest would only be asserting that TallyOwl agrees with a
    // second implementation of TallyOwl.
    //
    // What the ledger knows exactly is how many distinct defects there were and
    // how many occurrences each had. That is what this compares.
    let workspace = temporary_directory("errors");
    let installation = Installation::start(&workspace);
    let ledger_path = workspace.join("ledger.json");
    run_scenario(
        &installation.collector.local_address().to_string(),
        &ledger_path,
        &installation.credentials,
    );
    installation.deliver_all();

    for expected in read_ledger(&ledger_path) {
        if expected.errors_by_defect.is_empty() {
            continue;
        }
        let project = project_id(&installation, &expected.credential);
        let rows = installation
            .store
            .scan(
                project,
                0,
                tallyowl_obs::time::now_ms() + 1,
                tallyowl_store::TimeBasis::ReceivedAt,
            )
            .expect("the scan reads")
            .rows;

        let mut counted: BTreeMap<String, usize> = BTreeMap::new();
        let mut seen: std::collections::BTreeSet<[u8; 16]> = Default::default();
        for row in rows.iter().filter(|row| row.kind == "error") {
            if !seen.insert(row.event_id) {
                continue;
            }
            let group = row.properties["error_group"].0.to_display();
            *counted.entry(group).or_default() += 1;
        }

        assert_eq!(
            counted.len(),
            expected.errors_by_defect.len(),
            "one group for each defect, for {}",
            expected.credential
        );
        let mut produced: Vec<usize> = counted.values().copied().collect();
        let mut wanted: Vec<usize> = expected.errors_by_defect.values().copied().collect();
        produced.sort_unstable();
        wanted.sort_unstable();
        assert_eq!(
            produced, wanted,
            "the occurrences of each defect, for {}",
            expected.credential
        );

        // Every occurrence carries what a later fingerprint version needs to
        // rebuild the group, and the version it was computed at.
        for row in rows.iter().filter(|row| row.kind == "error") {
            assert!(row.properties.contains_key("error_group_inputs"));
            assert_eq!(row.properties["error_group_version"].0.to_display(), "1");
        }
    }
}

#[test]
fn every_trace_the_reference_application_produced_has_the_shape_the_ledger_says() {
    let workspace = temporary_directory("traces");
    let installation = Installation::start(&workspace);
    let ledger_path = workspace.join("ledger.json");
    run_scenario(
        &installation.collector.local_address().to_string(),
        &ledger_path,
        &installation.credentials,
    );
    installation.deliver_all();

    let mut checked = 0;
    for expected in read_ledger(&ledger_path) {
        for (trace_text, shape) in &expected.traces {
            let trace_id = id_from_hex(trace_text).expect("a trace identifier");
            let found = installation
                .store
                .lookup_correlated(tallyowl_store::segment::schema::TRACE_ID, &trace_id)
                .expect("the lookup reads");

            let spans = found.rows.iter().filter(|row| row.kind == "span").count();
            let errors = found.rows.iter().filter(|row| row.kind == "error").count();
            assert_eq!(spans, shape.spans, "spans in {trace_text}");
            assert_eq!(errors, shape.errors, "errors in {trace_text}");

            // The waterfall shape: a trace three deep has a span whose parent
            // is a span whose parent is a root.
            let depth = deepest(&found.rows);
            assert_eq!(depth, shape.max_depth, "depth of {trace_text}");
            checked += 1;
        }
    }
    assert!(checked > 0, "the scenario produced no traces to check");
}

/// How deep the parent chain goes in one trace.
fn deepest(rows: &[tallyowl_store::row::EventRow]) -> usize {
    let id_of = |row: &tallyowl_store::row::EventRow| {
        row.properties.get("span_id").map(|(v, _)| v.to_display())
    };
    let parent_of = |row: &tallyowl_store::row::EventRow| {
        row.properties
            .get("parent_span_id")
            .map(|(v, _)| v.to_display())
    };
    let by_id: BTreeMap<String, &tallyowl_store::row::EventRow> = rows
        .iter()
        .filter_map(|row| id_of(row).map(|id| (id, row)))
        .collect();

    rows.iter()
        .map(|row| {
            let mut depth = 0;
            let mut current = row;
            // A chain is bounded by the number of rows, so a cycle cannot loop
            // this even if a producer sent one.
            while let Some(parent) = parent_of(current).and_then(|id| by_id.get(&id)) {
                depth += 1;
                current = parent;
                if depth > rows.len() {
                    break;
                }
            }
            depth
        })
        .max()
        .unwrap_or(0)
}

#[test]
fn the_golden_signals_the_head_derived_account_for_every_span_the_ledger_names() {
    // PLAN.md Phase 6: service-operation golden signals derived from spans.
    // The reference application produces the spans, and the head derives the
    // request, error, and duration series from them. This asserts the derived
    // series against the same ledger the spans are asserted against, so a
    // rollup that lost or doubled a span fails here.
    let workspace = temporary_directory("golden-signals");
    let installation = Installation::start(&workspace);
    let ledger_path = workspace.join("ledger.json");
    run_scenario(
        &installation.collector.local_address().to_string(),
        &ledger_path,
        &installation.credentials,
    );
    installation.deliver_all();

    let mut checked = 0;
    for expected in read_ledger(&ledger_path) {
        let project = project_id(&installation, &expected.credential);
        let rows = installation
            .store
            .scan(
                project,
                START_AT - RANGE_BEFORE,
                START_AT + RANGE_AFTER,
                TimeBasis::OccurredAt,
            )
            .expect("scan")
            .rows;

        let spans = expected.count_by_kind.get("span").copied().unwrap_or(0);
        if spans == 0 {
            continue;
        }

        let value_of = |row: &tallyowl_store::row::EventRow, key: &str| -> f64 {
            row.properties
                .get(key)
                .and_then(|(value, _)| match value {
                    tallyowl_store::row::PropertyValue::Float(v) => Some(*v),
                    tallyowl_store::row::PropertyValue::Unsigned(v) => Some(*v as f64),
                    tallyowl_store::row::PropertyValue::Integer(v) => Some(*v as f64),
                    _ => None,
                })
                .unwrap_or(0.0)
        };

        // Every span is counted exactly once across every window and every
        // operation.
        let requested: f64 = rows
            .iter()
            .filter(|row| row.name == tallyowl_head::rollup::REQUESTS)
            .map(|row| value_of(row, "value"))
            .sum();
        assert_eq!(
            requested as usize, spans,
            "{}: the request signal counts every span once",
            expected.credential
        );

        // Every span also reached a duration histogram exactly once.
        let observed: f64 = rows
            .iter()
            .filter(|row| row.name == tallyowl_head::rollup::DURATION)
            .map(|row| value_of(row, "histogram_count"))
            .sum();
        assert_eq!(
            observed as usize, spans,
            "{}: the duration histogram holds every span once",
            expected.credential
        );

        // The error signal never exceeds the request signal, and the scenario's
        // failed spans reach it.
        let errored: f64 = rows
            .iter()
            .filter(|row| row.name == tallyowl_head::rollup::ERRORS)
            .map(|row| value_of(row, "value"))
            .sum();
        assert!(
            errored <= requested,
            "{}: more errors than requests",
            expected.credential
        );

        // Every derived row says it is derived, so a count of what the
        // application produced can leave it out.
        for row in rows
            .iter()
            .filter(|row| row.name.starts_with("tallyowl_service_operation"))
        {
            assert!(
                row.properties.contains_key("derived"),
                "a derived row says so"
            );
            assert_eq!(row.kind, "metric-point");
        }

        // Every signal names the service and the operation it belongs to, which
        // is what makes it a service-operation signal rather than a total.
        for row in rows
            .iter()
            .filter(|row| row.name == tallyowl_head::rollup::REQUESTS)
        {
            assert!(row.properties.contains_key("service"));
            assert!(row.properties.contains_key("operation"));
        }
        checked += 1;
    }
    assert!(checked > 0, "the scenario produced no spans to check");
}

#[test]
fn the_metrics_the_reference_application_published_match_the_ledger() {
    // PLAN.md Phase 6: counter, gauge, histogram, and exemplar APIs in the
    // first backend SDKs, with in-process aggregation and snapshot push.
    //
    // The application aggregated in process, so the ledger knows exactly what
    // each series must hold. A collector merge, a series budget, or a
    // projection that lost a label would each break one of these numbers, and
    // none of them would be visible from a count of rows.
    let workspace = temporary_directory("app-metrics");
    let installation = Installation::start(&workspace);
    let ledger_path = workspace.join("ledger.json");
    run_scenario(
        &installation.collector.local_address().to_string(),
        &ledger_path,
        &installation.credentials,
    );
    installation.deliver_all();

    let mut checked = 0;
    for expected in read_ledger(&ledger_path) {
        if expected.metrics.is_empty() {
            continue;
        }
        let project = project_id(&installation, &expected.credential);
        let rows = installation
            .store
            .scan(
                project,
                START_AT - RANGE_BEFORE,
                START_AT + RANGE_AFTER,
                TimeBasis::OccurredAt,
            )
            .expect("scan")
            .rows;

        let number = |row: &tallyowl_store::row::EventRow, key: &str| -> Option<f64> {
            match row.properties.get(key)? {
                (tallyowl_store::row::PropertyValue::Float(v), _) => Some(*v),
                (tallyowl_store::row::PropertyValue::Unsigned(v), _) => Some(*v as f64),
                (tallyowl_store::row::PropertyValue::Integer(v), _) => Some(*v as f64),
                _ => None,
            }
        };
        let text = |row: &tallyowl_store::row::EventRow, key: &str| -> Option<String> {
            match row.properties.get(key)? {
                (tallyowl_store::row::PropertyValue::Text(v), _) => Some(v.clone()),
                _ => None,
            }
        };

        for (name, shape) in &expected.metrics {
            let points: Vec<&tallyowl_store::row::EventRow> =
                rows.iter().filter(|row| row.name == *name).collect();
            assert_eq!(
                points.len(),
                shape.series,
                "{}: `{name}` has {} series and the ledger expects {}",
                expected.credential,
                points.len(),
                shape.series
            );

            for point in &points {
                assert_eq!(point.kind, "metric-point");
                assert_eq!(
                    text(point, "metric_kind").as_deref(),
                    Some(shape.kind.as_str())
                );
                // A cumulative point carries the start it counts from, which is
                // what makes a restart visible rather than a counter that ran
                // backwards.
                assert!(point.properties.contains_key("start_at"));
                assert!(point.properties.contains_key("end_at"));
                // The series identity reached storage, so `rate` groups by it
                // rather than by whatever the query happened to group by.
                assert!(point.properties.contains_key("series_key"));
            }

            if shape.kind == "histogram" {
                let observed: f64 = points
                    .iter()
                    .filter_map(|row| number(row, "histogram_count"))
                    .sum();
                let total: f64 = points
                    .iter()
                    .filter_map(|row| number(row, "histogram_sum"))
                    .sum();
                assert_eq!(
                    observed, shape.total,
                    "{}: `{name}` holds {observed} observations and the ledger expects {}",
                    expected.credential, shape.total
                );
                assert!(
                    (total - shape.sum).abs() < 1e-9,
                    "{}: `{name}` sums to {total} and the ledger expects {}",
                    expected.credential,
                    shape.sum
                );
                // Every bucket layout reached storage, so `histogram_merge` has
                // something to compare rather than two layouts it must refuse.
                let layouts: std::collections::BTreeSet<String> = points
                    .iter()
                    .filter_map(|row| text(row, "histogram_bounds"))
                    .collect();
                assert_eq!(
                    layouts.len(),
                    1,
                    "{}: `{name}` published more than one bucket layout",
                    expected.credential
                );
                // The exemplar links a point to a trace.
                assert!(
                    points
                        .iter()
                        .any(|row| row.properties.contains_key("exemplar_trace_id")),
                    "{}: no point of `{name}` carries an exemplar",
                    expected.credential
                );
            } else {
                let total: f64 = points.iter().filter_map(|row| number(row, "value")).sum();
                assert_eq!(
                    total, shape.total,
                    "{}: `{name}` totals {total} and the ledger expects {}",
                    expected.credential, shape.total
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "the scenario published no metrics to check");
}

#[test]
fn the_reference_application_produces_the_same_ledger_against_a_replicated_installation() {
    // Phase 7's last exit criterion: "the reference application runs unchanged
    // against the replicated installation and produces identical ledger
    // results."
    //
    // Nothing about the application, the driver, the collector, the head, or
    // the scenario differs from
    // `the_reference_application_produces_events_that_match_the_ledger`. The
    // one difference is that the head commits through a real tablet consensus
    // group with a durable log. That is the claim: the seam did not move.
    //
    // **This is one voter, not three.** Three voters over real sockets are
    // covered in `tallyowl-cluster`'s own tests; running the whole reference
    // application against three heads needs three data directories and is not
    // done here. docs/PHASE7_REPORT.md says so.
    let workspace = temporary_directory("ledger-replicated");
    let installation = Installation::start_replicated(&workspace);
    let ledger_path = workspace.join("ledger.json");

    let report = run_scenario(
        &installation.collector.local_address().to_string(),
        &ledger_path,
        &installation.credentials,
    );
    assert!(report.contains("seedstore-fast"), "{report}");
    assert!(installation.deliver_all() > 0, "nothing reached the head");

    for expected in read_ledger(&ledger_path) {
        let project = project_id(&installation, &expected.credential);
        let rows = installation
            .store
            .scan(
                project,
                START_AT - RANGE_BEFORE,
                START_AT + RANGE_AFTER,
                TimeBasis::OccurredAt,
            )
            .expect("scan")
            .rows;

        let mut logical: BTreeMap<[u8; 16], usize> = BTreeMap::new();
        for row in rows
            .iter()
            .filter(|row| !row.properties.contains_key("derived"))
        {
            *logical.entry(row.event_id).or_insert(0) += 1;
        }
        assert_eq!(
            logical.len(),
            expected.total_events,
            "{}: the ledger expects {} logical events and the replicated store holds {}",
            expected.credential,
            expected.total_events,
            logical.len()
        );

        let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
        let mut counted: BTreeMap<[u8; 16], ()> = BTreeMap::new();
        for row in rows
            .iter()
            .filter(|row| !row.properties.contains_key("derived"))
        {
            if counted.insert(row.event_id, ()).is_none() {
                *by_kind.entry(row.kind.clone()).or_insert(0) += 1;
            }
        }
        assert_eq!(
            by_kind, expected.count_by_kind,
            "{}: the breakdown by kind does not match the ledger",
            expected.credential
        );

        let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
        counted.clear();
        for row in rows
            .iter()
            .filter(|row| !row.properties.contains_key("derived"))
        {
            if counted.insert(row.event_id, ()).is_none() {
                *by_name.entry(row.name.clone()).or_insert(0) += 1;
            }
        }
        assert_eq!(
            by_name, expected.count_by_name,
            "{}: the breakdown by name does not match the ledger",
            expected.credential
        );

        // The scenario sends some identifiers twice on purpose, in different
        // batches. Two batches are two commits and the store holds two physical
        // rows; what must hold is that the ledger counted the identifier once
        // and the event is still retrievable. Consensus in front of the write
        // path changes neither, and this asserts exactly what the local
        // installation asserts rather than a stronger claim TallyOwl does not
        // make.
        for duplicate in &expected.duplicate_event_ids {
            assert!(
                expected.event_ids.contains(duplicate),
                "{}: a duplicate names an identifier the ledger never counted",
                expected.credential
            );
            assert!(
                installation
                    .store
                    .lookup_event(*duplicate)
                    .expect("lookup")
                    .is_some(),
                "{}: a duplicated event is missing entirely",
                expected.credential
            );
        }

        // An exact lookup on a high-cardinality value stays exact.
        for id in &expected.event_ids {
            assert!(
                installation
                    .store
                    .lookup_event(*id)
                    .expect("the lookup")
                    .is_some(),
                "{}: an event the ledger names is not retrievable by its ID",
                expected.credential
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 8: one person, three client surfaces, and the analyses over them.
// ---------------------------------------------------------------------------

/// The reference application's end user uses the web, rich, and mobile clients,
/// and the funnel, retention, and timeline results match the ledger.
///
/// That sentence is Phase 8's fourth exit criterion. The simulator wrote what it
/// expects **before it sent anything**, so this compares TallyOwl against a
/// prediction rather than against itself.
///
/// Every number here is a multiplication a person can do in their head: four
/// people, three surfaces each, three return days. The ledger states them; this
/// asks TallyOwl the same questions and compares.
#[test]
fn the_reference_application_end_user_uses_three_clients_and_the_analyses_match_the_ledger() {
    use tallyowl_control_api::types::{
        CompareOp, CorrelationBasis, FunnelQuery, FunnelStep, QueryForm, RetentionQuery,
        RetentionQuery_period as WirePeriod, TimeRange, TimelineQuery,
    };
    use tallyowl_head::identity::{Identity, Resolution, ANONYMOUS_ID, END_USER_ID};
    use tallyowl_wire::{control as wire, query, Value};

    let workspace = temporary_directory("phase8-ledger");
    let installation = Installation::start(&workspace);
    let ledger_path = workspace.join("ledger.json");
    run_scenario(
        &installation.collector.local_address().to_string(),
        &ledger_path,
        &installation.credentials,
    );
    assert!(installation.deliver_all() > 0, "nothing reached the head");

    let expected = read_ledger(&ledger_path)
        .into_iter()
        .find(|project| !project.anonymous_by_end_user.is_empty())
        .expect("one application runs the identity journey");
    let project = project_id(&installation, &expected.credential);

    let range = TimeRange {
        range_start: START_AT - RANGE_BEFORE,
        range_end: START_AT + RANGE_AFTER,
        basis: tallyowl_control_api::types::TimeBasis::OccurredAt,
        timezone: None,
    };
    let matches_name = |name: &str| {
        query::expression_ref(&query::compare(
            CompareOp::Eq,
            &query::expression::field(query::field("name")),
            &query::expression::literal(wire::write(&Value::Text(name.to_string()))),
        ))
    };
    let answer = |request| {
        installation
            .query
            .run(request)
            .expect("the analysis answers")
    };
    let number = |value: &tallyowl_control_api::types::TypedValue| match wire::read(value).unwrap()
    {
        Value::Unsigned(n) => n,
        Value::Integer(n) => n as u64,
        other => panic!("expected a number and got {other:?}"),
    };

    // --- Identity -----------------------------------------------------------
    //
    // Every anonymous identifier the person used resolves to the person, and
    // the graph the store holds says so.
    let rows = installation
        .store
        .scan(
            project,
            range.range_start,
            range.range_end,
            TimeBasis::OccurredAt,
        )
        .expect("scan")
        .rows;
    let identity = Identity::build(project, &rows);
    assert_eq!(
        identity.foreign_rows_ignored(),
        0,
        "every row scanned belongs to this project"
    );
    for (end_user, anonymous_ids) in &expected.anonymous_by_end_user {
        assert_eq!(
            anonymous_ids.len(),
            3,
            "{end_user} used the web, rich, and mobile clients"
        );
        for anonymous_id in anonymous_ids {
            let seen = rows
                .iter()
                .find(|row| {
                    tallyowl_head::identity::text(row, ANONYMOUS_ID).as_deref()
                        == Some(anonymous_id.as_str())
                        && tallyowl_head::identity::text(row, END_USER_ID).is_none()
                })
                .unwrap_or_else(|| {
                    panic!("the anonymous page view for {anonymous_id} reached the store")
                });
            assert_eq!(
                identity.who(seen, Resolution::LatestKnown).as_deref(),
                Some(end_user.as_str()),
                "{anonymous_id} is {end_user} once they identified"
            );
        }
    }

    // --- Funnel -------------------------------------------------------------
    //
    // Home, checkout, purchase, correlated by end user. Everybody reached every
    // step, on whichever surface they happened to be using.
    let mut request = query::empty_request(1, QueryForm::Funnel);
    request.funnel = Some(FunnelQuery {
        project_id: project.to_vec(),
        range: range.clone(),
        steps: expected
            .funnel
            .iter()
            .map(|(name, _)| FunnelStep {
                name: name.clone(),
                r#match: matches_name(name),
                exclusion: None,
            })
            .collect(),
        // A week, so the whole journey fits inside one sequence.
        window_ms: 7 * 86_400_000,
        basis: CorrelationBasis::EndUser,
        ordered: true,
        breakdown: None,
        resolution: None,
    });
    let funnel = answer(request);
    assert_eq!(funnel.rows.len(), expected.funnel.len());
    for (index, (name, reached)) in expected.funnel.iter().enumerate() {
        assert_eq!(
            number(&funnel.rows[index].values[2]) as usize,
            *reached,
            "the funnel step `{name}` does not match the ledger"
        );
    }

    // --- Retention ----------------------------------------------------------
    //
    // One cohort, because everybody signed up in the same period, and it
    // returns in full on every day the scenario said it would.
    let mut request = query::empty_request(1, QueryForm::Retention);
    request.retention = Some(RetentionQuery {
        project_id: project.to_vec(),
        range: range.clone(),
        initial: matches_name("journey-signed-up"),
        returning: matches_name("journey-visit"),
        period: WirePeriod::Day,
        periods: expected.retention.periods as u64,
        first_time_only: false,
        resolution: None,
    });
    let retention = answer(request);
    assert_eq!(
        retention.rows.len(),
        1,
        "everybody signed up in the same period, so there is one cohort"
    );
    let cohort = &retention.rows[0];
    assert_eq!(
        number(&cohort.values[1]) as usize,
        expected.retention.cohort_size,
        "the cohort size does not match the ledger"
    );
    for (period, returned) in expected.retention.returned_by_period.iter().enumerate() {
        // The columns after the size are pairs of count and rate.
        assert_eq!(
            number(&cohort.values[2 + period * 2]) as usize,
            *returned,
            "period {period} of the retention matrix does not match the ledger"
        );
    }

    // --- Timeline -----------------------------------------------------------
    //
    // One person's whole timeline, across every surface they used. It holds
    // exactly what the scenario sent for them and nothing from anybody else.
    for (end_user, items) in &expected.timeline_by_end_user {
        let mut request = query::empty_request(1, QueryForm::Timeline);
        request.timeline = Some(TimelineQuery {
            project_id: project.to_vec(),
            range: range.clone(),
            end_user_id: Some(end_user.clone()),
            session_id: None,
            kinds: Vec::new(),
            limit: 1_000,
            cursor: None,
            resolution: None,
        });
        let timeline = answer(request);
        assert_eq!(
            timeline.rows.len(),
            *items,
            "{end_user}'s timeline holds {} items and the ledger expects {items}",
            timeline.rows.len()
        );
    }
}

/// Phase 9's exit criterion, in one test.
///
/// > every attribution model matches the ledger for traffic that arrives from
/// > the reference marketing site landing pages.
///
/// The simulator writes the ledger **before** it sends anything. It builds the
/// journey so that every model divides the conversion value into whole units —
/// three touches exactly one decay half-life apart and a value of 70 — and then
/// this asks TallyOwl the same six questions through the head's own query
/// executor and compares.
///
/// The campaign parameters come out of real landing-page addresses in the
/// simulator, so what is compared here is what TallyOwl read from a browser's
/// address rather than what a scenario asserted about itself.
#[test]
fn every_attribution_model_matches_the_ledger_for_the_marketing_site_traffic() {
    use tallyowl_control_api::types::{
        AttributionModel, AttributionQuery, CampaignSummaryQuery,
        CampaignSummaryQuery_dimension as SummaryDimension, QueryForm, TimeRange,
    };
    use tallyowl_wire::{control as wire, query, Value};

    let workspace = temporary_directory("phase9-ledger");
    let installation = Installation::start(&workspace);
    let ledger_path = workspace.join("ledger.json");
    run_scenario(
        &installation.collector.local_address().to_string(),
        &ledger_path,
        &installation.credentials,
    );
    assert!(installation.deliver_all() > 0, "nothing reached the head");

    let expected = read_ledger(&ledger_path)
        .into_iter()
        .find(|project| !project.attribution.by_model.is_empty())
        .expect("one application runs the marketing journey");
    let project = project_id(&installation, &expected.credential);

    let range = TimeRange {
        range_start: START_AT - RANGE_BEFORE,
        range_end: START_AT + RANGE_AFTER,
        basis: tallyowl_control_api::types::TimeBasis::OccurredAt,
        timezone: None,
    };

    let ask = |model: AttributionModel| {
        let mut request = query::empty_request(
            tallyowl_head::query::ALGEBRA_VERSION,
            QueryForm::Attribution,
        );
        request.attribution = Some(AttributionQuery {
            project_id: project.to_vec(),
            range: range.clone(),
            conversion_goal: expected.attribution.goal.clone(),
            model,
            lookback_ms: expected.attribution.lookback_ms,
            touch_filter: None,
            breakdown: None,
            resolution: None,
        });
        installation
            .query
            .run(request)
            .expect("the attribution question answers")
    };

    let credited =
        |response: &tallyowl_control_api::types::QueryResponse| -> BTreeMap<String, String> {
            response
                .rows
                .iter()
                .map(|row| {
                    let campaign = match wire::read(&row.values[0]).unwrap() {
                        Value::Text(text) => text,
                        other => panic!("a campaign name, and got {other:?}"),
                    };
                    let value = match wire::read(&row.values[1]).unwrap() {
                        Value::Decimal { .. } => wire::read(&row.values[1]).unwrap().to_display(),
                        other => panic!("an exact decimal, and got {other:?}"),
                    };
                    (campaign, value)
                })
                .collect()
        };

    // --- Every model, against the ledger ------------------------------------
    for (name, model) in [
        ("first-touch", AttributionModel::FirstTouch),
        ("last-touch", AttributionModel::LastTouch),
        ("last-non-direct", AttributionModel::LastNonDirect),
        ("linear", AttributionModel::Linear),
        ("position", AttributionModel::Position),
        ("decay", AttributionModel::Decay),
    ] {
        let want = expected
            .attribution
            .by_model
            .get(name)
            .unwrap_or_else(|| panic!("the ledger predicts the `{name}` model"));
        let got = credited(&ask(model));
        for (campaign, amount) in want {
            let seen = got.get(campaign).map(String::as_str).unwrap_or("0");
            assert_eq!(
                seen,
                amount.as_str(),
                "the `{name}` model credited `{campaign}` with {seen} and the ledger says {amount}. \
                 The whole answer was {got:?}"
            );
        }

        // Whatever the model, the credited parts add up to the revenue the
        // question covered. A model that broke this would produce a campaign
        // report that disagreed with the revenue report, and nobody could say
        // which one was right.
        let total: f64 = got
            .values()
            .filter_map(|value| value.parse::<f64>().ok())
            .sum();
        let revenue = 70.0 * expected.attribution.conversions as f64;
        assert!(
            (total - revenue).abs() < 0.000_001,
            "the `{name}` model credited {total} of {revenue}"
        );
    }

    // --- One order, however many rows carried it ----------------------------
    //
    // The simulator sent every purchase twice under one order identifier, a
    // minute apart and with different event identifiers. That is not a
    // duplicate delivery — both rows are stored — and only the order fold makes
    // it one conversion.
    assert!(
        expected.attribution.order_repeats > 0,
        "the scenario has to send a repeat for this to prove anything"
    );
    let rows = installation
        .store
        .scan(
            project,
            range.range_start,
            range.range_end,
            TimeBasis::OccurredAt,
        )
        .expect("scan")
        .rows;
    let stored = rows
        .iter()
        .filter(|row| row.kind == "conversion" && row.name == expected.attribution.goal)
        .count();
    assert_eq!(
        stored,
        expected.attribution.conversions + expected.attribution.order_repeats,
        "both rows of each order reached storage"
    );

    // --- The classifier put each touch in the right channel -----------------
    //
    // A classifier that put the paid click in the organic column would show up
    // as a marketing budget somebody argues about, so it is asserted here
    // rather than left to a unit test on its own.
    let mut seen_by_channel: BTreeMap<String, usize> = BTreeMap::new();
    for row in rows.iter().filter(|row| row.kind == "campaign-touch") {
        *seen_by_channel
            .entry(
                tallyowl_head::campaign::channel_of(row)
                    .as_str()
                    .to_string(),
            )
            .or_default() += 1;
    }
    assert_eq!(
        seen_by_channel, expected.attribution.touches_by_channel,
        "the channels TallyOwl classified are not the ones the landing pages were"
    );

    // --- The campaign report ------------------------------------------------
    let mut summary = query::empty_request(
        tallyowl_head::query::ALGEBRA_VERSION,
        QueryForm::CampaignSummary,
    );
    summary.campaign_summary = Some(CampaignSummaryQuery {
        project_id: project.to_vec(),
        range: range.clone(),
        conversion_goal: expected.attribution.goal.clone(),
        model: AttributionModel::LastNonDirect,
        lookback_ms: expected.attribution.lookback_ms,
        dimension: Some(SummaryDimension::Campaign),
        touch_filter: None,
        resolution: None,
    });
    let report = installation
        .query
        .run(summary)
        .expect("the campaign report answers");

    let column = |name: &str| {
        report
            .columns
            .iter()
            .position(|held| held == name)
            .unwrap_or_else(|| panic!("the report has a `{name}` column: {:?}", report.columns))
    };
    let campaign_at = column("campaign");
    let cost_at = column("cost");
    let value_at = column("value");
    let return_at = column("return");

    let mut costs_seen: BTreeMap<String, String> = BTreeMap::new();
    for row in &report.rows {
        let Value::Text(campaign) = wire::read(&row.values[campaign_at]).unwrap() else {
            panic!("a campaign name");
        };
        let cost = wire::read(&row.values[cost_at]).unwrap().to_display();
        if cost != "0" {
            costs_seen.insert(campaign.clone(), cost);
        }
        // The return column is the credited value against the spend, and it is
        // absent rather than zero when nothing was spent: a zero would read as
        // "this campaign earned nothing for its spend".
        let spent = expected
            .attribution
            .cost_by_campaign
            .contains_key(&campaign);
        let has_return = !matches!(wire::read(&row.values[return_at]).unwrap(), Value::Null);
        assert_eq!(
            has_return, spent,
            "the return column for `{campaign}` disagrees with whether it had a cost"
        );
        if spent {
            let value = wire::read(&row.values[value_at]).unwrap().to_display();
            assert!(
                !value.is_empty(),
                "a campaign with a spend has a credited value"
            );
        }
    }
    assert_eq!(
        costs_seen, expected.attribution.cost_by_campaign,
        "the imported spend is not what the scenario imported"
    );
}

// ---------------------------------------------------------------------------
// Alerting, over the reference application's own traffic. Phase 10.
// ---------------------------------------------------------------------------

/// An alert rule over the events the reference application actually sent.
///
/// **`AGENTS.md`: a capability without reference-application coverage is not
/// complete.** Every alert test elsewhere builds its own rows, which proves the
/// rules and says nothing about whether a rule written against real application
/// traffic finds it. This asks the same question a person would: the scenario
/// ran, so is an alert on "any events at all" firing, and does its value agree
/// with what the ledger says arrived?
#[test]
fn an_alert_over_the_reference_applications_traffic_fires_and_agrees_with_the_ledger() {
    use tallyowl_control_api::types::{
        AlertRule, CompareOp, NotificationTarget, NotificationTarget_kind as TargetKind,
        ThresholdCondition, TimeRange,
    };

    let workspace = temporary_directory("alerting");
    let installation = Installation::start(&workspace);
    let ledger_path = workspace.join("ledger.json");
    run_scenario(
        &installation.collector.local_address().to_string(),
        &ledger_path,
        &installation.credentials,
    );
    assert!(installation.deliver_all() > 0, "nothing reached the head");

    let expected = read_ledger(&ledger_path)
        .into_iter()
        .next()
        .expect("the ledger names an application");
    let project = project_id(&installation, &expected.credential);
    let alerts = Arc::new(tallyowl_head::alerts::AlertService {
        store: Arc::clone(&installation.segmented),
        query: Arc::clone(&installation.query),
        metrics: Registry::new(),
        callbacks_available: false,
        pool: Default::default(),
    });

    let rule = AlertRule {
        rule_id: "any-traffic".into(),
        name: "Any traffic at all".into(),
        project_id: project.to_vec(),
        query: tallyowl_wire::query::trend(
            1,
            tallyowl_wire::query::events(
                &project,
                TimeRange {
                    range_start: START_AT - RANGE_BEFORE,
                    range_end: START_AT + RANGE_AFTER,
                    basis: tallyowl_control_api::types::TimeBasis::OccurredAt,
                    timezone: None,
                },
            ),
            RANGE_AFTER + RANGE_BEFORE,
            "events",
        ),
        interval_ms: 60_000,
        threshold: Some(ThresholdCondition {
            alias: "events".into(),
            compare: CompareOp::Gt,
            value: 0.0,
            sustained_ms: None,
        }),
        absence: None,
        notify: vec![NotificationTarget {
            kind: TargetKind::Webhook,
            url: Some("http://127.0.0.1:1/alerts".into()),
            secret_ref: None,
        }],
        enabled: true,
        escalate_after_ms: None,
        silenced_until: None,
        silence_reason: None,
        disabled_reason: None,
        updated_at: None,
        updated_by: None,
    };
    let rule = alerts.put_rule(&rule, "test").expect("the rule is stored");

    let first = alerts
        .evaluate_and_record(&rule)
        .expect("the rule evaluates");
    assert!(
        first.notify,
        "a rule over real traffic did not change state, so nothing would be told"
    );
    assert_eq!(first.instance.state, "firing");

    // **The value is the ledger's own count.** An alert value that did not
    // agree with what the application sent would be a number nobody could
    // reconcile, which is the failure `docs/ALERTS.md` section 2 exists to
    // prevent.
    let ledger_events = expected.event_ids.len() as f64;
    assert!(
        first.instance.observed_value >= ledger_events,
        "the alert counted {} and the ledger names {ledger_events}",
        first.instance.observed_value
    );

    // And a second evaluation over the same traffic says nothing, which is the
    // Phase 10 exit criterion seen from the reference application's side.
    let second = alerts
        .evaluate_and_record(&rule)
        .expect("the rule evaluates again");
    assert!(
        !second.notify,
        "a repeated evaluation over unchanged traffic sent a second notification"
    );
    assert_eq!(second.instance.notifications_sent, 1);
}
