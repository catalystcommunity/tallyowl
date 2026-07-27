# Decisions to approve

This is the working decision register. Each decision has one status from this
list:

| Status | Meaning |
| --- | --- |
| Accepted | The project made this decision. Implement it. |
| Accepted pending benchmark | The direction is set. A measurement can change the values, not the shape. |
| Recommended | A proposal. The project owner must approve or refuse it. |
| Open | The project has not made this decision. |

Give each decision one status. If the project accepts only part of a decision,
make the unresolved part a separate decision.

## Decision index

| ID | Decision | Status |
| --- | --- | --- |
| D1 | One native storage system | Accepted |
| D2 | Primary implementation language | Accepted |
| D3 | Embedded KV and segment encoding | Accepted pending benchmark |
| D4 | Collector durability topology | Accepted |
| D5 | App-to-collector acknowledgement | Accepted |
| D6 | Initial pilots | Accepted |
| D7 | LinkKeys relying-party mode | Accepted |
| D8 | Tenant model | Accepted |
| D9 | Privacy defaults | Accepted |
| D10 | Capacity envelope | Accepted pending benchmark |
| D11 | Session replay and the session model | Accepted |
| D12 | Metrics compatibility and self-observability | Accepted |
| D13 | External notification channels | Accepted |
| D14 | Repository license | Accepted |
| D15 | Replication implementation | Accepted pending benchmark |
| D16 | Tablet sizing and project sub-sharding | Accepted |
| D17 | Native page compression and sizing | Accepted pending benchmark |
| D18 | Query consistency and coordinator behavior | Accepted pending benchmark |
| D19 | App batching and backpressure defaults | Accepted |
| D20 | Property schema and high-cardinality indexing | Accepted pending benchmark |
| D21 | Exact versus approximate analytics | Accepted |
| D22 | Node trust and role enrollment | Accepted |
| D23 | Home-profile resource budgets | Accepted pending benchmark |
| D24 | Hot, warm, and cold storage tiering | Accepted pending benchmark |
| D25 | Reusable high-cardinality store module | Accepted |
| D26 | Cell hierarchy | Accepted |
| D27 | Regional write ownership and receipt policy | Accepted |
| D28 | Erasure timing and cold-tier erasure | Accepted |
| D29 | Documentation language | Accepted |
| D30 | Consent behavior | Accepted |
| D31 | Package, registry, and support window | Accepted for now |
| D32 | Source identity and tenancy resolution | Accepted |
| D33 | Retry backoff and timeout sweeping | Accepted |
| D34 | Browser unload flush | Accepted |
| D35 | Tail sampling location | Accepted |
| D36 | Deduplication window and outage buffer | Accepted |
| D37 | Reference application test bed | Accepted |
| D38 | One property namespace | Accepted |
| D39 | Error grouping fingerprint | Accepted |
| D40 | Attribution configuration | Accepted |
| D41 | Alerting scope | Accepted |
| D42 | Grafana integration | Accepted |
| D43 | Retention classes | Accepted |
| D44 | Segment checksums and content addressing | Accepted |
| D45 | Tail sampling rule language | Accepted |
| D46 | Repository scope | Accepted |

## Next decision order

Every decision now has a status. No decision waits for an approval.

These prototype and measurement tasks remain:

1. Test D3, D17, D20, D24, and D25 with storage benchmarks.
2. Build the reference application, then measure D10 and D23 with it.
3. Fix the D36 deduplication window from the D10 outage buffer target.
4. Test D15, D16, D26, and D27 with cell and consensus prototypes.
5. Test D18 with distributed correctness fixtures.
6. Measure the D35 provisional retention cost at the selected decision window.
7. Design the D28 segment encryption keys before cold tiering carries erasable
   data.
8. Select the D40 attribution default values in Phase 8.
9. Reopen D31 at the release candidate and select the distribution coordinates.

These numeric defaults are first values for measurement. The reference
application confirms or changes each one. They are not recommendations.

| Value | First value | Decision |
| --- | --- | --- |
| Session maximum lifetime | 12 hours | D11 |
| Tail decision window | 60 seconds | D35 |
| Late-span grace period | 30 seconds | D35 |

## D1. One native storage system — Accepted

TallyOwl owns one native storage system. It uses a checksummed append log,
bounded immutable telemetry segments, and an embedded transactional KV catalog.
The same format runs embedded or on sharded replicated TallyOwl storage nodes.
TallyOwl does not require an external analytics or control database.

TallyOwl Segment is the native physical format; Arrow and Parquet are not core
storage and query dependencies. Parquet is an optional export so stored data remains
accessible to DuckDB and other independent tools. The embedded KV catalog is
rebuildable from segment manifests and versioned control snapshots.

Rationale for a native store instead of an external analytics database:

- the home profile must run with no external database and low idle cost;
- exact high-cardinality lookup and aggregate scan must share one format;
- immediate tombstone visibility and bounded physical erasure must reach every
  tier, including cold objects;
- the deletion, retention, and tiering model must not depend on another
  product's roadmap.

This is the largest single cost in the plan. Reverse this decision if two
conditions occur together:

- the storage benchmarks in [STORAGE.md](STORAGE.md) section 15 do not meet the
  capacity envelope in D10;
- an external engine meets that envelope and keeps the erasure and tiering
  contract.

