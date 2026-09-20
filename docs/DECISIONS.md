# Decisions to approve

This is the working decision register. Each decision has one status from this
list:

| Status | Meaning |
| --- | --- |
| Accepted | The project made this decision. Implement it. |
| Accepted pending benchmark | The direction is set. A measurement can change the values, not the shape. |
| Recommended | A proposal. The project owner must approve or refuse it. |
| Withdrawn | The project made this decision and then reversed it. The record stays. |
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
| D10 | Capacity envelope | Accepted, both halves measured |
| D11 | Session replay and the session model | Accepted |
| D12 | Metrics compatibility and self-observability | Accepted |
| D13 | External notification channels | Accepted |
| D14 | Repository license | Accepted |
| D15 | Replication implementation | Accepted pending benchmark. Built; two prototype gaps remain |
| D16 | Tablet sizing and project sub-sharding | Accepted |
| D17 | Native page compression and sizing | Accepted, measured |
| D18 | Query consistency and coordinator behavior | Accepted pending benchmark |
| D19 | App batching and backpressure defaults | Accepted |
| D20 | Property schema and high-cardinality indexing | Accepted, measured |
| D21 | Exact versus approximate analytics | Accepted |
| D22 | Node trust and role enrollment | Accepted |
| D23 | Home-profile resource budgets | Accepted, partly measured |
| D24 | Hot, warm, and cold storage tiering | Accepted pending benchmark |
| D25 | Reusable high-cardinality store module | Accepted |
| D26 | Cell hierarchy | Accepted |
| D27 | Regional write ownership and receipt policy | Accepted |
| D28 | Erasure timing and cold-tier erasure | Accepted |
| D29 | Documentation language | Accepted |
| D30 | Consent behavior | Accepted |
| D31 | Package, registry, and support window | Accepted |
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
| D47 | Group-commit linger | Accepted |
| D48 | Collector deployment unit | Withdrawn |
| D49 | Row-group size and reader coalescing | Accepted |
| D50 | Approximate measure algorithms | Accepted |
| D51 | Saved query versioning | Accepted |
| D52 | Query explain output | Accepted |
| D53 | Durability and convergence, not recovery objectives | Accepted |
| D54 | Plain language for anything a person reads | Accepted |
| D55 | One entry point for build, test, and generate | Accepted |
| D56 | Test data generation and branch coverage | Accepted |
| D57 | Integrity checking is an operator choice | Accepted |
| D58 | Quorum loss: restore by default, unsafe recovery behind a flag | Accepted |
| D59 | Catalog snapshots, off by default | Accepted |
| D60 | A slow node alerts and says why | Accepted |
| D61 | Segment encryption keys | Accepted |

## Next decision order

Every decision now has a status. No decision waits for an approval.

### Measured

D3, D15, D17, D20, D24, D25, D33, and D44 now carry measured sections. D10
carries a measured storage half and an unmeasured end-to-end half. See
[BENCHMARKS.md](BENCHMARKS.md).

Nothing blocks the start of implementation. Every remaining item needs code
that does not exist yet.

### Open, waiting on the reference application

1. Measure D10 and D23 end to end. The storage half of D10 is measured; ingest
   rate, burst multiplier, query concurrency, and the outage buffer target are
   not storage measurements.
2. Fix the D36 deduplication window from the D10 outage buffer target.
3. Measure the D35 provisional retention cost at the selected decision window.
4. Confirm the segment cost at real value distributions. Section 12 measured
   generated values, and it showed that column data is the part a component
   measurement gets wrong.

### Open, waiting on an implementation

5. Test D15 against real storage and a real network. The prototype meets every
   criterion on an in-memory substrate. Election under disk latency, a
   non-isolation partition, and recovery from a corrupt log are unmeasured.
6. Test D16, D26, and D27 with cell prototypes.
7. Test D18 with distributed correctness fixtures.
8. ~~Design the D28 segment encryption keys before cold tiering carries erasable
   data.~~ Closed by D61, 2026-08-02.
9. Measure D20 deletion and cold-object lookup, which need a deletion path.
10. Measure the segment opens that follow a locator probe, locator merge cost
    during compaction, and locator fan-out across tablets. Section 12b measures
    the locator itself; these three are what follow it.
11. ~~Select the D40 attribution default values in Phase 9.~~ Closed by D40,
    2026-08-05. The values are in D40 and each one states the reason it holds.

### Open, scheduled

12. ~~Reopen D31 at the release candidate and select the distribution
    coordinates.~~ Closed by D31, 2026-08-12. Every coordinate is selected
    except the crates.io dependency clearance, which is the owner's and is
    named in L167.
13. Phase 10 carries four items the owner scheduled at the Phase 9 review:
    calendar periods in a supplied timezone, a materialised identity graph,
    aggregate pushdown, and the consensus-log constants as settings. See
    `PHASE9_REPORT.md` section 6.

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

### Measured, 2026-07-26

redb 3.1.3 meets the catalog requirement.

- A deduplication lookup on the ingest hot path costs 0.91 microseconds, at
  1,098,930 lookups each second.
- A manifest prefix scan reads 2,913,447 rows each second.
- The device bounds a durable commit, not the engine. Commits each second
  converge on the `fsync` ceiling of the storage.

Nothing in this workload argues for a write-oriented alternative, because the
write path is device bound rather than engine bound. Compare an alternative
only if a later workload shows an engine-bound limit.

See [BENCHMARKS.md](BENCHMARKS.md) section 4.

## D4. Collector durability topology — Accepted

Corndogs is the durable task boundary. Once it accepts a telemetry task, that
task remains until final TallyOwl storage returns a committed receipt. It need
not remain after completion.

### Amended and withdrawn, 2026-07-28

An amendment on 2026-07-27 moved the batch payload out of the Corndogs task and
into a collector spool. A measurement had shown that a payload inside a task
made the timeout sweep 800 times more expensive.

Corndogs then fixed the cause. Commit `b8c10b0` stores a payload in its own
bucket as raw bytes and adds a deadline index. The sweep is now a function of
expired tasks rather than live ones.

The amendment is withdrawn. The collector keeps the payload in the task.

Measured after the Corndogs change:

- the sweep is flat at 2.3 milliseconds for 1,000 and for 5,000 live tasks.
  The same runs took 3,272 and 16,792 milliseconds before;
- the payload-in-task accept path beats a collector spool in every case,
  including the one case where the spool used to win.

This decision therefore reads as it originally did. The collector keeps no
second journal, and Corndogs is the single intermediate durable handoff.

`durable_copies` covers the payload again, because the payload is in Corndogs.
The durability weakening that the withdrawn amendment introduced is gone.

Corndogs limits a payload to 16 MiB by default, through
`CORNDOGS_MAX_PAYLOAD_BYTES`. The D19 seal of 512 KiB sits well inside it.

See [BENCHMARKS.md](BENCHMARKS.md) section 11d.

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
- no automatic cross-project end user identity;
- permit opaque project and workspace end-user IDs;
- use an exact index for end-user IDs and erasure keys;
- TallyOwl stores the end-user ID that the app supplies;
- an app can transform an end-user ID before it sends the ID;
- campaign parameters only with configured consent behavior;
- semantic interactions only, no session replay;
- permit typed high-cardinality custom properties within resource limits;
- visible per-field redaction and drop counters.

