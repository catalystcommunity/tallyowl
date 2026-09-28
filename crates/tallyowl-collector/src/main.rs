//! `tallyowl-collector`.
//!
//! Intake accepts a batch from an app driver and puts it in the durable store.
//! The forwarder drains that store into the head and keeps the timeout sweep
//! running. Both roles are independently deployable, even though the home
//! profile runs them together.
//!
//! There is no development mode here, and there is not going to be one. A
//! developer runs the `home` profile, which is the smallest supported
//! production deployment, so a bug that appears in a home installation appears
//! on a workstation. See `docs/PLAN.md` Phase 1.

use std::sync::Arc;
use std::time::Duration;

use tallyowl_config::{check, Config};
use tallyowl_obs::health::Health;
use tallyowl_obs::log::{Logger, Severity};
use tallyowl_obs::metrics::Registry;

use tallyowl_collector::compat::{self, Edge};
use tallyowl_collector::durable::{CorndogsQueue, DurableQueue, QueueOptions};
use tallyowl_collector::forwarder::{
    self, Forwarder, ForwarderState, DELIVERY_CHECK, DURABLE_STORE_CHECK, SWEEP_CHECK,
};
use tallyowl_collector::head_client::RemoteHead;
use tallyowl_collector::intake::{Intake, Limits};
use tallyowl_collector::selfobs;
use tallyowl_collector::series::{SeriesBudget, SeriesLedger};
use tallyowl_collector::service::CollectorService;
use tallyowl_collector::tenancy::{KeyDirectory, TenancyResolver};
use tallyowl_collector::transport::{self, CertificateWatch, Exposure};

const SERVICE: &str = "tallyowl-collector";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_CONFIG_FILE: &str = "tallyowl.local.yaml";
/// The check that says intake is listening.
const INTAKE_CHECK: &str = "intake-listener";
/// The check that says every role thread is still running.
const ROLE_THREADS_CHECK: &str = "role-threads";

/// Stopping in order, when the host asks for it.
///
/// `docs/DELIVERY.md` section 10: stop intake, finish or release claimed tasks,
/// flush receipts. None of that ran before, because nothing listened for the
/// request to stop: a rolling restart killed the process mid-delivery and every
/// claimed batch waited out its claim before another collector could take it.
mod shutdown {
    use std::sync::atomic::{AtomicBool, Ordering};

    static REQUESTED: AtomicBool = AtomicBool::new(false);

    /// Whether the host has asked this process to stop.
    pub fn requested() -> bool {
        REQUESTED.load(Ordering::Relaxed)
    }

    #[cfg(unix)]
    extern "C" fn note(_signal: libc::c_int) {
        // Storing to an atomic is one of the few things a signal handler may do.
        REQUESTED.store(true, Ordering::Relaxed);
    }

    /// Listen for SIGTERM and SIGINT.
    #[cfg(unix)]
    pub fn listen() {
        for signal in [libc::SIGTERM, libc::SIGINT] {
            // SAFETY: `note` only stores to an atomic, which is safe in a signal
            // handler, and it lives for the whole process.
            unsafe {
                libc::signal(
                    signal,
                    note as extern "C" fn(libc::c_int) as libc::sighandler_t,
                );
            }
        }
    }

    #[cfg(not(unix))]
    pub fn listen() {}
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let config_file = config_file_from(&arguments);

    // `config check` runs before anything else starts, so a person can ask what
    // the configuration resolves to without starting a service.
    if arguments.first().map(|a| a.as_str()) == Some("config")
        && arguments.get(1).map(|a| a.as_str()) == Some("check")
    {
        let report = check::run_on_host(&config_file);
        print!("{}", report.text);
        std::process::exit(report.exit_code);
    }

