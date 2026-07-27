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
analytics. Correlation fields include request, trace, span, session, actor, and
custom IDs.

Recent hot and warm segments stay on local storage. Optional object storage can
contain older segments. All tiers use one query and deletion model.

TallyOwl hosts the dashboard and uses LinkKeys login. Corndogs provides durable
queues and workflows. CSIL defines the ingest and query contracts. Do not edit
generated code.

This repository is currently in design-first mode:

- [System design](docs/DESIGN.md)
- [Delivery and failure semantics](docs/DELIVERY.md)
- [Data model and analytics](docs/DATA_MODEL.md)
- [High-cardinality storage and correlation](docs/HIGH_CARDINALITY.md)
- [Cell architecture and growth](docs/CELLS.md)
- [Node identity and automated enrollment](docs/NODE_IDENTITY.md)
- [Native storage design](docs/STORAGE.md)
- [Query algebra](docs/QUERY.md)
- [Segment and manifest format](docs/SEGMENT_FORMAT.md)
- [Collection policy and retention](docs/POLICY.md)
- [Alerts](docs/ALERTS.md)
- [Deployment and Helm charts](docs/DEPLOYMENT.md)
- [Implementation plan](docs/PLAN.md)
- [Reference application and integration test bed](docs/TESTBED.md)
- [CI/CD design](docs/CI-CD.md)
- [Protocol specifications](csil/README.md)
- [Documentation language](docs/DOCUMENTATION.md)
- [Open decisions](docs/DECISIONS.md)

Every decision in [DECISIONS.md](docs/DECISIONS.md) has a status. The
remaining items are prototypes and measurements, not approvals.

The [Apache License, Version 2.0](LICENSE) applies to TallyOwl.