The project accepts the direct-personal-data prohibition and the end user-ID
exception. D30 holds the unresolved jurisdiction and consent behavior.

## D10. Capacity envelope — Accepted, measured

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

### Measured, 2026-08-03 — the ingest half

> These are the collector's rates, not the system's. See the 2026-08-04 re-run
> after it, which separates the two.

The reference application and its load harness now answer the ingest side, on
the home profile, on real storage. BENCHMARKS.md section 12c holds the whole
table and the method.

| Asked for | Answer |
| --- | --- |
| Events each second | **36,525** sustained with nothing refused, and **5,405** for one synchronous producer |
| p99 payload size | A batch of 256 items at the D19 defaults; each item carried an event name, a session, a route, and a unique request ID |
| Burst multiplier | **Not measured.** The harness could not offer faster than the system took; see below |
| Collector outage buffer target | **Still open.** `corndogs.maxDeliveryAge` defaults to 24 hours as a placeholder, and D36 pairs it with a deduplication window nothing yet bounds |
| Recovery time | **462 milliseconds** after `kill -9` under load, with no loss |
| Dashboard query concurrency | **Not measured.** Query latency is 51 milliseconds at p50 for both a point lookup and an aggregate over 387,177 events |

### Measured, 2026-08-04 — the ingest half, re-run

BENCHMARKS.md section 16 holds the whole table and the method.

| Asked for | Answer |
| --- | --- |
| Events each second | **67,624** at the collector, and **19,534** for one producer. The one-producer figure moved 3.6 times because both drivers now pipeline; see L055 |
| Events each second, end to end | **About 3,600.** The head commits at 17 batches each second, and every earlier figure in this decision was the collector's rather than the system's |
| Burst multiplier | **Still not measured.** The producer is fixed and the harness is now the slower half |
| Collector outage buffer target | **Answerable now.** The queue absorbs the gap between 67,624 and 3,600, so the buffer is sized by how long a burst lasts rather than by an outage alone. `storage.deduplicationWindow` bounds the pairing D36 asks for |
| Recovery | 543,994 events sent, accepted, and committed, with no loss |
| Dashboard query concurrency | **Not measured.** Query latency is 103 milliseconds at p50 for both a point lookup and an aggregate over 543,994 events |

**The distinction this run added is the important one.** A collector
acknowledges when Corndogs is durable and the head drains afterwards. Both rates
are real and only the head's is sustainable without a queue that keeps growing.

**The derived envelope held**, which is worth saying because several confident
predictions in this project did not. Section 12 derived 26,000 to 38,000 events
each second on this hardware and the 2026-08-03 measurement landed inside it.
The 2026-08-04 collector figure is above it, because the connection now serves
correlated batches at the same time.

**One finding changes what an application should do.** A driver flush waits for
its durable acknowledgement, so one synchronous producer is bounded by the round
trip rather than by anything in TallyOwl. DELIVERY.md section 3 already permits
an application to pipeline correlated batch calls, and neither maintained driver
does. Until one does, an application with a single telemetry worker gets about a
seventh of the ceiling.

Metric series and label cardinality, retention, and query concurrency remain
unanswered, because Phase 6 has not started and no scenario runs long enough for
retention to apply.

### Measured, 2026-08-01 — the storage half

`prototypes/segment-bench` writes a whole segment in the SEGMENT_FORMAT.md
layout to real storage, reads it back, verifies its checksums, and answers
20,000 point lookups. This replaces the earlier envelope, which added component
measurements together.

**48.9 bytes for each stored event** at one million rows for each segment: 27.3
of column data, 20.3 of exact index, and 1.3 of catalog receipt. 1 TiB holds
about 22.5 billion events. 10,000 events each second fills 39.3 GiB each day.
One app driver connection reaches roughly 26,000 to 38,000 events each second
on this hardware.

Three findings changed the design:

1. **Adding component measurements understated the cost by 15 percent.** The
   earlier 42.6 estimate assumed 21.0 bytes of column data. A real segment costs
   27.3, because the same eight columns carry correlated, higher-cardinality
   values and the compressor finds less to remove. Format overhead, which the
   estimate ignored, is genuinely free at under 0.005 bytes for each event.
2. **Cost for each event grows with segment size**, from 44.7 bytes at 250,000
   rows to 50.3 at 4 million, because a larger segment holds more distinct
   values. **A capacity number must state the segment size it assumes.**
3. **The `event_id` exact index was 25 percent of the whole segment**, the
   largest single line item in the format. SEGMENT_FORMAT.md now defaults a
   unique value to a block filter, which brings the segment to 39.75 bytes for
   each event, or 41.1 with the catalog receipt.

A field demoted to `stored` still removes its index cost. The access class
remains a capacity control as well as a query control.

The reference application still replaces the end-to-end numbers: ingest rate,
burst multiplier, query concurrency, and the outage buffer target are not
storage measurements. See [BENCHMARKS.md](BENCHMARKS.md) sections 12 and 12a.

Defaults optimize the home profile. Do not call a large profile production-ready
until measurements prove its storage and query behavior.

The D36 deduplication window depends on the collector outage buffer target in
this list. Select them together.

### Measured, 2026-08-04 — Phase 6, and the sustained figure is a range

BENCHMARKS.md section 17 holds the whole table and the method.

| Asked for | Answer |
| --- | --- |
| Events each second | **37,874** on the clean run, and 37,874 to 80,000 across four runs of one harness on one machine. The spread is the answer rather than noise: the collector's rate is bounded by how much room the Corndogs queue has, and that is bounded by how far behind the head is. One synchronous producer is **19,566**, stable to a tenth of a percent across all four and unmoved by Phase 6 |
| Metric points each second | **Not separated.** The series ledger and the merge pass are on the same path and neither is measurable on an event workload, which is the useful half of the answer: an installation that sends no metrics pays nothing for the feature |
| Active metric series and label cardinality | **Bounded rather than measured.** `metrics.maxSeriesForEachMetric` defaults to 100,000 for one name in one project and `metrics.maxBytesForEachMetric` to 64 MiB. Both are chosen, not measured; a measurement should set them. See L068 |
| Candidate segments for a high-cardinality lookup | **Answered.** 3,000 for a scattered layout over 30 days, and **228** with hot data scattered and cold data grouped by end user during compaction, which needs no change to ingest routing. `prototypes/locator-bench`, and BENCHMARKS.md section 18.4 |
| Burst multiplier | **Answered.** Eight unpaced producers offered **39,714 events each second and none were refused**. Every earlier figure measured a harness that paced itself; see L082. A larger offer needs more producer machines rather than a different harness mode |
| Recovery time | **314 milliseconds** after `kill -9` under load, with no loss and, this time, **no refusal**: 610,423 offered and 610,423 accepted while the head was gone |
| Dashboard query concurrency | **Still not measured**, but the query itself now is: a point lookup is **41 milliseconds at p50 and 42 at p99** and an aggregate is 104 and 120. The point lookup asks the locator rather than reading the range (L079), so the two are no longer the same number for the wrong reason |
| Collector outage buffer target | **Still open.** Section 17 shows the buffer is what sets the sustained rate, so the two settings are one decision |

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

