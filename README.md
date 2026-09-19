# TallyOwl

TallyOwl is a self-hosted telemetry and analytics system for web applications.
It collects errors, traces, events, product behavior, campaign data, and
push-based metrics. One data plane connects these data types.

An application must speak the CBOR Service Interface Language (CSIL) to send
telemetry. The application includes the
TallyOwl ingest specification and routes typed messages on its own connection.
There is no generic HTTP ingest endpoint.

The central integration rule prevents direct contact between an instrumented
browser and TallyOwl. Browser telemetry uses the application's existing,
same-origin CSIL connection. The application routes those typed messages to a
nearby TallyOwl collector over one persistent CSIL over TCP connection. The
collector acknowledges each correlated batch only after Corndogs durably
accepts it, then independently retries and forwards it to final TallyOwl
storage.

The native store supports exact high-cardinality correlation and aggregate
analytics. Correlation fields include request, trace, span, session, end user, and
custom IDs.

Recent hot and warm segments stay on local storage. Optional object storage can
contain older segments. All tiers use one query and deletion model.

TallyOwl hosts the dashboard and uses LinkKeys login. Corndogs provides durable
queues and workflows. CSIL defines the ingest and query contracts. Do not edit
generated code.

All eleven phases of [the plan](docs/PLAN.md) are built: the contract, the
local development loop, the embedded store, the durable telemetry paths,
replicated storage, product behavior, campaigns and attribution, alerts and
workflows, and production hardening — drills, a running soak, Helm chart
templates, CI workflows, and runbooks. A developer runs the `home` profile as
binaries on a workstation, with the same configuration, health, and delivery
path a deployment uses, and a home installation starts no consensus group and
opens no replication port. [CONTRIBUTING.md](CONTRIBUTING.md) says how to
start it.

The first release candidate is **0.1.0-rc.1**.
[RELEASE_NOTES.md](docs/RELEASE_NOTES.md) says what it contains, which
collection behavior changed, where each artifact is, and what is not in it.

[PHASE11_REPORT.md](docs/PHASE11_REPORT.md) is the current phase report; the
earlier reports say what each phase was when it was reported.
[ALPHA_REPORT.md](docs/ALPHA_REPORT.md) covers Phases 1 to 6 and holds the
alpha load-test results.
[IMPLEMENTATION_LOG.md](docs/IMPLEMENTATION_LOG.md) records each choice the
design did not make.

Runbooks: [operations](docs/RUNBOOK_OPERATIONS.md),
[incidents](docs/RUNBOOK_INCIDENT.md), [privacy](docs/RUNBOOK_PRIVACY.md),
and [integration](docs/RUNBOOK_INTEGRATION.md).

Design documents:

- [System design](docs/DESIGN.md)
- [Delivery and failure semantics](docs/DELIVERY.md)
- [Data model and analytics](docs/DATA_MODEL.md)
- [High-cardinality storage and correlation](docs/HIGH_CARDINALITY.md)
- [Cell architecture and growth](docs/CELLS.md)
- [Node identity and automated enrollment](docs/NODE_IDENTITY.md)
- [Threat model](docs/THREAT_MODEL.md)
- [Failure modes and recovery](docs/FAILURE_MODES.md)
- [Service conventions](docs/CONVENTIONS.md)
- [Native storage design](docs/STORAGE.md)
- [Query algebra](docs/QUERY.md)
- [Segment and manifest format](docs/SEGMENT_FORMAT.md)
- [Collection policy and retention](docs/POLICY.md)
- [Alerts](docs/ALERTS.md)
- [Deployment and Helm charts](docs/DEPLOYMENT.md)
- [Release notes](docs/RELEASE_NOTES.md)
- [Implementation plan](docs/PLAN.md)
- [Alpha report: what is built, what is measured, and what is not](docs/ALPHA_REPORT.md)
- [Phase 11 report: production hardening, the drills, and the soak](docs/PHASE11_REPORT.md)
- [Security review](docs/SECURITY_REVIEW.md)
- [Implementation log: every choice the design did not make](docs/IMPLEMENTATION_LOG.md)
- [Reference application and integration test bed](docs/TESTBED.md)
- [Benchmark results](docs/BENCHMARKS.md)
- [CI/CD design](docs/CI-CD.md)
- [Protocol specifications](csil/README.md)
- [Documentation language](docs/DOCUMENTATION.md)
- [Open decisions](docs/DECISIONS.md)

Every decision in [DECISIONS.md](docs/DECISIONS.md) has a status. The
remaining items are prototypes and measurements, not approvals.

The [Apache License, Version 2.0](LICENSE) applies to TallyOwl.
