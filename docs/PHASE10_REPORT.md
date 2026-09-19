# Phase 10 report: alerts and workflows

What was built, what was tested, and what was not built. Read
[IMPLEMENTATION_LOG.md](IMPLEMENTATION_LOG.md) L132 to L141 beside this: it
holds the reasoning and this holds the state.

**All three Phase 10 exit criteria pass.** Section 5 states each one and names
where it is proved.

**The four items carried in from earlier phases are all built.** A month is a
month (L134), the identity graph is materialised (L135), the general aggregate
is pushed down (L136), and the consensus-log constants are settings (L133).

**L131 is not closed, and section 2 says so plainly.** The append log is now
excluded by argument rather than by a run count, three real defects found on the
way are fixed, and the tool the next person needs is written down. The hang
itself was not reproduced.

## 1. L131, which outranked the phase order

The instruction was to close it first. Here is what was done and what it means.

**The tool.** `kernel.yama.ptrace_scope` never had to be relaxed. Yama refuses
an *attach* to an unrelated process and permits a tracer that is an **ancestor**
of the tracee, so a test binary started by `gdb --args` gives stacks. A `SIGINT`
to gdb interrupts the inferior and returns gdb to its command list, so a harness
can run a test under gdb, wait, and take every stack on a timeout. L131 recorded
"no stack was obtainable"; that was the one case yama refuses, met by trying to
attach to a process that had already wedged.

**The search.** 800 runs of `many_concurrent_commits_all_survive` under gdb, 210
runs of the whole `segmented_store` binary at the harness's own parallelism, a
purpose-built stress driver under a build with randomised delays at eleven points
across `append`, `reclaim_through`, `seal`, and `commit`, and a final 400-run
verification against the fixed code. **No hang.** That is on top of the 400 the
previous session ran.

**The result that matters.** `Wal::append` cannot wait indefinitely, and the
argument is written into the function rather than into a log entry. A caller
waits only while `committing` is true and leaves that wait the moment it is
false, because it then becomes the committer itself; a committer never waits,
acquires no other lock while it holds this one, and every path out of its loop —
including the failure path — clears `committing` and wakes the others. So a hang
that involves the append log has to involve a lock outside it.

That also reads L131's own isolation table correctly for the first time. Two
changes each moved the rate and neither removed it, because **both changed how
long the function holds the lock and neither changed whether it can wait**.

**Three defects the hunt found, and two are worse than the hang.**

| Defect | What it was |
| --- | --- |
| A refused write was reported as durable | A failed group commit left the group's *other* frames out of `pending` for ever. Those callers woke, found nothing pending, and returned `Ok`. A success receipt for bytes no device took is the one thing `docs/DELIVERY.md` section 3 says cannot happen |
| A busy log never reclaimed anything | Reclamation stepped aside while a group was in flight *or* anything was pending, and a committer keeps the role while callers keep arriving. **Measured: six callers, twenty-five seconds, no reclamation at all, and the log holding 100 percent of what it had taken.** It is 0.1 percent now. `docs/BENCHMARKS.md` section 21 |
| Two seals could take the log with them | A second seal can reach the catalog first, and the checkpoint would then step over the first seal's range while those rows were still only in memory |

**A fourth thing, and it is a lesson.**
`appending_while_a_reclamation_runs_beside_it_makes_progress` was kept as a
stress test for this interaction and **never once rewrote the file**. It passed
against everything because it exercised nothing. That is L123 again.

**It is still open.** The rate went from about one run in twenty to none in
1,600 across two sessions, and that is a real improvement and not a proof. What
changed is that the search space is now smaller and the tool is written down.

## 2. What was built

