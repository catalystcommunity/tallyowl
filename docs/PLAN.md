# Implementation plan

This plan gives a build order. It does not give a release schedule. The project
implements the system and then versions it with semantic versioning and
conventional commits, as its sibling repositories do.

Use gates for this plan. Do not start a later phase before approval of the
current contracts and failure tests. Build thin vertical slices before you
build all telemetry types.

Phase 1 is the local development loop, and it comes before every other build
task. The loop is the home profile rather than a development arrangement, so
configuration, health, logging, and the delivery path all take their final
shape on the first day. Every later phase then adds to a working system instead
of retrofitting one. See Phase 1 for why each of those four is load bearing.

The phase order puts the distributed storage work after errors, traces, and
metrics. The first consuming applications need correlated telemetry before they
need a replicated cluster. The storage format does not change between the two,
so this order costs nothing and removes a large risk from the critical path.

Each phase also grows the reference application. See [TESTBED.md](TESTBED.md).

## Phase 0 — approve architecture decisions

Deliverables:

- confirm Longhouse (Go) and Ichoi (Rust) as the first consuming applications;
- define the reference application scenarios and the ledger contract;
- capacity sketch for event, metric, and span rates, retention, and outage
  window, which the reference application later measures;
- threat model and privacy defaults, in [THREAT_MODEL.md](THREAT_MODEL.md);
- release and version policy (license is Apache-2.0);
- architecture decision records for storage, runtime, and durability.

Every decision in [DECISIONS.md](DECISIONS.md) has a status. The remaining
items are prototypes and measurements, not approvals.

Exit criteria:

- one documented end-to-end path from browser event to dashboard query;
- one documented durable backend event path;
- agreed native storage format, embedded topology, and clustered durability
  model;
- agreed delivery contract;
- stated durability for each receipt policy, infrastructure requirements for
  each profile, and convergence behaviour for a multi-region installation. D53
  replaces recovery objectives with those three.

## Phase 1 — the local development loop

**Nothing else starts until a developer can run, configure, and debug the
system on a workstation.** Every later phase adds to this loop. A phase that
builds a service before the loop exists pays to retrofit it.

### The principle: the development loop is the home profile

There is no development mode. A developer runs the `home` profile from
[DEPLOYMENT.md](DEPLOYMENT.md) section 3: one head, one collector, and one
Corndogs. That is the smallest supported production deployment, so the
workstation topology and the smallest real installation are the same topology.

Consequences, and they are the point:

- no `if development` branch anywhere in a service;
- no mock, no stub transport, and no in-process shortcut between the collector
  and the head;
- a setting that a developer changes locally is the same setting an operator
  changes in a chart, under the same name;
- a bug that appears in the home profile appears on a workstation.

Containers are not required. The services are binaries, so the loop runs
binaries. The container path exists for parity checking and for people who
prefer it. It is never the fast path.

### Configuration, designed once and for the product

Configuration is the part of this phase that prevents the most later work.
[CONVENTIONS.md](CONVENTIONS.md) section 5 gives the precedence. This phase
builds the loader that every binary uses.

**One name for one setting, in every place it appears.** A setting has one key
path, and that path is identical in the configuration file, the environment
variable, the command-line flag, and the Helm value:

| Place | Form |
| --- | --- |
| Helm value | `storage.receiptPolicy` |
| Configuration file | `storage: { receiptPolicy: ... }` |
| Environment variable | `TALLYOWL_STORAGE__RECEIPT_POLICY` |
| Command-line flag | `--storage.receipt-policy` |

A section separator becomes a double underscore in an environment variable, so
a key that holds an underscore stays unambiguous.

The configuration file is YAML, and its tree is the same tree as the chart
values. A rendered chart and a local file are then the same document, and a
developer reading DEPLOYMENT.md section 4 needs no translation.

Rules this phase implements:

- **Validate everything at startup and refuse to start on a bad value.** A
  service that starts with bad configuration fails later, in production, where
  nobody connects the failure to the setting.
- **A secret is a reference, never a value.** The file holds `file:`, `env:`,
  or a secret-store reference. The loader resolves it at startup, logs that it
  resolved and from where, and never logs the value.
- **No `.env` file in the binary workflow.** A `.env` file is a container
  convention, and it hides precedence. The binary workflow uses a
  configuration file plus real environment variables.