See [STORAGE.md](STORAGE.md).

## D2. Primary implementation language — Accepted

Rust supports the collector, head, storage, query, and the Rust app driver.
TypeScript supports the browser package and the dashboard. The project
maintains the Go app driver.

## D3. Embedded KV and segment encoding — Accepted pending benchmark

**Recommended starting point:** benchmark `redb` as the catalog because its
copy-on-write B+tree provides the desired Bolt-like operational model. Compare a
write-oriented LSM option if receipt, control, and catalog workloads expose a real
bottleneck.

Use a compact native column-page format for the first typed segment
implementation. Start with simple fixed-width, varint and delta, offset and bytes,
optional dictionary, and independently compressed and checksummed pages. Arrow and
Parquet belong only to an optional exporter crate or process.

**Accepted:** CSIL CBOR is interchange and WAL data. It is not the final query
format.

The source CBOR expires after verified native segments cover its WAL range. An
operator can configure a short CBOR retention period.

The accepted writer defaults are in D17. Do not make the format stable until
all required storage tests pass. See [STORAGE.md](STORAGE.md).

## D4. Collector durability topology — Accepted

Corndogs is the single intermediate durable handoff. Once it accepts a telemetry
task, that task remains until final TallyOwl storage returns a committed
receipt. It need not remain after completion, and the collector does not keep a
second copy in another journal.

The operator configures `durable_copies`, the total number of persisted
Corndogs copies required before collector acknowledgement. It defaults to `1`:
one durable local copy with no redundancy. "Durable" means that TallyOwl has
the data at the configured boundary.

The copy count and failure-domain placement determine outage survival.

Corndogs' clustered file setting counts follower acknowledgements, so the chart
translates it as `ack_count = durable_copies - 1`. Single-node file storage must
use a Corndogs fsync mode that does not acknowledge lossy interval-flushed
writes. Use `group` or `always`; `interval` and `never` are not durable.

Current Corndogs support for this setting:

- The shipped file backend is single-replica. It supports `durable_copies = 1`
  only.
- The clustered file backend that supplies `ack_count` is a Corndogs design
  document. It is not implemented. `durable_copies > 1` is therefore not
  available yet.
- The postgres backend gives replica failover through PostgreSQL, not through
  `ack_count`. `durable_copies` does not apply to it. Its durability is
  "PostgreSQL committed".

TallyOwl must refuse a `durable_copies` value that its configured Corndogs
backend cannot satisfy. It must not accept the value and then acknowledge a
weaker guarantee.

## D5. App-to-collector acknowledgement — Accepted

The app backend keeps one persistent, reconnecting CSIL over TCP connection to the
collector and pipelines correlated batches. The collector acknowledges a batch
only after Corndogs durably accepts its task. The app retains the stable batch
until that acknowledgement, then moves on; the collector owns eventual final
delivery.

Browser telemetry may remain best effort from browser to app. A host may expose
a correlated critical-event operation if it wants the browser to wait for the
same collector durability boundary.

## D6. Initial pilots — Accepted

Longhouse on Go and Ichoi on Rust, beginning with their todandlorna.com
deployments. Together they prove:

- including the TallyOwl CSIL service;
- composing generated routing with its existing connection;
- attaching trusted backend identity and context;
- forwarding to a collector;
- querying one event and one error in the dashboard.

They establish Go and Rust as the first maintained app drivers.

## D7. LinkKeys relying-party mode — Accepted

TallyOwl's public dashboard domain acts as an ordinary domain-backed relying
party. This is the default mode. TallyOwl owns the resulting application
session and authorization.

The DNS-less local mode is a supported fallback for an installation with no
stable domain. It is not the default.

Both modes work in the first release. Do not defer one of them.

A LinkKeys claim can map to a workspace role. The claim must come from a
trusted domain, and that domain must sign it. An installation administrator
selects the trusted domains. That administrator also maps each claim to a
role.

An unsigned claim never maps to a role. A claim from a domain that the
installation does not trust never maps to a role.

## D8. Tenant model — Accepted

Use this hierarchy:

```text
installation → workspace → project → environment and source
```

A person can belong to many workspaces. A project can receive data from many
domains and clusters.

A workspace is an operator-chosen boundary. An operator can use a workspace for
a separate organization, or for a team, or for any other grouping that fits the
installation. TallyOwl does not decide that meaning.

Many workspaces can share one deployment when the operator wants that. The
operator makes this choice, not TallyOwl.

TallyOwl does not add per-customer billing, usage accounting, or commercial
isolation features. An installation that hosts separate outside customers uses
separate deployments.

Tenant isolation must still be correct. The reference application proves it
with a second application in the same installation. See
[TESTBED.md](TESTBED.md).

## D9. Privacy defaults — Accepted

Accepted defaults:

- no raw IP retention;
- no request and response bodies, headers, cookies, query values, form values, or
  LinkKeys claim values;
- no automatic cross-project actor identity;
- permit opaque project and workspace actor IDs;
- use an exact index for actor IDs and erasure keys;
- TallyOwl stores the actor ID that the app supplies;
- an app can transform an actor ID before it sends the ID;
- campaign parameters only with configured consent behavior;
- semantic interactions only, no session replay;
- permit typed high-cardinality custom properties within resource limits;
- visible per-field redaction and drop counters.

