# Alpha report

What was built, what was measured, and what was not built. Read
[IMPLEMENTATION_LOG.md](IMPLEMENTATION_LOG.md) beside this: it holds the
reasoning, and this holds the state.

**Phase 6 is built and every one of its four exit criteria passes.** Metrics,
both compatibility receivers, self-observation, the metric query operators,
golden signals, and downsampling are in, with tests.

**Six defects were then found and fixed, four of them in the foundation.** A
decompression limit was refusing intact pages and making queries answer smaller
than the truth (L077); an idle head never sealed, so a quiet installation kept
its newest data in the append log forever (L078); a point lookup materialised
the whole range instead of asking the locator, at 1,063 milliseconds (L079);
nothing expired a row, so a home installation grew without bound (L080). The
remaining two were in the measurement harness itself (L082), which is why three
reports called the burst multiplier "not measured".

**An earlier draft of this report said the gate was held open by a duplication
defect. That was wrong, and section 3.4 records what it actually was.** A
Corndogs process started by `go run` outlived thirteen hours of `dev down` and
`rm -rf data/`, holding the delivery queue open on a deleted file, so each new
collector inherited tasks an earlier one had accepted. Every TallyOwl component
was correct: the collector did not count batches it never accepted, the
forwarder delivered each exactly once, and the head committed them as new
because its receipt store had been deleted with everything else. The development
loop is fixed and the measurement is re-run in section 3.

**Phase 7 was built afterwards, at the owner's request, and it is reported
separately.** See [PHASE7_REPORT.md](PHASE7_REPORT.md). It changes nothing in
this report except three things, and they are listed in section 10: the TLS
carrier is duplex, the configuration has ten more settings, and two lint
failures a newer clippy found in `tallyowl-store` are fixed. A home
installation still has one node, one tablet, and one voter, and it starts no
consensus group and opens no replication port.

**Phase 8 was built after that, and it is reported separately too.** See
[PHASE8_REPORT.md](PHASE8_REPORT.md). It adds identity, the four domain
operators, per-user erasure, saved analyses, dashboards, and collection policy,
and it finished Phase 7's three highest-value open items first. Two things in it
reach back into this report's scope and are named here so that a reader is not
surprised:

- **the anonymous identifier is stored**, where it used to be dropped at
  projection. `end_user_id` was already kept as a property and `anonymous_id`
  was not, so an event before an `identify` belonged to nobody;
- **the locator is built in one place**, where it was built in two that had gone
  out of step. A compacted segment was pruned out of every `trace_id` and
  `request_id` lookup that another segment also named, so the answer came back
  smaller with nothing marked incomplete. L102. Nothing in this report's
  measurements is affected — compaction did not run in any of them — but a
  long-running installation would have been.

**Alpha is met.** A clean clone builds, tests, and reaches
a round trip; the reference application's scenarios run and every ledger
assertion passes, now including metric assertions; an event, an error, a trace,
and a metric point each survive an abrupt process kill after acknowledgement; an
exact lookup on a high-cardinality value returns the right rows across more than
one segment; and section 3 holds the load-test results.

## 1. Where each phase stands

| Phase | Gives | State |
| --- | --- | --- |
| 1 | The local development loop and configuration | **Passes** |
| 2 | The contract, generated and verified across languages | **Passes** |
| 3 | Embedded storage: segments, WAL, catalog, indexes, tiering | **Passes** |
| 4 | The durable event vertical slice, end to end | **Partly.** See 1.4 |
| 5 | Errors and traces | **Passes**, with the deferrals in L049 |
| 6 | Metrics and compatibility receivers | **Passes.** See 1.6 |

### 1.4 Phase 4, unchanged from the previous report

Every Phase 4 deliverable is built. The one criterion that does not fully pass
is the last: the reference application's web surface exists and has its own
end-to-end tests, and it does not yet carry a share of a ledger scenario. See
section 4.

### 1.6 Phase 6, deliverable by deliverable

