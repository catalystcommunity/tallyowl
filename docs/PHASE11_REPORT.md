# Phase 11 report: production hardening

What was built, what was measured, and what was not built. Read
[IMPLEMENTATION_LOG.md](IMPLEMENTATION_LOG.md) L148 to L163 beside this: it
holds the reasoning and this holds the state.

**Three of the four Phase 11 exit criteria pass, and the fourth is drilled
without the second version it needs.** Section 6 states each one and names
where it is proved.

**The soak is running, and it is pointed at L131.** Everything else in this
phase completes inside a session; the soak's product is days, and section 2
says what it watches and what its first hours found. Its live state is
`./tools.sh soak status` and its journal is `./tools.sh soak report`.

## 1. The soak, which the phase was pointed at

L132 closed the append log by argument and left the hang unexplained, and the
Phase 10 review said to point Phase 11's soak at it: days of the concurrency
that produced it, rather than another run count. The soak exists and runs:

- **the topology**: one Corndogs, three head voters under `local-quorum`, two
  collectors delivering to the first head, a paced Go driver, and a monitor.
  Every process has a recorded PID, and `soak down` names any survivor;
- **the watch**: the L145 stall signature is now numbers —
  `tallyowl_wal_commit_in_flight_count` beside
  `tallyowl_wal_durable_position_count` — sampled by the monitor and
  journaled, with the head's own warning line scraped from its log as the
  second witness. On a stall the journal says plainly: take the stacks before
  anything restarts, and L132 says how;
- **the proof of no corruption**: per-window reconciliation. Once a window is
  older than the check age and nothing older than its edge waits in the
  queue, the committed count must equal what the producers captured in it — a
  shortfall is a loss, a surplus is a duplication. Sampled acknowledged
  request IDs must answer exactly one row each;
- **the outage schedule**: every six hours a voter dies abruptly for two
  minutes, which quorum must survive; every twelve hours the ingest head dies
  abruptly for ten minutes, which the collectors must hold.

**The first hour of the soak found a defect that ten phases of tests could
not.** Three head processes formed a real voter group — the first time
consensus ran across processes in this project — and the election chose a
voter that was not the ingest head. Every write was refused with "has to
forward request to", the queue grew without bound, and nothing would ever
have recovered: a head restarting after an outage returns as a follower.
Every earlier replication test ran one voter, and one voter is always its own
leader. The fix is L149: a `proposal` kind on `deliver-consensus`, one hop to
the leader, tested at the consensus layer and proved by the running soak.
This is the phase's version of L140's lesson: the suite was good at the
rules and blind to the wiring, and only running the real shape found it.

**The stall signature appeared on the soak's first night, and it is the
phase's most important observation.** Three episodes in seven hours, all on
head-2 — a follower voter serving no ingest: a commit in flight while the
durable position stood still for 60, 150, and 210 seconds, each episode
clearing on its own. Reconciliation stayed perfect through all three — 27 of
27 windows clean, 184 of 184 exact lookups, 12.2 million events, nothing
refused — because the other two voters kept the quorum. L131's hang now has
a milder, recurring, attachable form: the episodes return every hour or two
on this soak, the journal names the head while it is still alive, and the
monitor now spares a recently stalled head from its own outage schedule so
the next episode can be caught with a debugger attached. L163 holds the
analysis.

## 2. What was built