D31 holds the distribution coordinates, selected at the release candidate.

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

### Measured, 2026-08-01 — openraft meets the criteria

A working three-voter cluster now runs on openraft 0.9.24, with in-memory
storage and an in-process network. Every criterion in this decision has an
answer except behaviour against durable storage and a real network.

| Criterion | Result |
| --- | --- |
| Election | 6 ms to a leader from initialize; 985 ms to replace an isolated leader |
| Partition | The minority never commits. The majority commits in 10.9 ms |
| Rejoin | A returning node caught up in 528 ms |
| Membership change | Add a learner in 4 ms, promote to voter in 4 ms |
| Snapshot | A 256 KiB snapshot builds in under a millisecond |
| Large batches | 12,543 entries each second at 64 KiB, which is 784 MiB each second |
| Multi-group overhead | 200 groups, 600 instances, 21 MiB, every group elected a leader |
| API stability | Stable 0.9 was workable. The 0.10 line is at alpha with breaking changes |
| License | MIT or Apache-2.0 |

**Consensus is not the throughput limit.** openraft commits 49,284 entries each
second at 4 KiB with no durability. Group commit against the device reaches
about 16,900 durable frames each second. The storage beneath the algorithm is
the constraint.

**The multi-group design holds.** Six hundred Raft instances cost 21 MiB and
23 milliseconds to build, and every group elected a leader. This was the
TallyOwl-specific risk, because a general Raft library targets a few large
groups.

Still unmeasured, and needed before the selection becomes final:

- behaviour against durable storage, where every append pays an fsync;
- behaviour over a real network with loss, reordering, and delay;
- a partition that splits a group other than by isolating one node;
- recovery from a corrupt or truncated log.

Selection is no longer open on the grounds of doubt about the library. It stays
open until those four run against the real storage and transport.

### Built, 2026-08-04 — one of the four is closed and one is partly closed

Phase 7 built the integration: `crates/tallyowl-cluster` supplies the tablet
state machine, the durable log, the multiplexed transport, and the placement
integration, and openraft supplies the algorithm. The tests run real groups over
real loopback sockets against real durable storage.

| Left open by the prototype | State |
| --- | --- |
| Durable storage, where every append pays an fsync | **Closed.** The log is redb with immediate durability. A committed write survives dropping and reopening every process |
| A real network with loss, reordering, and delay | **Partly closed.** Real sockets, the real CSIL codec, and the real framing. Loopback does not lose, reorder, or delay |
| A partition that splits a group other than by isolating one node | **Still open** |
| Recovery from a corrupt or truncated log | **Still open** |

**The selection therefore stays open**, on the two remaining rows and not on any
doubt about the library. Nothing found during the integration argued against
openraft: the multi-group shape held, the membership API did what the prototype
said, and every defect found was TallyOwl's own. See
`docs/IMPLEMENTATION_LOG.md` L084 to L093.

See [BENCHMARKS.md](BENCHMARKS.md) section 10.

## D16. Tablet sizing and project sub-sharding — Accepted

The home profile starts with one embedded tablet. It has no controller quorum.
It has no tablet consensus group.

A cluster uses stable virtual shards. Many virtual shards can use one tablet.
The controller automatically splits, merges, and moves tablets.

The primary route depends on the telemetry type:

- A span uses its trace ID.
- A behavior event uses its session ID or end-user ID.
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
- Use Zstandard level 3 only for a float column.

These values are writer policies. They are not different storage formats.
Operators can configure all values.

Benchmarks can change a default before the format becomes stable. Correct
recovery and bounded memory are more important than compression ratio.

### Measured, 2026-07-26

The page benchmark changed one of these defaults.

Zstandard level 3 is 1.3 percent **larger** than level 1 across the winning
encodings. It loses on six of eight columns, because an encoded column has
already removed the redundancy that a higher level would find. The float column
is the exception and gains 22.4 percent.

An earlier draft permitted level 3 during cold compaction. The measurement does
not support that, so this decision now limits level 3 to a float column.

The 64 KiB page target holds. The ratio penalty against whole-column
compression is under 2.5 percent for every column except a float column.

Every encoding in the list won a column. See
[BENCHMARKS.md](BENCHMARKS.md) section 5.

### Measured again, 2026-08-01 — the page target must bind

The 64 KiB page target holds, but the format as first written did not reach it.
One page for each column for each row group produced a largest page of 520 KiB,
eight times the target, because a row group targets 8 to 16 MiB of uncompressed
data and a unique 16-byte column does not compress.

**A writer closes a page on encoded bytes, not on a row count.** A column
contributes as many pages to a row group as its own size needs.

The cost is 1.35 bytes for each event, and it falls on one column shape: a
high-cardinality repeated value, where a smaller page gives the compressor less
history. A unique column and a strongly repeated column both cost nothing,
because their bytes are already either incompressible or fully removed.

See [SEGMENT_FORMAT.md](SEGMENT_FORMAT.md) section 6 and
[BENCHMARKS.md](BENCHMARKS.md) section 12a.

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
- built-in event, request, trace, span, session, end user, and order correlation IDs are
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
correlation, end-user and session timelines, and high-cardinality metric
labels. It must also measure index build and merge cost, deletion, and
cold-object lookup.

### Measured, 2026-07-26

The promise holds. A value that is unique on every row retrieves in 580
nanoseconds at the median, from an index that costs 12 bytes for each row.

The measurement also shows that the writer must select a layout from measured
statistics. A wrong choice is catastrophic in both directions:

- the unique-lookup layout on a repeated value reaches a p99 of 4.7
  milliseconds, because every row shares one fingerprint;
- the term-postings layout on a unique column costs 27 bytes for each row,
  which exceeds the 16 bytes of data that it indexes.

No fingerprint collision occurred over one million values. Full-value
verification still runs on every probe, because a hash must never decide
correctness, and it costs nothing measurable.

Deletion and cold-object lookup remain unmeasured.

### Measured again, 2026-08-01 — a unique value needs a filter, not a list

The whole-segment measurement showed the `event_id` unique-lookup index costing
25 percent of the segment. That is more than any column, and more than the
column it indexes.

**A unique value gets a block filter by default.** At 12 bits for each key it
costs 2.00 bytes for each row against 12.00, saves 21 percent of the whole
segment, and answers a probe at 21 million lookups each second. It answers "no"
exactly and "maybe" at a measured 0.53 percent for each row group, and the
reader then decodes one column page for an exact answer.

Correctness is unchanged. Full-value verification already ran on every probe,
because a fingerprint never decides. A filter simply moves that verification
from the index to the column page.

`unique-lookup` stays in the format for a query that must locate a row without
decoding a page, and it now needs a measurement to justify itself.
`term-postings` stays the answer for a repeated value, because a filter cannot
give a row list.