| Deliverable | State |
| --- | --- |
| Counter, gauge, histogram, and exemplar APIs in the first backend SDKs | **Built.** `crates/tallyowl-driver-rust/src/metrics.rs` and `packages/driver-go/metrics.go`, with the same semantics and matching tests in both. An exemplar carries the trace on the point and on the envelope |
| In-process aggregation and snapshot push | **Built.** A thousand calls to one series produce one point. A host calls `publish_metrics` on its own period; the driver owns no timer. See L061 |
| Collector merge plus visible series byte and work budgets, without silently rejecting high cardinality | **Built.** `crates/tallyowl-collector/src/series.rs`. Exact per-project accounting, six settings under `metrics.*`, four counters and two gauges on the exposition endpoint. A refused point is a rejected item with `resource-exhausted` naming the metric; nothing is folded, sampled, or dropped. See L068 and L069 |
| Prometheus and OpenMetrics scrape receiver | **Built.** `crates/tallyowl-compat/src/{exposition,scrape}.rs`. Counters, gauges, histograms, summaries, `# UNIT`, `# EOF`, OpenMetrics exemplars, chunked bodies, and counter-reset detection the exposition format cannot express. No default target |
| OpenTelemetry metric and trace push receiver, normalizing immediately and refusing logs | **Built.** `crates/tallyowl-compat/src/{protobuf,otlp,receiver}.rs`. OTLP over HTTP with a hand-written protocol-buffer reader. `/v1/logs` answers `501` and says what to do instead. No default listener. See L063 |
| Prometheus and OpenMetrics exporter for collector, head, dashboard, query, and storage metrics | **Built**, and it was already there for the services. `Registry::snapshot` now feeds the native path from the same registry, so the endpoint and a TallyOwl chart cannot disagree |
| Recursion-protected internal project for dogfooding self-metrics | **Built.** `crates/tallyowl-collector/src/selfobs.rs` and `crates/tallyowl-head/src/selfobs.rs`. Off by default. The guard suppresses recording for the length of one push. See L072 |
| Metric projection, rollups, query operations, and charting | **Built.** The projection stores the labels, both period ends, and the series key; `rate`, `increase`, `histogram_merge`, and `quantile` are in the executor; `crates/tallyowl-head/src/rollup.rs` holds the rollups; the dashboard charts a rate and chooses its metric from what the head holds |
| Service-operation golden signals derived from spans | **Built.** Three series for each service operation, derived on commit, as ordinary metric points. Saturation is deliberately not derived. See L070 |
| Downsampling and retention tiers | **Partly.** The downsample pass produces the coarser points and the four retention classes are settings with their coupling refused at startup. **Nothing expires an expired row.** See L071 and section 4 |

Phase 6 exit criteria:

| Criterion | State |
| --- | --- |
| Restart and reset semantics for cumulative counters are correct | **Passes.** A cumulative point carries `start_at`, so a restart is a later start beside a smaller value rather than a counter running backwards. Covered in both drivers, in the scraper, and in the executor: `a_counter_reset_counts_the_new_value_rather_than_a_negative_step`, `a_value_that_falls_with_the_same_start_is_still_read_as_a_reset`, `a_counter_that_falls_moves_its_start_forward` |
| Histogram merge and quantile tests cover incompatible buckets | **Passes.** `histogram_merge_over_two_bucket_layouts_fails_and_names_both` and `a_quantile_over_two_bucket_layouts_fails_for_the_same_reason_a_merge_does`. The refusal names both layouts and says TallyOwl will not rebucket. The same rule holds in the collector merge and in the downsample pass |
| High-cardinality series keep correct values under configured resource limits | **Passes.** `a_high_cardinality_metric_inside_the_budget_is_admitted_exactly` admits 5,000 unique-label series and counts them exactly; `a_series_budget_refuses_in_the_open_and_keeps_every_admitted_series_correct` proves an admitted series keeps counting after a refusal, and that nothing became an overflow series |
| Exhausted limits cause explicit backpressure | **Passes.** `a_metric_point_past_the_series_budget_is_refused_by_identifier_and_the_batch_commits`: the over-budget point comes back on the receipt by event ID with `resource-exhausted`, the message names the metric, and the rest of the batch commits |

## 2. What exists

| Path | Holds |
| --- | --- |
| `crates/` | 11 hand-written Rust crates: config, obs, rpc, wire, store, **compat**, collector, head, driver-rust, golden, export |
| `packages/` | The Go app driver, the TypeScript browser package, and the TypeScript dashboard |
| `testbed/` | The reference application, its Go backend and web surface, its TypeScript web application, the ledger, the simulator, and the load harness |
| `generated/` | 3 packages for each of Rust, Go, and TypeScript. Committed, never hand-edited |
| `csil/` | 4 specifications. **Unchanged by Phase 6**: `MetricPointPayload` was already in the contract and it needed nothing |
| `charts/` | Two charts and their values, with a parity test. **Still no templates** |
| `tools/`, `tools.sh` | The task runner. Python behind a thin dispatcher |
| `prototypes/` | 9 benchmarks in their own Cargo workspace. Never built by a product build |

