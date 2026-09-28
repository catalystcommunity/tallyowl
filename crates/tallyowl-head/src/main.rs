//! `tallyowl-head`.
//!
//! One binary runs the head, the query service, and the durable store from one
//! directory, and it requires no external database. That is a Phase 3 exit
//! criterion, and it is true from Phase 1 because the shape was built that way
//! rather than assembled later.

use std::sync::Arc;
use std::time::Duration;

use tallyowl_collector_api::types::ReceiptPolicy;
use tallyowl_config::{check, Config};
use tallyowl_obs::health::Health;
use tallyowl_obs::log::{Logger, Severity};
use tallyowl_obs::metrics::Registry;
use tallyowl_store::{SegmentedStore, Store};

use tallyowl_head::admin;
use tallyowl_head::control::ControlService;
use tallyowl_head::ingest::Ingest;
use tallyowl_head::lifecycle::{self, LoopWatch, Stop};
use tallyowl_head::query::QueryService;
use tallyowl_head::sampling::{self, OpenTraces, TailSampler, TailSettings};
use tallyowl_head::service::HeadService;

const SERVICE: &str = "tallyowl-head";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_CONFIG_FILE: &str = "tallyowl.local.yaml";
const STORE_CHECK: &str = "storage";
const INGEST_CHECK: &str = "ingest-listener";
const SPACE_CHECK: &str = "disk-space";
const APPEND_LOG_CHECK: &str = "append-log";
const WORKFLOW_QUEUE_CHECK: &str = "workflow-queue";

const USAGE: &str = "\
tallyowl-head [--config <file>]

  tallyowl-head                          Run the head
  tallyowl-head config check             Say where every setting came from

The recovery verbs. Each needs the data directory to itself, so stop the head
before running one:

  tallyowl-head snapshot <directory>     Copy this installation into <directory>
  tallyowl-head restore <directory>      Restore a snapshot into the data directory
  tallyowl-head rebuild                  Rebuild the list of stored files by reading them
";

