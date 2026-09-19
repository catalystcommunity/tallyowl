# Reference application and integration test bed

## 0. Two levels of test

This document describes the system level. The unit and integration level uses a
different pattern, and the two are not alternatives.

**Unit and integration.** Tests generate the data they need, in the pattern that
other repositories here already use. A `DataUtils` helper exposes a
`Create<Thing>(setup)` call for each stored kind. It fills every field that the
test did not name with generated values, so a test declares only what it cares
about.

- Run inside a transaction and roll it back, so a test leaves nothing behind.
- Prefer in-memory storage, so the suite stays fast.
- Do not mock the storage interface. TallyOwl owns that interface, so a mock
  would only assert that the mock behaves like the mock.
- Cover branches, not the happy path. Exercise every decision in the code
  except operating-system logistics such as socket creation.

**System.** The reference application below proves that a product produces
correct analytics, which no unit test can show.

## 1. Purpose

Unit tests prove that one component obeys its contract. They do not prove that
a complete product produces correct analytics. The reference application closes
that gap.

The reference application is a complete simulated product. It uses TallyOwl
the way a real customer uses TallyOwl. The test bed then compares TallyOwl
query results with the exact truth that the simulation created.

This test bed proves these properties:

- many applications and many sources send data to one installation;
- every telemetry kind travels its full path from producer to dashboard query;
- every client type uses the same correlation IDs;
- analytics results are exactly correct, not merely plausible;
- the system keeps these properties during failure and upgrade.

## 2. The reference product

The reference product is `seedstore`. It is a fictional online store. A store
gives natural conversions, revenue, campaigns, funnels, and cohorts. It does
not name or copy any real product.

The test bed proves **behavior**, not one client implementation. It answers one
question for each surface: if a client of this kind sends these messages, does
TallyOwl do the correct thing?

Therefore a surface does not need a maintained app driver. A surface needs to
produce the message pattern that its kind of client produces.

`seedstore` has five surfaces:

| Surface | How it sends | Primary telemetry |
| --- | --- | --- |
| Marketing site | Browser package | Page views, campaign touches, consent |
| Web application | Browser package, TypeScript | Events, sessions, browser errors, browser spans |
| Backend | Go app driver | Backend events, errors, spans, metrics, conversions |
| Rich client | Rust app driver | Native events, errors, spans, offline buffering |
| Mobile client | Behavior simulation | Events, sessions, background and foreground behavior |
| Terminal client | Behavior simulation | Events and the session lifecycle without a browser |

Three surfaces use a maintained driver, so the test bed also proves those
drivers. The mobile client and the terminal client are behavior simulations.
They emit the same typed messages that a real client of that kind emits.

A behavior simulation creates no obligation to maintain a driver for that
language. See the client library rollout in [PLAN.md](PLAN.md).

The backend is the only surface that holds a TallyOwl credential. Every client
surface sends telemetry through the backend. This proves the central
integration rule at every client type.

A second, smaller application shares the installation. It proves multi-tenant
isolation with real traffic, not only with negative unit tests.

## 3. Non-goals

- The reference application is not a product. Do not publish it.
- The test bed is not the performance benchmark harness. Benchmarks stay in
  [STORAGE.md](STORAGE.md) and [HIGH_CARDINALITY.md](HIGH_CARDINALITY.md).
- The simulation contains no real personal data.
- The test bed does not replace unit tests or failure-injection tests.

## 4. Repository shape

The test bed follows the layout that `longhouse` and `piler` use:

```text
testbed/
  csil/
    seedstore.csil            ;; app schema, includes tallyowl-ingest.csil
  marketing/                  ;; static campaign landing pages
  api/                        ;; Go backend, holds the TallyOwl credential
  webapp/                     ;; TypeScript web application
  richclient/                 ;; Rust desktop client, uses the Rust driver
  mobile/                     ;; behavior simulation, no driver
  terminal/                   ;; behavior simulation, no driver
  simulator/
    scenarios/                ;; declarative scenario definitions
    ledger/                   ;; expected-result ledger writer
  harness/
    browser/                  ;; headless browser driver
    assertions/               ;; query-versus-ledger comparisons
  deploy/
    docker-compose.yml
  .reactorcide/
```

The second application reuses `api/` with different configuration. It does not
need its own client surfaces.