See [SEGMENT_FORMAT.md](SEGMENT_FORMAT.md) section 7 and
[BENCHMARKS.md](BENCHMARKS.md) section 12a.

### Measured at scale, 2026-08-01 — the locator, and what it costs

Every earlier measurement ran inside one segment. `prototypes/locator-bench`
builds real tablet locator runs at 100 million end users, 100,000 servers, and
30 days of retention.

**The answer is yes, with one change.** The locator itself is cheap: 2.4 GiB,
867,000 probes each second, 1.2 microseconds for each probe. The distinct-value
count is not what costs. What costs is the count of (value, segment) pairs,
which is a placement property.

At the layout as designed today, an unbounded end-user lookup returns **3,000
candidate segments** out of 4,470 retained. The locator does its job and the
query still opens most of the installation.

The change: **compaction groups cold rows by correlation value.** Two hot days
scattered plus 28 cold days grouped gives 228 candidate segments, a 13-fold
reduction, and it changes no routing. Sharding ingest by end-user ID reaches a
similar number and is rejected, because it scatters a trace's spans across
shards. A system cannot shard by both end user and trace.

Two supporting rules, both measured:

- a fingerprint is 64 bits. A 32-bit fingerprint collides 1.2 million times at
  100 million values;
- a time range prunes linearly, from 60 candidate segments at 30 days to 2 at
  one day. An unbounded high-cardinality lookup reads the whole retention
  window, so the query surface reports the candidate count before it runs.

This also invalidated a shortcut. Bytes for each pair is **not** a constant: it
moves 53 percent with density, so a size estimate must state the density it
assumes. See [BENCHMARKS.md](BENCHMARKS.md) section 12b and
[HIGH_CARDINALITY.md](HIGH_CARDINALITY.md) section 4.

Still unmeasured: the segment opens that follow a probe, locator merge cost
during compaction, and fan-out across tablets.

### The default access class is a capacity decision

An exact index costs between 2 and 12 bytes for each row, against 27 bytes of
column data for a whole event. Indexes cost about as much as the data they
serve, even after the filter change: 20.3 bytes of index against 27.3 of data
in the measured segment.

The project keeps `lookup` as the default for a dynamic scalar field. An
unexpected correlation property stays instantly usable, which is the promise
that this decision makes.

The cost is real, and an operator needs to see it. A project that sends several
unique-valued dynamic fields can multiply its stored bytes several times over.

Therefore:

- report index bytes for each field, not only total stored bytes;
- surface the largest indexes for a project in the dashboard;
- make demotion to `stored` a documented capacity control, not only a query
  control.

A demotion and a later promotion are both reversible. A class change starts a
background index build over retained raw data, so an operator can recover the
capability within the retention period.

See [BENCHMARKS.md](BENCHMARKS.md) sections 8, 12, 12a, and 12b.

## D21. Exact versus approximate analytics — Accepted

At large scale, some exact queries cost much more than a mergeable sketch. The
expensive queries include exact distinct end users and sessions, high-cardinality
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

## D23. Home-profile resource budgets — Accepted, partly measured

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

### Measured, 2026-08-03 — what the alpha run showed

> The disk row below was explained wrongly. See the 2026-08-04 re-run after it.

The load run in BENCHMARKS.md section 12c is the first time these budgets ran
together on one machine at load. What it settled:

| Budget | What the run showed |
| --- | --- |
| Head and storage | 36,525 events each second sustained on 8 cores with nothing refused. Memory was never a limit at this rate |
| Collector intake and forwarder | Both roles in one process, 8 concurrent driver connections, nothing refused |
| Corndogs disk budget and maximum task age | **Still open.** `corndogs.maxDeliveryAge` defaults to 24 hours and is a placeholder; D36 pairs it with a deduplication window nothing bounds |
| Default retention and disk-pressure thresholds | **Still open, and now urgent.** 387,177 events left 86 MiB on disk, of which the segments were 8.7 MiB. Nothing reclaims an append-log range or expires a receipt, so a home installation grows at 223 bytes for each event rather than 32 |
| Native self-telemetry volume | Not measured; Phase 6 has not started |
| Compaction and export | Not measured under load |

**Twelve applications is not the limit this run found.** The limit is the number
of concurrent producers, because a driver flush waits for its acknowledgement.
Twelve applications each with one telemetry worker would reach about 65,000
events each second between them, which is above the single-installation ceiling
this run measured, so the applications are not what runs out first.

### Measured, 2026-08-04 — what the re-run changed

BENCHMARKS.md section 16. Two of the rows above are now answered differently and
one of them was answered wrongly.

| Budget | What the re-run showed |
| --- | --- |
| Head and storage | **The head is the budget that binds.** It commits 17 batches each second, about 3,600 events, at a flat 60 milliseconds for each batch. The collector's 67,624 is absorbed by the queue. Memory was still never a limit |
| Corndogs disk budget | **Now sizeable, and larger than it looked.** The queue holds the difference between 67,624 and 3,600 for as long as a burst lasts, so the budget is a burst-duration decision and not only an outage one |
| Default retention and disk-pressure thresholds | **The previous row's explanation was wrong.** Reclamation exists now: the append log fell from 88.8 bytes for each event to 25.8. The directory figure barely moved, from 223 to 216, because the catalog is 157 of those bytes. That is the tablet locator holding one entry for each value and segment pair, and it is the price of exact high-cardinality lookup rather than reclaimable overhead |

**Twelve applications is a different question now.** One telemetry worker reaches
19,534 events each second rather than 5,405, so twelve of them would offer far
more than the head commits. The applications still are not what runs out first;
the head's per-batch catalog transaction is.

### Measured, 2026-08-04 — Phase 6

BENCHMARKS.md section 17.

| Budget | What the run showed |
| --- | --- |
| Native self-telemetry volume | **Bounded rather than measured.** Self-observation is off by default, and when it is on the cost is one batch of one point for each series in each period, because the recursion guard stops a push from measuring itself. See L072 |
| Default retention and disk-pressure thresholds | **Settled at last.** A fully sealed store costs **176.3 bytes for each event**: 39.9 in segments, against the 39.75 this envelope predicted, and 136.4 in the catalog. Retention expiry is built (L080) and a background segmenter empties the append log (L078). The catalog is now the whole question |
| Corndogs disk budget and maximum task age | **Still open, and now the same decision as the ingest ceiling.** The collector's sustained rate is the queue's room |
| Head and storage | **Accepted equals committed exactly**: 440,522 items accepted, 440,522 events committed, 2,631 batches accepted and 2,631 delivered. An earlier run appeared to store 1.61 rows for each event, and BENCHMARKS.md section 17.4 records that this was a Corndogs left running across a wipe rather than anything TallyOwl did |

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

### Measured, 2026-07-26

OpenDAL performs the ranged read that a cold query needs.

Round trips dominate a cold read, not bytes. A one-column aggregate fetches 6.2
percent of a 256 MiB segment and still needs 256 separate requests. The pages
sit at a stride and do not coalesce. The same column costs one request when it
is contiguous.

The row-group count therefore sets the cold read cost directly, and
SEGMENT_FORMAT.md gives no row-group size. That value needs an explicit choice
against this cost.