/// The request to stop, from the host.
///
/// Nothing listened for it before, so a rolling restart ended every commit and
/// query in progress, and the head never said it was leaving. This is the same
/// handler the collector has: the signal stores to an atomic and the main
/// thread does the work. See `lifecycle::shut_down` for the order.
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
    // A verb can come before or after `--config <file>`, so the options are
    // taken out first and what is left is the verb and its arguments.
    let words = without_options(&arguments);

    if words.first().map(String::as_str) == Some("config")
        && words.get(1).map(String::as_str) == Some("check")
    {
        let report = check::run_on_host(&config_file);
        print!("{}", report.text);
        std::process::exit(report.exit_code);
    }

    if let Some(verb) = words.first() {
        // The recovery verbs. They need the data directory to themselves, so
        // they run instead of the service rather than beside it.
        if admin::VERBS.contains(&verb.as_str()) {
            std::process::exit(admin::run(verb, &words[1..], &config_file));
        }
        if matches!(verb.as_str(), "-h" | "--help" | "help") {
            println!("{USAGE}");
            std::process::exit(0);
        }
        eprintln!("`{verb}` is not a verb this binary has.\n\n{USAGE}");
        std::process::exit(1);
    }

    let config = match Config::load_from_host(&config_file) {
        Ok(config) => config,
        Err(errors) => {
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

/// The arguments with every `--flag` and its value removed.
///
/// The loader owns the rule, so the verb parser and the loader cannot disagree
/// about which word is a value. See `loader::words_without_options`.
fn without_options(arguments: &[String]) -> Vec<String> {
    tallyowl_config::loader::words_without_options(arguments)
}

/// Run the maintenance pass, once each interval.
///
/// Compaction, retired-file reclamation, and receipt expiry all existed and
/// nothing ran them, so a home installation grew for ever. See L052 and L056.
/// The pass is never required for correctness: a tombstone already hides erased
/// rows, and an unexpired receipt only costs space. A failure is therefore
/// logged and retried rather than made a readiness failure.
/// Seal the open buffer when it is due, on a period.
///
/// **Sealing used to happen only inside a commit**, so the seal condition was
/// only ever evaluated when a batch arrived: traffic stopping was exactly when
/// sealing stopped. A drained load run left 241,920 of 438,866 rows in the
/// append log, 55 percent of them, and waiting did not help. An installation
/// that goes quiet kept its newest data in the least compact form it has,
/// indefinitely, and paid for it in disk and in recovery time. See L078.
///
/// The interval is a fraction of `max_open_ms`, so a buffer seals within a
/// small margin of its deadline rather than up to a whole period late.
fn run_segmenter(
    store: Arc<SegmentedStore>,
    logger: Arc<Logger>,
    max_open_ms: i64,
    background: &Background,
) {
    const LOOP: &str = "segmenter";
    let period = Duration::from_millis((max_open_ms.max(1_000) / 4) as u64);
    let (watch, stop) = background.for_loop(LOOP, period);
    std::thread::Builder::new()
        .name("tallyowl-segmenter".into())
        .spawn(move || {
            while !stop.wait(period) {
                watch.pass(LOOP, tallyowl_obs::time::now_ms, || match store.seal_if_due() {
                    Ok(Some(_)) => {}
                    Ok(None) => {}
                    Err(e) => logger.warning(
                        "A background seal did not finish. It will be tried again, and the rows stay durable in the append log meanwhile.",
                        &[("reason", &e.to_string())],
                    ),
                });
            }
        })
        .ok();
}

/// What every background loop is started with: the watch that runs its passes
/// and reports a loop that stopped, and the stop that ends it. See
/// `tallyowl_head::lifecycle`.
#[derive(Clone)]
struct Background {
    watch: Arc<LoopWatch>,
    stop: Arc<Stop>,
}

impl Background {
    /// Register one loop, and hand it the two things it runs with.
    fn for_loop(&self, name: &'static str, period: Duration) -> (Arc<LoopWatch>, Arc<Stop>) {
        self.watch
            .register(name, period, tallyowl_obs::time::now_ms());
        (Arc::clone(&self.watch), Arc::clone(&self.stop))
    }
}

/// Roll metric points up to a coarser resolution, on a period.
///
/// It reads the window that closed one resolution ago, so a pass never rolls up
/// a window that is still filling. And it does **not** remove the finer points:
/// `AGENTS.md` requires every derived projection to be rebuildable from
/// retained raw data, and a rollup that deleted its inputs could not be.
fn run_downsample(
    store: Arc<SegmentedStore>,
    logger: Arc<Logger>,
    metrics: Arc<Registry>,
    settings: tallyowl_head::rollup::DownsampleSettings,
    background: &Background,
) {
    const LOOP: &str = "downsample";
    if settings.resolution_ms <= 0 {
        logger.info(
            "No metric downsample pass runs, because `metrics.downsampleResolution` is zero.",
            &[],
        );
        return;
    }
    metrics.declare(
        "tallyowl_downsampled_points_total",
        tallyowl_obs::MetricKind::Counter,
        "Metric points a downsample pass produced at a coarser resolution.",
        &[],
    )
    .unwrap_or_else(|e| panic!("the metric `tallyowl_downsampled_points_total` is not a name the registry accepts: {}", e.0));
    // **The pass follows a durable watermark, so its period is only how soon a
    // closed window is rolled up.** It used to be what decided which window was
    // rolled up at all, and a pass that ran late or a head that restarted
    // across a boundary skipped one for good. See `rollup::downsample_due`.
    let period = Duration::from_millis((settings.resolution_ms.max(4_000) / 4).min(60_000) as u64);
    let (watch, stop) = background.for_loop(LOOP, period);
    std::thread::Builder::new()
        .name("tallyowl-downsample".into())
        .spawn(move || {
            while !stop.wait(period) {
                watch.pass(LOOP, tallyowl_obs::time::now_ms, || {
                    let report = tallyowl_head::rollup::downsample_due(
                        &store,
                        settings,
                        tallyowl_obs::time::now_ms(),
                    );
                    if report.points > 0 {
                        metrics.add(
                            "tallyowl_downsampled_points_total",
                            &tallyowl_obs::metrics::labels(&[]),
                            report.points,
                        );
                    }
                    if report.skipped_layouts > 0 {
                        logger.info(
                            "A downsample pass left some histograms at the finer resolution, because their bucket layout changed inside the window. TallyOwl does not rebucket a histogram on its own.",
                            &[("points", &report.skipped_layouts.to_string())],
                        );
                    }
                    for reason in &report.failures {
                        logger.warning(
                            "A downsample pass did not finish a window. It keeps its place and runs again.",
                            &[("reason", reason)],
                        );
                    }
                });
            }
        })
        .ok();
}

fn run_maintenance(
    store: Arc<SegmentedStore>,
    logger: Arc<Logger>,
    settings: tallyowl_store::compact::CompactionSettings,
    interval: Duration,
    background: &Background,
) {
    const LOOP: &str = "maintenance";
    let (watch, stop) = background.for_loop(LOOP, interval);
    std::thread::Builder::new()
        .name("tallyowl-maintenance".into())
        .spawn(move || loop {
            watch.pass(LOOP, tallyowl_obs::time::now_ms, || {
                // First, so a long-lived installation does not wait an interval for
                // its first reclamation after a restart.
                match tallyowl_store::compact::compact(&store, settings) {
                    Ok(outcome) => {
                        // Cold consolidation is in the condition and the fields
                        // because a maintenance pass nothing reports is invisible
                        // exactly when someone waits for its first run (L164).
                        if outcome.files_reclaimed > 0
                            || outcome.receipts_expired > 0
                            || outcome.rewritten > 0
                            || outcome.segments_expired > 0
                            || outcome.consolidated_sources > 0
                        {
                            logger.info(
                                "Maintenance reclaimed space.",
                                &[
                                    ("segments_rewritten", &outcome.rewritten.to_string()),
                                    // Not `files_reclaimed`: the logger's privacy
                                    // filter matches substrings, and "reclaimed"
                                    // contains "claim", so that name never reached
                                    // a line — only `refused_fields: 1` did.
                                    ("files_freed", &outcome.files_reclaimed.to_string()),
                                    ("receipts_expired", &outcome.receipts_expired.to_string()),
                                    ("rows_erased", &outcome.rows_erased.to_string()),
                                    ("segments_expired", &outcome.segments_expired.to_string()),
                                    ("rows_expired", &outcome.rows_expired.to_string()),
                                    (
                                        "consolidated_sources",
                                        &outcome.consolidated_sources.to_string(),
                                    ),
                                    (
                                        "consolidated_outputs",
                                        &outcome.consolidated_outputs.to_string(),
                                    ),
                                ],
                            );
                        }
                    }
                    Err(e) => logger.warning(
                        "Maintenance could not finish. It will run again.",
                        &[("reason", &e.to_string())],
                    ),
                }
                // The control records that age out: sessions, pending sign-ins, and
                // node records. See `control::reap_expired`.
                match tallyowl_head::control::reap_expired(&store, tallyowl_obs::time::now_ms()) {
                    Ok(reaped) if reaped.anything() => logger.info(
                        "Maintenance removed expired control records.",
                        &[
                            ("sessions", &reaped.sessions.to_string()),
                            ("pending_logins", &reaped.pending_logins.to_string()),
                            ("nodes", &reaped.nodes.to_string()),
                        ],
                    ),
                    Ok(_) => {}
                    Err(e) => logger.warning(
                        "Expired control records were not removed. It will run again.",
                        &[("reason", &e.message)],
                    ),
                }
            });
            if stop.wait(interval) {
                break;
            }
        })
        .expect("the maintenance thread starts");
}

/// Watch the device and report readiness, once each interval.
///
/// Sampling rather than checking on each write. `statvfs` is cheap and a write
/// path that called it for every batch would still be sampling, just less
/// predictably. The write path has its own check that refuses; this one is what
/// tells the world before that happens.
fn watch_disk_space(
    store: Arc<SegmentedStore>,
    health: Arc<Health>,
    logger: Arc<Logger>,
    metrics: Arc<Registry>,
    identity: Arc<tallyowl_head::identity::IdentityCache>,
    background: &Background,
) {
    const INTERVAL: Duration = Duration::from_secs(10);
    const LOOP: &str = "storage-watch";
    let (watch, stop) = background.for_loop(LOOP, INTERVAL);
    health.declare(SPACE_CHECK, "The device has not been read yet.");
    health.declare(APPEND_LOG_CHECK, "The append log has not been read yet.");
    for (name, help) in [
        (
            "tallyowl_identity_graph_reuses_count",
            "Questions answered from a materialised identity graph.",
        ),
        (
            "tallyowl_identity_graph_builds_count",
            "Times an identity graph was built from the whole history.",
        ),
    ] {
        metrics
            .declare(name, tallyowl_obs::MetricKind::Gauge, help, &[])
            .unwrap_or_else(|e| {
                panic!(
                    "the metric `{name}` is not a name the registry accepts: {}",
                    e.0
                )
            });
    }

    std::thread::spawn(move || {
        let mut said_it = false;
        let mut said_log = false;
        // What the append log looked like last time. A committer that is in
        // flight across two rounds without moving `durable_before` is what a
        // stall looks like from outside.
        let mut last_durable_before = 0u64;
        let mut stalled_rounds = 0u32;
        loop {
            watch.pass(LOOP, tallyowl_obs::time::now_ms, || {
            tallyowl_store::metrics::sample(&store, &metrics);

            // FAILURE_MODES.md section 10, append log: a log that could not make
            // a write durable stops accepting, and readiness has to say so. A
            // node that refuses every write and still reports ready keeps its
            // place in the rotation and refuses on behalf of the whole
            // installation.
            // **The stall detector. L131 and L132.** An intermittent hang in
            // the append log was found because somebody noticed four shells
            // were still running, two days later. The log's own state is the
            // first question anybody asks, and this puts it in the log rather
            // than behind a debugger.
            //
            // Reading it takes the log's lock, so a watcher that stops
            // reporting has said the most useful thing it can say: the log is
            // held and the next place to look is whoever holds it.
            match store.append_log_state() {
                state if state.committing && state.durable_before == last_durable_before => {
                    stalled_rounds += 1;
                    if stalled_rounds >= 2 {
                        logger.warning(
                            "A group commit has been in flight without making the append log durable for longer than expected. This is the state L131 records, and the reason it is written here is that the state was inside a lock nothing could read.",
                            &[
                                ("append_log", &state.to_line()),
                                ("rounds", &stalled_rounds.to_string()),
                            ],
                        );
                    }
                }
                state => {
                    stalled_rounds = 0;
                    last_durable_before = state.durable_before;
                }
            }

            // What the materialised identity graph saved. A rising miss count
            // with a flat hit count means the refresh period is shorter than
            // the gap between questions and every one is rebuilding. L135.
            let (hits, misses) = identity.counts();
            metrics.set_gauge(
                "tallyowl_identity_graph_reuses_count",
                &tallyowl_obs::metrics::labels(&[]),
                hits as i64,
            );
            metrics.set_gauge(
                "tallyowl_identity_graph_builds_count",
                &tallyowl_obs::metrics::labels(&[]),
                misses as i64,
            );

            match store.append_log_failure() {
                Some(reason) => {
                    health.fail(
                        APPEND_LOG_CHECK,
                        format!(
                            "The append log stopped accepting writes and this node cannot take                              data until it is restarted. {reason}"
                        ),
                    );
                    if !said_log {
                        logger.error(
                            "The append log stopped accepting writes.",
                            &[("reason", &reason)],
                        );
                        said_log = true;
                    }
                }
                None => health.pass(APPEND_LOG_CHECK),
            }

            let space = store.space();
            match space.device() {
                Ok(device) if space.is_low() => {
                    health.fail(
                        SPACE_CHECK,
                        format!(
                            "The device is nearly full: {} free, and {} is held back for \
                             recovery. Writes stop when the space above the reserve is gone.",
                            tallyowl_store::space::bytes(device.free_bytes),
                            tallyowl_store::space::bytes(space.reserve_bytes())
                        ),
                    );
                    if !said_it {
                        logger.warning(
                            "The device is nearly full. New data will be refused soon.",
                            &[
                                ("free_bytes", &device.free_bytes.to_string()),
                                ("reserve_bytes", &space.reserve_bytes().to_string()),
                            ],
                        );
                        said_it = true;
                    }
                }
                Ok(_) => {
                    if said_it {
                        logger.info("The device has room again.", &[]);
                        said_it = false;
                    }
                    health.pass(SPACE_CHECK);
                }
                Err(e) => {
                    // Not knowing is not the same as being full. Say so, and do
                    // not refuse writes over it.
                    health.fail(
                        SPACE_CHECK,
                        format!("The free space on the device could not be read. {e}"),
                    );
                }
            }
            });
            if stop.wait(INTERVAL) {
                break;
            }
        }
    });
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
    tallyowl_head::declare_metrics(&metrics);

    logger.info(
        "Starting.",
        &[
            ("profile", config.text("installation.profile")),
            ("receipt_policy", config.text("storage.receiptPolicy")),
            ("integrity_mode", config.text("integrity.mode")),
            (
                "config_file",
                config.file_path().unwrap_or("none; using defaults"),
            ),
        ],
    );

    if config.text("integrity.mode") == "none" {
        // D57: an operator who inherits an installation must not have to
        // discover that integrity checking is off.
        logger.warning(
            "Integrity checking is off. A damaged page will produce a wrong answer rather than an error.",
            &[],
        );
    }

    // L009 lets a service start with a key it does not know. It still says so,
    // because a misspelled key is otherwise a setting somebody believes is set.
    for warning in config.unknown_key_warnings() {
        logger.warning(&warning, &[]);
    }

    health.declare(STORE_CHECK, "The storage directory is not open yet.");
    health.declare(INGEST_CHECK, "Ingest is not listening yet.");

    let operational = tallyowl_obs::http::start(
        config.text("head.operationalListen"),
        Arc::clone(&health),
        Arc::clone(&metrics),
        Arc::clone(&logger),
    )?;
    logger.info(
        "Serving health and metrics.",
        &[("address", &operational.local_address().to_string())],
    );

    let data_dir = config.text("head.dataDir");
    // A rolling restart can start this process before the previous one has
    // finished exiting, and one process owns one data directory. Waiting a
    // bounded time turns a routine restart into a pause rather than a crash
    // loop, and a real conflict still fails with a message that names it.
    let sealing = tallyowl_store::Sealing {
        reserve_bytes: config.bytes("storage.reserveBytes").max(0) as u64,
        // `integrity.mode: none` used to log "Integrity checking is off" and
        // leave it on. The line above is now true.
        verify_on_read: config.text("integrity.mode") != "none",
        ..tallyowl_store::Sealing::default()
    };
    // Held as the concrete type as well as behind the trait: the space watcher
    // and the storage metrics are the store's own, not part of the `Store`
    // contract that D25 keeps small.
    let segmented: Arc<SegmentedStore> = match SegmentedStore::open_waiting_with(
        data_dir,
        std::time::Duration::from_secs(30),
        sealing,
    ) {
        Ok(store) => {
            logger.info(
                "Opened storage.",
                &[
                    ("directory", data_dir),
                    ("watermark", &store.commit_watermark().to_string()),
                    ("rows", &store.row_count().to_string()),
                ],
            );
            // FAILURE_MODES.md procedure 6: a damaged segment stays damaged
            // and a query over its range answers `incomplete-result`. That
            // refusal is true and unactionable unless somebody says which part
            // could not be read, so it is named here, once, at start-up.
            for reason in store.unreadable() {
                logger.error(
                    "Part of the stored data cannot be read. Every query over its range answers `incomplete-result` rather than a smaller number.",
                    &[("reason", &reason)],
                );
            }
            // The reserve is a property of the device and this process enforces
            // it alone. Two installations sharing a device each hold back the
            // same bytes and each believes those bytes are its own, so the
            // device is named here and in `tallyowl_storage_device_info`. See
            // L037 and L081.
            if let Ok(device) = store.space().device() {
                logger.info(
                    "Holding back a reserve so recovery can still write. The reserve belongs to the device, and this process is the only one enforcing it: an installation sharing this device holds back the same bytes and neither gets them in full.",
                    &[
                        ("device", &device.device_id.to_string()),
                        ("reserve_bytes", &store.space().reserve_bytes().to_string()),
                    ],
                );
            }
            health.pass(STORE_CHECK);
            Arc::new(store)
        }
        Err(e) => {
            // A head that cannot write must not accept data it would discard.
            logger.error(
                "Cannot open storage.",
                &[("directory", data_dir), ("reason", &e.to_string())],
            );
            return Err(Box::new(e));
        }
    };
    // Phase 7. A home installation has no replication address, so this returns
    // the local store unchanged and nothing below here can tell the difference.
    // A cluster node gets a store whose commit goes through its tablet group.
    // See `tallyowl_head::cluster`.
    //
    // D62. A head that signs issues its own certificate with the operator's
    // intermediate authority, and signs the requests of the nodes that enroll.
    // A head whose hops are all loopback or unix sockets needs no identity, and
    // the home profile is one of those.
    let allow_plaintext = config.boolean("transport.allowPlaintext");
    let signer = tallyowl_identity::install_signer(
        &config,
        segmented.catalog(),
        tallyowl_obs::time::now_ms(),
    )?;
    if signer {
        logger.info(
            "This head signs node certificates with `installation.signingCertificate`.",
            &[],
        );
    }
    let node_security = tallyowl_identity::for_head(
        &config,
        Arc::new(tallyowl_identity::SystemClock) as Arc<dyn tallyowl_identity::Clock>,
    )?;
    if let Some((_, _, handle)) = &node_security {
        // A new authority in `installation.authorities` takes effect with no
        // restart, which is how the authority rotates (D62).
        let reporting = Arc::clone(&logger);
        handle.watch_trust(
            std::time::Duration::from_millis(config.duration_ms("tls.reloadInterval") as u64),
            Arc::new(move |_, result| {
                reporting.info(
                    "Read the trusted authorities again.",
                    &[("result", &format!("{result:?}"))],
                )
            }),
        )?;
    }
    let renewal = node_security.as_ref().map(|(identity, trust, handle)| {
        handle.enrolled().publish_to(Arc::clone(&metrics));
        handle.enrolled().log_to(Arc::clone(&logger));
        (
            tallyowl_head::listeners::NodeSecurity {
                identity: Arc::clone(identity),
                trust: Arc::clone(trust),
                on_handshake_refused: Some(tallyowl_head::listeners::handshake_counter(&metrics)),
            },
            handle,
        )
    });
    let cluster = tallyowl_head::cluster::start(
        &config,
        Arc::clone(&segmented),
        &logger,
        tallyowl_head::cluster::Security {
            identity: renewal.as_ref().map(|(node, _)| Arc::clone(&node.identity)),
            trust: renewal.as_ref().map(|(node, _)| Arc::clone(&node.trust)),
            allow_plaintext,
        },
    )?;
    // A home installation has no group, and this does nothing there.
    cluster.export_metrics(&metrics, &logger);
    let store: Arc<dyn Store> = Arc::clone(&cluster.store);

    let receipt_policy = match config.text("storage.receiptPolicy") {
        "local-one" => ReceiptPolicy::LocalOne,
        "local-quorum" => ReceiptPolicy::LocalQuorum,
        "remote-one" => ReceiptPolicy::RemoteOne,
        _ => ReceiptPolicy::Custom,
    };

    // D35: tail sampling decides at the head, after the trace is complete. The
    // registry exists only when sampling does; a head with no sampling keeps
    // every trace, which is what no sampling means.
    // What runs, watches, and stops every background loop. See `lifecycle`.
    let background = Background {
        watch: LoopWatch::new(Arc::clone(&metrics), Arc::clone(&logger)),
        stop: Stop::new(),
    };

    let tail_enabled = config.boolean("sampling.tail.enabled");
    let open_traces = tail_enabled.then(OpenTraces::new);
    if tail_enabled {
        let sampler = Arc::new(TailSampler {
            store: Arc::clone(&segmented),
            // A dropped trace's tombstone goes through the tablet, so every
            // replica hides the spans rather than only this one.
            erasing: Arc::clone(&store),
            open: Arc::clone(open_traces.as_ref().expect("the registry exists")),
            settings: TailSettings {
                decision_window_ms: config.duration_ms("sampling.tail.decisionWindow"),
                late_span_grace_ms: config.duration_ms("sampling.tail.lateSpanGrace"),
                keep_percent: config.integer("sampling.tail.keepPercent") as f64,
                keep_slower_than_ms: config.duration_ms("sampling.tail.keepSlowerThan"),
            },
            metrics: Arc::clone(&metrics),
            logger: Arc::clone(&logger),
            stopping: std::sync::atomic::AtomicBool::new(false),
            late_after_grace: std::sync::atomic::AtomicU64::new(0),
        });
        // A sweep every second is far more often than a decision window of
        // a minute needs, and it keeps the work small each time.
        sampling::run(
            sampler,
            Duration::from_secs(1),
            Arc::clone(&background.watch),
            Arc::clone(&background.stop),
        );
        logger.info(
            "Applying tail-sampling rules to committed traces.",
            &[
                (
                    "decision_window_ms",
                    &config
                        .duration_ms("sampling.tail.decisionWindow")
                        .to_string(),
                ),
                (
                    "keep_percent",
                    &config.integer("sampling.tail.keepPercent").to_string(),
                ),
            ],
        );
    }

    // Collection policy, and the saved analyses a dashboard is made of. Both
    // live in the control catalog beside the workspaces, the projects, and the
    // keys, and both are read back here. A record that will not load is named
    // rather than made a start-up failure: the rest is still valid, and a head
    // that would not start because one stored record was unreadable is a head
    // an upgrade could brick. See L112.
    let (policy, policy_refused) =
        tallyowl_head::policy::PolicyService::open(Arc::clone(&segmented));
    let policy = Arc::new(policy);
    let (saved, saved_refused) = tallyowl_head::saved::SavedService::open(
        Arc::clone(&segmented),
        tallyowl_head::query::ALGEBRA_VERSION,
    );
    let saved = Arc::new(saved);
    let (attribution, attribution_refused) =
        tallyowl_head::attribution::AttributionService::open(Arc::clone(&segmented));
    let attribution = Arc::new(attribution);

    // The starter campaign dashboard, offered once for each project. An
    // operator who deletes it does not get it back: the catalog marks that it
    // was offered rather than checking whether it is there. See `starter`.
    let (seeded, seed_refused) = if config.boolean("dashboard.starterDashboard") {
        tallyowl_head::starter::seed(
            &segmented,
            &saved,
            config.text("dashboard.starterConversionGoal"),
            tallyowl_obs::time::now_ms(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    if !seeded.is_empty() {
        logger.info(
            "Wrote a starter campaign dashboard. Change it or remove it; it is offered once.",
            &[("projects", &seeded.join(","))],
        );
    }
    for reason in policy_refused
        .into_iter()
        .chain(saved_refused)
        .chain(attribution_refused)
        .chain(seed_refused)
    {
        logger.error(
            "Part of the stored control state did not load. Everything else did, and this head is running without it.",
            &[("reason", &reason)],
        );
    }
    logger.info(
        "Loaded the control state this installation keeps.",
        &[("policy_version", &policy.version().to_string())],
    );

    // One query service, shared by the dashboard, the operator tools, and every
    // alert evaluation. **An alert value always matches what a person sees in
    // the dashboard**, and one executor is how that is true by construction
    // rather than by discipline. `docs/ALERTS.md` section 2.
    let query_service = Arc::new(QueryService {
        store: Arc::clone(&store),
        max_runtime_ms: config.duration_ms("query.maxRuntime"),
        max_expression_depth: config.integer("query.maxExpressionDepth").max(1) as u32,
        guards: tallyowl_head::analysis::Guards::default(),
        attribution: Arc::clone(&attribution),
        policy: Arc::clone(&policy),
        identity: Arc::new(tallyowl_head::identity::IdentityCache::new(
            std::time::Duration::from_millis(
                config.duration_ms("query.identityRefresh").max(0) as u64
            ),
        )),
    });

    // ---- Alerting and workflows. Phase 10. --------------------------------
    //
    // Everything scheduled is a Corndogs task, because `AGENTS.md` says
    // Corndogs owns durable queue and workflow state and because a schedule
    // held in a process is a schedule a restart loses. See `workflows.rs`.
    tallyowl_head::alerts::declare(&metrics);
    tallyowl_head::query::declare_metrics(&metrics);
    tallyowl_head::query::configure_project_permits(
        config.integer("query.maxConcurrentForEachProject").max(1) as usize,
    );
    //
    // **The durable queue is tried until it answers.** It used to be tried once.
    // On a first Kubernetes install the head starts before Corndogs listens, so
    // the one attempt failed, the head logged a warning, and alert evaluation,
    // notifications, and the projector passes never ran until somebody
    // restarted it, while `/readyz` answered ready. The services are built
    // now over a queue that connects later, and `workflow-queue` fails
    // readiness until it has. See `lifecycle::LateQueue`.
    let alerting = Some(start_alerting(
        &config,
        Arc::clone(&segmented),
        Arc::clone(&query_service),
        Arc::clone(&metrics),
        Arc::clone(&logger),
        Arc::clone(&health),
        &background,
    ));

    let service = HeadService {
        ingest: Arc::new(Ingest {
            golden_signal_bucket_ms: config.duration_ms("metrics.goldenSignalResolution"),
            store: Arc::clone(&store),
            metrics: Arc::clone(&metrics),
            receipt_policy,
            open_traces,
            policy: Some(Arc::clone(&policy)),
            // D17: the head verifies each stamped destination. See
            // `ingest::SourceCheck`.
            sources: Some(tallyowl_head::ingest::SourceCheck {
                store: Arc::clone(&segmented),
                require_known: config.boolean("ingest.requireKnownSource"),
            }),
        }),
        enrollment: Arc::new(tallyowl_head::enrollment::EnrollmentService {
            store: Arc::clone(&segmented),
            metrics: Arc::clone(&metrics),
        }),
        query: Arc::clone(&query_service),
        control: Arc::new(ControlService {
            store: Arc::clone(&segmented),
            metrics: Arc::clone(&metrics),
            key_cache_ttl_ms: config.duration_ms("collector.keyCacheTtl"),
        }),
        sign_in: Arc::new(tallyowl_head::linkkeys::SignIn {
            store: Arc::clone(&segmented),
            metrics: Arc::clone(&metrics),
            settings: tallyowl_head::linkkeys::LinkKeysSettings {
                enabled: config.boolean("linkkeys.enabled"),
                trusted_domains: config
                    .list("linkkeys.trustedDomains")
                    .into_iter()
                    .filter(|domain| !domain.is_empty())
                    .collect(),
                callback_url: config.text("linkkeys.callbackUrl").to_string(),
                app_name: config.text("linkkeys.appName").to_string(),
                session_lifetime_ms: config.duration_ms("linkkeys.sessionLifetime"),
            },
        }),
        logger: Arc::clone(&logger),
        policy: Arc::clone(&policy),
        saved: Arc::clone(&saved),
        attribution: Arc::clone(&attribution),
        alerts: alerting.as_ref().map(|a| Arc::clone(&a.alerts)),
        workflows: alerting.as_ref().map(|a| Arc::clone(&a.workflows)),
    };

    let max_frame = config.bytes("corndogs.maxPayloadBytes") as usize;
    // The dashboard shares the head's dispatcher, so a query from a browser and
    // a query from an operator tool run the same code with the same
    // authorization. The carrier refuses every service but the control one, so
    // this is not an ingest surface and cannot become one.
    let service = Arc::new(service);
    // Kept before the service moves into the server, so the readiness watcher
    // can report what the materialised identity graph saved.
    let identity_cache = Arc::clone(&service.query.identity);
    let dashboard = if config.boolean("dashboard.enabled") {
        match tallyowl_head::dashboard::start(
            config.text("dashboard.listen"),
            Arc::clone(&service) as Arc<dyn tallyowl_rpc::Dispatcher>,
            tallyowl_head::dashboard::Settings {
                assets: std::path::PathBuf::from(config.text("dashboard.assets")),
                health: Some(Arc::clone(&health)),
                callback_path: config.text("dashboard.callbackPath").to_string(),
                allow_plaintext: config.boolean("dashboard.allowPlaintext"),
            },
            Arc::clone(&logger),
        ) {
            Ok(server) => {
                logger.info(
                    "Serving the dashboard.",
                    &[("address", &server.local_address().to_string())],
                );
                Some(server)
            }
            Err(e) => {
                // The dashboard is not the ingest path. A head that refused to
                // start because a browser surface could not bind would stop
                // taking telemetry over a page nobody was looking at.
                logger.warning(
                    "The dashboard could not start. Telemetry is unaffected.",
                    &[("reason", &e.to_string())],
                );
                None
            }
        }
    } else {
        None
    };

    let (server, ingest_exposure) = tallyowl_head::listeners::serve_ingest(
        config.text("head.listen"),
        service,
        max_frame,
        renewal.as_ref().map(|(node, _)| node),
        allow_plaintext,
    )?;
    health.pass(INGEST_CHECK);
    logger.info(
        "Accepting batches and queries.",
        &[
            ("address", &server.bound().to_string()),
            ("transport", ingest_exposure.describe()),
        ],
    );
    if ingest_exposure == tallyowl_head::listeners::IngestExposure::Plaintext {
        logger.warning(
            "`transport.allowPlaintext` is true, so `head.listen` serves plaintext on a network address, and collectors cross that network in the clear. Protect the network, or give this head an identity.",
            &[("address", config.text("head.listen"))],
        );
    }
    logger.info("Ready.", &[]);

    // FAILURE_MODES.md section 10: fail readiness before the device is full. A
    // node that waits until it is full has already refused writes another node
    // could have taken, and a load balancer needs the warning while there is
    // still somewhere to send the work.
    watch_disk_space(
        Arc::clone(&segmented),
        Arc::clone(&health),
        Arc::clone(&logger),
        Arc::clone(&metrics),
        identity_cache,
        &background,
    );

    // D12: the head can push the same instruments it exposes into the internal
    // project. It goes through a collector with the app driver, so its own
    // metrics meet the same acknowledgement rule as everything else. See
    // `selfobs`.
    let self_observation = tallyowl_head::selfobs::start(
        config.boolean("metrics.selfObservation.enabled"),
        Duration::from_millis(config.duration_ms("metrics.selfObservation.period") as u64),
        // The address the head dials. `collector.listen` is an address a
        // collector binds, which is the same thing only on one host.
        match config.text("metrics.selfObservation.endpoint") {
            "" => config.text("collector.listen"),
            endpoint => endpoint,
        },
        &config
            .secret("collector.apiKey")
            .map(|secret| secret.expose().to_string())
            .unwrap_or_default(),
        SERVICE,
        Arc::clone(&metrics),
        Arc::clone(&logger),
    );

    // Nothing sealed an idle store before this. See `run_segmenter` and L078.
    run_segmenter(
        Arc::clone(&segmented),
        Arc::clone(&logger),
        sealing.max_open_ms,
        &background,
    );

    // The metric downsample pass. It rolls delta points up to a coarser
    // resolution, and it does not remove the finer ones: a rollup that deleted
    // its own inputs could not be rebuilt from retained raw data. See `rollup`
    // and the implementation log.
    run_downsample(
        Arc::clone(&segmented),
        Arc::clone(&logger),
        Arc::clone(&metrics),
        tallyowl_head::rollup::DownsampleSettings {
            resolution_ms: config.duration_ms("metrics.downsampleResolution"),
            detailed_ms: config.integer("retention.detailed"),
            lateness_grace_ms: config.duration_ms("metrics.downsampleLatenessGrace"),
        },
        &background,
    );

    // L052: the append log, the retired segment files, and the receipts all had
    // a way to be reclaimed and nothing called it.
    run_maintenance(
        Arc::clone(&segmented),
        Arc::clone(&logger),
        tallyowl_store::compact::CompactionSettings {
            grace_ms: config.integer("compaction.gcGrace"),
            deduplication_window_ms: config.integer("storage.deduplicationWindow"),
            // POLICY.md section 4. Nothing expired a row before this, so a home
            // installation grew without bound. See L071 and L080.
            detailed_retention_ms: config.integer("retention.detailed"),
            rollup_retention_ms: config.integer("retention.rollup"),
            // HIGH_CARDINALITY.md: one person's cold rows sit in few segments.
            cold_group_after_ms: config.integer("compaction.coldGroupAfter"),
            cold_group_target_bytes: config.bytes("compaction.coldGroupTarget") as u64,
            cold_group_batch_bytes: config.bytes("compaction.coldGroupBatch") as u64,
            ..tallyowl_store::compact::CompactionSettings::default()
        },
        Duration::from_secs(300),
        &background,
    );

    // Report a loop that stopped making passes. It has a thread of its own and
    // does nothing else, so a loop that is stuck cannot also stop the report.
    {
        let watch = Arc::clone(&background.watch);
        let stop = Arc::clone(&background.stop);
        let health = Arc::clone(&health);
        std::thread::Builder::new()
            .name("tallyowl-loop-watch".into())
            .spawn(move || {
                while !stop.wait(Duration::from_secs(10)) {
                    watch.evaluate(&health, tallyowl_obs::time::now_ms());
                }
            })
            .expect("the loop watch starts");
    }

    // The dashboard and the self-observation publisher are owned here so they
    // live as long as the process. A dropped handle stops each one.
    shutdown::listen();
    while !shutdown::requested() {
        std::thread::sleep(Duration::from_millis(200));
    }

    // The operational listener stays up until the process exits, so `/readyz`
    // answers "not ready" for the whole stop. The replication listener stops
    // when `cluster` drops, after the requests that needed its peers finished.
    lifecycle::shut_down(lifecycle::ShutdownParts {
        health: &health,
        stop: &background.stop,
        logger: &logger,
        stop_listeners: vec![
            Box::new(|| server.stop()),
            Box::new(|| {
                if let Some(dashboard) = dashboard.as_ref() {
                    dashboard.stop();
                }
            }),
        ],
        wait_until_quiet: Box::new(|grace| server.wait_until_quiet(grace)),
        seal: Box::new(|| segmented.seal().map(|_| ()).map_err(|e| e.to_string())),
        grace: Duration::from_millis(config.duration_ms("head.shutdownGrace").max(0) as u64),
    });
    drop(self_observation);
    drop(cluster);
    Ok(())
}

/// What alerting and the workflow passes need to run.
struct Alerting {
    alerts: Arc<tallyowl_head::alerts::AlertService>,
    workflows: Arc<tallyowl_head::workflows::Workflows>,
}

/// Start the scheduler and the workers, and connect the durable queue behind
/// them.
///
/// **The sweep is the part nothing else can do.** Corndogs evaluates a task
/// timeout only when a caller invokes `CleanUpTimedOut`, so retry, backoff, and
/// dead-worker recovery all stop when the scheduler thread stops.
fn start_alerting(
    config: &tallyowl_config::Config,
    store: Arc<SegmentedStore>,
    query: Arc<QueryService>,
    metrics: Arc<Registry>,
    logger: Arc<Logger>,
    health: Arc<Health>,
    background: &Background,
) -> Alerting {
    use tallyowl_head::workflows::{Kind, Workflows};

    health.declare(
        WORKFLOW_QUEUE_CHECK,
        "The durable queue has not answered since this head started, so alert evaluation, notifications, and the projector passes are not running. The head keeps trying. Check that Corndogs is running at `corndogs.endpoint`.",
    );
    let late = lifecycle::LateQueue::new();
    {
        let late = Arc::clone(&late);
        let health = Arc::clone(&health);
        let logger = Arc::clone(&logger);
        let stop = Arc::clone(&background.stop);
        let endpoint = config.text("corndogs.endpoint").to_string();
        // D62: TLS to a Corndogs endpoint that is not loopback. The head's own
        // sidecar is on loopback and stays plaintext.
        let queue_options = tallyowl_queue::QueueOptions {
            connections: config.integer("corndogs.connections").max(1) as usize,
            call_timeout: std::time::Duration::from_millis(
                config.duration_ms("corndogs.callTimeout").max(1) as u64,
            ),
            metrics: None,
            tls: tallyowl_queue::QueueTls::for_endpoint(
                &endpoint,
                config.text("corndogs.tls.caFile"),
                config.text("corndogs.tls.serverName"),
                config.boolean("transport.allowPlaintext"),
            ),
        };
        std::thread::Builder::new()
            .name("tallyowl-queue-connect".into())
            .spawn(move || {
                let connected = lifecycle::connect_with_backoff(
                    || {
                        tallyowl_queue::CorndogsQueue::connect_with(&endpoint, queue_options.clone())
                            .map(|queue| Arc::new(queue) as Arc<dyn tallyowl_queue::DurableQueue>)
                    },
                    |delay| stop.wait(delay),
                    |attempt, failure, delay| {
                        // The first failure, and then one line in ten. A
                        // dependency that is down for an hour is one fact.
                        if attempt % 10 == 0 {
                            logger.warning(
                                "Alerting and the workflow passes are waiting, because the durable queue could not be reached. The head tries again.",
                                &[
                                    ("reason", &failure.to_string()),
                                    ("attempt", &(attempt + 1).to_string()),
                                    ("next_attempt_ms", &delay.as_millis().to_string()),
                                ],
                            );
                        }
                    },
                    Duration::from_secs(30),
                    tallyowl_obs::time::now_nanos() as u64,
                );
                if let Some(queue) = connected {
                    late.connect(queue);
                    health.pass(WORKFLOW_QUEUE_CHECK);
                    logger.info(
                        "Reached the durable queue. Alert evaluation, notifications, and the projector passes are running.",
                        &[],
                    );
                }
            })
            .expect("the queue connection thread starts");
    }
    let queue: Arc<dyn tallyowl_queue::DurableQueue> =
        Arc::clone(&late) as Arc<dyn tallyowl_queue::DurableQueue>;
    let workflows = Arc::new(Workflows::new(
        Arc::clone(&queue),
        Arc::clone(&metrics),
        Arc::clone(&logger),
        config.duration_ms("corndogs.maxDeliveryAge"),
    ));
    let alerts = Arc::new(tallyowl_head::alerts::AlertService {
        store: Arc::clone(&store),
        query: Arc::clone(&query),
        metrics: Arc::clone(&metrics),
        // A running head can deliver one, so a rule may name one.
        callbacks_available: true,
        pool: Arc::new(tallyowl_head::alerts::BudgetPool::new(
            config.integer("query.alertConcurrency").max(1) as usize,
        )),
    });

    let scheduler = Arc::new(tallyowl_head::passes::Scheduler::new(
        Arc::clone(&alerts),
        Arc::clone(&workflows),
        Arc::clone(&logger),
    ));
    let sweep_interval =
        Duration::from_millis(config.duration_ms("corndogs.sweepInterval").max(100) as u64);
    {
        const LOOP: &str = "alert-schedule";
        let workflows = Arc::clone(&workflows);
        let late = Arc::clone(&late);
        let health = Arc::clone(&health);
        let (watch, stop) = background.for_loop(LOOP, sweep_interval);
        std::thread::Builder::new()
            .name("tallyowl-alert-schedule".into())
            .spawn(move || loop {
                watch.pass(LOOP, tallyowl_obs::time::now_ms, || {
                    // Nothing to sweep and nowhere to queue until the queue
                    // answers. `workflow-queue` is what says so.
                    if !late.is_connected() {
                        return;
                    }
                    let _ = scheduler.tick(tallyowl_obs::time::now_ms());
                    workflows.sample();
                    // Retry and dead-worker recovery stop when the sweep does.
                    // That does not stop ingest, so it is `degraded`.
                    match scheduler.sweep_failure() {
                        Some(reason) => health.set(
                            WORKFLOW_QUEUE_CHECK,
                            tallyowl_obs::health::CheckState::Degraded {
                                cause: tallyowl_obs::health::Cause::Established(format!(
                                    "The workflow sweep is failing, so nothing scheduled is retried until it runs again. {reason}"
                                )),
                            },
                        ),
                        None => health.pass(WORKFLOW_QUEUE_CHECK),
                    }
                });
                if stop.wait(sweep_interval) {
                    break;
                }
            })
            .expect("the alert scheduler starts");
    }

    // A secret is a reference and never a value. `file:` and `env:` are the two
    // forms, and the same loader every other credential uses reads them.
    let secrets: tallyowl_head::passes::SecretLookup = Arc::new(|reference| {
        tallyowl_config::secret::parse_reference(reference)
            .ok()
            .and_then(|parsed| {
                tallyowl_config::secret::resolve(
                    &parsed,
                    &tallyowl_config::secret::HostSecretSource,
                )
                .ok()
            })
            .map(|secret| secret.expose().to_string())
    });

    // One worker for each queue. They are separate threads for the same reason
    // they are separate queues: a receiver that is not answering must not stop
    // an evaluation, and a rebuild must not stop either.
    let runners: Vec<(&'static str, Arc<dyn tallyowl_head::workflows::Runner>)> = vec![
        (
            Kind::AlertEvaluation.queue(),
            Arc::new(tallyowl_head::passes::AlertRunner {
                alerts: Arc::clone(&alerts),
                workflows: Arc::clone(&workflows),
                logger: Arc::clone(&logger),
                metrics: Arc::clone(&metrics),
            }),
        ),
        (
            Kind::Notification.queue(),
            Arc::new(tallyowl_head::passes::NotificationRunner {
                alerts: Arc::clone(&alerts),
                store: Arc::clone(&store),
                metrics: Arc::clone(&metrics),
                timeout: tallyowl_head::notify::DEFAULT_TIMEOUT,
                egress: tallyowl_head::notify::Egress::allowing(
                    config.list("alerts.allowedPrivateTargets"),
                ),
                secrets,
                // A native callback goes over CSIL-RPC to a service that
                // already speaks it. The connection is the authentication, so
                // there is no signature: see `notify::RpcCallbacks`.
                callbacks: Some(Arc::new(tallyowl_head::notify::RpcCallbacks {
                    max_frame_bytes: config.bytes("corndogs.maxPayloadBytes").max(0) as usize,
                })),
            }),
        ),
        (
            Kind::ProjectorRebuild.queue(),
            Arc::new(tallyowl_head::passes::ProjectorRunner {
                store: Arc::clone(&store),
                identity: Arc::clone(&query.identity),
                logger: Arc::clone(&logger),
                raw_retention_ms: config.duration_ms("retention.raw"),
                receipt_window_ms: config.duration_ms("storage.deduplicationWindow"),
                reserve_bytes: config.bytes("storage.reserveBytes").max(0) as u64,
                export_root: std::path::Path::new(config.text("head.dataDir")).join("exports"),
            }),
        ),
    ];
    // **More than one worker for notifications.** A receiver that accepts a
    // connection and answers slowly holds its worker for the whole delivery
    // timeout, and with one worker every other tenant's notification waited
    // behind it.
    let notification_workers = config.integer("alerts.notificationWorkers").clamp(1, 32);
    for (queue_name, runner) in runners {
        let workers = match queue_name == Kind::Notification.queue() {
            true => notification_workers,
            false => 1,
        };
        for worker in 0..workers {
            // A loop name lives as long as the process, and there are a handful.
            let name: &'static str = match worker {
                0 => queue_name,
                _ => Box::leak(format!("{queue_name}-{}", worker + 1).into_boxed_str()),
            };
            let idle = Duration::from_millis(250);
            let workflows = Arc::clone(&workflows);
            let runner = Arc::clone(&runner);
            let late = Arc::clone(&late);
            let (watch, stop) = background.for_loop(name, idle);
            std::thread::Builder::new()
                .name(format!("tallyowl-work-{name}"))
                .spawn(move || {
                    while !stop.is_set() {
                        let mut found = false;
                        watch.pass(name, tallyowl_obs::time::now_ms, || {
                            if late.is_connected() {
                                found = matches!(
                                    workflows.run_one(queue_name, runner.as_ref()),
                                    Ok(true)
                                );
                            }
                        });
                        // Nothing was waiting, or the queue did not answer.
                        // Asking again as fast as possible would be a busy
                        // loop against the durable store.
                        if !found && stop.wait(idle) {
                            break;
                        }
                    }
                })
                .expect("a workflow worker starts");
        }
    }

    Alerting { alerts, workflows }
}
