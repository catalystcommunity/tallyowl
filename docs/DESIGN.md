# TallyOwl system design

## 1. Status and scope

This document gives the proposed system architecture. It includes later
analysis types. Thus, early schemas do not prevent later functions.

This document uses these abbreviations:

- CBOR Service Interface Language (CSIL);
- Concise Binary Object Representation (CBOR);
- remote procedure call (RPC);
- write-ahead log (WAL);
- key-value (KV);
- certificate authority (CA);
- mutual Transport Layer Security (mTLS).

[DOCUMENTATION.md](DOCUMENTATION.md) holds the full list.

### Required constraints

- One logical TallyOwl system correlates all supported telemetry categories.
- Instrumented browsers use only the application's existing same-origin CSIL
  connection.
- An application's backend routes TallyOwl messages; it does not render
  analytics reports.
- Native hops use CSIL over TCP with TLS, not HTTP.
- A collector is a separate, reusable service, normally one deployment per
  cluster or failure domain rather than one sidecar per application pod.
- Collectors minimize application work and absorb batching, outages,
  backpressure, and retries.
- Corndogs provides durable queues and workflows.
- Collector receipt durability is configurable by total durable copy count and
  defaults to one non-redundant copy.
- Collectors use scoped API keys. Overlapping active keys permit safe rotation.
- The dashboard uses LinkKeys login.
- TallyOwl provides generated CSIL clients, a hand-written browser package,
  and hand-written app drivers.
- TallyOwl does not aggregate application logs.
- TallyOwl libraries push metrics. The metric types include counters, gauges,
  and histograms.
- Collectors can scrape Prometheus and OpenMetrics endpoints and accept
  OpenTelemetry pushes at the compatibility edge, then translate immediately
  into native CSIL telemetry.
- TallyOwl services expose their own operational metrics for Prometheus and Grafana
  and may also dogfood them into a recursion-protected internal TallyOwl
  project.
- Helm charts cover the head deployment and collector deployment.
- The default installation supports a home lab with many apps.
- Storage and query nodes can shard and replicate for high ingest rates.
- TallyOwl is Apache-2.0 licensed.

### In scope

- Browser and backend application events
- Page views, screen views, and sessions
- Errors, exception groups, releases, and regressions
- Distributed traces and spans
- Counters, gauges, histograms, and exemplars
- Users and end users, anonymous identities, aliases, groups, and cohorts
- Funnels, paths, retention, and feature and adoption analysis
- Campaign touchpoints, attribution, conversions, and revenue
- Saved queries, dashboards, alerts, and derived monitors
- Sampling, redaction, retention, and per-project collection policy

### Explicitly out of scope

- General-purpose log shipping, indexing, and search
- A browser-to-TallyOwl endpoint
- Editing generated CSIL output
- Using Corndogs task rows as the analytics query model
- Arbitrary remote code execution in query or workflow definitions
- Session replay and Document Object Model recording

## 2. System shape

```text
Instrumented browser
  │ existing same-origin CSIL connection
  ▼
Application backend ───────────────┐
                                   │ TallyOwl app driver, CSIL over TCP
Backend instrumentation ───────────┤
                                   ▼
                    Cluster or failure-domain collector
                         ├─ validate, redact, enrich
                         ├─ one durable Corndogs handoff
                         ├─ package and compress batches
                         └─ retry with stable batch IDs
                                   │ CSIL-RPC over TCP with TLS
                                   ▼
                             TallyOwl head
                         ├─ authenticate and authorize
                         ├─ ingest and query services
                         ├─ projection and alert workers
                         ├─ embedded or replicated control catalog
                         └─ dashboard + LinkKeys RP
                                   │
                    ┌──────────────┴──────────────┐
                    ▼                             ▼
          TallyOwl storage nodes         Corndogs workflows
          WAL + segments + catalog       alerts, rebuilds, maintenance
```

The "head" is a logical role, not one process or pod. Stateless ingest and query
replicas can scale independently. Collectors know a stable head address, not a
particular pod, cluster, or storage node.