A bounded cache needs several times the hot working set. In the dashboard
pattern a 4 MiB cache thrashed at a 1.3 percent hit rate against a 4 MiB hot
set. A 16 MiB cache reached 48.8 percent. More cache gave nothing, because the
ad hoc tail never repeats.

Upload limits and bucket-outage behaviour remain unmeasured.

See [BENCHMARKS.md](BENCHMARKS.md) section 9.

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

### Measured, 2026-07-26

Do not adopt Tantivy for the exact-ID index.

Against the native layouts on the same data, Tantivy is slower and larger:

- it builds 7 to 130 times slower;
- its index is 2.6 times larger on a unique column;
- a point lookup takes 7 times longer at the median;
- a point lookup takes 36 times longer at p99.

The cause is structural rather than a defect. Tantivy is a full-text engine
whose API takes text, so a 16-byte ID becomes 32 characters of hexadecimal
before it reaches the index. TallyOwl needs fixed-width binary exact lookup,
which is a narrower problem with a much cheaper answer.

See [BENCHMARKS.md](BENCHMARKS.md) section 8.

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
request. Telemetry for an erased end user can still be in a collector queue when
the request lands. That telemetry must not become visible when it arrives.
An erasure predicate therefore stays active until its horizon ends.

Physical reclamation differs by tier.

**Hot and warm tiers.** Compaction rewrites affected local segments. The first
target is 24 hours. This work stays local and bounded.

**Cold tier.** A segment encrypts under one project key. Destroying that key
erases the whole project instantly and reads nothing back.

There are no per-end-user keys. An earlier draft proposed them, so that an
erasure of one person would not rewrite cold objects. That costs a key for
every person and a key lookup for every person on a cold scan. The project
refused the trade.

### What end-user erasure actually promises

An erasure request for one end user is immediate and logical everywhere:

- the tombstone hides matching rows in every tier at once;
- the predicate stays active, so late arrivals never become visible;
- hot and warm segments rewrite within the 24-hour target;
- cold bytes physically disappear when normal retention expires them.

**TallyOwl does not promise immediate physical destruction of one end user's
cold data.** It promises immediate logical erasure and physical reclamation at
retention.

State that plainly wherever the product describes erasure. An operator whose
obligation needs faster physical destruction sets a shorter cold retention, or
does not enable the cold tier. TallyOwl does not guess at a jurisdiction. See
D30.

The default immutable-backup horizon is 30 days. It is configurable. An erasure
ledger is part of each restore. The ledger prevents a restore from making
erased end user data visible again.

TallyOwl stores the end-user ID that the app supplies. TallyOwl does not
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

Consent applies at the point where campaign data joins an identified end user. An
identified end user is personal data. The applicable policy then controls that
join, the retention, and any erasure.

TallyOwl does not guess a jurisdiction and does not change behavior by
geography. TallyOwl is not the policy authority for an application.

An application that must collect less configures that in its own collection
policy. The policy controls enabled telemetry kinds, campaign linking, and
property filtering. The operator makes that choice with knowledge that TallyOwl
does not have.

Consent state still travels with an applicable event and TallyOwl stores it, so
a later policy can act on it.

One boundary the implementation must respect: a session ID links touches
over time. That link is what makes attribution work and it is also the point
where a stricter policy applies. Keep campaign linking usable without a session
link, so an operator who turns off session-linked campaign data still measures
campaign performance.

The reference application must exercise both configurations. See
[TESTBED.md](TESTBED.md).

### How the implementation reads this

Phase 9 built it. Three rules come out of the paragraphs above and each one is a
behavior rather than a preference:

1. **Campaign linking has three levels**, and the middle one is the boundary
   this decision names. See [POLICY.md](POLICY.md) section 7.1.
2. **An absent consent state is not a refusal.** An application that never sent
   a consent state has not refused on behalf of its person, and TallyOwl does not
   guess. An installation that wants the stricter reading turns it on, which is
   the operator making the choice with knowledge that TallyOwl does not have.
3. **The consent state is stored whatever the setting is**, so a policy that
   changes next month can act on what arrived this month.

## D31. Package, registry, and support window — Accepted

D14 accepts the license. This decision holds the distribution items.

**Before the release candidate:** there was no client compatibility window. The
project was pre-alpha. The head did not have to accept an old client, and a
protocol change did not need a migration path.

**At the first release, 2026-08-12,** the four items this decision reserved
are selected. The tree carries one version everywhere, and `./tools.sh version
check` fails the build when two version sites disagree.

| Item | Selected |
| --- | --- |
| Version numbers | `semver-tags` computes them from the conventional commits since the last tag, as every repository here does. The first published release is 0.2.0: semver-tags starts an untagged repository at 0.1.0, and the commits in this one ask for a minor release |
| Package coordinates for TypeScript | npmjs, public access, in the organization's own scope: `@catalystcommunity/tallyowl-browser`. The publish is **staged**, and a maintainer approves it with 2FA. This needs npm 11.16 or newer, so the pinned Node is one that carries such an npm (L190) |
| Package coordinates for Go | The module path is the repository path. The app driver is `github.com/CatalystCommunity/tallyowl/packages/driver-go`, and a generated client is `github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-<name>-api`. Each module needs its own tag, which is the module directory and then the version |
| Package coordinates for Rust | **Deferred.** The owner turned crates.io off on 2026-08-12 until the release pages have proved themselves. A Rust application depends on the driver by Git revision meanwhile, which is how this repository already depends on csilgen and Corndogs. `./tools.sh release crates-plan` says what turning it on would publish, in push order. See L167 and L182 |
| Container registry | `containers.catalystsquad.com/public/catalystcommunity/tallyowl`, one image for the head and the collector. It is the registry every other service here uses, on the grant that already exists |
| Chart registry | The `catalystcommunity/charts` repository, which serves the packaged charts, and a GitHub release beside each tag |
| Binaries | The GitHub release page: one archive for each platform, holding both services and the license, with a `SHA256SUMS` beside it. The binaries come out of the image that was built, so a downloaded TallyOwl and a deployed TallyOwl are one build |
| Protocol versions the head accepts | The current version and the one before it, **enforced**: a driver and a collector each declare what they speak, collector intake and the head each check it against one list in `tallyowl_wire::protocol`, and a refusal names both ends and is counted. Support for a version ends one minor release after the release that replaces it, and the release notes say so before it ends. There is one protocol version today, so nothing a current client sends is refused. L183 |

**The version shape** is three numbers, with `-rc.N` available for a candidate.
Cargo, npm, Helm, and a Go module tag all read that shape the same way. A
release tag is the version with a `v` in front.

**The release is automatic.** A merge to main runs every gate and then the
release job, which writes the computed version into the tree, commits it, tags
it, and publishes. This is the one place where CI writes to the source, and the
owner accepted that exception on 2026-08-12: the alternative is a person
writing one version into nineteen files. See CI-CD.md section 1.

The support window now matters more than usual. An application compiles the
TallyOwl ingest schema into its own build. An application therefore upgrades on
its own schedule, and version skew across applications is normal.