The project accepts the direct-personal-data prohibition and the actor-ID
exception. D30 holds the unresolved jurisdiction and consent behavior.

## D10. Capacity envelope — Accepted pending benchmark

The product must cover both:

- an idle-friendly single-node home lab serving a dozen or more applications;
- a sharded replicated installation ingesting millions of events per minute.

Benchmarks still need concrete points for:

- events, spans, error occurrences, and metric points per second;
- average and p99 payload size;
- burst multiplier;
- active metric series and label cardinality;
- detailed and raw retention;
- collector outage buffer target;
- dashboard query concurrency;
- recovery point and recovery time objectives.

The reference application produces these measurements. The `seedstore`
simulator already makes realistic load across every surface, and its ledger
already proves the results. Scale its scenarios to get the capacity numbers.
Therefore one workload proves correctness and capacity together. See
[TESTBED.md](TESTBED.md).

The numbers arrive with the reference application, not before it.

Defaults optimize the home profile. Do not call a large profile production-ready
until measurements prove its storage and query behavior.

The D36 deduplication window depends on the collector outage buffer target in
this list. Select them together.

## D11. Session replay and the session model — Accepted

TallyOwl does not record the Document Object Model (DOM). It does not record a
replayable session. That work has a much larger privacy, payload, storage, and
playback surface than semantic events.

TallyOwl records application events. A session ties those events together.

The session is a first-class concept on every surface. A browser, a rich
client, a mobile client, and a terminal user interface all use the same
lifecycle:

```text
startSession()  -> returns an opaque session ID
   ... the client attaches that ID to its events ...
endSession()    -> closes the session
```

Rules:

- the client library issues the session ID; a person cannot select it;
- the session ID is opaque and has project scope;
- the client attaches the session ID to each event that belongs to the session;
- the library assumes no cookie and no browser storage, so a terminal user
  interface gets sessions with no extra work;
- a session that never receives `endSession` closes after a configurable
  maximum lifetime, because a client crash must not hold a session open
  forever.

The head validates each session ID. It drops an event that carries an invalid
session ID. It counts each drop in a metric with the reason.

That metric is the point. A misconfigured client shows up as a rising
invalid-session count instead of as quiet data loss.

Do not use the word "session recording". Call the excluded feature "session
replay". The two names then stay distinct, and no reader confuses the excluded
feature with the session model that TallyOwl supports.

## D12. Metrics compatibility and self-observability — Accepted

Collectors scrape Prometheus and OpenMetrics application endpoints and accept
OpenTelemetry metric and trace pushes. They normalize both to native TallyOwl types
and use CSIL for every subsequent hop. OpenTelemetry log ingestion remains out
of scope.

TallyOwl services expose Prometheus and OpenMetrics metrics. They can also push the
same instruments to a protected internal TallyOwl project.

The current scope excludes remote-write compatibility.

### Compatibility receivers do not listen by default

TallyOwl stays in single-binary territory, so the OpenTelemetry Protocol (OTLP)
receiver compiles into the collector. Its code size is small.

The receiver does not open a listening socket by default. An operator enables
the listener in configuration. A default TallyOwl installation therefore
accepts no OpenTelemetry traffic and exposes no OpenTelemetry port.

Prometheus and OpenMetrics scraping is outbound. The collector scrapes only the
targets that an operator configures. There is no default target.

The reason is exposure, not code size. A compatibility edge must be a choice
that an operator makes, never a port that appears because the binary contains
the feature.

## D13. External notification channels — Accepted

TallyOwl supports two notification channels:

- a generic **signed webhook** for an outside system;
- a native **CSIL callback** for a service that already holds a connection.

The webhook needs one secret shape and one retry policy. A downstream system builds another
channel, such as email or a chat service, on that webhook. TallyOwl does not
build it.

The CSIL callback needs no new secret handling, because the receiving service
already authenticates on its existing connection.

Both channels use the same alert instance, deduplication, and retry state.

## D14. Repository license — Accepted

Apache License, Version 2.0 applies to the repository and to TallyOwl-owned
generated packages.

D31 holds the unresolved distribution coordinates.

## D15. Replication implementation — Accepted pending benchmark

There is no all-storage-node Raft group. Each cell has three controllers by
default. An operator can select five controllers for a cell.

Tablets contain virtual shards. Each write-replicated tablet has its
own three-voter storage-node consensus group for leadership, ordered WAL,
committed receipts, membership, and snapshots. Non-voting read and export replicas
do not enlarge that quorum. A 400-node cell has three or five controller voters.
Storage nodes vote only in tablet groups for the tablets that they hold.

A large multi-region installation has many regional cells. A separate small
global directory maps projects to cells. The global directory does not manage
tablet membership.

Use an existing consensus implementation rather than inventing the algorithm.
The first candidates are `openraft` and `raft-rs`. Both have a compatible
license. The prototype must evaluate API stability, multi-group and tablet
overhead, large batch handling, snapshot transfer, membership changes, license
compatibility, and failure behavior. Selection remains open until controller
and tablet kill, partition, and restart plus tablet-movement tests pass.

## D16. Tablet sizing and project sub-sharding — Accepted

The home profile starts with one embedded tablet. It has no controller quorum.
It has no tablet consensus group.