TallyOwl uses a cell hierarchy. The home profile puts all TallyOwl head roles
in one process. A regional cell controls its tablets and storage nodes.

The home profile is one TallyOwl head process, not one container. A home
installation runs three containers:

```text
tallyowl-head        head roles, query, storage, dashboard, one data directory
tallyowl-collector   intake, forwarder, compatibility receiver
corndogs             durable queue, file backend, one volume
```

The head and the collector share one Corndogs deployment in the home profile.
They use separate queues. A larger installation can separate them.

"One binary and no external database" is true. It does not mean one container.

A large installation has a small global directory. This directory maps projects
to cells. It does not control each tablet.

## 3. Component responsibilities

### 3.1 Browser package

The browser package is deliberately thin:

- exposes typed helpers such as `capture`, `pageView`, `recordError`,
  `startSpan`, `identify`, `alias`, and `conversion`;
- exposes `startSession` and `endSession`, the same pair that every other
  client library uses;
- generates stable event, session, trace, and span IDs;
- adds browser-safe context such as route, release, locale, viewport, referrer,
  and campaign parameters according to policy;
- applies client-side sampling and field filtering when configuration is
  available;
- encodes the generated TallyOwl CSIL types;
- sends through a caller-supplied operation on the application's already-open
  CSIL transport;
- maintains a small bounded memory buffer for momentary carrier backpressure;
- flushes its buffer when the page becomes hidden or unloads.

It does not know the TallyOwl head address. It does not hold a collector key.
It does not open a second socket or make a cross-origin request.

The unload flush sends to the host application's own same-origin route. It may
use `sendBeacon`. This does not break the integration rule, because the flush
reaches the application and not a TallyOwl domain. The host application must
expose one same-origin route for it.

Without this flush, TallyOwl loses the session-end event and the page-exit
event. A funnel and a session-duration analysis need those events.

The flush remains best effort. The package does not promise durable delivery
after the browser closes the tab.

The host application includes the TallyOwl CSIL schema and registers the
generated TallyOwl service router on its existing carrier. The imported
contract supports both host shapes:

- an application already using CSIL-RPC invokes the TallyOwl capture operation
  through the same client, endpoint, authentication, and route dispatcher;
- an application already using CSIL-Events sends telemetry on the same
  multiplexed connection and can receive collection policy and control messages.

Neither form constructs a browser-side TallyOwl transport. The protocol phase
defines the operation syntax and wire IDs.

Application-specific schemas do not copy these definitions.

### 3.2 App driver

The app driver receives generated telemetry values from the host's CSIL router
and forwards them to a collector. It also exposes native instrumentation for
backend code.

The name is deliberate. The generated CSIL code supplies types, codecs, and
routing. The driver owns the connection, batching, retry, and backpressure, in
the same sense that a database driver owns those concerns.

It owns:

- mapping trusted server session identity onto browser-supplied telemetry;
- rejecting client attempts to assert workspace, project, or privileged end user
  identity;
- one persistent, reconnecting CSIL over TCP connection to the collector;
- bounded micro-batching, stable batch IDs, pipelined in-flight requests, and
  retry after a lost connection and acknowledgement;
- retaining each submitted batch until the collector acknowledges that
  Corndogs durably accepted it;
- propagation and extraction of trace context through application calls;
- metric aggregation and periodic snapshot push;
- explicit backpressure when its bounded unacknowledged-byte budget is full.

It does not own analytics storage, long outage buffering, batch retry policy, or
dashboard queries. It may lose an unacknowledged in-memory batch if the app
process dies; it must never describe that batch as durable. After a collector
ack, durable delivery is the collector's responsibility.

The first maintained drivers must match the first consuming applications.
csilgen can make packages for all its targets.

The project adds hand-written driver functions for each supported language.

### 3.3 Collector

The collector is the durability and traffic-shaping boundary nearest the apps.
One collector deployment may serve many applications in a cluster. Separate
collectors may serve other clusters, bare-metal hosts, or remote networks.

It owns:

- a CSIL over TCP intake service for app drivers;
- compatibility receivers for Prometheus and OpenMetrics scraping and for
  OpenTelemetry metric and trace push, but not logs;