- The container and compose workflow may use one. It reads `.local.env`, which
  is ignored by Git. `.local.example.env` is committed, holds no secret, and
  documents every key.
- Every value has a default that is safe for a home installation, so an empty
  configuration file starts a working system.

**`tallyowl config check` exists on the first day.** It resolves the full
configuration from every source, reports which source won for each value,
masks every secret, and exits non-zero on an invalid value. This one command
removes more debugging time than any other in this phase.

### The walking skeleton

Deliberately small, and real at every boundary that matters:

- a `tallyowl-collector` binary that serves `TallyOwlIngest` over CSIL-RPC and
  writes each accepted batch to Corndogs;
- a `tallyowl-head` binary that drains that Corndogs queue, writes to a
  directory, and answers one query operation;
- a Rust driver that sends one event;
- both binaries expose live and ready per CONVENTIONS.md section 3, log per
  section 4, and expose metrics per section 6.

Storage is a stub in this phase. The **path** is not: accept, durable queue,
drain, commit, receipt. That is the DELIVERY.md path, and building it now means
Phase 3 replaces a stub instead of introducing a boundary.

### The task runner

`tools.sh` is a thin front door. It calls the Python tooling, which uses
Reactorcide's runnerlib event lifecycle so a local run and a CI run execute the
same code. Assume `uv` provides Python. Pin csilgen to a released version.

Commands this phase delivers:

| Command | Does |
| --- | --- |
| `./tools.sh setup` | Fetches toolchains, generates from `csil/`, writes a local configuration file from the example |
| `./tools.sh gen` | Generates from `csil/` |
| `./tools.sh build` | Builds every service |
| `./tools.sh test` | Runs every test |
| `./tools.sh dev up` | Starts Corndogs, the head, and the collector, and follows their logs |
| `./tools.sh dev up --without head` | Starts the rest, so a debugger owns the head |
| `./tools.sh dev down` | Stops them and leaves the data directory |
| `./tools.sh dev reset` | Stops them and removes the data directory |
| `./tools.sh config check` | Runs `config check` against the local configuration |

`--without` is the debug loop. A developer runs two services from the task
runner and the third in a debugger, with no container indirection and no
attach dance.

Supervision lives in the task runner, never in a service. A service that knows
how to start its siblings has a development code path, which this phase exists
to prevent.

### Parity, enforced by a test

A rendered chart and a local configuration drift the moment somebody adds a
setting to one of them. A test therefore renders the `home` profile and asserts
that the chart values and the loader agree on every key, every type, and every
required value. CI fails on a mismatch.

This test is the reason the loop keeps matching production instead of slowly
becoming a development-only arrangement.

Deliverables:

- workspace and package layout;
- the configuration loader, with precedence, validation, secret references,
  and `config check`;
- structured safe logging, the error taxonomy, and the health conventions from
  CONVENTIONS.md, as shared code rather than as a convention that each service
  reimplements;
- the two walking-skeleton binaries and the Rust driver;
- `tools.sh` and the runnerlib pipeline entry points, with no Bash build, test,
  or deploy wrapper;
- the `home` chart skeleton and its values, enough for the parity test;
- an optional compose file, `.local.example.env`, and a `.local.env` that Git
  ignores;
- `CONTRIBUTING.md`, written against the commands above.

Suggested layout:

```text
csil/
  types/
  tallyowl-ingest.csil
  tallyowl-collector.csil
  tallyowl-control.csil
crates/
  tallyowl-types/
  tallyowl-config/
  tallyowl-obs/
  tallyowl-collector/
  tallyowl-head/
  tallyowl-store/
  tallyowl-driver-rust/
packages/
  driver-go/
  browser/
  dashboard/
generated/
charts/
  tallyowl/
  collector/
tools/
testbed/
prototypes/
docs/
```

Exit criteria:

- a clean clone reaches a working event round trip with `./tools.sh setup` and
  `./tools.sh dev up`, and nothing else;
- an event sent by the driver reaches the head through Corndogs and a query
  returns it;
- any one of the three processes runs under a debugger while the other two run
  normally;
- `config check` reports the winning source for every value and masks every
  secret;
- a bad configuration value stops startup and names the setting, the value, and
  a valid example;
- readiness fails when Corndogs is unreachable, and the failure text names the
  durable store in the language of CONVENTIONS.md section 1;
- the chart and loader parity test passes, and fails when a setting is added to
  only one of them;