**875 Rust tests, 52 Go tests, and 32 TypeScript tests pass.** Lint is clean in
all three languages, `cargo fmt --check` passes, and `./tools.sh gen-check`
passes. Phase 6 and the six fixes after it added 193 Rust tests, 15 Go tests,
and 5 TypeScript tests.

**The contract did not change.** That is worth saying plainly: Phase 6 is the
metrics phase and it needed no CSIL change, because `MetricPointPayload`,
`HistogramValue`, and `MetricKind` were designed in Phase 2 and were right. No
csilgen request was opened and none was needed.

`tallyowl-compat` is the one new crate. It holds both receivers and the shared
series key, and it depends on nothing outside the workspace except `getrandom`.

## 3. Load-test results

Re-run against this build on 2026-08-04. `docs/BENCHMARKS.md` section 17 holds
the whole method; this is the summary and the two findings.

**Hardware.** AMD Ryzen 7 5800X, 8 cores and 16 threads, 125 GiB of memory.

**Filesystem.** ext4 on an NVMe device, `/dev/nvme1n1p2`. **Not tmpfs.** Section
9 of the implementation prompt applies and was followed.

**Build.** `--release`. **Profile.** `home`. The harness is `testbed/cmd/load`,
seed 20260803, sending through the maintained Go app driver.

### 3.1 The measures

From an empty `data/` and a Corndogs this session started. Four earlier runs
inherited a queue and their numbers are kept in `BENCHMARKS.md` section 17 with
that caveat.

These are the numbers after the six fixes; `BENCHMARKS.md` section 18 holds the
comparison, and section 17 holds what they were before.

| Measure | Phase 6, fixed | Phase 5 | Reading |
| --- | --- | --- | --- |
| Sustained events each second, to first loss or refusal | **37,464** | 67,624 | Reached the top of the ramp with nothing refused. The 67,624 was the collector's burst-absorption figure, not a steady rate: see 3.2 |
| One synchronous producer | **19,547** | 19,534 | **Unmoved**, and stable to a tenth of a percent across five runs. Phase 6 put a series ledger and a merge pass on the same path and neither is measurable on an event workload |
| Items accepted against events committed | **440,522 and 440,522**, a ratio of **1.0000** | 543,994 and 543,994 | Nothing lost and nothing duplicated |
| **Burst offered, unpaced** | **39,714 each second, none refused** | Never offered | **Measured at last.** Three reports called this absent and one called it 0.71; all of them measured a harness that paced itself. See L082 |
| Bytes on disk for each accepted event, **settled** | **176.3** whole directory, **136.4** of it catalog | 215.8 | The first steady state this project has measured: every row sealed, 8 bytes left in the log. A sealed row costs **39.9** against the 39.75 the envelope predicted |
| Point lookup, p50 and p99 | **41.0** and **42.0** ms | 103.0 | **26 times faster.** The executor asks the locator instead of materialising the range. See L079 |
| Aggregate, p50 and p99 | **104.0** and **120.3** ms | 102.0 | Unmoved, and it should be: an aggregate over a range reads the range |
| Candidate segments for a high-cardinality lookup | **3,000 scattered, 228 with user-grouped compaction** | Not measured | `prototypes/locator-bench`, run at target scale. See BENCHMARKS.md section 18.3 |
| Candidate segments for each high-cardinality lookup | **Still not usefully measured.** 2 sealed segments | 3 | Unchanged. `prototypes/locator-bench` is the right place |
| Recovery after an abrupt kill under load | **314 ms**, no loss, **and no refusal** | 462 ms | See 3.3 |

### 3.2 The sustained figure is a range, and the range is the finding

Five runs on one machine: **77,382**, **49,776**, **80,000**, **37,874**, and
**37,464** events each second. The previous report said the published ceiling
was the collector's rather than the system's. This shows what that costs.

A collector acknowledges when Corndogs is durable, and the head drains
afterwards at a flat 17 batches each second. So the collector's rate is bounded
by **how much room the queue has**, and the queue's room is bounded by how far
behind the head is. A run against a drained head reaches the top of the ramp; a
run that starts behind reaches a third of it.