See `docs/RELEASE_NOTES.md`, `docs/CI-CD.md` section 6, and L173 to L180.

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

### Resolved upstream, 2026-07-28

The sweep was never the expensive part. Decoding payloads during the sweep was.

| Live tasks | 256-byte payload | 512 KiB payload |
| --- | --- | --- |
| 1,000 | 6.1 ms | 3,272 ms |
| 5,000 | 22.0 ms | 16,792 ms |

The file backend stored a task as JSON, and Go encodes a `[]byte` field as
base64. Every sweep decoded every live task and its payload only to discard it.

TallyOwl reported this to the Corndogs project, which fixed both the cause and
a related one. A payload now lives in its own bucket as raw bytes. A
deadline index makes the sweep seek to the first expired entry and stop at the
first live one.

Measured after the change, with 512 KiB payloads: 2.3 milliseconds at 1,000
live tasks and 2.3 milliseconds at 5,000. The cost is flat, because it follows
expired tasks rather than live ones.

TallyOwl therefore needs no spool, no dedicated Corndogs, and no workaround.
The forwarder calls `CleanUpTimedOut` on its interval, and D4 stands as
written.

See [BENCHMARKS.md](BENCHMARKS.md) sections 11a and 11d.

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

Attribution is a pure function over immutable touches. A model parameter is
configuration, not code, and not a schema decision.

An operator configures these values for each project:

- the position-based weights;
- the time-decay half-life;
- the lookback window;
- the enabled models.

A change to a parameter recomputes the result. Raw touches never change.
Each result names its model, its model version, and its settings version.

### The shipped default values

**Phase 9 selects these.** Each one holds for a stated reason, because a default
that nobody can argue with is a default that nobody can change with confidence.

| Value | Default | Why |
| --- | --- | --- |
| First-touch position weight | 0.4 | The first touch is the discovery |
| Last-touch position weight | 0.4 | The last touch is the decision |
| Middle share | 0.2, divided equally | What the touches between them did |
| Decay half-life | 7 days | A touch a week before a purchase earns half of what a touch on the day earns. A shorter value hides everything but the last week; a longer value makes the decay model behave like the linear model |
| Lookback window | 30 days | Most purchase decisions fit inside it, and a person can hold a journey of that length in their head |
| Touchpoint retention | 90 days | Three times the lookback, so an operator can make the window wider two times before the coupling below refuses the query |
| Enabled models | All six | A model that nobody enables cannot be compared with the one they do enable, and the comparison is the reason there are six |

The position model has two special cases:

- **one touch** takes all of it;
- **two touches** divide what the two ends were given, in proportion. There is
  no middle to hold the rest, and an operator who set 0.4 and 0.4 meant the two
  ends equally.

An operator changes any of these for a project. A change recomputes every later
result and rewrites nothing.

### Lookback and retention

The lookback window must not exceed the touch retention for that project.

If touches age out inside the window, attribution moves credit to later
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

### Measured, 2026-07-26

xxHash3-64 reaches 9,112 MB each second over 64 KiB pages. BLAKE3-256 reaches
3,857 MB each second. xxHash3 is 2.4 times faster.

Both rates are far above the device write rate, so a single-function design
using BLAKE3 everywhere would not create a throughput problem on this hardware.

This decision therefore stands on its stated reason and not on speed: a content
address needs collision resistance and a page checksum does not.

See [SEGMENT_FORMAT.md](SEGMENT_FORMAT.md) and [BENCHMARKS.md](BENCHMARKS.md)
section 7.

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

## D47. Group-commit linger — Accepted

The append log seals a group after a configurable linger. The default is
2 milliseconds.

A measurement made this a decision rather than an implementation detail. The
device ceiling on the reference hardware is 186 fsync operations each second.
The same append path reached:

| Mode, 128 concurrent writers | Frames each second | p99 |
| --- | --- | --- |
| One fsync for each frame | 150 | 23.3 s |
| Group commit, no linger | 503 | 1.53 s |
| Group commit, 2 ms linger | 16,923 | 9.4 ms |
| Group commit, 10 ms linger | 7,721 | 18.6 ms |

Without a linger a group holds only what arrived while the previous fsync was
in flight. The linger is what makes a group large.

A longer linger is worse. At 10 milliseconds the device sits idle between
syncs, and both throughput and latency degrade.

The committer must release its lock while it writes and calls fsync. A
committer that holds the lock across the sync prevents accumulation and defeats
the mechanism. A prototype made that mistake and reported no gain at all.

A group has a byte bound and a count bound as well as the time bound. A large
group therefore cannot exhaust memory or hold a caller past its deadline.

This value is for the TallyOwl append log. The Corndogs setting of the same
name is a separate decision with a different measured answer. See D33 and
[BENCHMARKS.md](BENCHMARKS.md) sections 11b and 13.

## D48. Collector deployment unit — Withdrawn

This decision co-located intake and forwarder, because a node-local payload
spool cannot be read by a forwarder in another pod.

The spool is gone. D4 explains why: Corndogs commit `b8c10b0` removed the
reason for it. Nothing else required co-location.

Therefore the original rule stands. Intake, forwarder, and compatibility
receiver are independently deployable, and they coordinate through Corndogs
rather than pod-local state.

Two consequences of the withdrawn decision also go away:

- a collector holds no durable state, so it needs no persistent volume and
  stays a stateless Deployment;
- payload durability is Corndogs durability again, so `durable_copies` means
  what D4 says it means.

The record stays here rather than disappearing, because the reasoning was
correct for the design it addressed. A node-local spool breaks independent
deployability. Co-location was the right trade while the spool was necessary.

## D49. Row-group size and reader coalescing — Accepted

A row group targets 8 to 16 MiB of uncompressed column data. A 256 MiB segment
therefore holds roughly 16 to 32 row groups.

A cold reader merges adjacent page ranges when the gap costs less than a round
trip.

The measured reason: row-group count sets the cold request count for a column
read. At 16 columns in a 256 MiB segment, 256 row groups cost 256 requests and
3.8 seconds at 15 milliseconds of latency. Sixteen row groups cost 16 requests
and 240 milliseconds.

The coalescing rule matters more than the size. One round trip costs about 15
milliseconds, while 16 MiB of unwanted bytes costs about 160 milliseconds at
100 MB each second. A reader should therefore merge across a gap of roughly
1.5 MiB or less. The measured latency and bandwidth of the configured object
store give the exact threshold.

Both values are configurable. A larger row group costs more decode memory and
gives coarser statistics pruning. Do not raise it to remove round trips that
coalescing already removes.

See [BENCHMARKS.md](BENCHMARKS.md) section 9 and
[SEGMENT_FORMAT.md](SEGMENT_FORMAT.md).

## D50. Approximate measure algorithms — Accepted

An exact distinct count over a long window cannot merge from partial states
without moving raw rows, and QUERY.md forbids that. A sketch is a bounded
summary that answers such a question approximately and merges.

Selection rule: **prefer accuracy over speed, provided the summary merges and
persists.** A sketch that cannot merge is useless to a distributed query. A
sketch that cannot persist cannot back a rollup.

