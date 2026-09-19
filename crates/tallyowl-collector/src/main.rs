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
use tallyowl_collector::durable::{CorndogsQueue, DurableQueue};
use tallyowl_collector::forwarder::{
    self, Forwarder, ForwarderState, DURABLE_STORE_CHECK, SWEEP_CHECK,
};
use tallyowl_collector::head_client::RemoteHead;
use tallyowl_collector::intake::{Intake, Limits};
use tallyowl_collector::selfobs;
use tallyowl_collector::series::{SeriesBudget, SeriesLedger};
use tallyowl_collector::service::CollectorService;
use tallyowl_collector::tenancy::{KeyDirectory, TenancyResolver};

const SERVICE: &str = "tallyowl-collector";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_CONFIG_FILE: &str = "tallyowl.local.yaml";
/// The check that says intake is listening.
const INTAKE_CHECK: &str = "intake-listener";

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
    // A counter that nothing declares is silently dropped when it is added to,
    // so a policy that blocked a thousand events would report nothing. The
    // running loop found this one: the drop counter was missing from the
    // exposition while the block itself worked.
    tallyowl_collector::policy::Refresher::declare_metrics(&metrics);

    let roles = config.list("collector.roles");
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
    if roles.iter().any(|r| r == "intake") {
        health.declare(INTAKE_CHECK, "Intake is not listening yet.");
    }
    if roles.iter().any(|r| r == "forwarder") {
        health.declare(
            SWEEP_CHECK,
            "The retry sweep has not run yet, so a failed delivery would not be tried again.",
        );
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
    let queue: Arc<dyn DurableQueue> = match CorndogsQueue::connect(endpoint) {
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
    logger.info("Reached the durable store.", &[("address", endpoint)]);

    let forwarder_state = ForwarderState::new();
    let max_frame = config.bytes("corndogs.maxPayloadBytes") as usize;

    // One client to the head, shared by both roles. The forwarder commits
    // batches through it and intake resolves credentials through it, because
    // the head owns the control catalog and a collector stores no key.
    let head = Arc::new(RemoteHead::new(config.text("head.endpoint"), max_frame));

    let mut threads = Vec::new();
    if roles.iter().any(|r| r == "forwarder") {
        let forwarder = Arc::new(Forwarder {
            queue: Arc::clone(&queue),
            queue_name: config.text("corndogs.deliveryQueue").to_string(),
            quarantine_queue: config.text("corndogs.quarantineQueue").to_string(),
            head: Arc::clone(&head) as Arc<dyn tallyowl_collector::head_client::HeadClient>,
            health: Arc::clone(&health),
            metrics: Arc::clone(&metrics),
            logger: Arc::clone(&logger),
            state: Arc::clone(&forwarder_state),
            sweep_staleness_ms: config.duration_ms("corndogs.sweepInterval") * 3,
            max_payload_bytes: config.bytes("corndogs.maxPayloadBytes").max(0) as u64,
            max_delivery_age_ms: config.duration_ms("corndogs.maxDeliveryAge"),
        });
        let interval = Duration::from_millis(config.duration_ms("corndogs.sweepInterval") as u64);
        threads.extend(forwarder::run(forwarder, interval));
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
    if roles.iter().any(|r| r == "intake") {
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
            series: Arc::new(SeriesLedger::new(SeriesBudget {
                max_series_for_each_metric: config.integer("metrics.maxSeriesForEachMetric") as u64,
                max_bytes_for_each_metric: config.bytes("metrics.maxBytesForEachMetric") as u64,
                max_labels: config.integer("metrics.maxLabels") as usize,
                max_label_value_bytes: config.integer("metrics.maxLabelValueBytes") as usize,
                max_label_bytes: config.integer("metrics.maxLabelBytes") as usize,
                max_merge_points: config.integer("metrics.maxMergePoints") as usize,
                idle_expiry_ms: config.duration_ms("metrics.idleSeriesExpiry"),
            })),
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

        let server = tallyowl_rpc::serve(
            config.text("collector.listen"),
            Arc::new(service),
            max_frame,
        )?;
        health.pass(INTAKE_CHECK);
        logger.info(
            "Accepting telemetry.",
            &[("address", &server.local_address().to_string())],
        );
        intake_server = Some(server);

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
        compat_running = Some(compat::start(
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

    logger.info("Ready.", &[]);

    for thread in threads {
        let _ = thread.join();
    }
    if let Some(running) = &compat_running {
        running.stop();
    }
    if let Some(publisher) = &self_observation {
        publisher.stop();
    }
    // Only reached when every role thread stopped. An intake-only process has
    // none, so it parks here rather than exiting.
    if intake_server.is_some() {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    Ok(())
}