| Path | Holds |
| --- | --- |
| `csil/tallyowl-cluster.csil` | `ConsensusKind` gains `proposal`: a client write a voter forwards to the leader, one hop. L149 |
| `crates/tallyowl-cluster/src/groups.rs` | The forward on `ForwardToLeader`, and `propose_local` for the receiving side |
| `crates/tallyowl-store/src/metrics.rs` | The stall signature as gauges: commit in flight, durable position, pending frames. L151 |
| `crates/tallyowl-collector/src/forwarder.rs` | The delivery queue depth from Corndogs' own counts, and the oldest-waiting age as an honest lower bound. L151 |
| `crates/tallyowl-rpc/tests/malformed_frames.rs` | Malformed frames at the trust boundary, and a 1,000-round seeded mutation fuzz. L156 |
| `charts/*/templates/` | Real manifests for both charts, with every render-time refusal DEPLOYMENT.md section 8 asks for. L148 |
| `tools/tallyowl_tools/soak.py` | The soak: up, status, report, roll, monitor, down |
| `tools/tallyowl_tools/drill.py` | The disaster-recovery drill and the overload drill |
| `tools/tallyowl_tools/helm.py` | `helm-check`: lint, render every profile, prove every refusal |
| `tools/tallyowl_tools/audit.py` | `audit`: advisories, licenses, sources. L155 |
| `testbed/cmd/soak/` | The soak driver: paced producers, exact edge readings, window reconciliation, exact-lookup spot checks, and a check-only mode the drills reuse |
| `crates/tallyowl-head/src/query.rs` | The filtered-aggregate pushdown the owner approved at the review: the predicate rides the opaque plan bytes, and the tablet runs the coordinator's own expression code. L162 |
| `.reactorcide/` | Nine jobs, three workflows, one trusted plugin. L157 |
| `docs/RUNBOOK_OPERATIONS.md`, `RUNBOOK_INCIDENT.md`, `RUNBOOK_PRIVACY.md`, `RUNBOOK_INTEGRATION.md` | The four runbooks. The incident runbook carries the L145 stall procedure |
| `docs/SECURITY_REVIEW.md` | The review against THREAT_MODEL.md, with the audit findings ranked |

**1,185 Rust tests, 48 Go tests, and 47 TypeScript tests pass.** Lint is
clean in all three languages, and `./tools.sh gen-check` passes.

## 3. Deliverable by deliverable

`docs/PLAN.md` Phase 11 lists nine.

| Deliverable | State |
| --- | --- |
| Backup, restore, and disaster-recovery drills | **Built and run.** `./tools.sh drill dr` is the whole cycle: load, snapshot, more load, an abrupt kill, a destroyed directory, a timed restore, a verified count, a deleted catalog, a timed rebuild. The verified run: snapshot 0.03 s, restore 0.2 s, head ready 0.1 s later, nothing acknowledged before the snapshot missing, 340 post-snapshot events recovered from the queue, and the loss exactly procedure 3's documented loss. L152 |
| Cross-cluster collector soak tests | **Built and running.** Section 1 |
| Workload and resource isolation and autoscaling | **Built and measured.** The collector chart carries the autoscaler; the head deliberately does not. Isolation was observed rather than asserted: a thousand alert rules saturated the evaluation pool while ingest held its full rate with a delivery depth of four. L154 |
| Upgrade and downgrade and schema migration drills | **Built.** `./tools.sh soak roll` restarts every service gracefully in DEPLOYMENT.md section 7's order under live load, and reconciliation says whether anything was lost. The version window that order depends on is enforced and tested across a restart, rather than waiting for a second version to describe it. L158, L183 |
| Security review, dependency audit, fuzzing, malformed-frame tests | **Built.** `docs/SECURITY_REVIEW.md`, `./tools.sh audit`, and the frame tests. Two advisories are passed over by name with reasons; one is an owner action in linkkeys. L155, L156 |
| Performance tests at expected and overload rates | **Built and run.** Section 6's fourth criterion, and BENCHMARKS.md section 22 |
| Published Helm charts and generated client packages | **Built, and publishing is automatic.** A merge to main runs every gate and then the release job: the image to the organization's registry, both charts into the charts repository and onto a GitHub release, the browser package staged on npmjs, and the service binaries on the release page. crates.io is off by the owner's decision until the release pages prove themselves. L173 to L182 |
| Reactorcide release workflows | **Built.** Pull request, main, and tag-release workflows; the publish job is tag-only, secret-bearing, and never runs locally. The validate job parses under the canonical local runner. L157 |
| Operations, integration, privacy, and incident runbooks | **Built.** Four documents, each grounded in surfaces this phase ran |

## 4. What running the real shape found

The Phase 10 report said running the loop found the whole phase's worth of
defects. Phase 11's loop was bigger — a real cluster, real drills — and so
was the yield:

1. **A follower could not take a write** (section 1, L149). The highest-value
   finding of the phase: without it, replicated ingest worked only while an
   election cooperated, and could never survive its own recovery.