| Measure | Algorithm | Why |
| --- | --- | --- |
| `count_distinct_approx` | HyperLogLog++ | Merges, persists, and states an error for a given precision |
| `quantile_approx` | DDSketch | States a guaranteed relative error |
| `top_k_approx` | Space-Saving | Merges, and bounds its error by counter count |

D21 requires every approximate result to name its method and its error bound.
An algorithm that can state only an empirical accuracy cannot satisfy that
rule. t-digest is more common in observability and is excellent at an extreme
tail, and it cannot state a bound. DDSketch can.

Because accuracy comes before speed, size each sketch for accuracy and not for
footprint:

- HyperLogLog++ precision defaults high enough for an error near 0.5 percent,
  rather than the common 2 percent;
- DDSketch relative accuracy defaults tight, and the result reports the value
  that applied;
- Space-Saving keeps enough counters that a normal top-k result is exact.

Every value is configurable for a project that prefers a smaller footprint. A
result always names the value that produced it.

An exact measure never falls back to a sketch. D21 already requires that, and
this decision does not weaken it.

## D51. Saved query versioning — Accepted

A saved query records the algebra version of its author. It always executes on
the current algebra.

QUERY.md section 16 forbids changing the meaning of an existing operator and
permits only optional additions. Running on the current version is therefore
safe, and the recorded version serves diagnosis.

TallyOwl does not keep old execution paths alive. Pinning would build machinery
to survive a violation of section 16. Enforce the rule instead.

**Enforcement.** Golden query tests replay saved trees across versions and
require identical results. A change that alters an existing operator's meaning
fails the build rather than silently moving a number on a dashboard.

Before beta, the project can break this contract along with any other. Section
16 becomes binding at beta, and the golden tests become a release gate at the
same point. See D31.

## D52. Query explain output — Accepted

An explain operation returns two layers, and it always returns both.

**The plan tree.** Full detail for each node:

- the operator;
- the estimated rows and bytes;
- the segments it would touch;
- whether it reaches the cold tier;
- the indexes it would use;
- the exactness of each measure.

An engineer reads this layer. It carries the verbosity that a serious query
tool needs.

**The summary.** Plain language derived from that tree. A person uses it to
decide whether to run the query, to adjust it, or to drop it. They never have
to read the tree.

A summary states the cost in human terms, names the largest contributor, and
suggests the adjustment that helps most. An example: this query reads about
40 GiB across 8 months. Most of that is cold storage. A shorter time range
reduces it most.

The second layer is the point of this decision. A plan tree that only a
specialist can read pushes every cost question to that specialist.

An estimate that the planner cannot make returns unknown. It never returns a
guess, because an operator will build a budget rule on whatever number appears.

## D53. Durability and convergence, not recovery objectives — Accepted

An earlier draft asked for a recovery point objective and a recovery time
objective. Those are the wrong artifact for this design, and this decision
replaces them with three statements.

**A process crash is not a recovery event.** Every write is atomic. A restart
loses nothing that TallyOwl acknowledged, and it needs no recovery procedure.
There is no recovery time to state, because there is no recovery.

**Storage redundancy is an infrastructure requirement.** The durability of a
volume or a bucket belongs to the operator's infrastructure. TallyOwl states
what it needs, such as a volume that honours fsync and an object store with its
own durability guarantee. TallyOwl does not restate another product's
durability numbers as its own objective. See [DEPLOYMENT.md](DEPLOYMENT.md).

**Region loss is a convergence problem.** A multi-region installation is
eventually consistent across regions. The useful statements are how divergence
stays bounded, how convergence proceeds, and how an operator observes it
through watermarks and replica lag. A region does not recover to a point in
time. It converges.

What TallyOwl therefore owes an operator:

- the durability that each receipt policy gives, which D27 states;
- the infrastructure that each profile requires, which DEPLOYMENT.md states;
- the convergence behaviour and the metrics that show it, which CELLS.md
  states.

What an operator owes themselves: a backup policy and a storage class that meet
their own obligations. TallyOwl cannot choose those.

## D54. Plain language for anything a person reads — Accepted

A person who does not operate TallyOwl must understand every message that
reaches them. An error, a health state, or a status on a screen reaches an
executive, a support conversation, or a customer email.

This governs error codes and messages, health states, dashboard labels, and
notification text. It does not govern internal storage terms such as tablet or
locator run, which no such reader sees.

The word "actor" failed this rule and is gone. An **end user** is a person or
account that a customer's application identifies. An **operator** or a
**member** signs in to TallyOwl.

[CONVENTIONS.md](CONVENTIONS.md) holds the rule and the vocabulary.

## D55. One entry point for build, test, and generate — Accepted

`tools.sh` at the repository root is what a person types. It is a thin
dispatcher and holds no logic.

Orchestration lives in Python modules that use the runnerlib event lifecycle.
Reactorcide jobs call the same modules, so local and CI cannot drift.

`uv` provisions Python. A developer needs `uv` and nothing else for any verb
that does not need a cluster.

An earlier rule forbade every `.sh` wrapper. That rule aimed at shell
orchestration, and it caught a dispatcher that every other repository here
already has. The rule now forbids logic in shell rather than shell itself.

The entry point exists to hold two rules in code rather than in prose:

- csilgen runs from `csil/`, because an `include` resolves against the working
  directory;
- csilgen is a pinned release rather than a local build, once csilgen publishes
  releases.

## D56. Test data generation and branch coverage — Accepted

Tests generate the data they need. A `DataUtils` helper exposes a
`Create<Thing>(setup)` call for each stored kind. It fills every field that the
test did not name, so a test declares only what it cares about. Other
repositories here already use this pattern.

Rules:

- run inside a transaction and roll it back;
- prefer in-memory storage, so the suite stays fast;
- do not mock the storage interface. TallyOwl owns that interface, so a mock
  would only prove that the mock behaves like the mock;
- cover branches rather than the happy path, and exercise every decision except
  operating-system logistics such as socket creation.

The reference application is the system level and does not replace this. See
[TESTBED.md](TESTBED.md).

Scenarios grow. Start with enough to prove the ledger mechanism, then add one
for each capability as its phase permits.

**Turn every regression into a permanent scenario.** A defect then cannot
return quietly, and the set becomes thorough without anyone predicting where
the defects appear.

## D57. Integrity checking is an operator choice — Accepted

Checksums existed at every level and nothing read them until a query touched
the data. A cold segment could be damaged for its whole retention period
undetected.

`integrity.mode` selects one of three levels:

| Level | What it catches | Cost |
| --- | --- | --- |
| `none` | Nothing | None |
| `verify-on-read` | Damage in data a query touches, when it touches it | A checksum over bytes already in memory |
| `scrub` | Damage anywhere | Continuous background read IO |

**`verify-on-read` is the default.** A wrong answer is the failure this project
most wants to avoid, and BENCHMARKS.md section 7 measured xxHash3 at 9,112 MB
each second, far above the device read rate. The default costs almost nothing.

**`none` is a legitimate choice, and TallyOwl permits it.** An installation
that wants the last of the read throughput, and accepts a wrong answer over a
damaged page, may select it. The dashboard shows that integrity checking is
off, because an operator who inherits an installation must not have to discover
that.