- no secret and no personal data appears in any log at any level;
- Git ignores every local configuration, data directory, and environment file.

## Phase 2 — contract foundation

Phase 1 generates from `csil/` and uses the result. This phase makes the
contract trustworthy across languages.

Deliverables:

- generation from the written `csil/` specifications, which already validate
  and generate for Rust, Go, and TypeScript;
- package-mode clients;
- generated-code drift check in CI;
- golden CBOR vectors and cross-language round-trip tests;
- the Go and TypeScript drivers reaching the Phase 1 collector.

Exit criteria:

- every maintained language encodes each golden vector to identical bytes;
- no generated file is hand-maintained;
- a fake host can route a typed browser event over an existing multiplexed
  carrier into a fake collector;
- the reference application skeleton builds and its ledger writer runs.

## Phase 3 — embedded storage foundation

Deliverables:

- the segment and manifest format in [SEGMENT_FORMAT.md](SEGMENT_FORMAT.md);
- compact native column pages and typed decode and query buffers with no
  Arrow and Parquet dependency in the always-on path;
- typed sparse dynamic columns plus exact high-cardinality locator and posting
  indexes;
- checksummed framed WAL with durable receipts and crash recovery;
- embedded transactional KV catalog spike and selection;
- bounded segment writer and manifest generations;
- raw and typed event segments;
- tombstones and affected-segment compaction;
- manifest snapshot query with time, project, and kind pruning;
- exact request, trace, span, session, end user, and custom-property lookup;
- optional hot and warm to cold object-storage tiering with bounded local cache;
- optional Parquet exporter and direct DuckDB verification;
- snapshot, restore, and catalog rebuild command;
- per-end-user key material and cryptographic erasure for the cold tier;
- storage and capacity metrics through native and Prometheus and OpenMetrics paths.

Failure work, from [FAILURE_MODES.md](FAILURE_MODES.md):

- torn and reordered writes;
- crash before and after every fsync and catalog boundary;
- orphan, partial, and corrupt segments;
- commit success and receipt loss retry;
- tombstone visibility during concurrent reads;
- disk full at each point in FAILURE_MODES.md section 10;
- format upgrade and downgrade refusal;
- the three integrity levels in D57, and a damaged segment producing
  `incomplete-result` that names it;
- generation pinning and the compaction garbage-collection grace period;
- an erasure that lands mid-compaction, proving no resurrection;
- a crash between an erasure commit and its acknowledgement;
- a catalog rebuild with catalog snapshots and without, each asserting exactly
  what FAILURE_MODES.md section 7 says survives.

Exit criteria:

- one binary runs the head, query service, and durable store from one directory;
- one binary requires no external database;
- accepted data survives abrupt process and host restart;
- a point or end user deletion disappears immediately, rewrites only intersecting
  bounded local segments, and erases cold data by key destruction;
- a read of cold data after key destruction fails and cannot recover the value;
- a tombstone hides a matching event that arrives after the erasure request;
- mostly-unique request IDs remain exactly retrievable without scanning every
  retained segment;
- an interrupted cold-tier upload never evicts the only valid segment copy;
- a clean Parquet export is queryable in DuckDB.

## Phase 4 — durable event vertical slice

Deliverables:

- persistent, reconnecting, pipelined app-to-collector CSIL over TCP intake;
- collector ack emitted only after Corndogs reaches the configured total
  durable-copy count, defaulting to one;
- independently selectable collector `intake`, `forwarder`, and
  `compatibility-receiver` roles, with all roles available in one small process;
- scoped source API key model with overlapping active keys;
- reusable role tokens for automated stateless-node enrollment;
- node-generated keys and short-life mTLS certificates;
- collector Corndogs queues and delivery worker;
- timeout sweep owned by the forwarder role, with readiness tied to it;
- backoff through the Corndogs timeout and state swap;
- source-to-tenancy resolution at collector intake;
- batch format, compression, receipt, partial rejection, and retry;
- head ingest gateway;
- embedded raw segments and project and control catalog;
- canonical generic event projection;
- event trend and breakdown query;
- minimal LinkKeys login and workspace and project authorization;
- dashboard showing ingest health and an event chart.

Failure work:

- commit and receipt loss;
- collector and worker process kills;
- head outage and recovery;
- queue and disk limit;
- duplicate projection correctness;
- invalid schema quarantine;
- key rotation and revocation;
- stopped timeout sweep;
- a `durable_copies` value that the configured Corndogs backend cannot satisfy.

