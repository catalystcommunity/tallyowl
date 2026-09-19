//! Every TallyOwl setting, declared once.
//!
//! **One name for one setting, in every place it appears.** A setting has one
//! key path, and that path is identical in the configuration file, the
//! environment variable, the command-line flag, and the Helm value:
//!
//! | Place | Form |
//! | --- | --- |
//! | Helm value | `storage.receiptPolicy` |
//! | Configuration file | `storage: { receiptPolicy: ... }` |
//! | Environment variable | `TALLYOWL_STORAGE__RECEIPT_POLICY` |
//! | Command-line flag | `--storage.receipt-policy` |
//!
//! This list is the reason those four cannot drift. The loader derives all four
//! forms from the key path, and the chart parity test walks this list against
//! the chart values. A setting that reaches only one of them fails that test.
//!
//! Every value has a default that is safe for a home installation, so an empty
//! configuration file starts a working system.

use crate::value::Kind;

/// One setting.
#[derive(Debug, Clone)]
pub struct Setting {
    /// The one key path, in the form a Helm value and a configuration file use.
    pub path: &'static str,
    pub kind: Kind,
    /// The text form of the home-profile default.
    pub default: &'static str,
    /// What this setting does, for `config check` and for the chart.
    pub help: &'static str,
    /// A valid example, which a refusal message shows. See CONVENTIONS.md
    /// section 5: a configuration error names the setting, the value, and a
    /// valid example.
    pub example: &'static str,
}

const RECEIPT_POLICIES: &[&str] = &["local-one", "local-quorum", "remote-one", "custom"];
const INTEGRITY_MODES: &[&str] = &["none", "verify-on-read", "scrub"];
const CORNDOGS_BACKENDS: &[&str] = &["file", "postgres"];
const CORNDOGS_FSYNC_MODES: &[&str] = &["group", "always", "interval", "never"];
const SLOW_NODE_ACTIONS: &[&str] = &["alert", "demote"];
/// What the cell controller may do without an operator. CELLS.md section 6 says
/// the first releases use `recommendation-only` until tests prove safe
/// automatic control, so that is the default.
const PLACEMENT_MODES: &[&str] = &["automatic", "recommendation-only", "paused"];
const LOG_LEVELS: &[&str] = &["debug", "info", "warning", "error"];
const PROFILES: &[&str] = &["home", "replicated", "scaled"];