**Two of the five were not measuring what they looked like.** The first two
inherited a Corndogs that outlived every reset (section 3.4), and one of the
others aborted its ramp on scheduler jitter at the very bottom (L082). The last
two are the comparable pair, and they agree to one percent: **37,874 and
37,464**. That agreement is worth more than the spread, because it is the first
time two runs of this harness measured the same thing.

**This makes the collector outage buffer and the ingest ceiling one decision
rather than two**, which is what D10 and D23 now say.

### 3.3 The kill test was better than the last one, on the measure that matters

The head was killed with `SIGKILL` twelve seconds into a run. The producers kept
going and **not one batch was refused**: 610,423 offered, 610,423 accepted,
while the head was not there. The collector's durability boundary is Corndogs
and not the head, and this is the first run that demonstrates it under a real
outage rather than by argument.

The head answered `Ready.` **314 milliseconds** after restart, opening a store
that held eight more rows than the last reading before the kill. Nothing was
lost.

### 3.4 The row count did add up, once the queue was actually empty

The first Phase 6 run reported 7,925 batches accepted, 9,998 delivered, and 1.61
rows in the store for each accepted event. **It was the development loop, not
TallyOwl.**

`./tools.sh dev up` starts Corndogs as `go run main.go run` when no binary is on
the path. `go run` compiles to a temporary executable and runs it as a **child**,
so the loop recorded the wrapper's process identifier and `dev down` stopped the
wrapper. The server kept running, and its open file was

```
data/corndogs/corndogs.bolt (deleted)
```

Every `rm -rf data/` unlinked the path while the process held the inode. One
Corndogs carried the delivery queue for **13 hours** across four supposed
resets, so each new collector inherited tasks an earlier collector had accepted.

**Every component behaved correctly.** This collector never accepted those
batches, which is why they were absent from its count. The forwarder delivered
each exactly once, which is why no batch identifier ever repeated. The head
committed them as new because its receipt store had been deleted along with
everything else. The extra rows were real data from earlier runs.

The clean re-run in section 3.1 correlates end to end: **2,624 batches accepted,
2,624 delivered, every identifier distinct, and the item counter matching the
harness exactly.**

**Two changes came out of it.** `dev up` now starts each service in its own
process group and `dev down` stops the group, so a wrapper's child cannot
outlive it; and `dev down` checks each service address afterwards and names any
process still holding one. Its first run named the orphan and its identifier in
one line.

**The acceptance log line also carried no batch identifier**, so the two halves
of the path could not be correlated until one was added. `Intake::submit` has
two callers and only the RPC handler logged, so a task could reach the queue
with nothing recording which producer made it. Both are fixed. See L074 for the
method, which is the part worth keeping.

### 3.5 The settled store, and a page guard that refused intake's own data

The first measurement of a **settled** store — everything delivered, sealed, and
idle — needed a defect fixed first. Every query answered `incomplete-result`,
and once the refusal was made to name its cause it said:

> A stored page claims to expand far more than real data does. We did not expand
> it.

The page had passed its checksum. A **decompression ratio limit** was rejecting
intact data, because a page where every row holds the same release or service
name compresses to almost nothing — a high ratio is what telemetry looks like.
The limit had already been raised once for the same reason and real data passed
the raised one too. It is removed, and the length check that already existed
catches a lying header. See L077.

**This was the worst class of defect in the product**: every query over the
affected range answered `incomplete-result` for the life of the process, on
ordinary data, permanently, with nothing naming why.

With it fixed, the settled store answered a point lookup at **1,063 ms** at p50,
stable across runs — ten times the last published figure, and the first query
measurement taken with the backlog, the page cache, and the refusals all out of
the way. It was what L045 costs: the executor materialised the rows in the range
instead of using the locator.

**That number is now historical.** Fixing the page guard is what made it
measurable, and measuring it is what made the next fix obvious: the executor
asks the locator for an exact lookup now, and the same query answers in **41
ms**. See L079 and section 3.1. It is recorded here because the sequence is the
point — a false refusal was hiding a real cost, and neither would have been
found without naming the first one.

### 3.6 A damaged segment is now named

A store that had accumulated several overlapping load runs reached a state where
one segment could not be read, and every query over its range then answered
`incomplete-result` for the rest of that process's life. The refusal was correct
and unactionable: nothing said which segment.