A cluster uses stable virtual shards. Many virtual shards can use one tablet.
The controller automatically splits, merges, and moves tablets.

The primary route depends on the telemetry type:

- A span uses its trace ID.
- A behavior event uses its session ID or actor ID.
- A metric point uses its series ID.
- Other data uses a stable event key.

All other correlation fields use exact indexes. Thus, one route does not have
to contain all relationships.

The controller uses sustained load to start an automatic change. It measures
stored bytes, ingest rate, query load, compaction debt, and disk pressure.
Hysteresis prevents frequent topology changes. Operators can stop automatic
changes or use a recommendation-only mode.

Benchmarks will give the initial numeric split and merge limits.

## D17. Native page compression and sizing — Accepted pending benchmark

The first benchmark uses these defaults:

- Seal a microsegment at 1 second or 8 MiB.
- Use a 32 MiB to 64 MiB home segment target.
- Use a 256 MiB cluster segment target.
- Use a 64 KiB compressed page target.
- Use Zstandard level 1 for hot and warm data.
- Permit Zstandard level 3 during cold compaction.

These values are writer policies. They are not different storage formats.
Operators can configure all values.

Benchmarks can change a default before the format becomes stable. Correct
recovery and bounded memory are more important than compression ratio.

## D18. Query consistency and coordinator behavior — Accepted pending benchmark

Accepted behavior:

- `committed` queries require a voting replica at the requested commit
  watermark;
- `bounded-stale` queries may use read replicas and return their exact
  watermark and lag;
- storage nodes perform filters and partial aggregates locally;
- the query coordinator merges bounded partial states rather than pulling raw
  rows except for explicit event and trace detail queries;
- correctness is the default: a normal dashboard and API query fails with a typed
  incomplete-result error when a required tablet is unavailable;
- explicitly requested partial mode may return data only when the result marks
  itself incomplete and identifies missing tablets and time ranges;
- exact lookup verifies full typed values after hashed routing and index pruning.

An exact result applies to a declared snapshot watermark. Detail queries use
the current committed watermark. ID lookup, trace assembly, and erasure use the
current committed watermark.

A dashboard can use an exact watermark that is not more than five seconds old.
The result gives the watermark. A missing tablet causes a typed error by
default. A caller must request a partial result.

The query planner uses standard relational operators. Public queries use a
versioned typed CSIL algebra. A PromQL adapter can translate metric queries.
A bounded SQL adapter is not part of the first storage release.

The prototype will select compatible MIT or Apache-2.0 parser components. The
prototype will also set fan-out limits.

## D19. App batching and backpressure defaults — Accepted

The first app drivers use these defaults:

- Seal a batch at 256 items.
- Seal a batch at 512 KiB.
- Seal a batch after 100 milliseconds.
- Use a 1 MiB maximum ordinary frame.
- Use a 4 MiB maximum exceptional frame.
- Permit 8 MiB of unacknowledged data on one collector connection.
- Seal the current batch when a critical event occurs.
- Use a shutdown flush deadline of 2 seconds.

All values are configurable. Retries use capped jitter while the process is
alive.

If the buffer is full, the driver waits until its deadline. It then returns a
typed backpressure error. It does not report success for discarded data.

## D20. Property schema and high-cardinality indexing — Accepted pending benchmark

Accepted behavior:

- support high cardinality for all telemetry and dynamic properties;
- built-in event, request, trace, span, session, actor, and order correlation IDs are
  exact-indexed;
- dynamic scalar fields enter typed sparse native columns and default to exact
  `lookup` indexing, with `facet` and `stored` access classes available;
- cardinality alone never causes silent coalescing, dropping, or rejection;
- byte, field-count, value-size, CPU, memory, and storage quotas remain
  configurable and visible;
- low-cardinality materialized views and rollups accelerate common aggregates
  without replacing the high-cardinality source.

A dynamic scalar field uses exact `lookup` indexing by default. An operator can
change the field to `facet` or `stored`.

If one field name has different types, TallyOwl stores typed variants. A query
selects one type or requests an explicit conversion. The dashboard shows the
type conflict.

The first limits are:

- 128 properties for one item;
- 64 bytes for a property name;
- 4 KiB for one indexed string or byte value;
- 64 KiB for all dynamic properties in one item.

All limits are configurable. A larger value can use the `stored` class if the
project policy permits it.

The prototype must measure mostly-unique values, cross-service request
correlation, actor and session timelines, high-cardinality metric labels, index
build and merge cost, deletion, and cold-object lookup.

## D21. Exact versus approximate analytics — Accepted

At large scale, some exact queries cost much more than a mergeable sketch. The
expensive queries include exact distinct actors and sessions, high-cardinality
breakdowns, and long-window funnels.

Every query and result type says whether it is exact or approximate and names the
method and error bound. Detail, exact lookup, erasure membership, and bounded-window
queries default to exact. Long-range rollups may use explicitly selected
mergeable sketches; they never silently replace an exact query.

## D22. Node trust and role enrollment — Accepted

The control plane owns the installation certificate authority (CA). A large
installation can use an offline root CA and an online intermediate CA.

An operator creates a role token. The role token is an API credential. It can
enroll more than one node when its policy permits this use.

Each role token has these controls:

- permitted node roles;
- permitted cells, regions, and projects;
- expiration time;
- optional maximum use count;
- optional network restrictions;
- rate limits;
- audit identity;
- revocation state.