    let config = match Config::load_from_host(&config_file) {
        Ok(config) => config,
        Err(errors) => {
            // A service that starts with bad configuration fails later, in
            // production, where nobody connects the failure to the setting.
            eprintln!("{SERVICE} cannot start.");
            for error in &errors {
                eprintln!("  {}", error.message);
            }
            eprintln!(
                "\nRun `{SERVICE} config check` to see every setting and where it came from."
            );
            std::process::exit(1);
        }
    };

    if let Err(e) = run(config) {
        eprintln!("{SERVICE} stopped: {e}");
        std::process::exit(1);
    }
}

/// What this process runs, read from `collector.roles`.
///
/// The compatibility edge offers what it scrapes and receives to the same
/// accept path a native batch takes, so it needs that path built whether or not
/// this process also listens for app drivers. A process with only the
/// `compatibility-receiver` role used to pass validation and then do nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RolePlan {
    /// Build the accept path: tenancy, the series ledger, policy, and intake.
    accept_path: bool,
    /// Listen on `collector.listen` for app drivers.
    intake_listener: bool,
    /// Drain the durable store into the head and keep the sweep running.
    forwarder: bool,
}

fn plan_roles(roles: &[String]) -> RolePlan {
    let has = |name: &str| roles.iter().any(|role| role == name);
    RolePlan {
        accept_path: has("intake") || has("compatibility-receiver"),
        intake_listener: has("intake"),
        forwarder: has("forwarder"),
    }
}

fn config_file_from(arguments: &[String]) -> String {
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == "--config" {
            return arguments
                .get(index + 1)
                .cloned()
                .unwrap_or_else(|| DEFAULT_CONFIG_FILE.to_string());
        }
        if let Some(value) = arguments[index].strip_prefix("--config=") {
            return value.to_string();
        }
        index += 1;
    }
    std::env::var("TALLYOWL_CONFIG_FILE").unwrap_or_else(|_| DEFAULT_CONFIG_FILE.to_string())
}

fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let level = Severity::parse(config.text("log.level")).unwrap_or(Severity::Info);
    let logger = Arc::new(Logger::new(SERVICE, VERSION, level));
    let metrics = Registry::new();
    let health = Health::new();

    Intake::declare_metrics(&metrics);
    Forwarder::declare_metrics(&metrics);
    CorndogsQueue::declare_metrics(&metrics);
    RemoteHead::declare_metrics(&metrics);
    declare_listener_metrics(&metrics);
    transport::declare_metrics(&metrics);
    shutdown::listen();
    // A counter that nothing declares is silently dropped when it is added to,
    // so a policy that blocked a thousand events would report nothing. The
    // running loop found this one: the drop counter was missing from the
    // exposition while the block itself worked.
    tallyowl_collector::policy::Refresher::declare_metrics(&metrics);

    let roles = config.list("collector.roles");
    let plan = plan_roles(&roles);
    logger.info(
        "Starting.",
        &[
            ("profile", config.text("installation.profile")),
            ("roles", &roles.join(",")),
            (
                "config_file",
                config.file_path().unwrap_or("none; using defaults"),
            ),
        ],
    );
    // L009 lets a collector start with a key it does not know. It still has to
    // say so: a misspelled key otherwise keeps its default in silence.
    for warning in config.unknown_key_warnings() {
        logger.warning(&warning, &[]);
    }

    // D62. The certificate applications see, for intake and the OpenTelemetry
    // receiver. A collector on loopback or a unix socket needs none.
    let allow_plaintext = config.boolean("transport.allowPlaintext");
    let certificates = transport::application_certificates(
        &config.list("tls.certificateDirectories"),
        tallyowl_rpc::material::now_ms(),
    )?;
    let mut exposed: Vec<(&'static str, String, Exposure)> = Vec::new();
    let mut certificate_listeners: Vec<&'static str> = Vec::new();

    // A secret is a reference. Log that it resolved and from where, never the
    // value.
    let credential = match config.secret("collector.apiKey") {
        Ok(secret) => {
            if secret.is_empty() {
                // Not a failure: an application presents its own key on its own
                // request, and this setting is only the default for a
                // connection that presents none. Say it plainly, because a
                // person whose driver sends no key needs to know where to look.
                logger.info(
                    "No default credential is configured. Every batch must carry its own key. Make one with `tallyowl-head provision <project>` and point `collector.apiKey` at it.",
                    &[],
                );
                String::new()
            } else {
                logger.info(
                    "Resolved the collector credential.",
                    &[("from", &secret.source_description())],
                );
                secret.expose().to_string()
            }
        }
        Err(e) => {
            logger.error("The collector credential could not be resolved.", &[]);
            return Err(Box::new(e));
        }
    };

    // Readiness starts false and each check proves itself. A collector that
    // cannot reach the durable store must never accept data it would discard.
    health.declare(DURABLE_STORE_CHECK, "Cannot reach the durable store yet.");
    if plan.intake_listener {
        health.declare(INTAKE_CHECK, "Intake is not listening yet.");
    }
    if plan.forwarder {
        health.declare(
            SWEEP_CHECK,
            "The retry sweep has not run yet, so a failed delivery would not be tried again.",
        );
        health.declare(DELIVERY_CHECK, "The delivery loop has not turned yet.");
    }

    let operational = tallyowl_obs::http::start(
        config.text("collector.operationalListen"),
        Arc::clone(&health),
        Arc::clone(&metrics),
        Arc::clone(&logger),
    )?;
    logger.info(
        "Serving health and metrics.",
        &[("address", &operational.local_address().to_string())],
    );

    let endpoint = config.text("corndogs.endpoint");
    // D62: TLS to a Corndogs endpoint that is not loopback.
    let queue_tls = tallyowl_queue::QueueTls::for_endpoint(
        endpoint,
        config.text("corndogs.tls.caFile"),
        config.text("corndogs.tls.serverName"),
        config.boolean("transport.allowPlaintext"),
    );
    let queue_secured = queue_tls.is_some();
    let queue_options = QueueOptions {
        connections: config.integer("corndogs.connections").max(1) as usize,
        call_timeout: Duration::from_millis(config.duration_ms("corndogs.callTimeout") as u64),
        metrics: Some(Arc::clone(&metrics)),
        tls: queue_tls,
    };
    let queue: Arc<dyn DurableQueue> = match CorndogsQueue::connect_with(endpoint, queue_options) {
        Ok(queue) => Arc::new(queue),
        Err(e) => {
            // Failing to start is the right answer. The alternative is a
            // collector that listens, accepts, and throws data away.
            logger.error(
                "Cannot reach the durable store.",
                &[("address", endpoint), ("reason", &e.message)],
            );
            return Err(Box::new(e));
        }
    };
    health.pass(DURABLE_STORE_CHECK);
    logger.info(
        "Reached the durable store.",
        &[
            ("address", endpoint),
            ("transport", if queue_secured { "tls" } else { "plaintext" }),
        ],
    );

    let forwarder_state = ForwarderState::new();
    let max_frame = config.bytes("corndogs.maxPayloadBytes") as usize;

    // Two connections to the head, one for each job, and each with a deadline.
    // A call holds its connection for the whole round trip. When both jobs
    // shared one, a slow commit held every intake worker whose key had just
    // expired, and a head that stopped answering held all of them for ever.
    let head_call_timeout =
        Duration::from_millis(config.duration_ms("collector.headCallTimeout") as u64);
    // D62. A head on a network is reached over mutual TLS, with an identity
    // this collector enrolls for at every start: a key in memory, since a
    // collector keeps no state. Until the first certificate arrives a call
    // to the head fails as retryable. Intake does not call the head for that,
    // so it keeps acknowledging into Corndogs, and the forwarder waits.
    let node_security = tallyowl_identity::for_collector(
        &config,
        Arc::new(tallyowl_identity::SystemClock) as Arc<dyn tallyowl_identity::Clock>,
    )?;
    if let Some((_, _, handle)) = &node_security {
        handle.enrolled().publish_to(Arc::clone(&metrics));
        handle.enrolled().log_to(Arc::clone(&logger));
        // A new authority in `installation.authorities` takes effect with no
        // restart, which is how the authority rotates (D62).
        let reporting = Arc::clone(&logger);
        handle.watch_trust(
            Duration::from_millis(config.duration_ms("tls.reloadInterval") as u64),
            Arc::new(move |_, result| {
                reporting.info(
                    "Read the trusted authorities again.",
                    &[("result", &format!("{result:?}"))],
                )
            }),
        )?;
        logger.info(
            "Enrolling with the head for a node certificate.",
            &[("head", config.text("head.endpoint"))],
        );
    }
    let head_for = || {
        let remote = match &node_security {
            Some((identity, trust, _)) => RemoteHead::mutual(
                config.text("head.endpoint"),
                max_frame,
                tallyowl_identity::HEAD_SERVER_NAME,
                Arc::clone(identity),
                Arc::clone(trust),
            ),
            None => RemoteHead::new(config.text("head.endpoint"), max_frame),
        };
        Arc::new(
            remote
                .with_call_timeout(head_call_timeout)
                .with_metrics(Arc::clone(&metrics)),
        )
    };
    // The forwarder commits batches on this one.
    let delivery_head = head_for();
    // Intake resolves credentials and fetches policy on this one, because the
    // head owns the control catalog and a collector stores no key.
    let head = head_for();

    let mut threads = Vec::new();
    if plan.forwarder {
        let forwarder = Arc::new(Forwarder {
            queue: Arc::clone(&queue),
            queue_name: config.text("corndogs.deliveryQueue").to_string(),
            quarantine_queue: config.text("corndogs.quarantineQueue").to_string(),
            head: Arc::clone(&delivery_head)
                as Arc<dyn tallyowl_collector::head_client::HeadClient>,
            health: Arc::clone(&health),
            metrics: Arc::clone(&metrics),
            logger: Arc::clone(&logger),
            state: Arc::clone(&forwarder_state),
            sweep_staleness_ms: config.duration_ms("corndogs.sweepInterval") * 3,
            max_payload_bytes: config.bytes("corndogs.maxPayloadBytes").max(0) as u64,
            max_delivery_age_ms: config.duration_ms("corndogs.maxDeliveryAge"),
        });
        let interval = Duration::from_millis(config.duration_ms("corndogs.sweepInterval") as u64);
        let depth_interval =
            Duration::from_millis(config.duration_ms("corndogs.depthInterval") as u64);
        threads.extend(forwarder::run_with(forwarder, interval, depth_interval));
        logger.info(
            "Running the forwarder role.",
            &[
                ("head", config.text("head.endpoint")),
                (
                    "sweep_interval_ms",
                    &config.duration_ms("corndogs.sweepInterval").to_string(),
                ),
            ],
        );
    }

    let mut intake_server = None;
    let mut compat_running: Option<compat::Running> = None;
    let mut self_observation: Option<selfobs::Publisher> = None;
    if plan.accept_path {
        // The credential the compatibility edge presents. A scrape target and
        // an OpenTelemetry exporter carry no TallyOwl key, so the collector
        // presents its own and that is what chooses the project.
        let compat_credential = credential.clone();
        // The collection policy this collector applies. `docs/POLICY.md`
        // section 7: it is fetched, cached, and applied at a batch boundary.
        // A collector that has never fetched one holds none and collects
        // everything, which is what an installation with no policy means.
        let held_policy = Arc::new(tallyowl_collector::policy::Held::new(
            config.duration_ms("collector.policyInterval") * 3,
        ));
        let intake = Arc::new(Intake {
            queue: Arc::clone(&queue),
            queue_name: config.text("corndogs.deliveryQueue").to_string(),
            tenancy: Arc::new(TenancyResolver::new(
                Arc::clone(&head) as Arc<dyn KeyDirectory>,
                config.duration_ms("collector.keyCacheGrace"),
            )),
            limits: Limits {
                max_batch_bytes: config.bytes("collector.maxBatchBytes"),
                max_event_bytes: config.bytes("collector.maxEventBytes"),
                max_properties: config.integer("collector.maxProperties"),
            },
            durable_copies: config.integer("corndogs.durableCopies") as u64,
            series: Arc::new(
                SeriesLedger::new(SeriesBudget {
                    max_series_for_each_metric: config.integer("metrics.maxSeriesForEachMetric")
                        as u64,
                    max_bytes_for_each_metric: config.bytes("metrics.maxBytesForEachMetric") as u64,
                    max_labels: config.integer("metrics.maxLabels") as usize,
                    max_label_value_bytes: config.integer("metrics.maxLabelValueBytes") as usize,
                    max_label_bytes: config.integer("metrics.maxLabelBytes") as usize,
                    max_merge_points: config.integer("metrics.maxMergePoints") as usize,
                    idle_expiry_ms: config.duration_ms("metrics.idleSeriesExpiry"),
                })
                .with_max_metric_names(
                    config.integer("metrics.maxMetricNamesForEachProject") as u64,
                ),
            ),
            metrics: Arc::clone(&metrics),
            stamped: vec![
                ("cell".to_string(), config.text("cell.id").to_string()),
                ("region".to_string(), config.text("cell.region").to_string()),
            ],
            policy: Some(Arc::clone(&held_policy)),
        });

        let compat_intake = Arc::clone(&intake);
        let service = CollectorService {
            intake,
            health: Arc::clone(&health),
            logger: Arc::clone(&logger),
            forwarder_state: Arc::clone(&forwarder_state),
            credential: credential.clone(),
            roles: roles.clone(),
            policy: Some(Arc::clone(&held_policy)),
        };
        // The fetch loop. It starts after intake is built and before the
        // listener opens, so the first batch this collector accepts is judged
        // against a policy rather than against nothing.
        let refresher = Arc::new(tallyowl_collector::policy::Refresher {
            held: Arc::clone(&held_policy),
            source: Arc::clone(&head) as Arc<dyn tallyowl_collector::policy::PolicySource>,
            tenancy: Arc::clone(&compat_intake.tenancy),
            credential: compat_credential.clone(),
            metrics: Arc::clone(&metrics),
            logger: Arc::clone(&logger),
            stopping: Arc::clone(&forwarder_state),
        });
        refresher.fetch_once();
        let policy_interval =
            Duration::from_millis(config.duration_ms("collector.policyInterval") as u64);
        threads.push(tallyowl_collector::policy::run(
            Arc::clone(&refresher),
            policy_interval,
        ));
        logger.info(
            "Fetching the collection policy from the head.",
            &[
                (
                    "interval_ms",
                    &config.duration_ms("collector.policyInterval").to_string(),
                ),
                ("applied_version", &held_policy.version().to_string()),
            ],
        );

        // Only the `intake` role listens for app drivers. A process that runs
        // the compatibility edge alone builds the same accept path and opens no
        // port for them.
        if plan.intake_listener {
            // Intake is the trust boundary, so it has its own limits. The frame
            // limit used to be the durable store's payload limit, which is 32 times
            // a batch, and an unauthenticated peer could make a collector decode
            // that much before anything checked a credential.
            let panic_logger = Arc::clone(&logger);
            let options = tallyowl_rpc::ServerOptions::new(
                config.bytes("collector.maxFrameBytes") as usize,
            )
            .max_connections(config.integer("collector.maxConnections").max(0) as usize)
            .idle_timeout(Duration::from_millis(
                config.duration_ms("collector.idleTimeout").max(0) as u64,
            ))
            .on_panic(Arc::new(move |service, op, said| {
                panic_logger.error(
                    "An intake request failed inside the collector. The caller was told, and the connection kept serving.",
                    &[("service", service), ("operation", op), ("reason", said)],
                );
            }))
            .on_handshake_refused({
                let metrics = Arc::clone(&metrics);
                Arc::new(move || {
                    metrics.increment(
                        transport::HANDSHAKES_REFUSED,
                        &tallyowl_obs::metrics::labels(&[("listener", transport::LISTENER_INTAKE)]),
                    );
                })
            });
            let (server, exposure) = transport::serve_intake(
                config.text("collector.listen"),
                Arc::new(service),
                options,
                certificates.clone(),
                allow_plaintext,
            )?;
            health.pass(INTAKE_CHECK);
            logger.info(
                "Accepting telemetry.",
                &[
                    ("address", &server.bound().to_string()),
                    ("transport", exposure.describe()),
                ],
            );
            if exposure == Exposure::Tls {
                certificate_listeners.push(transport::LISTENER_INTAKE);
            }
            exposed.push((
                "collector.listen",
                config.text("collector.listen").to_string(),
                exposure,
            ));
            intake_server = Some(server);
        }

        // The compatibility edge, if an operator asked for it. It goes through
        // the same intake, so a scraped counter meets the same limits, the same
        // tenancy stamp, and the same durable-acknowledgement rule as a native
        // batch. See D12 and `compat`.
        Edge::declare_metrics(&metrics);
        let edge = Arc::new(Edge {
            intake: compat_intake,
            credential: compat_credential,
            metrics: Arc::clone(&metrics),
            logger: Arc::clone(&logger),
        });
        self_observation = selfobs::start(
            config.boolean("metrics.selfObservation.enabled"),
            Duration::from_millis(config.duration_ms("metrics.selfObservation.period") as u64),
            SERVICE,
            Arc::clone(&metrics),
            Arc::clone(&edge),
        );
        let open_telemetry_listen = config.text("compatibility.openTelemetry.listen");
        let open_telemetry_tls = if config.boolean("compatibility.openTelemetry.enabled") {
            let (tls, exposure) = transport::receiver_tls(
                open_telemetry_listen,
                certificates.as_ref(),
                allow_plaintext,
            )?;
            logger.info(
                "The OpenTelemetry receiver is on.",
                &[
                    ("address", open_telemetry_listen),
                    ("transport", exposure.describe()),
                ],
            );
            if exposure == Exposure::Tls {
                certificate_listeners.push(transport::LISTENER_OTLP);
            }
            exposed.push((
                "compatibility.openTelemetry.listen",
                open_telemetry_listen.to_string(),
                exposure,
            ));
            tls
        } else {
            None
        };
        compat_running = Some(compat::start_with(
            compat::Settings {
                open_telemetry_enabled: config.boolean("compatibility.openTelemetry.enabled"),
                open_telemetry_listen: config
                    .text("compatibility.openTelemetry.listen")
                    .to_string(),
                scrape_targets: config
                    .list("compatibility.prometheus.targets")
                    .into_iter()
                    .map(|t| t.to_string())
                    .collect(),
                scrape_interval: Duration::from_millis(
                    config.duration_ms("compatibility.prometheus.interval") as u64,
                ),
                scrape_timeout: Duration::from_millis(
                    config.duration_ms("compatibility.prometheus.timeout") as u64,
                ),
                open_telemetry_tls,
            },
            compat::Limits {
                scrape_max_body_bytes: config.bytes("compatibility.prometheus.maxBodyBytes").max(0)
                    as usize,
                scrape_workers: config.integer("compatibility.prometheus.workers").max(1) as usize,
            },
            edge,
        ));
    }

    if !config.boolean("compatibility.openTelemetry.enabled") {
        // Say it out loud. An operator who expects a port and does not find one
        // should read the reason in the log rather than in a packet capture.
        logger.info(
            "The OpenTelemetry receiver is off and no port is open for it. Turn it on with `compatibility.openTelemetry.enabled`.",
            &[],
        );
    }
    if config.list("compatibility.prometheus.targets").is_empty() {
        logger.info(
            "No scrape target is configured, so the collector reaches nothing. Name each one in `compatibility.prometheus.targets`.",
            &[],
        );
    }

    let exposed_view: Vec<(&str, &str, Exposure)> = exposed
        .iter()
        .map(|(setting, address, how)| (*setting, address.as_str(), *how))
        .collect();
    if let Some(warning) = transport::plaintext_warning(&exposed_view) {
        logger.warning(&warning, &[]);
    }
    let watch_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let (Some(set), false) = (&certificates, certificate_listeners.is_empty()) {
        CertificateWatch::new(
            Arc::clone(set),
            certificate_listeners.clone(),
            Duration::from_millis(config.duration_ms("tls.reloadInterval").max(1) as u64),
            Arc::clone(&metrics),
            Arc::clone(&logger),
        )
        .spawn(Arc::clone(&watch_stop))?;
    }

    logger.info("Ready.", &[]);

    // Watch the role threads and the request to stop. A role thread that ends
    // on its own is a role that has stopped, and a process that only joined its
    // threads in order never found out about any thread but the first.
    health.declare(
        ROLE_THREADS_CHECK,
        "The role threads have not been checked yet.",
    );
    let mut listener_published = ListenerPublished::default();
    while !shutdown::requested() {
        let ended: Vec<String> = threads
            .iter()
            .filter(|thread| thread.is_finished())
            .map(|thread| thread.thread().name().unwrap_or("unnamed").to_string())
            .collect();
        if ended.is_empty() {
            health.pass(ROLE_THREADS_CHECK);
        } else {
            let names = ended.join(", ");
            health.fail(
                ROLE_THREADS_CHECK,
                format!("These collector threads have ended and their work has stopped: {names}. Restart this collector."),
            );
            // Liveness as well. Nothing inside this process starts the thread
            // again, so the honest request is a restart.
            health.stop_living();
            logger.error(
                "A collector thread ended on its own. Its work has stopped until this process restarts.",
                &[("threads", &names)],
            );
            threads.retain(|thread| !thread.is_finished());
        }
        if let Some(server) = &intake_server {
            listener_published.publish(&metrics, &server.stats());
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // Stop in the order DELIVERY.md section 10 gives: intake first, then what
    // is already in progress, then everything else.
    let grace = Duration::from_millis(config.duration_ms("collector.shutdownGrace").max(0) as u64);
    logger.info(
        "Stopping. Intake closes first, and work in progress gets time to finish.",
        &[("grace_ms", &grace.as_millis().to_string())],
    );
    if let Some(server) = &intake_server {
        health.fail(INTAKE_CHECK, "This collector is stopping.");
        server.stop();
        if !server.wait_until_quiet(grace) {
            logger.warning(
                "Some intake requests did not finish before the collector stopped. Each app driver sends its batch again, with the same batch ID.",
                &[("in_flight", &server.stats().in_flight().to_string())],
            );
        }
    }
    forwarder_state.stop();
    // The policy thread sleeps for its whole interval and holds no work, so
    // nothing waits for it.
    let holds_work = |thread: &std::thread::JoinHandle<()>| {
        !thread.is_finished() && thread.thread().name() != Some("tallyowl-policy")
    };
    let stopping_since = std::time::Instant::now();
    while threads.iter().any(holds_work) && stopping_since.elapsed() < grace {
        std::thread::sleep(Duration::from_millis(25));
    }
    let unfinished: Vec<&str> = threads
        .iter()
        .filter(|thread| holds_work(thread))
        .map(|thread| thread.thread().name().unwrap_or("unnamed"))
        .collect();
    if !unfinished.is_empty() {
        // Not a loss. A batch still claimed goes back to the queue when its
        // claim expires, and the head counts a repeated batch once.
        logger.warning(
            "Some collector threads did not finish before the collector stopped. A batch that was being delivered goes back to the queue when its claim expires.",
            &[("threads", &unfinished.join(", "))],
        );
    }
    if let Some(running) = &compat_running {
        running.stop();
    }
    if let Some(publisher) = &self_observation {
        publisher.stop();
    }
    watch_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    logger.info("Stopped.", &[]);
    Ok(())
}

const LISTENER_OPEN: &str = "tallyowl_intake_connections_open_count";
const LISTENER_IN_FLIGHT: &str = "tallyowl_intake_requests_in_flight_count";
const LISTENER_REFUSED: &str = "tallyowl_intake_connections_refused_total";
const LISTENER_IDLE_CLOSED: &str = "tallyowl_intake_connections_idle_closed_total";
const LISTENER_PANICS: &str = "tallyowl_intake_handler_panics_total";

fn declare_listener_metrics(metrics: &Registry) {
    use tallyowl_obs::MetricKind::{Counter, Gauge};
    for (name, kind, help) in [
        (LISTENER_OPEN, Gauge, "Intake connections open now."),
        (
            LISTENER_IN_FLIGHT,
            Gauge,
            "Intake requests a handler is working on now.",
        ),
        (
            LISTENER_REFUSED,
            Counter,
            "Intake connections closed at once because `collector.maxConnections` was reached.",
        ),
        (
            LISTENER_IDLE_CLOSED,
            Counter,
            "Intake connections closed because they sent nothing for `collector.idleTimeout`.",
        ),
        (
            LISTENER_PANICS,
            Counter,
            "Intake requests that failed inside the collector. Each one is a defect, and each caller was told.",
        ),
    ] {
        metrics.declare(name, kind, help, &[]).unwrap_or_else(|e| {
            panic!("the metric `{name}` is not a name the registry accepts: {}", e.0)
        });
    }
}

/// What the listener counters read at the last publish. The listener keeps
/// totals and the registry takes increments, so this holds the difference.
#[derive(Default)]
struct ListenerPublished {
    refused: u64,
    idle_closed: u64,
    panics: u64,
}

impl ListenerPublished {
    fn publish(&mut self, metrics: &Registry, stats: &tallyowl_rpc::ServerStats) {
        let none = tallyowl_obs::metrics::labels(&[]);
        metrics.set_gauge(LISTENER_OPEN, &none, stats.open_connections() as i64);
        metrics.set_gauge(LISTENER_IN_FLIGHT, &none, stats.in_flight() as i64);
        for (name, now, held) in [
            (
                LISTENER_REFUSED,
                stats.refused_connections(),
                &mut self.refused,
            ),
            (
                LISTENER_IDLE_CLOSED,
                stats.idle_closed(),
                &mut self.idle_closed,
            ),
            (LISTENER_PANICS, stats.handler_panics(), &mut self.panics),
        ] {
            metrics.add(name, &none, now.saturating_sub(*held));
            *held = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn the_compatibility_receiver_role_alone_builds_the_accept_path_and_opens_no_driver_port() {
        // It passed validation and then did nothing, because the edge started
        // only inside the `intake` branch.
        let plan = plan_roles(&roles(&["compatibility-receiver"]));
        assert!(plan.accept_path, "the edge offers into the accept path");
        assert!(!plan.intake_listener, "and no port opens for app drivers");
        assert!(!plan.forwarder);
    }

    #[test]
    fn the_home_profile_runs_intake_and_the_forwarder_together() {
        let plan = plan_roles(&roles(&["intake", "forwarder"]));
        assert_eq!(
            plan,
            RolePlan {
                accept_path: true,
                intake_listener: true,
                forwarder: true,
            }
        );
    }

    #[test]
    fn a_forwarder_alone_builds_no_accept_path() {
        let plan = plan_roles(&roles(&["forwarder"]));
        assert!(!plan.accept_path);
        assert!(!plan.intake_listener);
        assert!(plan.forwarder);
    }
}