- an operator must enable a receiver before it listens or scrapes;
- authentication of local callers when the cluster trust model requires it;
- schema and version validation and hard size limits;
- normalization, redaction, enrichment, sampling, and resource protection
  without treating high cardinality itself as invalid;
- durable acceptance into Corndogs;
- fair batching by destination, workspace, project, and priority;
- compression and transfer to the head;
- retry, jitter, outage buffering, quarantine, and operator-visible health;
- remote policy caching with a last-known-good configuration;
- collector self-metrics, never recursively sent without a loop guard.

The collector acknowledges durable intake only after Corndogs accepts the task.
The Corndogs payload is the intermediate durable copy. The collector does not
need a second journal.

The collector completes the task only after the final storage receipt. Corndogs
can then remove the task.

The task is queue state, not retained telemetry history. The native intake path
does not bypass this acknowledgement boundary.

The receipt includes the configured total durable-copy count. One copy is the
home and small-business default and means persisted but not redundant. Clustered
operators can require more copies; the collector waits for that configured
Corndogs durability result before replying.

Collector runtime roles are independently deployable:

- **intake:** owns persistent app connections, validates batches, submits them
  to Corndogs, and returns durable acknowledgements;
- **forwarder:** claims Corndogs tasks, optionally coalesces compatible
  payloads, sends stable batches to the head, and completes tasks after final
  storage receipts;
- **compatibility receiver:** performs Prometheus and OpenMetrics scrapes and
  receives OpenTelemetry metrics and traces before submitting native batches
  through the same Corndogs boundary.

A batch payload travels inside the Corndogs task. Corndogs stores a payload in
its own bucket as raw bytes, so a large payload does not slow the timeout
sweep. A collector therefore holds no durable state of its own. See D4.

TallyOwl stays in single-binary territory, so the OpenTelemetry Protocol (OTLP)
receiver compiles into the collector. It does not open a listening socket by
default. An operator enables the listener in configuration.

Scraping is outbound and has no default target. A default installation
therefore accepts no OpenTelemetry traffic, exposes no OpenTelemetry port, and
scrapes nothing. A compatibility edge is always an operator choice, never a
port that appears because the binary contains the feature.

The home profile runs all roles in one collector process or pod. Larger
installations scale intake, forwarder, and compatibility deployments
independently. They coordinate through Corndogs rather than pod-local state.

### 3.4 TallyOwl head

The head contains these independently scalable roles:

- **ingest gateway:** authenticates collector keys, checks scope and quotas,
  validates batches, writes accepted telemetry, and returns a batch receipt;
- **projector:** transforms raw envelopes into query-optimized typed segments
  and rollups;
- **query service:** implements bounded, authorized analytics queries;
- **control service:** manages workspaces, projects, environments, sources,
  keys, policies, retention, saved objects, and role bindings;
- **workflow workers:** claim Corndogs tasks for alerts, projection rebuilds,
  deletion, export, and maintenance;
- **dashboard host:** serves the browser application, performs LinkKeys RP
  login, owns secure sessions, and provides a same-origin CSIL carrier.

These may begin in one binary with role flags while retaining separate modules
and resource controls.

## 4. Protocol architecture

### 4.1 CSIL packages

The repository should have three composable specifications:

- `tallyowl-ingest.csil`: telemetry envelopes and browser and backend event
  operations intended for inclusion by applications, with RPC and Events
  routing surfaces over the same types;
- `tallyowl-collector.csil`: durable intake, batch transfer, receipts, policy
  sync, and collector health;
- `tallyowl-control.csil`: dashboard queries and administrative operations.

The entry specifications include shared rules from `csil/types/`.
Generated output lives under clearly named `generated/` directories and is
reproducible through one repository command.

An application that includes the ingest specification gets a TallyOwl service in its
existing generated router. The application composes that router with its other
services and keeps its existing carrier, whether unary RPC or persistent Events.
The hand-written package accepts the host's transport seam; it never constructs
a browser transport.

### 4.2 Transport use