`SegmentedStore::unreadable()` now reports the reasons, the head logs them at
start-up, and a lookup that finds damage records the reason rather than only
setting the flag. `docs/FAILURE_MODES.md` procedure 6 requires the naming and it
was missing.

## 4. What is not built, and where it goes

| Not built | Where it goes | Why not |
| --- | --- | --- |
| OTLP over gRPC, and the OTLP JSON encoding | Beside `crates/tallyowl-compat/src/receiver.rs` | An exporter reaches this receiver with one environment variable. See L063 |
| A native exponential histogram | `csil/tallyowl-ingest.csil` and the executor | Converting one to explicit bounds would invent bounds nobody chose. See L064 |
| A TLS scrape | `crates/tallyowl-compat/src/scrape.rs` | An `https` target is refused rather than reached in the clear. `rustls` is already a workspace dependency. See L073 |
| The reference application's rich-client, mobile, and terminal surfaces | `testbed/richclient/`, `testbed/mobile/` | Time. The web surface is built |
| The web surface inside the scenario runner | `testbed/cmd/run-scenario` | The scenario stream is driver-shaped and the web path is ingest-shaped |
| Phase 7 in full | See PLAN.md | Not started, and it should not be |
| Pipelining over a TLS connection | `crates/tallyowl-rpc/src/tls.rs` | A duplex carrier over a split rustls connection. See L058, and it must be fixed before Phase 7 measures the replicated write path |
| A project selector in the dashboard | `packages/dashboard/` | The dashboard reads the first project the session can read |
| Exact retention, to the row | `crates/tallyowl-store/src/compact.rs` | Expiry drops or rewrites a **segment**, so a row lives past its retention until the segment holding it is rewritten. Bounded by the segment's time span. See L080 |
| User-grouped cold compaction | `crates/tallyowl-store/src/compact.rs`, in `group_by` | The measurement is done and says it takes the locator from 28.81 GiB to 2.44 at target scale. It is the largest remaining win and nobody has built it. See BENCHMARKS.md section 18.4 |

## 5. Everything marked `Revisit: yes`, with a recommendation

**This table is generated against the log and holds every entry marked for
revisit.** An earlier version had drifted: it listed seven entries that were no
longer marked and omitted eight that were. 38 entries, in order.

**It covers Phases 1 to 6 only.** Phase 7 added fourteen log entries, seven of
them marked for revisit, and [PHASE7_REPORT.md](PHASE7_REPORT.md) section 8
gathers those. Two lists rather than one, because the two phases are reviewed as
two things.

Six of them are marked **Superseded**, **Answered**, or **Done**: they are kept
because the log entry still says `Revisit: yes` and a reader following the log
would otherwise look for an open question that is closed.