| Path | Holds |
| --- | --- |
| `crates/tallyowl-head/src/alerts.rs` | The rules, the evaluation, and the state machine: threshold, absence, the three outcomes, the five states, dedup, escalation, silence, resolve, and the budget rule that disables a rule that keeps overrunning |
| `crates/tallyowl-head/src/notify.rs` | The signed webhook and the native callback, the notification body, and the capped-jitter backoff |
| `crates/tallyowl-head/src/workflows.rs` | The durable work queue: submit, claim, park, quarantine, sweep, and the lag, failure, and quarantine an operator reads |
| `crates/tallyowl-head/src/passes.rs` | The runners — alert evaluation, notification delivery, and the four projector passes — and the scheduler |
| `crates/tallyowl-queue` | The durable task boundary, moved out of the collector so that both use one. L137 |
| `crates/tallyowl-head/src/calendar.rs` | Calendar periods in the supplied timezone. L134 |
| `crates/tallyowl-head/src/identity.rs` | The materialised identity graph, and the bound that keeps an answer independent of how fresh it is. L135 |
| `crates/tallyowl-store/src/store.rs` | `Store::partial_aggregates`: the seam the pushed-down aggregate travels through, carrying bytes this contract never reads. L136 |
| `crates/tallyowl-store/src/control.rs` | Alert rules, alert instances, and notification attempts, durable beside the rest of the control plane |
| `csil/tallyowl-control.csil` | `AlertOutcome`, `SilenceRequest`, `ResolveRequest`, `NotificationDelivery`, `WorkflowStatus`, `RunWorkflowRequest`, and six operations at wire IDs 26 to 31 |
| `csil/tallyowl-cluster.csil` | `PartialKind` gains `aggregate`, and the request and response carry the plan and the partial states |
| `packages/dashboard/src/view.ts` | The alert list, the workflow table, and the notification attempts |
| `crates/tallyowl-driver-rust/examples/put_alert.rs` | Writes a rule and reads its state back over the real control socket, so alerting can be checked through the running loop |

**1,163 Rust tests, 48 Go tests, and 47 TypeScript tests pass**, against 1,088,
52, and 39 at the end of Phase 9. Lint is clean in all three languages,
`cargo fmt --check` passes, and `./tools.sh gen-check` passes.

**The Go figure needs a note.** The Phase 9 report says 52. `go test ./... -v`
counts 48 `=== RUN` lines today and nothing was removed from the Go side in this
phase, so one of the two counts was taken differently — probably by counting
subtests. 48 is what `-v` reports.

## 3. Deliverable by deliverable

`docs/PLAN.md` Phase 10 lists six, plus the four carried in.

| Deliverable | State |
| --- | --- |
| Scheduled metric, query, and error alert definitions | **Built.** A rule holds a whole `QueryRequest`, so any query the algebra answers is an alert: a metric rate, a count of errors, a funnel measure. Threshold and absence are the two kinds `docs/ALERTS.md` section 1 allows |
| Corndogs evaluation and notification workflows | **Built.** Two queues, a scheduler that also runs the sweep, and one worker for each queue. A delay is a task timeout and a state swap, never a polling loop and never a held claim |
| Deduplicated alert instances, silence, resolve, and escalation state | **Built.** The instance is durable, so a restart does not resend. Silence suppresses notification and still records state. Resolve clears the silence with it. Escalation repeats while a rule stays firing, and a rule that asks for none sends one notification for each change |
| Signed webhook and native CSIL callback notification channels | **Built.** The webhook is signed over the timestamp and the body, reaches an `https` address (L142), and retries with capped jitter. The native callback delivers over CSIL-RPC to one operation on `TallyOwlAlertReceiver`, with no signature because the connection is the authentication (L143) |
| Projector rebuild, retention, deletion, and export workflows | **Built.** Rebuild forgets what is derived and rebuilds the catalog from checksummed manifests; retention expires receipts and applies a stable retention predicate; deletion re-applies the erasure ledger; export writes Parquet through `tallyowl-export` |
| Operator UI for workflow lag, failure, and quarantine | **Built.** `workflowTable` draws all three, marks a queue with work in quarantine, and says plainly that nothing was dropped. `alertList` and `deliveryList` are beside it |
| **Carried:** calendar periods and a real month | **Built.** L134 |
| **Carried:** materialise the identity graph | **Built.** L135 |
| **Carried:** push the general aggregate down | **Built.** L136 |
| **Carried:** consensus-log constants become settings | **Built.** L133 |

## 4. What the running loop found, and it was the whole phase's worth of it

`./tools.sh dev reset`, `./tools.sh dev up`, and then:

```sh
cargo run --release -p tallyowl-driver-rust --example put_alert
cargo run --release -p tallyowl-driver-rust --example send_one_event
cargo run --release -p tallyowl-driver-rust --example put_alert -- show
curl -s http://127.0.0.1:5111/metrics | grep alert
```