| Hop | CSIL mode | Carrier | Authentication |
| --- | --- | --- | --- |
| Instrumented browser → its app | RPC or Events, matching the host app | Existing same-origin browser carrier | Existing app session |
| App backend → collector | Pipelined RPC with one durable receipt per batch | One persistent TLS over TCP connection | Workload identity or local scoped key |
| Prometheus and OpenMetrics target → collector | Compatibility scrape | HTTP inside configured trust boundary | Target or network policy as configured |
| OpenTelemetry exporter → collector | Compatibility receiver | Supported OTLP carrier | Receiver credential or workload identity |
| Collector → head | RPC batch submit and policy fetch | TLS over TCP | TallyOwl source API key; optional mTLS |
| Dashboard browser → head | RPC or Events | Same-origin browser carrier | TallyOwl session from LinkKeys login |
| Head workers → Corndogs | RPC | TLS over TCP | Deployment secret or workload identity |

RPC gives a typed receipt at a durability boundary. Use Events for the existing
browser stream and intentional best-effort intake.

### 4.3 Compatibility

- Every envelope carries a schema version and producer library version.
- CSIL field additions are optional until all supported readers accept them.
- Assign each wire ID one time. Do not use the ID for a different field.
- Apply explicit compatibility rules to unknown optional fields.
- Quarantine an unknown required variant. Do not infer its meaning.
- Collector and head negotiate batch format and compression capability.
- The head supports at least the current and previous protocol versions during
  rolling upgrades.
- Golden CBOR vectors cover every envelope and receipt in every maintained
  language.

Current csilgen capabilities are sufficient for this design. The current design
does not need a csilgen change request.

## 5. Native storage architecture

TallyOwl does not require an external analytics database. It owns a small
storage engine built from three simple pieces:

1. a checksummed append log is the immediate final-storage durability boundary;
2. bounded immutable telemetry segments hold sealed data in an open,
   column-oriented representation;
3. an embedded transactional key-value catalog holds manifests, batch receipts,
   control state, tombstones, and small indexes.

All accepted telemetry shares this format. Typed event, error, span, metric,
identity, and campaign segments retain the stable IDs needed for correlation.
High-cardinality exact correlation is a primary storage workload. Derived
rollups are also segments. Retained source segments can reproduce them.

The single-node home-lab mode embeds storage in the TallyOwl head process and
uses one data directory. It needs no external database or storage service. A
cluster uses the same on-disk format: projects map to virtual shards grouped
into tablets. Each tablet has one write leader. The tablet satisfies its
configured receipt policy before it acknowledges a write.

Sealed segments flow to configured voters and optional read replicas.

Deletes publish tombstones immediately so queries stop returning matching rows.
Background compaction rewrites only segments that contain deleted or superseded
rows. It never recreates a logical table.

Workers remove a fully covered segment after all safety periods end.

TallyOwl Segment is a compact native page format; Arrow and Parquet are not
dependencies of the always-on storage and query path. Parquet is an optional export
contract, so a DuckDB user can still query exported data without running
TallyOwl. The catalog is rebuildable from self-describing, checksummed segment
manifests, so an embedded KV implementation is replaceable and never becomes the
only description of stored telemetry.

CSIL CBOR is the interchange and WAL representation. The projector writes
committed data to typed native columns and indexes.

The projector verifies the segment. TallyOwl does not keep a second indefinite
CBOR copy by default.

Recent hot data and normal warm data remain on local storage according to byte,
age, and disk-watermark policy. If configured, older immutable segment bundles
move to object storage while their manifests, routing summaries, and useful
index metadata remain queryable. The same query plan spans local and cold
segments and uses bounded local caches.

Object storage is optional. The home profile does not require it.

Queries snapshot a manifest, prune tablets and segments by
project, kind, time, and statistics, scan only required native pages, apply tombstones,
and merge partial aggregates. Large installations distribute that plan to read
replicas; small installations run the same plan in-process.

The full write path, directory format, crash recovery, deletion, replication,
query, backup, and export design is in [STORAGE.md](STORAGE.md).

Multi-cluster collection still needs only TLS over TCP reachability to the head.
Storage nodes do not need to share a Kubernetes cluster with collectors.