TallyOwl stores only a hash of the role token. TallyOwl permits multiple
active role tokens. Thus, an operator can rotate tokens without a coordinated
stop.

A new node generates its private key. The node authenticates the controller
with a configured CA or certificate fingerprint. The node then sends its role
token and certificate request.

The control plane assigns a node ID. It signs a short-life certificate for the
permitted role. The node uses mutual Transport Layer Security (mTLS) after this
enrollment. The node uses its current certificate to get a replacement
certificate.

A role token can automatically enroll a stateless ingest, query, collector, or
worker node. A storage token can enroll a storage process. The controller still
controls tablet placement.

A role token cannot add a controller voter. A role token cannot change a tablet
voter set. These operations need an approved controller-quorum change.

The first certificate lifetime is 24 hours. Renewal starts after 8 hours. Both
values are configurable.

The CA signing key belongs to the control-plane signing role. It does not belong
to the public dashboard process. The first release uses operator volume and
bucket encryption. Native segment encryption needs a separate key design.

## D23. Home-profile resource budgets — Accepted pending benchmark

Set default idle and active budgets for:

- head and storage memory and threads;
- collector intake and forwarder memory and connections;
- Corndogs disk budget and maximum task age;
- compaction and export CPU and I/O;
- native self-telemetry volume;
- default retention and disk-pressure thresholds.

The home profile must support at least 12 applications without manual worker
tuning. Operators can configure each budget.

Measure all default budgets together before release.

## D24. Hot, warm, and cold storage tiering — Accepted pending benchmark

The same native immutable segment and index bundle spans all tiers:

- hot recent data, WAL, microsegments, and locator heads on local disk;
- warm sealed segments on local disk;
- cold older segments in optional object storage with local manifests and a
  bounded page and index cache.

Object storage is optional. The home profile does not use it by default.

When an operator enables object storage, use these initial defaults:

- Keep 24 hours of hot data on local storage.
- Keep the next 7 days of warm data on local storage.
- Put older retained data in the cold tier.
- Start normal tier movement at 70 percent disk use.
- Increase tier movement at 85 percent disk use.
- Stop ingest before unsafe exhaustion at approximately 95 percent disk use.

All values are configurable for each data class. Local removal occurs only
after TallyOwl verifies the cold data and its indexes.

Cold data stays queryable. A normal query can read cold data. The result gives
the cold byte count and cache information.

Prototype Apache OpenDAL as the Apache-2.0 object-access abstraction with
selected backends compiled as features. Benchmarks will set the cache size,
range-read layout, upload limit, and bucket-outage behavior.

## D25. Reusable high-cardinality store module — Accepted

The native database is the `tallyowl-store` Rust crate. It provides schemas,
WAL integration, segments, indexes, tombstones, snapshots, tier locations, and
bounded query operations.

The crate is a separate module with its own versioned API contract. Telemetry
code is a consumer of that contract and does not reach around it.

TallyOwl does not claim that the crate is a general-purpose store. TallyOwl
does not design it for a second consumer, and no second consumer exists.

The reason for the boundary is discipline, not reuse. A real API contract keeps
TallyOwl honest about how it uses its own storage engine. It also forces a
clean design at that seam.

If the crate later proves useful to another project, that is welcome. Keep it
easy to adopt. Do not add generality before someone needs it.

The prototype must test compatible existing components. Tantivy is a candidate
for term dictionaries, postings, fast fields, merges, and deletion.

Tantivy must meet the TallyOwl durability, tier, snapshot, and exactness
requirements. TallyOwl does not automatically use it as the source database.

The embedded catalog and object-access layer remain replaceable.

## D26. Cell hierarchy — Accepted

TallyOwl uses a cell hierarchy. The home profile puts all cell roles in one
process.

A large installation has a small global directory. The global directory maps
projects to cells. It does not manage each tablet.

Each regional cell has its own controller quorum. The cell controls its storage
nodes and tablets. Existing cells continue data operations if the global
directory is temporarily unavailable.

This hierarchy supports installations from one node to 10,000 nodes. It does
not require one global consensus group for all nodes.

## D27. Regional write ownership and receipt policy — Accepted

Each tablet has one write region at one time. Other regions can have read,
export, or recovery replicas. A fenced control-plane operation changes the write
region.

Different tablets can have different write regions. Thus, an installation can
write in many regions without multi-writer conflicts for one tablet.

TallyOwl supports these receipt policies:

- `local-one`: one local voter and one fsynced copy;
- `local-quorum`: a local tablet quorum;
- `remote-one`: a local quorum and one remote durable copy;
- a custom failure-domain policy.

TallyOwl does not acknowledge an uncommitted entry in a multi-voter group.
This rule has no exception.

Therefore `local-one` is legal only for a tablet with one voter. It is the
default and the only policy in the home and embedded profiles. A tablet with
more than one voter must use `local-quorum`, `remote-one`, or a custom policy.
`local-quorum` is the default for a multi-voter tablet.

The control plane refuses a `local-one` configuration on a multi-voter tablet.
It does not silently upgrade or downgrade the policy.

An operator can change the policy. The receipt gives the policy that the write
satisfied. Before a stronger policy becomes active, TallyOwl builds its
replica set. The controller then commits the new tablet configuration.