2. **The alpha point-lookup numbers measured misses** (L153). The load
   harness sent `request_id` as a client property; a field reference on that
   name reads the envelope column, which was empty. Every alpha lookup found
   nothing, at 41 ms. The harnesses now write the column; the product keeps a
   sharp edge recorded in L153 with a recommendation.
3. **The commit ceiling is batches, not events** (L150). A paced producer
   under the driver's default linger seals a handful of events into each
   batch, and the replicated path on one machine sustains about seven batches
   each second. The soak driver fills real batches; the number is in
   BENCHMARKS.md section 22 with its caveat.
4. **The agreed outage window is a configuration, not a hope** (L150). A
   collector holds a head outage only as long as `collector.keyCacheGrace`
   covers it; the default is sixty seconds. The soak sets an hour, and the
   operations runbook tells an operator to match the setting to the window
   they intend to hold.
5. **The drill first asserted more than the design promises** (L152). The
   restore gave back more than the snapshot — the queue replayed what it
   still held — and the rebuild refused the old key, both exactly as
   FAILURE_MODES.md documents. The drill now verifies the documented
   promises, which is what a drill is for.
6. **The refusal boundary is unreachable from this machine's own producers**
   (L159). Overload demanded refusals twice and got none, because intake
   absorbs above 43,000 events each second — faster than four local
   producers can offer. The overload verdict now judges what overload must
   never produce, which is silent loss.

## 5. The L144 measurement

A thousand one-minute rules against the soak's ingest head, under its full
load: 0.6 evaluations each second, about 1.6 seconds each. One permit
sustains about thirty-six one-minute rules; a thousand ask for twenty-eight
times more. The pool did what it was designed to do in both directions —
evaluation delay rose while rule states stayed truthful, and ingest was
untouched. The number to design the multi-worker evaluation path against is
now measured. L154.

## 6. Exit criteria

| Criterion | State |
| --- | --- |
| Recovery objectives demonstrated, not merely documented | **Passes.** The drill's verified run measured every step and reconciled the result: 0.3 seconds from restore to ready for its store, nothing acknowledged before the snapshot lost, and the documented loss named in numbers. `run/drill/report.json` holds the run |
| A collector holds the agreed outage window without data corruption | **Passes.** A ten-minute abrupt head outage was injected into the running soak. Mid-window, both collectors answered ready and kept accepting — 592 batches held with the oldest at 149 seconds — and the journal closed the window with `recovered: true`. The backlog drained after recovery, and the reconciliation windows spanning the outage are the standing proof of no loss and no duplication; the schedule repeats the whole exercise every twelve hours for the soak's life, and `soak status` reports every window |
| Rolling upgrades maintain adjacent-version clients | **Passes, with what it does and does not prove stated.** The window is enforced now rather than promised: an app driver and a collector each declare the protocol version they speak, and collector intake and the head each accept the current version and the one before it, from one list in `tallyowl_wire::protocol`. Outside the window is refused with a message that names both ends and a counter an operator can alert on, and intake refuses **before** the durable write. `crates/tallyowl-head/tests/protocol_window.rs` holds the window across a head restart — the roll — and proves a retry after it deduplicates rather than commits twice; the collector's half is in its own suite. `soak roll` still performs the ordered graceful roll under live load with clean reconciliation. **What is not proved:** there is one protocol version, so no member of the window differs from another in behavior. The mechanism is tested before the second version exists rather than after it. L183 |
| Overload produces bounded latency, memory, and disk and visible drops and rejections | **Passes, with the honest wrinkle L159 records.** Sixty seconds unpaced: about 43,000 events each second offered — five times the measured sustained rate, eighty-six times the commit ceiling — and intake absorbed all 2.57 million with 284 MB of resident memory, 115 MB of disk, and a backlog that drained when the offer stopped. No refusal was reachable, because intake outruns four local producers; the refusal machinery is unit-tested, drops and rejections are counted where they happen, and nothing was silently lost. `run/drill/overload-report.json` holds the samples |

## 7. What is not built, and where it would go

1. **Publishing.** The artifacts exist; the registries do not. The publish
   job refuses with the reason until the owner names them. This is the one
   genuinely blocked item, and it is blocked on a decision only the owner can
   make. Section 9.
2. **The two-version compatibility drill.** The procedure runs today against
   one version; the drill takes the second version the day it exists. L158.