| Entry | What | Recommendation |
| --- | --- | --- |
| L003 | A recursive CSIL type travels as encoded bytes | **Ask the csilgen maintainer to box a recursive type in the Rust generator.** The only option that costs nothing in Go and TypeScript ergonomics |
| L004 | `package_name` and `go_module` were the wrong way round | Keep the suffix. Two packages of one name cannot share a workspace |
| L007 | Priority defaults are implemented at intake | Keep it. The driver seals a critical event into its own batch, so the case is rarer than it looks |
| L009 | `config check` fails on a key that matches no setting | Keep it. Making start-up refuse would break an upgrade where a chart carries a setting the older binary does not know |
| L012 | The forwarder retries at a fixed first delay, not a full backoff curve | **Superseded.** L043 and L044 built the full curve with jitter |
| L014 | Both charts carry the whole settings tree | **Worth fixing, and it cost thirteen more edits this run.** One setting still needs four |
| L016 | The design documents now trail the code, and here is exactly where | **Superseded.** L045 enforces the depth limit |
| L017 | The contract could not be encoded in two of its three languages | Only if csilgen grows a way to declare a discriminant. Nothing else here should change |
| L018 | The TypeScript transport arrives through the task runner | Publishing the transport to npm is the right answer and is not TallyOwl's to make |
| L022 | What Phase 3 has and has not, and where the rest goes | Settled by L023 on the order. The cold tier stays the largest unbuilt storage surface |
| L025 | DuckDB is a toolchain addition | Keep it. The shared installer already fetches seven toolchains |
| L030 | What the segment format gained that the documents did not ask for | **Measured now.** BENCHMARKS.md section 18.4: rebuilding the locator on compaction is what makes user-grouped cold segments possible, and that is the largest remaining win |
| L032 | Cold tiering verifies by content address rather than by a returned put | Keep the read-back as the default. The optimisation needs a backend that can prove the same thing more cheaply |
| L035 | A snapshot carries the log, and it carries it last | Use a redb-level copy when redb offers one |
| L036 | Disk exhaustion has no behaviour, and a setting nobody reads said so | **Answered.** L037 built the reserve rule and L081 answers the shared-device point it left open |
| L037 | Disk exhaustion, and the reserve rule that shapes it | **Answered in L081.** The hard guarantee holds regardless of sharing; the soft one cannot cross processes, so the device is reported instead. FAILURE_MODES.md section 10 and DEPLOYMENT.md now state it |
| L039 | Credentials are issued, and the head is the only place one is stored | This is the whole of revocation latency. The right number depends on how the owner expects a revocation to be used |
| L044 | Retry stops at an age, and the age is half of a decision nobody has made | **Take it from BENCHMARKS.md section 18.** The outage buffer and the ingest ceiling are one decision, because the collector's rate is the queue's room |
| L045 | The query algebra, and the three rules that shaped it | **Half done.** A point lookup asks the locator now (L079, 1,063 ms to 41). An aggregate still materialises its range, and what it needs is a byte budget rather than a different plan |
| L047 | The scrubber names what it removed | Keep it narrow. A scrubber that mangles ordinary messages gets turned off |
| L048 | A tail decision needs one durable record, and a tombstone is the other half | Both are small. Rebuilding the registry at start-up is the more important one |
| L050 | Authorization, and the three leaks it closes | A project-scoped role may be what an installation with many projects wants. Wider change |
| L051 | The load test found that one producer gets a seventh of the ceiling | **Still the highest-value remaining item**, and BENCHMARKS.md section 18.3 now prices it: the catalog is 3.4 times the data it indexes |
| L052 | Two bytes-for-each-event numbers, and the difference is a missing feature | **Done.** The reclamation is built and L056 and L060 record what it cost |
| L054 | Signing in is not authorization | D7 says an administrator maps each claim to a role, and this does not. It belongs with the wider role work in L050 |
| L055 | Pipelining needed the server half, not only the driver half | **Take the defaults from BENCHMARKS.md section 18.** One producer is stable at 19,547 across five runs and nothing has tried another window |
| L056 | The append log was reclaimed by deleting all of it, including frames nobody had segmented | Three chosen numbers. D36 ties two of them to the outage buffer, which section 18 shows is a burst-duration decision |
| L057 | Role tokens, and the two rules the type system holds rather than a check | An installation-scoped role is the right answer; L050 names it as the wider change |
| L058 | TLS went under the carrier exactly as predicted, and I blamed rustls for my own shortcut | **Fix it before Phase 7 measures replication.** It is a shortcut, not a rustls limit |
| L059 | The dashboard's carrier is one frame in a POST, and it refuses every service but one | CSIL-Events over a WebSocket, when a view needs to poll |
| L060 | The load test caught a regression I had just written | **A segmented append log removes the copy instead of amortising it**, and Phase 7 wants that shape anyway |
| L063 | The OpenTelemetry receiver reads protocol buffers over HTTP, and | **Wait for a real exporter that needs one.** `http/protobuf` is one environment variable, and JSON is the cheaper of the two additions |
| L064 | An exponential histogram is refused rather than converted | **Leave it refused.** An exporter on the default aggregation sends one, so the pressure will come; inventing bounds is worse than refusing |
| L066 | A bucket layout travels as two canonical number lists | **A typed vector column is the right shape** and it belongs with the Phase 3 storage work rather than with metrics |
| L068 | A series budget refuses in the open and never folds | **Measure them.** They are the only Phase 6 numbers that were chosen rather than measured |
| L072 | Self-observation suppresses recording while it publishes | Keep it off by default. One setting turns on both services, and nothing checks that the credential points at a project an operator meant |
| L073 | A scrape is refused over HTTPS rather than reached in the clear | Worth fixing. A scrape target behind a service mesh with mutual TLS is an ordinary deployment |
| L080 | Retention expiry, and the prune that comes before the read | **Accept it for now.** Exactness needs a rewrite of every segment straddling a cutoff, which costs a read of nearly everything |

## 6. Design documents changed