**The first run found a defect the whole test suite missed.** A rule was
written, an event was sent, twenty seconds passed, and nothing had been
evaluated. The scheduler applied a rule's stagger as `now + offset`, and `now`
moves: every tick set the deadline a few seconds into the future, so a rule was
permanently about to be due and never was. L140.

Every test drove the *runner* with a piece of work it built itself, which is the
right way to test an evaluation and says nothing about whether one is ever
scheduled. **This is the third time this project has recorded that lesson** —
L124 is the second — and a scheduler is wiring.

**The second run, after the fix, is the exit criterion seen live:**

```text
any-events: Firing since 1786232316226, value Some(4.0), notifications Some(1)
tallyowl_alert_evaluations_total{outcome="value"} 7
tallyowl_alert_state_changes_total{state="firing"} 1
tallyowl_notifications_total{channel="webhook",outcome="failed"} 5
```

Seven evaluations, one state change, **one notification**. Five delivery
attempts against an address nothing is listening on, all failed, all retried
with backoff — and the alert stayed `firing` throughout, which is the rule that
a failed delivery never changes the alert state. The notification queue reported
31 seconds of lag while it was backing off, which is what the operator interface
is for.

## 5. Exit criteria

| Criterion | State |
| --- | --- |
| Repeated evaluations do not duplicate notifications | **Passes.** `a_repeated_evaluation_in_one_state_sends_one_notification` evaluates four times and asserts one notification and a `notifications_sent` of one. `a_worker_restart_does_not_resend_a_notification` opens the data directory twice and proves the state survives the process, because a rule about state changes is worth nothing if a restart forgets the state. The running loop showed seven evaluations and one notification |
| Retries survive worker restart | **Passes.** `a_retry_survives_a_worker_that_died_holding_the_claim`: the work is durable before a worker touches it, a claimed task is not handed out twice, and the sweep is what returns it. `a_retryable_failure_is_parked_with_a_rising_attempt_count` proves the attempt count survives the wait, which is the half L012 records as easy to lose |
| Deletion tombstones prevent replay resurrection | **Passes.** `a_deletion_tombstone_stops_a_replay_bringing_the_data_back` erases a row, commits it again under a different batch identifier so deduplication does not hide the case, and asserts it stays gone. It then runs the deletion pass and asserts the predicate is still there, which is what a rebuilt catalog depends on |

**Every one of the nine tests `docs/ALERTS.md` section 9 requires is built**, in
`crates/tallyowl-head/tests/alerts_and_workflows.rs`, named for the property
rather than for the function.

## 6. What is not built, and where it would go

1. **Nothing first.** The native callback had no transport when this report was
   first written. It has one: L143. A receiving service declares
   `TallyOwlAlertReceiver.notify` and takes the same body a webhook receives.
2. **Nothing second.** A webhook could not reach an `https` address when this
   report was first written. It can now: L142 added the outbound TLS client,
   with the platform trust store first and a bundled set as the fallback.
3. **Nothing third.** The evaluation budget was a deadline and not a pool when
   this report was first written. `query.alertConcurrency` is the pool now:
   L144. What is still unmeasured is whether one is the right number, and
   nothing has run a thousand rules to find out.
4. **The workflow lag is a lower bound.** The depths come from Corndogs, which
   reports how many tasks are in a state and not when each one arrived, so the
   age is measured from the oldest thing **this process** is still waiting on. A
   restart loses the age and keeps the depth, and the lag then reads as zero
   until the next submission. L146.
5. **An aggregate over a filter is not pushed down**, and neither is `rate`,
   `increase`, `quantile`, or `histogram_merge`. Both are correctness reasons
   rather than gaps, and section 7 of `docs/QUERY.md` states them. L136.
6. **The charts still have no templates.** True since before Phase 7.
7. **Nothing seventh.** L097 was carried through three phases as "not a
   deferral a decision can lift". It is closed: L147. The marker it wanted in
   the segment format has been on every row since Phase 3, and a copy onto a
   node that already holds part of the tablet reconciles rather than refusing.

## 7. Design documents that changed, and why