**What exists as of 2026-08-10:** `api/` including its web surface in
`api/web.go`, `webapp/`, `simulator/`, `ledger/`, the load harness in
`cmd/load`, and the soak driver in `cmd/soak`, which offers paced load for
days and continuously reconciles what was acknowledged against what a query
answers (see PHASE11_REPORT.md section 1). `marketing/`, `richclient/`,
`mobile/`, `terminal/`, `harness/`, and `deploy/` are not built. `webapp/` has its own end-to-end tests in Go and
TypeScript and does not yet carry a share of a ledger scenario, because the
scenario stream is app-driver shaped and the browser path is ingest shaped.

## 5. The ledger: how correctness is proved

The simulator is the source of truth. Before it sends anything, the simulator
writes a **ledger**. The ledger records the exact expected result of every
analysis the test bed checks.

The simulator knows these facts exactly:

- how many end users started each funnel and how many completed it;
- which end users returned in each retention period;
- which touch each attribution model must credit;
- the exact conversion value and currency total for each campaign;
- how many occurrences belong to each error group;
- the exact span count and parent and child shape of each trace;
- which events an erasure request must remove.

The test then runs the equivalent TallyOwl query and compares the result with
the ledger. A mismatch is a failure. This makes analytics correctness a test
result, not a judgment.

The ledger uses exact decimal values for money. It does not use floating-point
comparison for revenue or attribution results.

The test compares an approximate query with its declared error bound. The test
also asserts that the result marked itself approximate. An exact query that
returns an approximate result is a failure, even when the number is close.

## 6. Simulation model

A scenario is a declarative file. It defines end users, cohorts, campaigns,
releases, and a time span. The simulator expands the scenario into an ordered
event stream.

The simulator has these properties:

- **Seeded.** One seed produces one identical run. A failure is reproducible.
- **Virtual clock.** A scenario can simulate 90 days in a short real time.
  Retention, cohort, and attribution windows need this.
- **Real clock mode.** A smaller scenario runs against the real clock. This
  mode exercises timeouts, batching deadlines, and session heartbeats.
- **Multi-end user.** End users have devices, sessions, and identity transitions.
- **Cross-device.** One end user uses the web application, the rich client, and
  the mobile client. This exercises `identify`, `alias`, and end-user timelines.
- **Late and duplicate data.** The simulator deliberately sends late events and
  duplicate event IDs. The ledger records the correct logical result.

End user identity moves through the states that break naive implementations:

```text
anonymous (marketing site)
  -> anonymous (web application, same device)
    -> identified (sign-in)
      -> identified on a second device
        -> erased
```

**Built, from Phase 8.** `expandJourney` in the simulator is the cross-device
part of this. Each person uses three client surfaces, each surface has its own
project-scoped anonymous identifier, and each one identifies to the same known
identifier. The ledger then predicts three things the sessions alone cannot
reach:

- **the funnel**, step by step, counted in people rather than in events;
- **the retention matrix**, one cohort by as many periods as the scenario's
  return days;
- **the timeline**, as a count of items for each person across every surface.

The journey uses its own event names and runs beside the sessions rather than
inside them. **A fixture whose steps could also be matched by other traffic is
one nobody can work out by hand**, and every number in this part of the ledger
is a multiplication a reader can do in their head. See L111.

## 7. Headless browser layer

A simulated transport alone cannot prove the browser package. Two paths
therefore exist.

**Real browser path.** A headless Chromium instance loads the marketing site
and the web application. This path proves:

- the browser package uses the application's existing same-origin connection;
- the browser package opens no other connection and contacts no TallyOwl
  domain;
- the unload flush reaches the backend (see D34);
- consent state changes collection behavior;
- campaign parameters arrive from a real landing-page URL;
- a real browser error produces a correct stack trace after scrubbing.

The test asserts the negative case with a network log. Any request to a
TallyOwl domain from the browser is a failure.

**Synthetic path.** A fast driver produces the same typed events without a
browser. Volume scenarios use this path. Both paths must produce identical
ledger results for the same scenario.

## 8. Coverage matrix

Each surface must exercise its telemetry kinds and its assertions:

| Capability | Produced by | Asserted by |
| --- | --- | --- |
| Page views and sessions | Marketing site, web application | Session count, duration, exit rate |
| Named events | All client surfaces | Trend, breakdown, exact count |
| Funnels | Web application, mobile client | Step completion against the ledger |
| Retention and cohorts | Simulator virtual clock | Period membership against the ledger |
| Paths | Web application | Bounded path result |
| Browser errors | Web application | Group, release, regression, scrubbing |
| Backend errors | Backend | Group, trace link, occurrence count |
| Rich client errors | Rich client | Group, offline delivery after reconnect |
| Traces and spans | Backend, all clients | Waterfall shape, exact parent and child |
| Metrics | Backend | Counter rate, gauge, histogram quantile |
| Compatibility metrics | Scrape target, push exporter | Normalized value equality |
| Identity and aliasing | Cross-device end user | Timeline continuity, no leakage |
| Campaigns | Marketing site landing pages | Touchpoint classification |
| Attribution | Simulator conversion stream | Each model against the ledger |
| Revenue and cost | Backend, cost import | Exact decimal totals and return |
| Sessions | All client surfaces, including a terminal user interface | Session lifecycle, invalid-ID drops and their metric |
| Tail sampling | Multi-service traces | Kept trace keeps every span; dropped trace leaves none |
| Properties | Client, driver, and collector origins | Stamped origins, and a refused client attempt to set a protected name |
| Consent | Marketing site | Campaign linking without a session link still measures the campaign |
| Erasure | Harness | Rows disappear; ledger confirms scope |
| Multi-tenant isolation | Second application | Cross-project query returns nothing |

## 9. Failure injection in the test bed

The test bed runs the failure cases with a complete application attached. This
finds problems that component tests do not find.

Required cases:

- kill the backend during a batch and confirm no false durable claim;
- kill the collector and confirm the app driver retries the same batch ID;
- stop the head and confirm the collector holds the outage window;
- fill the collector queue and confirm explicit backpressure;
- kill the head after commit but before receipt and confirm one logical event;
- take a tablet offline and confirm a typed incomplete-result error;
- revoke a key mid-run and confirm the next batch fails;
- run a rolling upgrade with adjacent protocol versions during live traffic;
- run an erasure while ingest continues and confirm no resurrection.

Every case ends with a ledger comparison. The system must be correct after the
failure, not merely alive.

## 10. Determinism and flake control

A flaky integration suite gets ignored, and an ignored suite proves nothing.

- Every run records its seed. A failure report gives the reproduction command.
- Assertions wait for a declared watermark. They do not sleep for a fixed time.
- Query assertions state their freshness requirement explicitly.
- The suite fails on an unexpected warning, not only on an incorrect number.
- The harness collects the full ledger, the query results, and the difference.
- A quarantined test must have an issue reference and an expiry date.

## 11. CI integration

The test bed runs as its own Reactorcide job. See [CI-CD.md](CI-CD.md).

- A pull request runs the fast scenario with the synthetic browser path.
- A main-branch build runs the full scenario with the real browser path.
- A scheduled build runs the long virtual-clock scenario and the failure cases.
- The job needs no publish or deploy secret.
- The job runs against real processes, never mocks.

The test bed must also run on one developer machine with one command.

## 12. Scenarios grow

Do not write every scenario before the test bed runs. Start with enough to
prove the ledger mechanism, then add one for each capability as its phase makes
it possible.

**Every regression adds a scenario.** A defect that reached a person becomes a
scenario with a ledger assertion, so it can never return quietly. The set therefore becomes
thorough without anyone predicting where the defects appear.

A scenario names its seed, so a failure reproduces exactly.

## 13. Build order

The test bed grows with the system. Do not build it all at the start.

1. Add the backend, the web application, and the ledger with generic events.
2. Add the marketing site, campaigns, and the real browser path.
3. Add errors and traces.
4. Add metrics and the compatibility receivers.
5. Add the rich client and the mobile client.
6. Add identity, funnels, retention, and paths.
7. Add attribution, revenue, and cost. **Built.** The marketing journey sends
   three touches exactly one decay half-life apart, an `identify` after all of
   them, and a purchase with an order identifier that is delivered twice. Every
   model therefore divides the conversion value into whole units, and the ledger
   states the answer for each of the six before anything is sent.
8. Add the failure cases and the upgrade case.

Each step lands with the phase that makes it possible. See [PLAN.md](PLAN.md).


## Alerting

The reference application's own traffic is what an alert rule runs against in
`an_alert_over_the_reference_applications_traffic_fires_and_agrees_with_the_ledger`.
It asks the two questions a person would: does a rule over real application
traffic fire, and does its value agree with what the ledger says arrived?

**The second half is the point.** Every other alert test builds its own rows,
which proves the rules and says nothing about whether an alert over a real
application finds it. An alert value that disagreed with the ledger would be a
number nobody could reconcile against their own records, and
[ALERTS.md](ALERTS.md) section 2 puts that above every other property.