Exit criteria:

- a browser event travels through an application's existing connection;
- the app never knows the head address;
- the app never sends a workspace or project ID;
- a durable backend event survives collector and head restart and appears once
  logically in a query;
- an unload flush reaches the collector through the application;
- tenant isolation tests attempt cross-workspace ingest and query;
- the reference application backend and web application produce events that
  match the ledger exactly.

Test the vertical slice first through Longhouse with the Go app driver. Also
test it through Ichoi with the Rust app driver.

## Phase 5 — errors and traces

Deliverables:

- browser and backend error capture and scrubber;
- trace and span SDK core and context propagation;
- error occurrence and versioned grouping projectors;
- release tracking and regression detection;
- trace search and waterfall and error-to-trace linking;
- head sampling policy and always-keep rules for critical errors;
- head-side tail sampling: provisional retention class, decision window,
  projector decision, and tombstone for a dropped trace;
- late-span grace period and the rule that a late span cannot change an applied
  decision;
- source map and debug-symbol workflow if the first consumers require it.

Exit criteria:

- one frontend error and one backend error correlate to a trace, session, and release;
- the projector rebuilds grouping at a new version from raw data;
- sensitive-data fixtures prove default scrubbing;
- a tail-sampled trace keeps every one of its spans, and a dropped trace leaves
  no queryable span;
- an always-keep error survives when the tail rules drop its trace;
- the reference application produces browser, backend, and rich-client errors
  that match the ledger.

## Phase 6 — metrics and compatibility receivers

Deliverables:

- counter, gauge, histogram, and exemplar APIs in first backend SDKs;
- in-process aggregation and snapshot push;
- collector merge plus visible series byte and work budgets without silently
  rejecting high cardinality;
- Prometheus and OpenMetrics scrape receiver;
- OpenTelemetry metric and trace push receiver with immediate native normalization
  and explicit rejection of log ingestion;
- Prometheus and OpenMetrics exporter for collector, head, dashboard, query, and
  storage metrics;
- recursion-protected TallyOwl internal project for dogfooding self-metrics;
- metric projection, rollups, query operations, and charting;
- service-operation golden signals derived from spans where appropriate;
- downsampling and retention tiers.

Exit criteria:

- restart and reset semantics for cumulative counters are correct;
- histogram merge and quantile tests cover incompatible buckets;
- high-cardinality series keep correct values under configured resource limits;
- exhausted limits cause explicit backpressure.

## Phase 7 — replicated storage proof

This phase begins when an installation needs more than one storage node. The
segment format does not change here. A home installation never enters this
phase.

Failure work, from [FAILURE_MODES.md](FAILURE_MODES.md) section 6:

- a node that is alive but slow, asserting detection, the reported cause, and
  `unknown` when no cause can be established;
- quorum lost permanently, recovered by restore;
- quorum lost permanently, recovered by unsafe recovery, asserting the audit
  record, the degraded mark, and the mark reaching query and explain output;
- a voter with an exhausted disk, and quorum continuing without it.

Deliverables:

- three-or-five-voter controller quorum for topology and configuration only;
- regional cell control and a small global project-to-cell directory;
- virtual-shard → tablet mapping, placement, split, merge, and movement;
- existing Rust consensus implementation integrated as multiplexed,
  three-voter tablet groups, selected from `openraft` and `raft-rs`;
- write replication with quorum receipts, without a cluster-wide storage-node
  consensus group;
- non-voting read and export replicas;
- distributed query planning and partial aggregation for the generic event
  slice;
- correctness-first query failure and partial-result semantics and exact
  high-cardinality lookup across tablets;
- online replica addition and removal and tablet movement;
- one write region for each tablet and fenced regional failover;
- configurable `local-quorum` and `remote-one` receipts, and refusal of
  `local-one` on a multi-voter tablet;
- replicated tombstone and compaction generation safety;
- cluster snapshot, bootstrap, and restore;
- failure-domain-aware Helm placement and disruption rules.

Exit criteria:

- acknowledged batches survive the agreed number of storage-node losses;
- leader loss during write returns either a prior receipt or one logical retry;
- read replicas expose exact freshness and bounded-stale behavior;
- tablet movement completes online with checksummed parity;
- a multi-node topology test proves that only controllers and each tablet's
  replica set participate in their respective consensus groups;