| Document | Change |
| --- | --- |
| `docs/ALERTS.md` | New section 9 states what this build does and where each rule lives, including the three things that are shaped by what one process can do. The required tests moved to section 10 |
| `docs/QUERY.md` | Section 7 says which measures are pushed down and why the rest are not, and that a storage node runs the coordinator's own aggregation. Section 8 says an interval is a calendar unit and a fixed duration never reads the timezone. Section 10 no longer says the coordinator never pulls rows for an aggregate, because it does for the ones it cannot push down. Section 12.2 states calendar periods |
| `docs/STORAGE.md` | Section 5 states that a rewrite asks a group commit to stand down rather than stepping aside, why the difference matters, and that a refused write stops the log |
| `docs/FAILURE_MODES.md` | Section 10 states what a refused append-log write does to every later write, and why |
| `docs/BENCHMARKS.md` | New section 21: what a busy append log reclaims, before and after |
| `docs/TESTBED.md` | An alerting section, and what the reference-application assertion asks |
| `docs/PLAN.md` | Phase 10 marked built |

**One measurement contradicted an assumption in this run** and it is section 21:
reclamation was believed to work and did not, on any installation that was busy.

## 8. Everything marked `Revisit: yes`, with a recommendation

| Entry | What | Recommendation |
| --- | --- | --- |
| L132 | The append-log hang is narrowed and not explained | **Leave it open, and point Phase 11's soak at it.** The append log is excluded by argument, the log now reports its own state and the head warns on the stall signature (L145), and running the suite under gdb is five lines. A soak is the first thing in this project's history that will run that concurrency for days |
| L135 | A materialised identity graph is refreshed every five minutes, so a late `identify` can go unseen by a question about a range that ended before it | **Measure how often a late identity row arrives.** Nothing has. `query.identityRefresh` is the setting and zero restores the old behaviour exactly |
| L136 | An aggregate over a filter is not pushed down | **Leave it.** It needs a storage-level predicate that is not the head's expression language, and nothing runs more than one tablet outside the tests, so nothing can measure what it is worth |
| L144 | The evaluation pool holds one permit, because one worker is what the head runs | **Measure it before raising it.** A thousand rules on a one-minute interval is the case, and this project has a load harness. Raising the pool without adding workers changes nothing, which is worth knowing before somebody raises it expecting it to |
| L142 | A webhook is the one outbound connection to something TallyOwl does not own, and it is the one place a public root store is used | **Read the boundary rather than the code.** Every other hop verifies against the installation's own authority. A change that let these roots reach another hop would accept any certificate on the internet as a TallyOwl node |

## 9. Anything blocked

Nothing. No credential, no account, no external service, and no csilgen
capability was needed. Every contract change validated and generated for Rust,
Go, and TypeScript on the first attempt.

**L129 is done, and the Phase 9 report's sentence about it is out of date.**
`tools/tallyowl_tools/generate.py` pins `CSILGEN_VERSION = "0.1.0"` and the
TypeScript transport at the release tag `transport-typescript/v0.2.0`, which is
exactly what L129 decided. `csilgen` on the path reports `0.1.0`, so `gen` emits
no drift warning and `gen-check` passes.

The revision numbers in the Phase 9 report describe the state before L129 was
carried out. **This report repeated them without checking, and that was wrong.**

One caveat that the code already states rather than hides: `0.1.0` is a coarse
version and will not move for most changes, so the version check is a courtesy.
`gen-check` is what protects the build, because it generates into a temporary
directory and fails on any drift from what is checked in whatever the version
says.

## 10. Known defects, ranked

1. **The append-log hang.** L131 and L132. Not reproduced in 1,600 runs across
   two sessions, not explained, and the highest-value open item in the log. The
   log reports its own state now and the head warns on the stall signature
   (L145), so the next occurrence is a diagnosis rather than a discovery.
2. **Nothing second.** The workflow counts were the head's own when this report
   was first written, so a restart zeroed them. They come from Corndogs'
   `GetQueueAndStateCounts` now, which is durable and sees every head. L146.
3. **Nothing third.** A `csil-callback` target on an installation with no
   callback transport was a rule that could be saved and would never notify.
   That was written down here and then fixed rather than shipped: the rule is
   refused when it is written, with the reason, exactly as a stale read is.