## D28. Erasure timing and cold-tier erasure — Accepted

An erasure tombstone gives immediate logical removal. The tombstone applies to
every tier at once. It also applies to data that arrives after the erasure
request. Telemetry for an erased actor can still be in a collector queue when
the request lands. That telemetry must not become visible when it arrives.
An erasure predicate therefore stays active until its horizon ends.

Physical reclamation differs by tier.

**Hot and warm tiers.** Compaction rewrites affected local segments. The first
target is 24 hours. This work stays local and bounded.

**Cold tier.** Compaction must not rewrite every intersecting cold object. One
actor can touch thousands of cold objects across a retention period. Download,
rewrite, and upload of that set is too expensive for a 24-hour target.

The cold tier therefore uses cryptographic erasure:

- a segment encrypts each actor's rows with a key derived for that actor;
- the catalog holds the key material, not the object;
- erasure destroys the key and records the destruction;
- the cold bytes then cannot be read by TallyOwl or by an object-store reader;
- normal retention reclaims the object space later.

This decision needs the segment encryption key design that D22 defers. The two
are one design. Do not implement cold tiering for a project that permits
erasure until the key design exists.

The default immutable-backup horizon is 30 days. It is configurable. An erasure
ledger is part of each restore. The ledger prevents a restore from making
erased actor data visible again.

TallyOwl stores the actor ID that the app supplies. TallyOwl does not
transform the ID. An app can transform the ID before it sends data.

## D29. Documentation language — Accepted

All technical documentation uses ASD-STE100 Simplified Technical English. The
current project reference is Issue 9, dated 2025-01-15.

Project technical terms have controlled meanings. The project does not use
synonyms only for style variation. See [DOCUMENTATION.md](DOCUMENTATION.md).

## D30. Consent behavior — Accepted

D9 accepts the privacy defaults. This decision gives the consent behavior.

Consent applies to personal data. TallyOwl does not require consent where no
personal data is present.

A person who lands on a campaign page produces campaign facts: the campaign,
the platform, the referrer, and the landing page. TallyOwl captures those facts
by default. They show that a campaign works. They are not tied to an
identified person.

Consent applies at the point where campaign data joins an identified actor. An
identified actor is personal data. The applicable policy then controls that
join, the retention, and any erasure.

TallyOwl does not guess a jurisdiction and does not change behavior by
geography. TallyOwl is not the policy authority for an application.

An application that must collect less configures that in its own collection
policy. The policy controls enabled telemetry kinds, campaign capture, and
property filtering. The operator makes that choice with knowledge that TallyOwl
does not have.

Consent state still travels with an applicable event and TallyOwl stores it, so
a later policy can act on it.

One boundary the implementation must respect: a session ID links touchpoints
over time. That link is what makes attribution work and it is also the point
where a stricter policy applies. Keep campaign capture usable without a session
link, so an operator who turns off session-linked campaign data still measures
campaign performance.

The reference application must exercise both configurations. See
[TESTBED.md](TESTBED.md).

## D31. Package, registry, and support window — Accepted for now

D14 accepts the license. This decision holds the distribution items.

**Before the release candidate:** there is no client compatibility window. The
project is pre-alpha. The head does not have to accept an old client, and a
protocol change does not need a migration path. Do not spend effort on version
skew during this period.

**At the release candidate:** select these items.

- package coordinates and registries for each maintained language;
- container registry;
- chart registry;
- how many protocol versions the head accepts, and for how long.

The support window then matters more than usual. An application compiles the
TallyOwl ingest schema into its own build. An application therefore upgrades on
its own schedule, and version skew across applications becomes normal.

Reopen this decision at the release candidate. Do not let the pre-alpha
exemption survive into a release.

## D32. Source identity and tenancy resolution — Accepted

An instrumented application does not know about TallyOwl workspaces. It holds a
credential, and that credential has project scope.

The workspace is the tenancy boundary. The project identifies the application.

Resolution sequence:

1. An app driver authenticates to collector intake with its project credential.
2. The collector resolves the project ID and its workspace ID from the control
   catalog. It does this on the first batch only.
3. The collector holds that mapping in memory.
4. The collector stamps the workspace ID and project ID on every envelope.
5. The collector discards any tenancy value that arrived in a payload.

The mapping is stable. A project ID and a workspace ID never change. Therefore
the in-memory mapping needs no expiry.

Revocation is a separate control. A revoked credential must fail at the next
batch. Do not let the tenancy cache keep a revoked credential alive.

TallyOwl resolves IDs only. A project name and a workspace name are display
properties for a person. A name never travels on the ingest path and never
takes part in routing or authorization.

### Descriptive values

One typed property namespace carries every other descriptive value. `env` with
a value of `prod` is a common example. `region` with a value of `us-west2` or
`PA-datacenter` is another.

Each property records its origin. The collector stamps an operator property
from its own configuration, and an application cannot set a protected name. A
property never grants access and never selects a destination. See D38.

### Collector-to-head identity

The collector-to-head key identifies the collector. The head verifies that
each stamped destination is inside that collector's permitted project set.

Consequences that the implementation must satisfy:

- an audit record stores the effective workspace and project at ingest time,
  because the key alone does not identify them;