The full cell hierarchy, growth sequence, and failure model are in
[CELLS.md](CELLS.md).

## 6. Identity, tenancy, and authorization

Hierarchy:

```text
TallyOwl installation
└── workspace
    ├── members and role bindings
    └── project
        ├── environments
        ├── sources and collectors
        ├── active API keys
        ├── collection policy
        └── saved analytics objects
```

LinkKeys authenticates a human and returns verified identity facts. TallyOwl
then creates its own short-lived secure session and maps the stable
`UUID@domain` identity to workspace memberships. LinkKeys claims may inform
role assignment only through explicit installation policy; arbitrary claims do
not automatically grant access.

Initial roles:

- installation administrator;
- workspace owner;
- workspace administrator;
- analyst and editor;
- viewer;
- project ingester (machine only).

The service authorizes each query and change against workspace and project
scope.

### Tenancy resolution

The workspace is the tenancy boundary. The project identifies the application.

An instrumented application does not know about TallyOwl workspaces. Its
credential has project scope. TallyOwl resolves the rest on the server side.
See D32.

```text
app driver credential            project scope
  → collector resolves the project ID and its workspace ID (first batch only)
    → collector holds that mapping in memory
      → collector stamps workspace ID, project ID, and its properties
        → head verifies the destination is in the collector's project set
```

Rules:

- the service ignores a workspace or project ID that arrives in a payload;
- the collector stamps tenancy; a client cannot assert it;
- a collector-to-head key identifies the collector, not one project;
- an audit record stores the effective workspace and project at ingest time,
  because the key alone does not identify them;
- a change to a project-to-workspace mapping is an audited control operation;
- key rotation does not change a tenancy mapping.

The mapping is stable, so the in-memory cache needs no expiry. A project ID and
a workspace ID never change.

Revocation is separate. A revoked credential fails at the next batch. The
tenancy cache must not keep a revoked credential alive.

TallyOwl resolves IDs only. A project name and a workspace name are display
properties for a person. A name never travels on the ingest path, and a name
never takes part in routing or authorization.

### Properties and origin

One typed property namespace carries every descriptive value. A property is not
tenancy and it grants no access. D20 gives the types, access classes, and
limits. D38 gives the reasoning.

Each property records where it came from:

| Origin | Set by | Example |
| --- | --- | --- |
| `client` | The calling code, at the event site | `cart_value` = `42.10` |
| `driver` | The app driver, from its configuration | `env` = `prod` |
| `collector` | The collector, from operator configuration | `region` = `us-west2` |

A key-name access control list gives the names that an application cannot set.
The collector refuses a client value for a protected name and counts the
refusal. An operator can therefore trust a property that names a `collector`
origin.

A property never selects a destination. A query can filter on origin.

Two applications correlate their work with shared request and trace IDs. They
do not need a shared workspace. A query that crosses projects needs an explicit
identity policy.

### Collector API keys

- A source may have multiple simultaneously active keys.
- Keys have an ID and prefix for lookup and a random secret shown once.
- TallyOwl stores only a slow or keyed hash.
- TallyOwl does not write a plaintext key to a log.
- Each key has a source set, an operation scope, and an optional environment
  scope. The control catalog maps a source to its workspace and project.
- Keys have creation, optional expiry, last-used, and revoked timestamps.
- Rotation is create → deploy alongside old → observe use → revoke old.
- The head enforces revocation at the next batch.
- A long authorization cache must not hide revocation.
- Rate-limit and audit records identify the key ID, never the secret.

## 7. Configuration and control

TallyOwl versions collection policy and applies scope inheritance:

```text
installation defaults
  → workspace overrides
    → project overrides
      → environment overrides
        → source overrides
```

A compiled policy snapshot includes:

- enabled telemetry kinds;
- head sampling rules;
- tail sampling rules and the decision window;
- error and transaction keep rules;
- property allow and deny and redaction rules;
- maximum event, batch, and attribute sizes;
- approved indexed attributes;
- metric label budgets;
- session and campaign settings;
- privacy and consent behavior;
- retention classes and priority;
- emergency kill switches.