| Document | What changed |
| --- | --- |
| `docs/BENCHMARKS.md` | Section 17 holds the first Phase 6 run and section 17.4 records why four of its numbers describe a system being handed work nobody offered it. **Section 18 is the one to use**: it holds the run after the six fixes, the first steady state, the burst, and the candidate-segment count |
| `docs/DECISIONS.md` D10 and D23 | Both carry the Phase 6 measurements. D10 gains the burst answer and the candidate-segment count; D23 gains the settled disk figure it has been unable to state for four runs |
| `docs/FAILURE_MODES.md` section 10 | Gains the shared-device rule: the reserve belongs to the device and one process enforces it, the hard guarantee is unaffected, and the device is reported so sharing is visible. A code comment already pointed here for it and this document did not say it |
| `docs/DEPLOYMENT.md` | Gains "One installation, one device", the operator half of the same rule |
| `docs/IMPLEMENTATION_LOG.md` | L061 to L082 |

**No design document was contradicted by the code this run.** Phase 6's design
documents — `DATA_MODEL.md` section 3.4, `QUERY.md` sections 7 and 12.7,
`POLICY.md` section 4, and D12 — were specific enough to implement directly, and
several are quoted in the code at the point where they are obeyed.

**One document was silently wrong in the other direction**, which is worse and
worth naming: `crates/tallyowl-store/src/space.rs` said "FAILURE_MODES.md section
10 states the constraint" about the shared-device rule, and section 10 did not
state it. A comment that cites a document nobody checked is how a reader
concludes a question was answered when it was not. Section 10 states it now.

**`docs/DEPLOYMENT.md` section 4 is further behind.** It is a curated list
titled "Values that matter", and this run added thirteen settings under
`metrics.*`, `retention.*`, and `compatibility.prometheus.*`. L014 records that
one setting costs four edits, and this run paid it thirteen more times.

## 7. Blocked

**Nothing.**

Phase 6 needed no csilgen change, no credential, and no external service. The
OpenTelemetry receiver was the one place a dependency looked likely, and section
10.1 of the implementation prompt applied: the payload is a protocol buffer, the
reader is 200 lines, and adding a code generator to the always-on collector for
one boundary was the larger change.

**What is not exercised end to end**: an actual browser round trip against a
running LinkKeys domain, unchanged from the previous report; and an OpenTelemetry
push from a real SDK. The receiver is tested against protocol-buffer messages
this repository writes to the OTLP field numbers, over a real socket, but nobody
has pointed a real exporter at it.

## 8. Known defects, ranked

**Six were fixed after the first Phase 6 report** and are listed in section 8.1
rather than here, because the report that named them is the one a reader will
have seen.

1. **The head commits about 3,600 events each second, and every headline ingest
   number is the collector's.** Section 3.2. The cost is one durable catalog
   transaction for each batch and it grows with the catalog. This is the
   sustained rate, and it is the largest single thing in the way of a bigger
   installation.
2. **The catalog is 3.4 times the size of the data it indexes.** Section 3.1:
   136.4 of the 176.3 bytes each event costs, of which 48.6 are the tablet
   locator. `prototypes/locator-bench` says user-grouped compaction takes the
   locator from 28.81 GiB to 2.44 at target scale, and nothing has built it.
3. ~~**A TLS connection serves one request at a time.** L058.~~ **Fixed** by
   Phase 7, before anything measured replication over it. See section 10 and
   L084.
4. **An aggregate over a large range holds the range in memory.** L045. The
   point lookup no longer does (L079), and an aggregate genuinely has to read
   its range, so what is missing is a byte budget rather than a different plan.
   `ResultMetadata.scanned_bytes` is still reported as zero.
5. **Retention expires a segment rather than a row.** L080. A row inside a
   segment whose newest row is recent lives past its retention until that
   segment is rewritten. It is bounded by the segment's time span and it is not
   exact.
6. **The open-trace registry is lost on a restart.** L048. A trace that had not
   been decided stays undecided, which is safe rather than lossy.
7. **`resolve-key` has no rate limit.** It belongs in the Phase 11 security
   review.
8. **The tail rules are built in rather than compiled from a policy.** D45 says
   they should use the query expression tree, and that evaluator exists.
9. **Three documents carry the setting tree.** L014, and thirteen more settings
   this run.

### 8.1 Fixed after the first Phase 6 report

Each of these was reported as open, or reported wrongly, and is now closed.