- a change to a project-to-workspace mapping is an audited control operation;
- key rotation does not change a tenancy mapping.

Two applications correlate their work with shared request and trace IDs. They
do not need a shared workspace to do this. A query that crosses projects needs
an explicit identity policy. See [DESIGN.md](DESIGN.md) section 10.

## D33. Retry backoff and timeout sweeping — Accepted

Corndogs expresses a delay with the task timeout and the state swap. A worker
parks a task in a waiting state, gives the task a timeout, and names the ready
state as the automatic target state:

```text
UpdateTask(uuid,
           new_state = "backoff",
           auto_target_state = "queued",
           timeout = <backoff seconds>)
```

Corndogs returns the task to `queued` when the timeout expires. TallyOwl does
not need to enumerate tasks, and no worker holds a claim during the wait.

Corndogs evaluates a timeout only when a caller invokes `CleanUpTimedOut`.
Therefore:

- the collector forwarder role owns the sweep;
- the forwarder calls `CleanUpTimedOut` for each queue it serves;
- the sweep interval is configurable and defaults to one second;
- readiness fails when the sweep stops, because a stopped sweep silently stops
  retry and worker recovery.

The sweep cost differs by backend. The file backend scans every live task and
decodes each one. The cost grows with queue depth, and queue depth grows during
a head outage. The postgres backend uses one set-based update.

Therefore the benchmark set must measure sweep latency at the outage-buffer
depth that D10 selects. If the cost is too high, store the batch payload in a
collector-local content-addressed spool and put only the reference in the task.
That change reintroduces a second local journal, so make it only with measured
justification.

## D34. Browser unload flush — Accepted

A browser loses buffered events when a person closes a tab. Session end and
exit events are exactly the events that a funnel and a session-duration
analysis need.

The browser package therefore flushes on `pagehide` and on a
`visibilitychange` to hidden. It sends the flush to the host application's own
same-origin endpoint. It may use `sendBeacon` for this flush.

This does not break the integration rule. The flush reaches the application,
not a TallyOwl domain. The application then uses its normal app driver path.

The host application must expose one same-origin route for the flush. The
browser package documentation states this requirement.

The flush remains best effort. The browser package does not claim durable
delivery after a tab closes.

## D35. Tail sampling location — Accepted

TallyOwl supports head sampling and tail sampling. Tail sampling needs a
complete trace before it decides.

The decision happens at the head, after tablet routing. It does not happen at
the collector.

A span routes by its trace ID (D16). Therefore one tablet already holds every
span of one trace. No component buffers a trace across collectors, and no
collector coordinates with another collector.

Sequence:

1. The collector forwards every span. It applies head sampling only.
2. The head commits the spans to the tablet that owns the trace ID.
3. Those spans enter a provisional retention class.
4. The projector applies the tail rules when the decision window closes.
5. A kept trace moves to its normal retention class.
6. A dropped trace gets a tombstone. Compaction reclaims the space.

This costs the ingest and the temporary storage of spans that TallyOwl later
drops. It removes a distributed buffering component from the design. The
decision window bounds that storage cost, and TallyOwl can measure it. A
cross-collector buffer gives neither property.

These values are configurable:

- the decision window;
- the provisional retention class limits;
- the late-span grace period after the window closes.

A span that arrives after the grace period cannot change a decision that
TallyOwl already applied. The projector records the late arrival and applies
the existing decision.

Always-keep rules run at the collector, not at the head. An unhandled error or
a critical business event must survive even when the tail rules later drop its
trace.

## D36. Deduplication window and outage buffer — Accepted

These values are one decision, not two. The head must retain a batch receipt
for longer than a collector can retry:

```text
dedup_window >= max_collector_outage_buffer + max_replay_window + safety margin
```

An automated retry must stop before the dedup window ends. A replay after that
point needs an explicit operator workflow.

D10 selects the outage buffer target. That selection then fixes the dedup
window. Do not select them separately.

## D37. Reference application test bed — Accepted

The project maintains a complete simulated application and an integration test
bed. Unit tests do not prove that a product produces correct analytics.

The simulator writes an expected-result ledger before it sends data. The tests
compare TallyOwl query results with that ledger. Analytics correctness is
therefore a test result.

The test bed covers a marketing site, a web application, a backend, a rich
client, and a mobile client. A second application proves tenant isolation.

See [TESTBED.md](TESTBED.md).

## D38. One property namespace — Accepted

An earlier draft had a separate tag namespace. This decision removes it.

A tag and an attribute were both typed key-value pairs. They filtered the same
way, grouped the same way, and supported the same cardinality. A caller had to
choose a namespace for no reason. Storage gave no reason either: a value that
repeats in a segment costs almost nothing after dictionary and run-length
encoding.

TallyOwl therefore has one typed property namespace. D20 gives its types,
access classes, and limits.

Each property records its origin:

| Origin | Meaning |
| --- | --- |
| `client` | The calling code supplied it at the event site. |
| `driver` | The app driver supplied it from its own configuration. |
| `collector` | The collector stamped it from operator configuration. |

A key-name access control list gives the names that an application cannot set.
The collector refuses a client value for a protected name. It counts the
refusal in a metric. It does not accept the value and hide the conflict.

A query can filter on origin. An operator can therefore trust that
`region=us-west2` came from collector configuration.