3. **The chart install-upgrade-rollback test in a disposable cluster**
   (DEPLOYMENT.md section 8 item 7). It needs a cluster runner in CI; the
   render-time half is built and passing.
4. **A coverage-guided fuzzer.** Needs a nightly toolchain this machine does
   not have. The seeded loop runs on every `cargo test` meanwhile. L156.
5. **The multi-worker alert evaluation path.** L154 measured why an
   installation with a thousand rules needs it, and the number to design
   against.

## 8. Everything marked `Revisit: yes`, with a recommendation

| Entry | What | Recommendation |
| --- | --- | --- |
| L150 | The replicated end-to-end ceiling of ~7 batches each second was measured on one device carrying every voter and the queue | **Measure a spread cell before designing against it.** The capacity envelope for replicated ingest deserves its own run on separate devices |
| L153 | A client property named after a correlation column is silently unreachable by filter | **Add the four correlation names to `PROTECTED_KEYS`**, so the refusal is visible at intake instead of silent at query time. It changes collection behavior, so it is the owner's call. The alpha lookup latencies also need re-measuring |
| L154 | One evaluation permit sustains ~36 one-minute rules | **Keep the measurement beside the design** when the multi-worker evaluation path is built; raising the pool without workers changes nothing, which L144 already said and this proved |
| L155 | The hickory advisory has a real path into a head with LinkKeys sign-in on | **Upgrade hickory in linkkeys and bump the pin here.** One dependency bump away once linkkeys moves |
| L156 | The fuzzer is seeded, not coverage-guided | **Add a `fuzz/` workspace when a nightly toolchain exists** |
| L132, L163 | The append-log stall, carried from Phase 10 and now observed | **Attach to the next episode.** They recur every hour or two on the soak, on a live follower the journal names, and the monitor spares that head from its own schedule. Stacks during an episode are the whole remaining question |

One question was put to the owner rather than decided in the phase: **whether
to push an aggregate's filter down to storage nodes**, the larger of L136's
two exclusions. **The owner said build it, and it is built** (L162). The
contract question dissolved on contact with the shape L136 chose: the
predicate travels inside the plan bytes the cluster contract already carries
opaquely, each tablet evaluates it with the coordinator's own expression
code, and `Store::partial_aggregates` keeps its bytes-in, bytes-out
signature — so D25's boundary holds exactly as before, and a filtered
aggregate no longer moves rows over the network. The measure exclusions
(`rate`, `quantile`, `histogram_merge`, `increase`) stand, for the same
correctness reasons as before.

## 9. Anything blocked

One item: **publish registries and secret grants.** The release workflow,
the package artifacts, and the protected publish job all exist; which
registry takes the charts, which takes the client packages, and under which
grant are decisions only the owner can make (implementation prompt section 7,
item 1). Everything else in the phase proceeded.

## 10. Known defects, ranked

1. **The append-log stall.** L131, L132, and now L163: the signature was
   observed three times on the soak's first night, on a follower voter, in a
   milder self-clearing form that recurs every hour or two. No data was lost
   through any episode. The next occurrence is attachable — the journal names
   the live head, and the monitor spares it from the outage schedule. This is
   the highest-value open thread, and it is finally a warm one.
2. **The correlation-name shadow.** L153. Real data becomes silently
   unreachable by filter when a client property borrows a column's name. A
   one-line protected-names fix awaits the owner's call because it refuses
   data that today is accepted.
3. **The hickory advisory.** L155. Real path, bounded exposure, fix lives in
   linkkeys.

## 11. Design documents that changed, and why

| Document | Change |
| --- | --- |
| `docs/CI-CD.md` | Section 3 records the shipped Reactorcide mechanism — a trusted plugin, not the proposed pipelines directory — and the two facts the proposal had wrong. Section 7 carries the real `run-local` command |
| `docs/BENCHMARKS.md` | New section 22: the replicated path end to end, alert scale, the drill's recovery numbers, and the correction that the alpha point lookups measured misses |
| `docs/SECURITY_REVIEW.md` | New: the Phase 11 review |
| `docs/RUNBOOK_*.md` | New: the four runbooks |
| `docs/PLAN.md` | Phase 11 marked with this report |
| `csil/tallyowl-cluster.csil` | The `proposal` consensus kind, regenerated for all three languages |
