# TallyOwl repository guidance

## Product language

- Describe TallyOwl on its own terms. Do not name comparison products in code,
  comments, documentation, examples, issues, fixtures, or generated artifacts.
- TallyOwl collects telemetry and analytics, but it is not a log aggregation
  system.

## Documentation language

- Write all technical documentation in ASD-STE100 Simplified Technical English.
- Use ASD-STE100 Issue 9 until the project selects a later issue.
- Use the rules in [docs/DOCUMENTATION.md](docs/DOCUMENTATION.md).
- Use one approved term for one meaning.
- Define each TallyOwl technical noun and technical verb.
- Do not use a synonym only to add variation.

## Protocol boundaries

- CSIL is the source of truth for public types and service interfaces.
- Browser instrumentation uses an application's existing same-origin CSIL
  connection. It must not create a TallyOwl connection or contact a TallyOwl
  domain.
- Native TallyOwl server-to-server traffic uses CSIL over TLS over TCP. Do not add a
  generic HTTP ingest API.
- Limit browser-only HTTP to document assets, LinkKeys redirects, browser
  carrier establishment, and the same-origin unload flush to the host
  application's own route.
- A compatibility edge can scrape a Prometheus or OpenMetrics endpoint over
  HTTP. It can also receive OpenTelemetry metrics and traces on the expected
  OpenTelemetry transport. TallyOwl services can expose their own Prometheus
  and OpenMetrics endpoints. OpenTelemetry logs are out of scope. Normalize at
  the collector. Every later hop uses native CSIL.
- A compatibility receiver never listens or scrapes by default. An operator
  enables it. Do not open a port because the binary contains the feature.
- Never edit generated CSIL code. Change the `.csil` source and regenerate.
- If csilgen lacks a required capability, discuss it with the project owner
  first. An approved request belongs in
  `~/repos/catalystcommunity/csilgen/docs/csilgen-requests/` and must follow that
  directory's README.

## Integration boundaries

- Corndogs owns durable queue and workflow state. It is not the analytics query
  store.
- Corndogs evaluates a task timeout only when a caller invokes
  `CleanUpTimedOut`. The forwarder role owns that sweep. Retry, backoff, and
  dead-worker recovery all stop when the sweep stops.
- Express a delay with the Corndogs task timeout and the state swap. Do not
  build a polling loop and do not hold a claim while waiting.
- App backends keep a persistent CSIL over TCP connection to collector intake.
  Collector acknowledgement means Corndogs durably accepted the batch; do not
  acknowledge on socket receipt or an in-process queue.
- Collector intake and forwarder roles must remain independently deployable,
  even though the home profile runs them together.
- Collector receipts report the configured total durable-copy requirement.
  The default is one durable Corndogs copy; redundancy is an operator choice,
  not part of the word "durable."
- An instrumented application does not know about TallyOwl workspaces. Its
  credential has project scope. The collector resolves the project and its
  workspace once, holds the mapping in memory, and stamps it. Never accept
  tenancy from a payload.
- Resolve IDs, never names. A workspace name and a project name are display
  properties and never travel on the ingest path.
- One typed property namespace carries every descriptive value. Each property
  records its origin: client, driver, or collector. A protected name refuses a
  client value and counts the refusal. A property grants no access.
- A session works on every surface, including a terminal user interface.
  `startSession` issues an opaque ID and `endSession` closes it. Drop an event
  with an invalid session ID and count the drop.
- TallyOwl does not do session replay and does not record the Document Object
  Model. Do not write "session recording" for either idea.
- LinkKeys owns human authentication. TallyOwl owns its sessions,
  project and workspace membership, and authorization.
- Collectors authenticate with scoped TallyOwl API keys. Keep multiple active
  keys for each source. This permits rotation without a coordinated cutover.
- A reusable role token can enroll permitted node roles. Store only its hash.
- A role token must have role, location, expiration, rate, and audit controls.
- A role token cannot add a controller voter or change a tablet voter set.
- Each enrolled node generates its private key. The control plane signs only
  the certificate request.
- Generated clients provide types, codecs, and routing seams. The hand-written
  app driver provides buffering, batching, configuration, and host integration.
  Call it an app driver, never an adapter or an SDK.

## Delivery and data

- Never claim exactly-once transport. The contract is durable at-least-once
  delivery with stable IDs and logically idempotent ingestion.
- A success receipt is meaningful only after the receiving durability boundary
  has accepted the data.
- Keep raw accepted telemetry append-only for its configured retention period.
  Make all derived projections and rollups reproducible from retained raw data.
- TallyOwl owns its storage format and storage nodes. Do not introduce a
  required external analytics database.
- Keep the storage catalog rebuildable from checksummed segment manifests.
  Export to open formats, especially Parquet, is a first-class compatibility
  contract.
- Arrow and Parquet are optional export dependencies, not dependencies of the
  always-on collector, storage, query, or dashboard path.
- Deletion becomes visible through tombstones immediately and physically
  reclaims local data by rewriting only affected bounded segments. The cold
  tier erases by destroying key material, never by rewriting every intersecting
  object.
- A tombstone is a standing predicate. It also hides matching data that arrives
  after the erasure request.
- Never acknowledge an uncommitted entry in a multi-voter group. `local-one` is
  therefore legal only for a single-voter tablet.
- Do not create a cluster-wide consensus group containing all storage nodes.
  Use a small controller quorum and small per-tablet replica groups.
- Use regional cells and a small global project-to-cell directory at large
  scale. Do not put tablet placement in the global directory.
- Enforce payload, attribute, batch, and resource limits at trust boundaries.
  Support high-cardinality values. These values include request, trace,
  session, actor, and other exact-correlation IDs.
- Do not silently drop, coalesce, or reject a value because it has high
  cardinality.
- Treat CSIL CBOR as a wire and WAL representation. After successful segment
  projection, queryable telemetry belongs in native typed pages and indexes,
  not a long-lived catch-all CBOR payload.
- Keep hot and warm local storage and optional cold object storage as tiers of the
  same segment format. Do not require cold storage in the home profile.
- Never record secrets, credentials, request bodies, claim values, or raw
  personal data by default.

## Working discipline

- Keep accepted constraints, recommendations, and unresolved decisions visibly
  distinct in design documents.
- Never run `git add`, `git commit`, or `git push` unless the user explicitly
  asks for that exact operation. The user owns staging, commits, and pushes.
- Use Reactorcide for CI/CD. Put job definitions under `.reactorcide/jobs/` and
  use runnerlib for pipeline and workflow behavior.
- Avoid Bash and shell-script orchestration. Prefer Python pipeline modules that
  use runnerlib APIs and invoke tools with argument arrays rather than shell
  strings. Do not add `.sh` build, test, and deploy wrappers.
- Add failure-path and compatibility tests with every protocol or storage
  change.
- Keep the reference application current. A capability without
  reference-application coverage is not complete. See `docs/TESTBED.md`.
- Review security, privacy, backpressure, partial failure, upgrade, and
  multi-tenant isolation implications before considering a change complete.
- The repository license is Apache-2.0. Dependencies and copied assets must be
  compatible with it.
- Agents do not stage, commit, or push; the user performs those operations.
