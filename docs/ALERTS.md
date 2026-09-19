# Alerts

## 1. Scope

Grafana is the primary alerting path for an operator who runs it. The Grafana
datasource plugin reaches TallyOwl product data, not only TallyOwl operational
metrics. See D42.

Built-in alerting serves an installation that does not run Grafana. It is
deliberately small.

Built-in alerting has two rule kinds:

- **threshold**, a condition on a measure from a saved query;
- **absence**, a rule that fires when expected data stops arriving.

Baseline comparison, seasonality, and anomaly detection are out of scope.
Grafana does that work. See D41.

## 2. Evaluation is a scheduled query

An alert evaluates by running its saved query on a schedule. It uses the same
typed algebra that the dashboard uses. See [QUERY.md](QUERY.md).

This has one property that matters more than any other: an alert value always
matches what a person sees in the dashboard. A separate evaluation engine gives
a second set of semantics. An alert then fires on a number that nobody can
reproduce, and that destroys trust in every alert.

The cost is detection latency. It equals the evaluation interval. TallyOwl
states that plainly instead of implying that an alert is immediate.

## 3. Outcome model

An evaluation produces one of three outcomes.

| Outcome | Meaning |
| --- | --- |
| `value` | The query returned rows. The measure has a value. |
| `no-data` | The query succeeded and returned no rows. |
| `error` | The query failed, or it could not return a correct result. |

`no-data` is a separate outcome because a threshold on a count never fires when
ingest stops. A dead pipeline returns no rows. It does not return a low number.
An absence rule is a rule over `no-data`.

## 4. Correctness rules

These two rules prevent an alert from lying.

**An alert evaluates at `committed` consistency.** A `bounded-stale` read can
return a value from a replica that lags. The alert would then fire on
replication lag and report it as a change in the data.

**An alert never fires on a partial result.** A missing tablet must not look
like a metric drop. A partial result produces the `error` outcome, and the
alert enters the `unknown` state. See D18.

An operator therefore learns that TallyOwl could not answer, which is a
different fact from a value that crossed a threshold.

## 5. State

| State | Meaning |
| --- | --- |
| `ok` | The condition is not satisfied. |
| `firing` | The condition is satisfied. |
| `no-data` | The evaluation returned no rows and an absence rule applies. |
| `unknown` | The evaluation could not produce a correct result. |
| `silenced` | An operator suppressed notification for a period. |

A state change sends a notification. A repeated evaluation in the same state
does not.

TallyOwl stores the alert instance, its state, the time of the change, and the
observed value. A restart therefore does not resend a notification for a state
that already fired.

## 6. Notification

Two channels exist. See D13.

- a generic **signed webhook** for an outside system;
- a native **CSIL callback** for a service that already holds a connection.

A downstream system builds another channel, such as email or a chat service, on
the webhook. TallyOwl does not build it.

A notification carries the rule, the state, the observed value, the evaluation
time, and a link to the query. A notification never carries telemetry rows.

Delivery retries with capped jitter. A failed delivery is visible in a metric
and in the operator interface. A failed delivery never changes the alert state.

## 7. Resource control

Alerts share the storage and query path with the dashboard. An alert must never
starve a person who is looking at a screen.

- alert evaluation uses a separate query budget pool;
- the scheduler staggers evaluations to avoid a burst on an interval boundary;
- an evaluation that exceeds its budget produces the `error` outcome;
- TallyOwl disables and reports an alert that repeatedly exceeds its budget.

An installation with many alerts on a short interval creates real load. One
thousand rules on a one-minute interval is about seventeen evaluations each
second. Each one is a full query.

## 8. Metrics

TallyOwl reports these values through its native path and its Prometheus and
OpenMetrics endpoint:

- evaluations started, finished, and failed;
- evaluation delay against the schedule;
- evaluation duration;
- outcomes by kind;
- state changes;
- notifications sent, failed, and retried;
- rules disabled by budget.

Evaluation delay is the indicator that matters. A rising delay means that
alerts no longer detect at their configured interval.

## 9. What this build does, and where each rule lives

Built. `crates/tallyowl-head/src/alerts.rs` holds the rules, the evaluation, and
the state machine; `notify.rs` holds the two channels; `workflows.rs` and
`passes.rs` hold the schedule, the retry, and the quarantine.

**Every evaluation and every notification is a durable task.** A schedule held
in a process is a schedule a restart loses, and Corndogs owns durable queue and
workflow state. Retry, backoff, and dead-worker recovery all stop when the
`CleanUpTimedOut` sweep stops, so the sweep runs beside the scheduler.

**The evaluation and the notification are separate queues.** A receiver that is
not answering must not stop the next evaluation, and an evaluation that is slow
must not delay a notification that is ready.

**A notification for a state that has since changed is dropped.** It waited in a
queue while the alert recovered. The recovery queued its own notification.

**A `https` webhook address gets TLS.** The public trust roots come from the
platform store, so an operator whose receiver uses a private certificate
authority adds a root the ordinary way; a bundled set is the fallback for a
container built from scratch. **These roots are only ever used here.** Every
other TLS hop verifies against the installation's own authority, and a public
root store there would accept any certificate on the internet as a TallyOwl
node. See L142.

**The separate query budget pool is `query.alertConcurrency`.** It bounds how
many alert evaluations occupy the storage and query path at once. A full pool
puts the evaluation back on the queue with a backoff; it never marks the rule
`unknown`, because a full pool is not a failed evaluation and a rule that went
quiet whenever the installation was busy would be a rule an operator learns to
ignore. An evaluation also gets half the runtime a person's query gets, so one
heavy alert cannot hold a permit indefinitely. See L144.

**What the pool does not do.** It reserves nothing for a person looking at a
screen. There is no way to do that from inside one process without a scheduler
this system does not have. It bounds what alerting takes, which is the half that
can be bounded, and the evaluation delay in section 8 is what says whether the
bound is costing detection latency.

**What one permit sustains is measured.** Under sustained ingest load, one
permit and one worker evaluate about 0.6 rules each second — about thirty-six
rules on a one-minute interval. A thousand such rules ask for twenty-eight
times more, and each rule is then evaluated about every twenty-eight minutes
while ingest stays untouched. An installation that needs more rules needs
more evaluation workers and a larger pool together, because raising the pool
alone changes nothing. See L154 and BENCHMARKS.md section 22.

## 10. Required tests

1. A repeated evaluation in one state sends one notification.
2. A worker restart does not resend a notification.
3. Ingest stops and the absence rule fires.
4. A missing tablet produces `unknown` and sends no threshold notification.
5. An alert query that requests `bounded-stale` gets a refusal.
6. A budget-exhausted evaluation produces `error` and not a false `ok`.
7. A silenced rule suppresses notification and still records state.
8. A webhook failure retries and does not change the alert state.
9. An alert value matches the same query run from the dashboard.


Every one of the nine is in
`crates/tallyowl-head/tests/alerts_and_workflows.rs`, named for the property
rather than for the function it exercises.