/// The complete setting list. Keep it sorted by key path, because `config check`
/// prints it in this order and a person reads it that way.
pub const SCHEMA: &[Setting] = &[
    // ---- Catalog -----------------------------------------------------------
    Setting {
        path: "catalog.snapshots.enabled",
        kind: Kind::Boolean,
        default: "false",
        help: "Take periodic catalog snapshots. Off by default; the default recovery for a lost catalog is a restore from the ordinary backup. See D59.",
        example: "false",
    },
    Setting {
        path: "catalog.snapshots.keep",
        kind: Kind::Integer,
        default: "2",
        help: "How many catalog snapshots to retain. Two survives a snapshot that is itself damaged, which one cannot.",
        example: "2",
    },
    Setting {
        path: "catalog.snapshots.period",
        kind: Kind::Duration,
        default: "1h",
        help: "Time between catalog snapshots.",
        example: "1h",
    },
    // ---- Cell --------------------------------------------------------------
    Setting {
        path: "cell.controllers",
        kind: Kind::Integer,
        default: "1",
        help: "Controller quorum size for this cell. A home installation has one and no quorum. A cell uses three or five.",
        example: "3",
    },
    Setting {
        path: "cell.id",
        kind: Kind::Text,
        default: "home",
        help: "Identifies this cell inside the installation.",
        example: "us-west-1",
    },
    Setting {
        path: "cell.region",
        kind: Kind::Text,
        default: "home",
        help: "The region label for placement and for write ownership.",
        example: "us-west2",
    },
    // ---- Collector ---------------------------------------------------------
    Setting {
        path: "collector.apiKey",
        kind: Kind::Secret,
        default: "",
        help: "A reference to the scoped TallyOwl API key this collector presents. Never a key value. Use `file:` or `env:`.",
        example: "file:/etc/tallyowl/collector.key",
    },
    Setting {
        path: "collector.keyCacheGrace",
        kind: Kind::Duration,
        default: "60s",
        help: "How long a collector may keep using a key answer it already had, when it cannot reach the head. An outage extends what already worked; a credential the collector never resolved still cannot start. See DELIVERY.md section 9.",
        example: "60s",
    },
    Setting {
        path: "collector.keyCacheTtl",
        kind: Kind::Duration,
        default: "30s",
        help: "How long a collector holds a resolved key before it asks the head again. This is how long a revocation takes to reach an application, so keep it short.",
        example: "30s",
    },
    Setting {
        path: "collector.listen",
        kind: Kind::Text,
        default: "127.0.0.1:5100",
        help: "Where collector intake accepts CSIL over TCP from an app driver.",
        example: "0.0.0.0:5100",
    },
    Setting {
        path: "collector.maxBatchBytes",
        kind: Kind::Bytes,
        default: "512KiB",
        help: "The largest batch collector intake accepts. D19 seals a batch here.",
        example: "512KiB",
    },
    Setting {
        path: "collector.maxEventBytes",
        kind: Kind::Bytes,
        default: "64KiB",
        help: "The largest single telemetry item collector intake accepts.",
        example: "64KiB",
    },
    Setting {
        path: "collector.maxProperties",
        kind: Kind::Integer,
        default: "128",
        help: "The most properties one item may carry. See D20.",
        example: "128",
    },
    Setting {
        path: "collector.operationalListen",
        kind: Kind::Text,
        default: "127.0.0.1:5101",
        help: "Where the collector serves health and metrics. This address serves nothing else.",
        example: "0.0.0.0:5101",
    },
    Setting {
        path: "collector.policyInterval",
        kind: Kind::Duration,
        default: "30s",
        help: "How often a collector fetches the collection policy from the head. This is how long a policy change and a kill switch take to reach a collector, so keep it short. A fetch that has nothing new to send costs a few bytes. See POLICY.md section 7.",
        example: "30s",
    },
    Setting {
        path: "collector.roles",
        kind: Kind::TextList,
        default: "intake,forwarder",
        help: "Which collector roles this process runs. Intake, forwarder, and compatibility-receiver are independently deployable, even though the home profile runs them together.",
        example: "intake,forwarder",
    },
    // ---- Compaction --------------------------------------------------------
    Setting {
        path: "compaction.coldGroupAfter",
        kind: Kind::Duration,
        default: "48h",
        help: "How old a segment must be before compaction consolidates its time bucket, grouped by the end-user value, so one person's cold rows sit in few segments. Zero disables the pass. See HIGH_CARDINALITY.md.",
        example: "48h",
    },
    Setting {
        path: "compaction.coldGroupBatch",
        kind: Kind::Bytes,
        default: "32MiB",
        help: "The most stored source bytes one consolidation pass rewrites. The rows of a pass are held decompressed in memory, many times their stored size — a 64 MiB bite measured 12 to 22 GB resident — so this bounds the memory of the pass; a large backlog converges over passes on the maintenance interval. Raise it only with the memory to pay for it.",
        example: "64MiB",
    },
    Setting {
        path: "compaction.coldGroupTarget",
        kind: Kind::Bytes,
        default: "2MiB",
        help: "The size cold consolidation aims each rewritten segment at. A hit decompresses every candidate segment whole, so this is also the price of a cold exact lookup. See BENCHMARKS.md section 24.1.",
        example: "2MiB",
    },
    Setting {
        path: "compaction.gcGrace",
        kind: Kind::Duration,
        default: "1h",
        help: "How long to wait before deleting an unpinned generation. It must exceed the longest query the budget permits.",
        example: "1h",
    },
    // ---- Compatibility receivers -------------------------------------------
    Setting {
        path: "compatibility.openTelemetry.enabled",
        kind: Kind::Boolean,
        default: "false",
        help: "Open the OpenTelemetry receiver. A compatibility receiver never listens by default; an operator enables it. See D12.",
        example: "false",
    },
    Setting {
        path: "compatibility.openTelemetry.listen",
        kind: Kind::Text,
        default: "127.0.0.1:4318",
        help: "Where the OpenTelemetry receiver listens once an operator enables it. 4318 is the OpenTelemetry Protocol over HTTP port; 4317 is the gRPC one, which this build does not serve.",
        example: "0.0.0.0:4318",
    },
    Setting {
        path: "compatibility.prometheus.interval",
        kind: Kind::Duration,
        default: "60s",
        help: "Time between scrapes of each configured target.",
        example: "60s",
    },
    Setting {
        path: "compatibility.prometheus.targets",
        kind: Kind::TextList,
        default: "",
        help: "Endpoints the collector scrapes. There is no default target; scraping is outbound and an operator names each one.",
        example: "http://127.0.0.1:9100/metrics",
    },
    Setting {
        path: "compatibility.prometheus.timeout",
        kind: Kind::Duration,
        default: "5s",
        help: "How long one scrape may take before the collector gives up on that target and counts the failure.",
        example: "5s",
    },
    // ---- Corndogs ----------------------------------------------------------
    Setting {
        path: "corndogs.backend",
        kind: Kind::Enum(CORNDOGS_BACKENDS),
        default: "file",
        help: "Which Corndogs storage backend this installation runs. It decides which durable-copy counts are possible.",
        example: "file",
    },
    Setting {
        path: "corndogs.deliveryQueue",
        kind: Kind::Text,
        default: "tallyowl-delivery",
        help: "The Corndogs queue that carries accepted batches from a collector to the head.",
        example: "tallyowl-delivery",
    },
    Setting {
        path: "corndogs.durableCopies",
        kind: Kind::Integer,
        default: "1",
        help: "Copies Corndogs must hold before a collector acknowledges a batch. The default of one means one fsynced copy and no redundancy.",
        example: "1",
    },
    Setting {
        path: "corndogs.endpoint",
        kind: Kind::Text,
        default: "127.0.0.1:5080",
        help: "The Corndogs address. Corndogs owns durable queue and workflow state.",
        example: "127.0.0.1:5080",
    },
    Setting {
        path: "corndogs.fsyncMode",
        kind: Kind::Enum(CORNDOGS_FSYNC_MODES),
        default: "group",
        help: "The Corndogs file-backend flush mode. `interval` and `never` acknowledge writes that a power loss can destroy, so TallyOwl refuses to start against them.",
        example: "group",
    },
    Setting {
        path: "corndogs.maxDeliveryAge",
        kind: Kind::Duration,
        default: "24h",
        help: "How long a batch may keep being retried before it goes to quarantine. D36 pairs this with the deduplication window: an automatic retry must stop before the head forgets the batch ID, or a later retry commits a second logical batch.",
        example: "24h",
    },
    Setting {
        path: "corndogs.maxPayloadBytes",
        kind: Kind::Bytes,
        default: "16MiB",
        help: "The Corndogs payload limit. It must exceed the batch seal size, because a batch payload travels inside the Corndogs task.",
        example: "16MiB",
    },
    Setting {
        path: "corndogs.quarantineQueue",
        kind: Kind::Text,
        default: "tallyowl-quarantine",
        help: "The Corndogs queue that holds a batch no retry can fix.",
        example: "tallyowl-quarantine",
    },
    Setting {
        path: "corndogs.sweepInterval",
        kind: Kind::Duration,
        default: "1s",
        help: "How often the forwarder calls the Corndogs timeout sweep. Retry, backoff, and dead-worker recovery all stop when the sweep stops. See D33.",
        example: "1s",
    },
    // ---- Dashboard ---------------------------------------------------------
    Setting {
        path: "dashboard.assets",
        kind: Kind::Text,
        default: "packages/dashboard/dist",
        help: "Where the built dashboard bundle is. `./tools.sh build` writes it.",
        example: "/usr/share/tallyowl/dashboard",
    },
    Setting {
        path: "dashboard.callbackPath",
        kind: Kind::Text,
        default: "/sign-in/callback",
        help: "The path the LinkKeys callback arrives at. It must be the path part of `linkkeys.callbackUrl`, or a sign-in returns to an address nothing serves.",
        example: "/sign-in/callback",
    },
    Setting {
        path: "dashboard.enabled",
        kind: Kind::Boolean,
        default: "true",
        help: "Serve the dashboard. It carries the sign-in callback route and the same-origin browser carrier, and it never carries telemetry.",
        example: "true",
    },
    Setting {
        path: "dashboard.listen",
        kind: Kind::Text,
        default: "127.0.0.1:5120",
        help: "Where the dashboard serves its document, its bundle, and the browser carrier. This is the origin `linkkeys.callbackUrl` must name.",
        example: "127.0.0.1:5120",
    },
    // ---- Dashboard ---------------------------------------------------------
    Setting {
        path: "dashboard.starterConversionGoal",
        kind: Kind::Text,
        default: "purchase",
        help: "The conversion goal the starter campaign dashboard asks about. A conversion goal is the application's own word, so set it to the one this installation sends.",
        example: "purchase",
    },
    Setting {
        path: "dashboard.starterDashboard",
        kind: Kind::Boolean,
        default: "true",
        help: "Write a starter campaign dashboard for each project the first time the head sees it. It is offered once: an operator who removes it does not get it back. Turn it off to provision dashboards yourself. See PHASE9_REPORT.md.",
        example: "true",
    },
    // ---- Head --------------------------------------------------------------
    Setting {
        path: "head.dataDir",
        kind: Kind::Text,
        default: "./data/head",
        help: "Where the head keeps its segments, its append log, and its catalog.",
        example: "/var/lib/tallyowl/head",
    },
    Setting {
        path: "head.endpoint",
        kind: Kind::Text,
        default: "127.0.0.1:5110",
        help: "The head address a collector forwarder reaches. An application never knows this address.",
        example: "127.0.0.1:5110",
    },
    Setting {
        path: "head.listen",
        kind: Kind::Text,
        default: "127.0.0.1:5110",
        help: "Where head ingest accepts CSIL over TCP from a collector forwarder.",
        example: "0.0.0.0:5110",
    },
    Setting {
        path: "head.operationalListen",
        kind: Kind::Text,
        default: "127.0.0.1:5111",
        help: "Where the head serves health and metrics. This address serves nothing else.",
        example: "0.0.0.0:5111",
    },
    // ---- Installation ------------------------------------------------------
    Setting {
        path: "installation.id",
        kind: Kind::Text,
        default: "local",
        help: "Identifies one logical TallyOwl installation. Every release in one installation uses the same value.",
        example: "acme-production",
    },
    Setting {
        path: "installation.profile",
        kind: Kind::Enum(PROFILES),
        default: "home",
        help: "The placement and replication profile. A profile never changes the stored format or the application integration.",
        example: "home",
    },
    // ---- Integrity ---------------------------------------------------------
    Setting {
        path: "integrity.mode",
        kind: Kind::Enum(INTEGRITY_MODES),
        default: "verify-on-read",
        help: "How hard TallyOwl looks for damage. `verify-on-read` catches damage in data a query touches and costs almost nothing. `none` is a legitimate choice and the dashboard shows it. See D57.",
        example: "verify-on-read",
    },
    Setting {
        path: "integrity.scrub.period",
        kind: Kind::Duration,
        default: "7d",
        help: "How long one full scrub pass takes.",
        example: "7d",
    },
    Setting {
        path: "integrity.scrub.rateLimit",
        kind: Kind::Bytes,
        default: "16MiB",
        help: "Read bandwidth each second that a scrub may use.",
        example: "16MiB",
    },
    // ---- LinkKeys ----------------------------------------------------------
    Setting {
        path: "linkkeys.appName",
        kind: Kind::Text,
        default: "TallyOwl",
        help: "The name somebody sees at their LinkKeys domain when they approve this installation.",
        example: "TallyOwl at Acme",
    },
    Setting {
        path: "linkkeys.callbackUrl",
        kind: Kind::Text,
        default: "http://127.0.0.1:5120/sign-in/callback",
        help: "Where a sign-in comes back to. A request that names any other address is refused, so a caller cannot redirect a sign-in somewhere it chose.",
        example: "https://tallyowl.example/sign-in/callback",
    },
    Setting {
        path: "linkkeys.enabled",
        kind: Kind::Boolean,
        default: "false",
        help: "Accept LinkKeys sign-ins. Off by default, because an installation with no trusted domains has nothing to accept. An operator can always issue a session with `tallyowl-head session create`.",
        example: "true",
    },
    Setting {
        path: "linkkeys.sessionLifetime",
        kind: Kind::Duration,
        default: "24h",
        help: "How long a session lasts after a sign-in.",
        example: "24h",
    },
    Setting {
        path: "linkkeys.trustedDomains",
        kind: Kind::TextList,
        default: "",
        help: "The LinkKeys domains whose assertions this installation accepts. Empty means none. D7: a claim from a domain the installation does not trust never maps to a role.",
        example: "example.com",
    },
    // ---- Logging -----------------------------------------------------------
    Setting {
        path: "log.level",
        kind: Kind::Enum(LOG_LEVELS),
        default: "info",
        help: "The quietest severity that reaches the log. An end-user ID appears at debug only, and the project policy can forbid even that.",
        example: "info",
    },
    // ---- Metrics -----------------------------------------------------------
    Setting {
        path: "metrics.downsampleResolution",
        kind: Kind::Duration,
        default: "1h",
        help: "The coarser resolution a metric downsample pass rolls delta points up to. DATA_MODEL.md section 6 names one minute, then hourly. Set it to 0 to run no downsample pass.",
        example: "1h",
    },
    Setting {
        path: "metrics.goldenSignalResolution",
        kind: Kind::Duration,
        default: "60s",
        help: "The window a service-operation golden-signal rollup covers. The head derives request, error, and duration series from spans at this resolution. Set it to 0 to derive none.",
        example: "60s",
    },
    Setting {
        path: "metrics.idleSeriesExpiry",
        kind: Kind::Duration,
        default: "1h",
        help: "How long a metric series stays active with nothing arriving for it. A series that stopped is not a series that costs, so the budget gets its place back.",
        example: "1h",
    },
    Setting {
        path: "metrics.maxBytesForEachMetric",
        kind: Kind::Bytes,
        default: "64MiB",
        help: "Retained bytes for one metric name in one project. Counted exactly from the points, not estimated from a row count.",
        example: "64MiB",
    },
    Setting {
        path: "metrics.maxLabelBytes",
        kind: Kind::Integer,
        default: "4096",
        help: "Bytes in all the labels of one metric series together.",
        example: "4096",
    },
    Setting {
        path: "metrics.maxLabelValueBytes",
        kind: Kind::Integer,
        default: "1024",
        help: "Bytes in one metric label value.",
        example: "1024",
    },
    Setting {
        path: "metrics.maxLabels",
        kind: Kind::Integer,
        default: "32",
        help: "Labels on one metric series.",
        example: "32",
    },
    Setting {
        path: "metrics.maxMergePoints",
        kind: Kind::Integer,
        default: "10000",
        help: "How many metric points in one batch the collector will merge. Past this the batch travels unmerged, which costs bytes and never costs correctness.",
        example: "10000",
    },
    Setting {
        path: "metrics.maxSeriesForEachMetric",
        kind: Kind::Integer,
        default: "100000",
        help: "Active series for one metric name in one project. A new series past this is refused in the open with `resource-exhausted`; TallyOwl never folds one into an overflow series. See D12 and DATA_MODEL.md section 3.4.",
        example: "100000",
    },
    Setting {
        path: "metrics.selfObservation.enabled",
        kind: Kind::Boolean,
        default: "false",
        help: "Push each service's own instruments into the protected internal project. Off by default, because it costs ingest capacity that an installation may want for its own data. See D12.",
        example: "true",
    },
    Setting {
        path: "metrics.selfObservation.period",
        kind: Kind::Duration,
        default: "60s",
        help: "Time between self-observation snapshots.",
        example: "60s",
    },
    // ---- Node --------------------------------------------------------------
    Setting {
        path: "node.failureDomain",
        kind: Kind::Text,
        default: "default",
        help: "The failure domain this node is in: a rack, a zone, or a host. Placement puts at most one voter of a tablet in each domain, so one domain's loss never ends a quorum.",
        example: "rack-7",
    },
    Setting {
        path: "node.name",
        kind: Kind::Text,
        default: "",
        help: "What the control plane calls this node. Empty means the head assigns one at enrollment, which is the ordinary case; set it only when a node has to keep a name across a rebuild.",
        example: "storage-041",
    },
    // ---- Placement ---------------------------------------------------------
    Setting {
        path: "placement.concurrentChanges",
        kind: Kind::Integer,
        default: "1",
        help: "How many tablet splits, merges, or movements may run at once in one cell. A cell that started every change it wanted would move more data than it could carry.",
        example: "1",
    },
    Setting {
        path: "placement.mergeBelow",
        kind: Kind::Bytes,
        default: "8GiB",
        help: "Merge two adjacent tablets when both hold less than this. It is far below half of `placement.splitAbove` on purpose: making it exactly half would let a tablet split and merge repeatedly around one size.",
        example: "8GiB",
    },
    Setting {
        path: "placement.mode",
        kind: Kind::Enum(PLACEMENT_MODES),
        default: "recommendation-only",
        help: "What the cell controller may do on its own. It decides in every mode; the mode selects whether it acts. CELLS.md section 6 names recommendation-only for the first releases.",
        example: "automatic",
    },
    Setting {
        path: "placement.slowNode.action",
        kind: Kind::Enum(SLOW_NODE_ACTIONS),
        default: "alert",
        help: "What to do about a node that answers slowly. Automatic demotion during a network-wide slowdown cascades, so `alert` is the default. See D60.",
        example: "alert",
    },
    Setting {
        path: "placement.slowNode.duration",
        kind: Kind::Duration,
        default: "5m",
        help: "How long a node must stay slow before the state changes.",
        example: "5m",
    },
    Setting {
        path: "placement.slowNode.factor",
        kind: Kind::Integer,
        default: "4",
        help: "The multiple of the group median that counts as slow.",
        example: "4",
    },
    // ---- Query -------------------------------------------------------------
    Setting {
        path: "placement.splitAbove",
        kind: Kind::Bytes,
        default: "64GiB",
        help: "Split a tablet that holds more than this. A split moves no virtual shard: it divides the shard range.",
        example: "64GiB",
    },
    Setting {
        path: "query.alertConcurrency",
        kind: Kind::Integer,
        default: "1",
        help: "How many alert evaluations may run at once. This is the separate budget pool an alert evaluation draws from, so alerting cannot occupy more of the storage and query path than this. One is what the head's single evaluation worker already gives; setting it higher only has an effect alongside more workers.",
        example: "1",
    },
    Setting {
        path: "query.identityRefresh",
        kind: Kind::Duration,
        default: "5m",
        help: "How long a materialised identity graph is used before it is built again. It is also the longest an identity row that arrived late can go unseen by a question about a range that ended before it. Zero rebuilds for every question, which is what this installation did before the graph was materialised.",
        example: "5m",
    },
    Setting {
        path: "query.maxExpressionDepth",
        kind: Kind::Integer,
        default: "16",
        help: "How deeply a query may nest expressions. A tree deeper than this is refused before it is evaluated, so a hostile query cannot exhaust memory during planning. QUERY.md section 5 gives the default.",
        example: "16",
    },
    Setting {
        path: "query.maxFanOut",
        kind: Kind::Integer,
        default: "256",
        help: "How many tablets one query may ask at once. A query that needs more is refused with a typed error that names both numbers, rather than opening a connection to every tablet in the cell. QUERY.md section 10.",
        example: "256",
    },
    Setting {
        path: "query.maxRuntime",
        kind: Kind::Duration,
        default: "30s",
        help: "The longest a query may run. The compaction grace period must exceed it, or compaction can delete a generation a running query still reads.",
        example: "30s",
    },
    // ---- Replication -------------------------------------------------------
    Setting {
        path: "replication.keepAfterSnapshot",
        kind: Kind::Integer,
        default: "512",
        help: "Consensus log entries kept after a snapshot. Enough that an ordinary restart, and a follower that fell a little behind, catch up from the log rather than transferring a snapshot and then segments. With the setting below this bounds a tablet's consensus log at about 4,608 entries rather than at the number of batches the installation has ever taken. See D27 and BENCHMARKS.md section 20.",
        example: "512",
    },
    Setting {
        path: "replication.listen",
        kind: Kind::Text,
        default: "",
        help: "Where this node answers other storage nodes: consensus messages, snapshot chunks, segment copies, and partial aggregates. Empty means the home profile, which has one node and no peer to answer. This address is never reached by an application.",
        example: "0.0.0.0:5200",
    },
    Setting {
        path: "replication.peers",
        kind: Kind::TextList,
        default: "",
        help: "The addresses of the other storage nodes this one starts with. A cell normally learns its peers from its controller quorum; this is what a first bootstrap uses.",
        example: "10.0.0.11:5200,10.0.0.12:5200",
    },
    Setting {
        path: "replication.snapshotEvery",
        kind: Kind::Integer,
        default: "4096",
        help: "Committed consensus entries between snapshots. This is what bounds the consensus log on disk. Lower it and a tablet keeps less log and makes more small segments, because a tablet snapshot seals; raise it and the log grows. The default is roughly 710,000 rows at the measured 173 events for each batch, which is near the 800,000 the store's own sealing policy targets.",
        example: "4096",
    },
    Setting {
        path: "replication.writeTimeout",
        kind: Kind::Duration,
        default: "10s",
        help: "How long a write waits for its tablet group to commit before it is refused. Longer than an election, so an ordinary leader change costs a retry; short enough that an application is not left holding a batch for a partition's lifetime.",
        example: "10s",
    },
    // ---- Retention classes -------------------------------------------------
    //
    // POLICY.md section 4 and D43. A telemetry kind maps to a class, and the
    // couplings between the classes are correctness rules that `validate`
    // refuses at startup rather than preferences.
    Setting {
        path: "retention.audit",
        kind: Kind::Duration,
        default: "2555d",
        help: "How long control-plane, deletion, and export records are kept. It is never shorter than the deletion horizon. See POLICY.md section 4.",
        example: "2555d",
    },
    Setting {
        path: "retention.detailed",
        kind: Kind::Duration,
        default: "30d",
        help: "How long events, spans, error occurrences, and metric points are kept. This is the main analytic range.",
        example: "30d",
    },
    Setting {
        path: "retention.raw",
        kind: Kind::Duration,
        default: "0s",
        help: "How long accepted interchange CBOR is kept. Zero keeps none, which is the default: an ordinary query never needs it.",
        example: "24h",
    },
    Setting {
        path: "retention.rollup",
        kind: Kind::Duration,
        default: "395d",
        help: "How long aggregates and downsampled series are kept. It must be at least as long as `retention.detailed`, because a rollup that expires first leaves a gap no query can fill.",
        example: "395d",
    },
    // ---- Sampling ----------------------------------------------------------
    Setting {
        path: "sampling.tail.decisionWindow",
        kind: Kind::Duration,
        default: "60s",
        help: "How long after a trace's last span the head decides whether to keep it. The provisional storage cost is bounded by this. See D35.",
        example: "60s",
    },
    Setting {
        path: "sampling.tail.enabled",
        kind: Kind::Boolean,
        default: "false",
        help: "Apply tail-sampling rules to committed traces. Off by default, because a home installation keeps everything and dropping data is an operator's decision.",
        example: "false",
    },
    Setting {
        path: "sampling.tail.keepPercent",
        kind: Kind::Integer,
        default: "100",
        help: "The share of ordinary traces to keep, from 0 to 100. A trace holding an error or a conversion, a trace with a failed span, and a slow trace are kept whatever this says.",
        example: "10",
    },
    Setting {
        path: "sampling.tail.keepSlowerThan",
        kind: Kind::Duration,
        default: "2s",
        help: "A trace at least this long is always kept. A slow trace is the one somebody is looking for.",
        example: "2s",
    },
    Setting {
        path: "sampling.tail.lateSpanGrace",
        kind: Kind::Duration,
        default: "30s",
        help: "How long after a decision a late span is still expected. A span later than this cannot change an applied decision; it is counted and the existing decision applies. See D35.",
        example: "30s",
    },
    // ---- Storage -----------------------------------------------------------
    Setting {
        path: "storage.coldTier.enabled",
        kind: Kind::Boolean,
        default: "false",
        help: "Move older retained segments to object storage. The home profile does not use it.",
        example: "false",
    },
    Setting {
        path: "storage.deduplicationWindow",
        kind: Kind::Duration,
        default: "72h",
        help: "How long the head remembers a batch ID, so a repeated batch stays one logical commit. D36 pairs this with `corndogs.maxDeliveryAge`: it must be longer, or an automatic retry can outlive the head's memory and commit a second logical batch.",
        example: "72h",
    },
    Setting {
        path: "storage.receiptPolicy",
        kind: Kind::Enum(RECEIPT_POLICIES),
        default: "local-one",
        help: "What a tablet must satisfy before the head returns a committed receipt. `local-one` is legal only for a tablet with one voter. See D27.",
        example: "local-one",
    },
    Setting {
        path: "storage.reserveBytes",
        kind: Kind::Bytes,
        default: "1GiB",
        help: "Space held back so that recovery can still write when the disk is otherwise full.",
        example: "1GiB",
    },
    Setting {
        path: "storage.tabletVoters",
        kind: Kind::Integer,
        default: "1",
        help: "Voting replicas for each tablet. The home profile has one.",
        example: "1",
    },
];