TallyOwl applies head sampling at the collector and tail sampling at the head.

Tail sampling needs a complete trace before it decides. A span routes by its
trace ID, so one tablet already holds every span of one trace. The projector
therefore makes the decision after commit, and no component buffers a trace
across collectors.

Spans wait in a provisional retention class until the decision window closes. A
kept trace moves to its normal retention class. A dropped trace gets a
tombstone, and compaction reclaims the space.

Always-keep rules run at the collector. An unhandled error or a critical
business event survives even when the tail rules later drop its trace. See D35.

Collectors get policy over CSIL-RPC. They cache the last valid version and
report the active version.

The head authenticates a policy update and activates it atomically. Invalid
policy never replaces valid policy.

## 8. Dashboard and query surface

The dashboard initially needs:

- installation, workspace, and project administration;
- live ingest health, drops, queues, lag, retries, and policy versions;
- event explorer with saved filters and breakdowns;
- error groups, occurrences, releases, regressions, and trace context;
- trace waterfall and service and operation latency;
- metrics charts with counter rates, gauges, histogram quantiles, and exemplars;
- funnels, paths, retention, cohorts, and end-user timelines;
- campaigns, attribution models, conversions, revenue, and cost imports;
- dashboards, saved queries, alerts, and notification history.

All data queries use the typed CSIL query algebra in [QUERY.md](QUERY.md). The
algebra constrains each query to these parts:
dimensions, measures, filters, grouping, interval, comparison period, ordering,
and bounded pagination. TallyOwl does not expose arbitrary SQL to the browser.
Operators who need unrestricted offline analysis can export Parquet and use
DuckDB or another independent tool without coupling that work to live ingest.

## 9. Operational behavior

TallyOwl observes itself with the same metric model. It uses a distinct
internal project and a recursion guard.

Every service also exposes the same instruments in Prometheus and OpenMetrics
format. Therefore an existing Prometheus and Grafana installation can monitor
TallyOwl. That installation does not first depend on TallyOwl health.

Required service indicators include:

- intake requests, bytes, validation failures, and auth failures;
- durable enqueue latency and failures;
- collector queue depth, oldest task age, disk use, retries, and quarantine;
- head batch commit latency and dedup hits;
- projector lag and failures;
- query latency, scanned bytes, timeouts, and concurrency rejection;
- storage WAL, fsync and quorum latency, segment and compaction pressure, capacity,
  tablet leadership, and replica lag;
- alert evaluation delay and notification failures.

Readiness must fail when a role cannot safely do its job. Ingest readiness fails
if the process cannot reach its durable store; it must not accept and discard.

## 10. Security and privacy baseline

- TLS on traffic crossing a pod trust boundary unless an explicitly documented
  service mesh provides it.
- Strict maximum frame size before allocation and decompression ratio limits.
- Per-source quotas and fair scheduling prevent one app from exhausting a
  collector or workspace.
- Client values are data, never trusted routing or authorization facts.
- Server SDKs capture no headers, bodies, cookies, tokens, SQL parameters, or
  LinkKeys claim values by default.
- The collector normalizes and scrubs error stack frames before storage.
- TallyOwl does not retain IP addresses by default.
- If enabled, the collector derives coarse geography and discards the source
  address.
- TallyOwl prohibits direct personal data by default.
- TallyOwl permits a stable end-user ID in project or workspace scope.
- An exact index supports end-user timelines and erasure.
- Cross-project links need an explicit identity policy.
- Consent state travels with applicable browser events.
- Drop a disallowed event before durable enqueue when possible.
- The deletion workflow uses tombstones and corrects derived data.
- The workflow also writes an audit completion record.
- Audit records cover login, membership, key, policy, retention, export, and
  deletion changes without copying sensitive payloads.

### Dashboard security

The dashboard is the largest attack surface, because it is the only surface a
person reaches with a browser. The bounded query model is not sufficient by
itself.

- The dashboard carrier is same-origin, but the same-origin policy does not
  protect a WebSocket. The server must verify the `Origin` header on every
  carrier handshake and refuse an unknown origin.