A driver hoists a property that is constant across a batch into the batch
envelope. The collector expands it again. This is a transport encoding. It is
not visible above the transport and it does not change any query.

## D39. Error grouping fingerprint — Accepted

The projector computes the fingerprint. A producer never controls its group.

`fingerprint_v1` uses the first rule that applies:

1. The occurrence has one or more in-app frames. Hash the exception type and
   the top five in-app frames. Use module and function only. Collapse repeated
   frames from recursion.
2. The occurrence has frames, but none are in-app. Hash the exception type and
   the top five frames.
3. The occurrence has no frames. Hash the exception type and the message with
   literals replaced by placeholders.

Rule 3 exists because a browser error often arrives with no usable stack. A
design that used frames alone would put every such error in one group.

The fingerprint excludes line numbers and addresses. A reformatting change or a
line shift therefore does not split a group.

TallyOwl stores the fingerprint inputs, the rule that applied, and the
fingerprint version. A later version rebuilds all groups from retained raw
data.

The first release has no per-project rule configuration.

### Merge and split overrides

An operator can merge groups and split a group. TallyOwl records the override
against the set of fingerprints, not against a group ID.

Therefore a regroup at a new fingerprint version keeps the operator decision. A
group ID would not survive that rebuild.

## D40. Attribution configuration — Accepted

Attribution is a pure function over immutable touchpoints. A model parameter is
configuration, not code, and not a schema decision.

An operator configures these values for each project:

- the position-based weights;
- the time-decay half-life;
- the lookback window;
- the enabled models.

A change to a parameter recomputes the result. Raw touchpoints never change.
Each result names its model and its model version.

Phase 8 selects the shipped default values. There is no migration cost in
selecting them later.

### Lookback and retention

The lookback window must not exceed the touchpoint retention for that project.

If touchpoints age out inside the window, attribution moves credit to later
touches. The result looks correct and is wrong.

TallyOwl therefore refuses an attribution query whose window exceeds the
retained range. It returns a typed error that names both values. It does not
return a partial credit result.

## D41. Alerting scope — Accepted

Grafana is the primary alerting path for an operator who runs it. TallyOwl
built-in alerting serves an installation that does not.

Built-in alerting has two rule kinds:

- **threshold:** a condition on a measure from a saved query. The condition can
  test above, below, or outside a range, and can require a duration.
- **absence:** a rule that fires when expected data stops arriving.

Absence is separate because a threshold on a count never fires when ingest
stops. A dead pipeline returns no rows. It does not return a low number.

Baseline, seasonality, and anomaly detection are out of scope. Grafana does
that work. See [ALERTS.md](ALERTS.md).

## D42. Grafana integration — Accepted

TallyOwl provides a Grafana datasource plugin. TallyOwl does not emulate the
Prometheus HTTP query API.

The plugin translates the Grafana query model into the native typed query
algebra. TallyOwl therefore keeps one query contract, and Grafana reaches
product data, not only TallyOwl operational metrics.

The plugin uses generated CSIL clients. It needs no new protocol.

The plugin lives in this repository and versions with the query algebra. A
breaking algebra change cannot ship without the plugin change in the same
commit.

## D43. Retention classes — Accepted

Retention uses named classes. A telemetry kind maps to a class. A project can
override the duration for one kind.

| Class | Holds | Note |
| --- | --- | --- |
| `provisional` | Spans inside an open tail-sampling decision window | Bounded by the decision window |
| `raw` | Accepted interchange CBOR | Short, and optional |
| `detailed` | Events, spans, error occurrences, metric points | The main analytic range |
| `rollup` | Aggregates and downsampled series | Longer than `detailed` |
| `audit` | Control-plane and deletion records | Longest |

Named classes give the tiering, erasure, and cost documents one vocabulary. A
per-kind override covers the project that needs one exception.

A query result names the class that bounded its range when data expired inside
the requested window.

## D44. Segment checksums and content addressing — Accepted

The segment format uses two hash functions for two different jobs.

| Use | Function | Reason |
| --- | --- | --- |
| Column page checksum | xxHash3-64 | Corruption detection on the hot path |
| Footer checksum | xxHash3-64 | Corruption detection |
| Segment content address | BLAKE3-256 | Identity, backup verification, restore |

A page checksum detects a damaged read. It does not need collision resistance
against an attacker.

A content address identifies a segment across a backup, a restore, and a cold
object store. It needs collision resistance.

See [SEGMENT_FORMAT.md](SEGMENT_FORMAT.md).

## D45. Tail sampling rule language — Accepted

A tail rule uses the typed expression tree in [QUERY.md](QUERY.md) section 5.
TallyOwl does not define a second expression language.

The expression evaluates over the assembled trace. It can read trace-level
values:

- total duration;
- span count;
- error presence;
- the root service and root operation;
- any indexed property on any span in the trace.

A rule pairs an expression with a typed sampling clause: keep all, keep a
percentage, or keep the first count in an interval.

Rules evaluate in order. The first match decides. A trace that matches no rule
uses the default keep rate.

## D46. Repository scope — Accepted

Everything that belongs to TallyOwl lives in this repository. That includes the
services, the client packages, the reference application, the Helm charts, the
Grafana plugin, and a project website when one exists.

This is a product repository, not a monorepo of unrelated projects. One version
therefore describes one coherent product.