| Was | Now | Entry |
| --- | --- | --- |
| A decompression ratio limit refused checksum-valid pages, so every query over the affected range answered `incomplete-result` permanently | Removed. The declared length is checked instead, which catches a lying header without refusing honest data | L077 |
| An idle head never sealed, so a quiet installation kept its newest rows in the append log forever | A background segmenter seals on the interval `max_open_ms` implies. The append log settled at **8 bytes** | L078 |
| A point lookup materialised the whole range: **1,063 ms** | It asks the locator: **41 ms** | L079 |
| Nothing expired a row, so a home installation grew without bound | Expiry by retention class, pruning before it reads | L080 |
| The reserve was per data directory with no answer for a shared device | The hard guarantee holds regardless; the device is now named so sharing is visible | L081 |
| The burst multiplier was "not measured" in three reports | It was the harness pacing itself. **39,714 each second offered unpaced, none refused** | L082 |
| A seal could cover a log frame whose rows it had not taken | An append gate excludes a seal from that window | L078 |
| Self-observation suppressed every thread's metrics, not its own | The guard is thread-local | L076 |
| `Dataset::Events` counted TallyOwl's own derived rollups | It means what its name says, and the testbed no longer filters by hand | L070 |
| A duplication defect was ranked as the reason the gate stayed open | It was a Corndogs that `dev down` never stopped, holding a deleted file for 13 hours | L074 |

## 9. Reproducing everything here

```sh
./tools.sh setup          # toolchains, generation, a local configuration
./tools.sh build          # or `cargo build --release` for the load test
./tools.sh test           # Rust, Go, and TypeScript
./tools.sh lint
./tools.sh gen-check
./tools.sh dev up         # provisions a key and a session, then starts the loop
cargo run -p tallyowl-driver-rust --example send_one_event

# the load test, against the running loop. Start from an empty `data/`.
cd testbed && go run ./cmd/load 127.0.0.1:5100 127.0.0.1:5110 \
    "$(cat ../data/collector.key)" "$(cat ../data/operator.session)"

# the query half alone, against a store that already holds data
LOAD_QUERIES_ONLY=1 go run ./cmd/load 127.0.0.1:5100 127.0.0.1:5110 \
    "$(cat ../data/collector.key)" "$(cat ../data/operator.session)"
```

The load harness records its seed, 20260803, in its own output.

**Turning on what is off by default**, which is how a compatibility edge and
self-observation are meant to be reached:

```yaml
compatibility:
  openTelemetry:
    enabled: true
    listen: 127.0.0.1:4318     # OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
  prometheus:
    targets: ["http://127.0.0.1:9100/metrics"]
metrics:
  selfObservation:
    enabled: true
```

## 10. What Phase 7 changed inside the alpha scope

Phase 7 is reported in [PHASE7_REPORT.md](PHASE7_REPORT.md). Three of its
changes touch things this report covers, and they are listed here so that a
reader of this document is not surprised by them.

**The TLS carrier is duplex.** L058 recorded a defect: a secured connection
served one request at a time, because `tls.rs` used `rustls::StreamOwned`.
`crates/tallyowl-rpc/src/duplex.rs` replaces it, and the server loop and the
pipelining client are now one piece of code for the plain path and the secure
one. Section 8 of this report listed that defect; **it is fixed**. Two tests
cover it. See L084.

**The configuration has ten more settings.** `node.name`,
`node.failureDomain`, `placement.mode`, `placement.splitAbove`,
`placement.mergeBelow`, `placement.concurrentChanges`, `query.maxFanOut`,
`replication.listen`, `replication.peers`, and `replication.writeTimeout`. Every
one of them defaults to what a home installation already did, and three new
cross-setting rules refuse a configuration that would not work. The chart parity
test covers all ten.

**Two lint failures are fixed** in `crates/tallyowl-store`, in
`segment/format.rs` and `space.rs`. A newer clippy found them in files Phase 7
never touched. Both were cosmetic.

**The development loop now says when it is running a stale binary.**
`./tools.sh build` builds debug and `./tools.sh dev up` prefers release, so a
developer who built and started the loop could silently run an older binary. It
was found by exactly that: a round-trip check after Phase 7 passed against a
release build from before Phase 7 existed. The loop now names the binary it
chose and says the other one is newer. L074 records what this class of thing
cost the project once already.

**Nothing else in this report changed.** A home installation is one node, one
tablet, one voter, and `local-one`; it starts no consensus group, opens no
replication port, and uses the same local store it used before.