**`scrub` adds detection everywhere and repair where a second copy exists.** At
one copy, which is the home profile, it is detection only. That is still worth
having: knowing on the day beats knowing during an incident. The alert says so
plainly rather than implying a repair that cannot happen.

A query over a damaged segment returns `incomplete-result` and names what it
could not read. It never silently returns a smaller answer.

See [FAILURE_MODES.md](FAILURE_MODES.md) section 5.

## D58. Quorum loss: restore by default, unsafe recovery behind a flag — Accepted

A tablet that loses two of three voters permanently has no safe automatic
answer. The surviving replica may hold a log behind the last committed entry,
and nothing can determine which writes are missing.

**Restore from a snapshot is the default and the documented path.** It never
loses an acknowledged write that the snapshot covers. It loses everything after
the snapshot that Corndogs no longer holds, and it takes as long as a restore
takes.

**Unsafe recovery exists behind an explicit flag.** It forces a single-voter
membership from the surviving log and returns the tablet in minutes. It can
lose an acknowledged write, and it cannot say which.

The rejected option was to ship only one of these. Restore alone leaves an
operator with no answer when the outage cost exceeds the data cost. Unsafe
recovery alone makes a data-losing command the ordinary path.

A fast path that hides its cost becomes the habitual path, so this one cannot
hide its cost:

- explicit confirmation naming the tablet;
- an audit record in the `audit` retention class;
- the affected time range marked degraded, reported by every overlapping query
  and its explain output;
- the mark never expires on its own. Clearing it records who accepted the loss.

See [FAILURE_MODES.md](FAILURE_MODES.md) sections 6.2, 11 procedure 3, and 11
procedure 4.

## D59. Catalog snapshots, off by default — Accepted

STORAGE.md section 3.3 says a repair command rebuilds the segment catalog by
scanning manifests. That is true and it covers one of the twelve things the
catalog holds. Receipts, tombstones, auth, node fencing state, sessions, and
saved dashboards are not in any segment manifest.

Two of those matter beyond inconvenience: **lost tombstones resurrect erased
data**, and **lost receipts duplicate on retry**.

`catalog.snapshots.enabled` turns on periodic catalog snapshots, and it is
**off by default**. The default recovery for a lost catalog is a restore from
the ordinary backup. `catalog.snapshots.keep` retains a fixed number, and the
default is 2 whenever snapshots are on. Two survives a snapshot that is itself
damaged, which one cannot.

This is deliberately small. Continuous change shipping and a recovery point
measured in seconds are a later feature, and nothing here prevents adding them.

The tombstone consequence is not left to this decision. D28's erasure ledger is
durable independently of the catalog and survives a rebuild, because an erasure
that a rebuild can undo is not an erasure. See
[FAILURE_MODES.md](FAILURE_MODES.md) sections 7 and 9.

## D60. A slow node alerts and says why — Accepted

A dead node is easy. A node that answers slowly holds up writes while looking
healthy, and a check that asks only "are you there" says yes.

`placement.slowNode.action` selects `alert` or `demote`. **`alert` is the
default.** Automatic demotion during a network-wide slowdown cascades: every
node looks slow against a moving median, and membership churns while the real
fault is elsewhere. `demote` is available for an operator who accepts that.

**The alert must name a cause.** A slow node that reports only "slow" sends an
operator to look at the wrong thing. The node reports the most specific cause
it can establish, from device errors, device saturation, latency without error
or saturation, write volume, compaction pressure, memory pressure, and network
latency.

**`unknown` is a valid answer and must be reported as one.** A node that
guesses a cause it cannot establish sends an operator down a wrong path, which
is worse than sending them nowhere. The alert then carries the raw evidence:
append latency, fsync latency, queue depth, and accepted bytes, each against
the group median.

This is an instance of the CONVENTIONS.md section 1 rule. "Slow for an unknown
reason, and here is the evidence" is a better message than a confident wrong
one.

See [FAILURE_MODES.md](FAILURE_MODES.md) section 6.1.


## D61. Segment encryption keys — Accepted

D28 defers this: "The key design is separate work and gates cold tiering for a
project that permits erasure." This decision is that work. It changes nothing
D28 decided; it says how.

### One key for each project

D28 already refused per-end-user keys, because that costs a key for every person
and a key lookup for every person on a cold scan. This decision does not reopen
it. `docs/PLAN.md` Phase 3 said "per-end-user key material", which contradicted
D28; the plan is amended rather than the decision.

**What this gives, stated plainly.** Destroying a project key erases that whole
project instantly, and an object-store reader without the key reads nothing
useful. It does **not** physically destroy one end user's cold bytes. An
end-user erasure stays what D28 says it is: immediate and logical in every tier,
hot and warm rewritten within the 24-hour target, and cold bytes reclaimed when
retention expires them.

### Where a key lives

A project key is generated locally and stored in the catalog, wrapped by an
installation root key. The root key is a secret reference in configuration —
`file:`, `env:`, or a secret store — and CONVENTIONS.md section 5 already
requires that a secret is a reference and never a value.

The alternatives were an external key manager, required or optional. Required
contradicts D1 and STORAGE.md section 1: the home profile runs one binary and one
data directory with no external service. Optional, behind a seam with one
implementation, is speculative generality of the kind D25 warns against, and can
be added when a second implementation exists.

**What this asks of an operator.** DEPLOYMENT.md section 5a gains one
requirement: the installation root key is theirs to protect and to back up. A
data directory restored without its root key holds unreadable cold segments.
That is the same property that makes erasure work, and it cuts both ways.

### What is encrypted

The data region and the index region. The prologue, the header, and the footer
stay readable, so a reader still prunes by project, kind, and time without a key,
and the tablet locator is local and unencrypted so routing still works.

The index region is encrypted because it holds fingerprints of end-user,
session, request, and trace identifiers. D9 makes the end-user ID an erasure
key. A readable fingerprint index sitting in a bucket would let anyone with
object-store access enumerate and correlate exactly the values erasure exists to
make unreadable, so leaving it in the clear would weaken the guarantee this
decision exists to provide.

### Rotation

A project key has generations. Each encrypted segment names the generation it
used, so rotation writes a new generation and leaves already-written objects
readable until retention expires them or compaction rewrites them. Destroying a
project destroys every generation.

Rotation without generations would mean re-encrypting every cold object on the
spot, which is the cost D28 refused for per-end-user keys and refuses again here.

### Destruction

Destruction removes the wrapped key from the catalog and writes the destruction
to the erasure ledger, which is durable independently of the catalog and travels
with a snapshot and a restore. FAILURE_MODES.md section 9 rule 3 gives the
reason: an erasure that a rebuild can undo is not an erasure. A restored catalog
therefore cannot resurrect a destroyed project key.

### What this does not decide

The cipher and the nonce discipline are implementation choices and belong in the
implementation log rather than here, because changing them changes no promise
this decision makes and no document that describes one.

See [SEGMENT_FORMAT.md](SEGMENT_FORMAT.md) section 11 and D28.