- Session cookies use `Secure`, `HttpOnly`, and `SameSite`.
- A state-changing operation needs a token that a cross-site page cannot read.
- A Content Security Policy restricts script sources and refuses inline script.
- The dashboard sets a frame policy that prevents click interception.
- The service authorizes each query operation. It does not authorize only at
  login.
- A query result never contains data from a project outside the caller's
  scope, including in an error message or a diagnostic count.

### Node enrollment and transport identity

The control plane owns an installation certificate authority (CA). A large
installation can use an offline root CA and an online intermediate CA. The home
profile can use one installation CA.

A new node:

1. generates its private key locally;
2. authenticates the controller with a configured CA or certificate
   fingerprint;
3. sends a role token and a certificate request;
4. receives a short-life certificate for a permitted role.

The role token is a reusable API credential when its policy permits reuse. The
policy controls roles, cells, regions, projects, expiration, use count, and
network restrictions.

TallyOwl stores only a hash of the role token. TallyOwl permits multiple
active tokens for safe rotation. All token use is in the audit record.

The node uses mutual Transport Layer Security (mTLS) after enrollment. The node
uses its current certificate to get a replacement certificate.

A role token can enroll a stateless node without a manual operation. A storage
role token can enroll a storage process. The controller controls tablet
placement.

A role token cannot add a controller voter. It cannot change a tablet voter
set. These changes need controller-quorum approval.

The initial certificate lifetime is 24 hours. Renewal starts after 8 hours.
Both values are configurable.

Only the controller signing role can use the signing key. The public dashboard
process cannot use it.

The full token, enrollment, negotiation, and certificate design is in
[NODE_IDENTITY.md](NODE_IDENTITY.md).

## 11. Implementation shape

Current direction:

- Rust workspace for collector, head services, shared hand-written SDK core,
  storage nodes, and CSIL-generated Rust code;
- maintained Go and Rust app drivers;
- TypeScript dashboard and browser package;
- native TallyOwl append and segment storage with an embedded transactional KV
  catalog;
- Corndogs deployments for collector delivery queues and head workflows;
- one head Helm chart with role-specific deployments and optional development
  dependencies;
- one collector Helm chart, able to reference an existing Corndogs service or
  install a tightly scoped local instance.

Protocol and data boundaries are more important than implementation language.
Change the language now if another language decreases driver cost.

Longhouse (Go) and Ichoi (Rust) are the first pilot applications. Their
todandlorna.com deployments provide the initial dogfood path.

### Deployment profiles

The charts expose profiles without creating different products or formats:

- **home:** one head and storage pod with one persistent volume;
- **home collector:** one collector with one durable Corndogs copy;
- **home limits:** no read replica, cold object store, or separate projector;
- **replicated:** one regional cell, three tablet voters, stateless gateways,
  and separate workflow workers;
- **scaled:** multiple regional cells, a global directory, read replicas, export
  replicas, and role-specific resource pools.

The home profile is the default. Resource budgets control background work.

A profile change changes placement and replication. It does not change stored
data or app integration.

## 12. CI/CD

TallyOwl uses Reactorcide. Trusted job definitions live in
`.reactorcide/jobs/`; pipeline code lives separately from application code when
untrusted contribution execution requires that boundary.

Pipeline orchestration uses runnerlib's workflow context, change detection,
dependencies, conditions, matrix expansion, outputs, and secret masking. Jobs
run Python entry modules rather than Bash scripts. Python invokes compilers,
test runners, generators, Helm, and image tools with explicit argument arrays
and `shell=False`.

[CI-CD.md](CI-CD.md) gives the pipeline and trust boundaries.

## 13. Verification

Unit tests prove that one component obeys its contract. They do not prove that
a product produces correct analytics.

The project therefore maintains a reference application and an integration test
bed. The reference application has a marketing site, a web application, a
backend, a rich client, and a mobile client. A simulator writes an expected
result ledger before it sends data. The tests compare TallyOwl query results
with that ledger.

A second application in the same installation proves multi-tenant isolation
with real traffic.

[TESTBED.md](TESTBED.md) gives the reference application and test bed design.