- existing cells continue data operations during a global-directory outage;
- a reusable role token enrolls permitted nodes but cannot change a voter set;
- the reference application runs unchanged against the replicated installation
  and produces identical ledger results.

Scale simulations are a milestone, not a gate. A 400-node cell simulation and a
10,000-node multi-cell simulation must show bounded controller and global
directory work. Do not block the phase on hardware that the project does not
have.

## Phase 8 — product behavior

Deliverables:

- sessions, identify, alias, group association, and traits;
- end user and session timelines;
- per-user erasure across detailed data, derived state, local segments, cold
  objects, and caches;
- funnel, retention, path, and cohort query operations;
- late event and identity-merge semantics;
- saved analyses and dashboard composition;
- collection policy UI for event and property filtering.

Exit criteria:

- anonymous-to-known conversion works without cross-project leakage;
- funnel and retention fixtures have explainable exact results;
- query cost guards reject pathological analysis safely;
- the reference application end user uses the web, rich, and mobile clients, and
  the resulting funnel, retention, and timeline results match the ledger.

## Phase 9 — campaigns and business outcomes

Deliverables:

- campaign and referrer capture and classification;
- touchpoint, conversion, exact value and currency, and cost import types;
- versioned attribution models and windows;
- campaign, conversion, value, cost, and return dashboards;
- consent-aware collection and attribution behavior;
- idempotent conversion and order handling.

Exit criteria:

- first, last, non-direct, linear, position, and decay fixtures are stable;
- model changes recompute from immutable facts;
- missing consent excludes data according to policy;
- every attribution model matches the ledger for traffic that arrives from the
  reference marketing site landing pages.

## Phase 10 — alerts and workflows

Deliverables:

- scheduled metric, query, and error alert definitions;
- Corndogs evaluation and notification workflows;
- deduplicated alert instances, silence, resolve, and escalation state;
- signed webhook and native CSIL callback notification channels;
- projector rebuild, retention, deletion, and export workflows;
- operator UI for workflow lag, failure, and quarantine.

Exit criteria:

- repeated evaluations do not duplicate notifications;
- retries survive worker restart;
- deletion tombstones prevent replay resurrection.

## Phase 11 — production hardening

Deliverables:

- backup and restore and disaster-recovery drills;
- cross-cluster collector soak tests;
- workload and resource isolation and autoscaling;
- upgrade and downgrade and schema migration drills;
- security review, dependency audit, fuzzing, and malformed-frame tests;
- performance tests at expected and overload rates;
- published Helm charts and generated client packages;
- Reactorcide release workflows with protected secret-bearing publish and deploy
  jobs and locally runnable validation jobs;
- operations, integration, privacy, and incident runbooks.

Exit criteria:

- recovery objectives demonstrated, not merely documented;
- a collector holds the agreed outage window without data corruption;
- rolling upgrades maintain adjacent-version clients;
- overload produces bounded latency, memory, and disk and visible drops and rejections.

## Cross-cutting test suites

Maintain from the first applicable phase:

- CSIL compatibility and golden bytes;
- generated-code reproducibility;
- multi-tenant authorization and source scoping;
- key rotation and revocation;
- crash and failure injection at durability boundaries;
- duplicate and late data;
- redaction and prohibited-field fixtures;
- cardinality and decompression bombs;
- query resource budgets;
- Helm render and upgrade tests;
- real storage integration tests, not mocks for storage semantics;
- branch coverage of every decision that is not operating-system logistics;
- the reference application suite, at the coverage the current phase permits.

## Client library rollout

Generation can produce many packages. Maintain an ergonomic driver after a
consumer demonstrates a need:

1. TypeScript browser package;
2. Go app driver for Longhouse;
3. Rust app driver for Ichoi;
4. mobile and native drivers when a concrete application needs them.

The reference application does not create that need. Its mobile and terminal
surfaces are behavior simulations. They prove that TallyOwl responds correctly
to those message patterns. They do not prove a driver, and they do not oblige
the project to maintain one. See [TESTBED.md](TESTBED.md).

Each driver must inject an existing carrier or router. It must expose explicit
best-effort and durable methods. Do not claim support from generated shapes
alone. First, implement buffering, context propagation, and metric aggregation
for that language.

The reference application uses each maintained driver. A driver without
reference-application coverage is not maintained.