/// Find a setting by its key path.
pub fn find(path: &str) -> Option<&'static Setting> {
    SCHEMA.iter().find(|s| s.path == path)
}

/// The environment variable form of a key path: `TALLYOWL_` plus the path with
/// a section separator as a double underscore and each word in upper case.
///
/// A section separator becomes a double underscore so that a key that holds an
/// underscore stays unambiguous.
pub fn environment_name(path: &str) -> String {
    let mut out = String::from("TALLYOWL_");
    for (index, section) in path.split('.').enumerate() {
        if index > 0 {
            out.push_str("__");
        }
        out.push_str(&camel_to_upper_snake(section));
    }
    out
}

/// The command-line flag form of a key path: `--` plus the path with each
/// section in kebab case. The section separator stays a full stop, so the flag
/// reads like the file.
pub fn flag_name(path: &str) -> String {
    let sections: Vec<String> = path.split('.').map(camel_to_kebab).collect();
    format!("--{}", sections.join("."))
}

fn camel_to_upper_snake(section: &str) -> String {
    let mut out = String::new();
    for c in section.chars() {
        if c.is_ascii_uppercase() && !out.is_empty() {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}

fn camel_to_kebab(section: &str) -> String {
    let mut out = String::new();
    for c in section.chars() {
        if c.is_ascii_uppercase() && !out.is_empty() {
            out.push('-');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn the_four_forms_of_one_setting_match_the_documented_example() {
        // CONVENTIONS.md section 5 gives this exact row. If this test fails, the
        // loader and the documentation disagree, and the documentation wins.
        assert_eq!(
            environment_name("storage.receiptPolicy"),
            "TALLYOWL_STORAGE__RECEIPT_POLICY"
        );
        assert_eq!(
            flag_name("storage.receiptPolicy"),
            "--storage.receipt-policy"
        );
    }

    #[test]
    fn a_three_section_path_keeps_every_separator_distinct() {
        assert_eq!(
            environment_name("catalog.snapshots.enabled"),
            "TALLYOWL_CATALOG__SNAPSHOTS__ENABLED"
        );
        assert_eq!(
            environment_name("integrity.scrub.rateLimit"),
            "TALLYOWL_INTEGRITY__SCRUB__RATE_LIMIT"
        );
        assert_eq!(
            flag_name("integrity.scrub.rateLimit"),
            "--integrity.scrub.rate-limit"
        );
    }

    #[test]
    fn every_key_path_is_unique() {
        let mut seen = BTreeSet::new();
        for setting in SCHEMA {
            assert!(
                seen.insert(setting.path),
                "the key path `{}` appears twice",
                setting.path
            );
        }
    }

    #[test]
    fn every_environment_name_is_unique() {
        // Two different key paths that collapse to one environment variable
        // would make precedence undefined. The double underscore exists to
        // prevent that, and this test proves it holds for the whole schema.
        let mut seen = BTreeSet::new();
        for setting in SCHEMA {
            let name = environment_name(setting.path);
            assert!(
                seen.insert(name.clone()),
                "the environment name `{name}` appears twice"
            );
        }
    }

    #[test]
    fn every_default_parses_as_its_own_kind() {
        for setting in SCHEMA {
            if setting.kind == Kind::Secret && setting.default.is_empty() {
                continue;
            }
            crate::value::parse(&setting.kind, setting.default).unwrap_or_else(|e| {
                panic!(
                    "the default `{}` for `{}` does not parse: {}",
                    setting.default, setting.path, e.reason
                )
            });
        }
    }

    #[test]
    fn every_example_parses_as_its_own_kind() {
        for setting in SCHEMA {
            crate::value::parse(&setting.kind, setting.example).unwrap_or_else(|e| {
                panic!(
                    "the example `{}` for `{}` does not parse: {}",
                    setting.example, setting.path, e.reason
                )
            });
        }
    }

    #[test]
    fn the_schema_stays_sorted_by_key_path() {
        let paths: Vec<&str> = SCHEMA.iter().map(|s| s.path).collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(paths, sorted, "`config check` prints the schema in order");
    }

    #[test]
    fn every_setting_explains_itself() {
        for setting in SCHEMA {
            assert!(!setting.help.is_empty(), "`{}` has no help", setting.path);
            assert!(
                !setting.example.is_empty(),
                "`{}` has no example, and a refusal message needs one",
                setting.path
            );
        }
    }
}
