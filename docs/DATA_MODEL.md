# Data model and analytics

## 1. Principles

1. Every accepted item has one common envelope and a typed payload.
2. Raw accepted data is append-only for its configured retention.
3. Retained raw data can reproduce all derived datasets.
4. Stable IDs connect errors, traces, events, actors, sessions, metrics, and
   campaigns.
5. Routing and authorization context comes from trusted infrastructure, not
   client fields.
6. Dynamic typed properties and very high-cardinality values are first-class.
   Resource limits constrain bytes and work, not the number of distinct values
   as a correctness shortcut.
7. Event time, receive time, and commit time are separate facts.

## 2. Common envelope

The exact CSIL types come during protocol implementation. The logical envelope
contains:

| Field | Purpose |
| --- | --- |
| `event_id` | Producer-generated UUIDv7; the logical deduplication key in project scope |
| `kind` | Typed payload discriminator |
| `schema_version` | Payload contract version |
| `occurred_at` | Producer event time |
| `observed_at` | SDK observation time when different |
| `received_at` | Trusted intake time from the collector or the head |
| `sequence` | Optional producer-session sequence |
| `workspace_id` | Tenancy boundary; the collector stamps it |
| `project_id` | Identifies the application; the collector stamps it |
| `source_id` | Authenticated source |
| `properties` | Typed key-value pairs, each with an origin; the only dynamic namespace |
| `release` | Deploy and release identifier |
| `service` | Logical emitting service |
| `request_id` | Optional exact correlation across services, events, and spans |
| `sdk_name`, `sdk_version` | Producer library identity |
| `session_id` | Browser and app session correlation |
| `actor_id` | Scoped known actor identifier |
| `anonymous_id` | Scoped anonymous identity |
| `trace_id`, `span_id` | Trace correlation |
| `consent` | Collection and marketing consent state and policy version |
| `measurements` | Bounded numeric values with units |
| `payload` | Canonical typed CSIL value; CBOR in transit and in the WAL, then projected into native typed storage |

The collector stamps and verifies workspace, project, and source. It resolves
the project and its workspace from the credential, not from the payload. A
browser cannot select these values, and an application does not need to know
its workspace. See D32.

One typed property namespace carries the remaining descriptive values. Each
property records an origin of `client`, `driver`, or `collector`. The collector
stamps an operator property such as `region` = `us-west2` from its own
configuration, and it refuses a client value for a protected name. A property
never grants access and never selects a destination. See D38.

TallyOwl stores IDs. A workspace name and a project name are display properties
and never travel on the ingest path.

The host backend compares browser actor identity with the authenticated app
session.

A browser supplies an untrusted `event_id`. Deduplication is therefore scoped
to a project, and the app driver namespaces or replaces a browser-supplied ID
before it seals a batch. Without this rule, a client can suppress a later
legitimate event by sending its ID first.

## 3. Telemetry kinds

### 3.1 Product and behavior events

- named event;
- page and screen view;
- session start, end, and heartbeat;
- interaction with semantic target;
- feature exposure;
- identify, alias, and group association;
- conversion and business outcome.

Useful dimensions include route, page title, device class, locale, release,
environment, feature, and bounded typed properties.

#### Sessions

A session ties events together. It is not a browser concept. A browser, a rich
client, a mobile client, and a terminal user interface all use one lifecycle:

```text
startSession()  -> returns an opaque session ID
   ... the client attaches that ID to its events ...
endSession()    -> closes the session
```

- the client library issues the session ID; a person cannot select it;
- the session ID is opaque and has project scope;
- the library assumes no cookie and no browser storage;
- a session with no `endSession` closes after a configurable maximum lifetime,
  because a client crash must not hold a session open forever.

The head validates each session ID. It drops an event that carries an invalid
session ID and counts the drop in a metric with the reason. A misconfigured
client therefore appears as a rising invalid-session count, not as quiet data
loss. See D11.

Default collection excludes raw DOM selectors, free-form HTML, and input
values.

### 3.2 Errors

An occurrence records:

- exception and error type and scrubbed message;
- normalized stack frames;
- handled and unhandled and severity;
- mechanism and runtime;
- release, environment, service, route and operation;
- trace, span, session, and actor links;
- breadcrumbs represented as bounded event references or scrubbed summaries;
- original group fingerprint inputs and resulting fingerprint version.

The projector derives an error group. The producer does not control the group.
The projector records each grouping version. Retained data can reproduce each
group.

#### Fingerprint

`fingerprint_v1` uses the first rule that applies. See D39.

1. The occurrence has one or more in-app frames. Hash the error type and the
   top five in-app frames. Use module and function only. Collapse repeated
   frames from recursion.
2. The occurrence has frames, but none are in-app. Hash the error type and the
   top five frames.
3. The occurrence has no frames. Hash the error type and the message with
   literals replaced by placeholders.

Rule 3 exists because a browser error often arrives with no usable stack. A
design that used frames alone would put every such error in one group.

The fingerprint excludes line numbers and addresses. A reformatting change
therefore does not split a group.

TallyOwl stores the inputs, the rule that applied, and the version. A later
version rebuilds every group from retained raw data.

#### Merge and split

An operator can merge groups and split a group. TallyOwl records that override
against the set of fingerprints, not against a group ID.

A regroup at a new fingerprint version therefore keeps the operator decision. A
group ID would not survive the rebuild.

Group state includes occurrence counts, release data, regression markers,
owner, status, assignment, and activity history.

### 3.3 Traces and spans

A span records:

- trace ID, span ID, optional parent ID;
- operation and resource name;
- service and span kind;
- start and duration and end;
- status;
- bounded attributes and events;
- sampling decision and reason;
- links to other traces and spans;
- optional error event ID;
- optional metric exemplars.

Trace propagation uses standard fixed-width IDs in TallyOwl libraries. An
allowlist controls baggage. Trace propagation excludes credential and arbitrary
user data.

### 3.4 Metrics

Metric snapshots represent:

- counter delta or cumulative value;
- gauge observation;
- histogram count, sum, and explicit buckets;
- monotonicity and temporality;
- unit and description;
- start and end timestamps;
- bounded label set;
- optional trace and span exemplar.

Backend libraries aggregate in process and push snapshots periodically through
the collector. They should offer familiar counter, gauge, and histogram APIs.
The collector can merge compatible snapshots before transfer.

TallyOwl controls metric-series cost for each metric name:

- configurable active-series and retained-byte budgets;
- maximum labels, label bytes, and individual value length;
- exact accounting and visible cost estimates for high-cardinality labels;
- optional operator-configured overflow or rejection policy;
- local and head-side pressure and backpressure counters.

TallyOwl supports high-cardinality metric series. It does not silently put
these series in an overflow series.

Request, trace, span, session, and actor IDs usually cost less as event fields
or exemplars. An operator can also use them as metric labels.

Collectors support three metric entry paths:

- native TallyOwl push from the Go and Rust app drivers;
- Prometheus and OpenMetrics scraping of configured application endpoints;
- OpenTelemetry metric and trace push with the receiver protocol that existing
  SDKs and exporters use. TallyOwl rejects log records.

Compatibility receivers normalize temporality, types, units, labels, and
resource attributes into this metric model immediately. From the collector to
the head and storage nodes, the representation and transport are native CSIL.

TallyOwl services also publish Prometheus and OpenMetrics endpoints. Existing
capacity dashboards and alerts can use these endpoints.

The services can push the same instruments to an internal TallyOwl project.
A recursion guard and separate retention protect this project.

### 3.5 Identity and groups

- Anonymous IDs are random and project scoped.
- The trusted app backend supplies each known actor ID.
- TallyOwl stores the supplied actor ID in its project or workspace scope.
- An app can transform an actor ID before it sends the ID.
- `identify` links an anonymous timeline to a known actor from that point.
- `alias` is an explicit, auditable merge edge; it does not rewrite raw events.
- Group associations model organization, account, and team membership with validity
  time.
- Profiles contain only approved traits and support per-field sensitivity and
  retention policy.

The identity projector makes a versioned graph. Queries can use event-time
identity or latest-known identity.

### 3.6 Campaigns and attribution

A touchpoint records:

- landing, session, and event ID;
- referrer and referring domain;
- source, medium, campaign, term, and content;
- permitted external click IDs;
- channel classification and classifier version;
- first and last touch position;
- occurred time and consent state.

A conversion records:

- conversion ID and goal;
- actor, anonymous, and session links;
- exact decimal value and currency;
- order and reference ID when policy permits;
- campaign and touch links if supplied;
- event time and attribution window.

The attribution projector makes versioned results. Initial models:

- first touch;
- last touch;
- last non-direct touch;
- linear multi-touch;
- position based;
- time decay.

A model change recalculates attribution. It does not change raw touchpoints or
conversions.

A separate typed import contains campaign cost data. Campaign and time identify
each cost record. Return queries do not need a cost event property.

## 4. Storage layers

### Raw source segments

The accepted CBOR envelope exists in Corndogs and the committed WAL until a
checksummed native segment safely covers its log range.

Segment storage contains:

- typed envelope and payload columns;
- dynamic sparse columns;
- exact indexes;
- the payload hash and the batch ID;
- the collector receive time and the head commit time;
- the ingest version.

Segment storage does not retain an indefinite duplicate CBOR payload by
default.

The native detailed segment is the replayable source for retained data.
TallyOwl creates a separate segment only when a derived computation needs it.

An installation can keep raw CBOR for a short period. This option supports
validation and format migration. Ordinary queries do not need raw CBOR.

### Canonical typed views and projections

- `events`
- `error_occurrences`
- `error_groups` and group activity
- `spans`
- `metric_points`
- `identity_edges` and profile facts
- `campaign_touches`
- `conversions`
- `campaign_costs`

These names identify logical views. A rebuild can make a new physical segment
generation.

### Rollups

- time-bucket event and unique counts;
- error group and release counts;
- service-operation latency and error rate histograms;
- metric time series;
- daily actor and session activity;
- funnel step candidates;
- campaign and conversion summaries.

Rollups record input watermark, projector version, and lateness horizon.

### Control catalog

Control records live in TallyOwl's embedded transactional KV catalog on one
node and its replicated metadata state in a cluster.

- installations, workspaces, projects, environments, and sources;
- LinkKeys identity mappings and memberships;
- sessions and role bindings;
- API key and role-token IDs, hashes, scopes, limits, and revocation;
- node certificate identities, roles, serials, and expiration;
- collection policy versions;
- retention and privacy settings;
- saved queries, dashboards, cohorts, and funnels;
- alert definitions and notification targets;
- audit records and deletion and export workflow state.

## 5. Query model

The typed query contract should support:

- time range, timezone, and event-time and receive-time choice;
- one or more measures;
- dimensions and time interval;
- typed filter expressions with bounded nesting;
- cohort, actor, and session membership filters;
- ordering and bounded cursor pagination;
- comparison windows;
- sampling and approximation indicator;
- result metadata including scanned range, freshness, and warnings.

Domain-specific operations build on it:

### Events

- count, unique actors and sessions, numeric sums, averages, and quantiles;
- breakdown and trends;
- individual event, actor, and session timeline.
- exact lookup by event, request, trace, span, session, actor, and order ID and dynamic
  typed properties.

### Funnels

- ordered or unordered steps;
- within duration;
- same actor, session, and group constraint;
- exclusion steps;
- conversion time distribution;
- breakdown and comparison.

### Retention and cohorts

- initial and returning event definitions;
- daily, weekly, and monthly periods;
- first-time versus recurring semantics;
- behavioral and property cohorts;
- point-in-time cohort versioning.

### Paths

- previous and next paths around an event;
- semantic page and interaction nodes;
- bounded depth and minimum frequency;
- loop and noise collapsing.

### Errors and traces

- group trends, affected actors, releases, and environments;
- regression and release comparison;
- exact request, trace, span, session, and actor correlation across services and
  telemetry kinds;
- trace search by duration, status, service, operation, and attributes;
- service graph and latency and error correlations;
- jump from an error to its trace, events, session, and release.

### Metrics

- rate and increase for counters;
- last, minimum, maximum, and average for gauges;
- bucket merge and quantiles for histograms;
- label grouping and filtering;
- exemplars linking a point to a trace.

### Campaigns

- sessions, actors, conversions, value, cost, and return;
- model and window selection;
- channel, campaign, and content breakdown;
- first-touch versus converting-touch comparison;
- assisted conversions.

## 6. Retention, deletion, and privacy

An operator configures retention for each workspace, project, and data class:

- optional short-lived raw interchange envelopes;
- detailed events, spans, and error occurrences;
- metrics resolution tiers;
- derived actor profiles;
- aggregate rollups;
- quarantine and audit records.

A downsample policy keeps one-minute metrics, then hourly metrics. It removes
the raw points. The dashboard must show the resolution and the approximation.

A deletion target is an actor, a project, a workspace, a set of event IDs, or a
time range. A deletion workflow:

1. records an immutable request and authorization;
2. creates a tombstone preventing replay from resurrecting deleted data;
3. deletes or masks raw and canonical rows;
4. destroys the applicable cold-tier key material;
5. rebuilds affected rollups;
6. invalidates caches and cohorts;
7. records verifiable completion and exceptions.

The tombstone stays active until its horizon ends. Telemetry for an erased
actor can still be in a collector queue when the request lands. The ingest path
applies active erasure predicates to newly accepted data, so a late arrival
never becomes visible.

Backups have a documented deletion horizon. TallyOwl must not imply immediate
physical removal from immutable backups when that is not true.

TallyOwl prohibits direct personal data by default. Examples include names, email
addresses, credentials, headers, bodies, and form values.

TallyOwl permits an opaque actor ID. Correlation and actor erasure need this
ID. TallyOwl treats it as sensitive data.

The actor ID has project or workspace scope. An exact index supports lookup and
erasure. The host app supplies the erasure key.

## 7. Limits to settle before implementation

Initial defaults need an explicit capacity exercise:

- maximum event and batch sizes;
- max attributes and measurements and value lengths;
- max stack frames and breadcrumbs;
- metric label and series byte and resource budgets;
- browser and app buffer budgets;
- raw and detailed retention;
- allowed lateness and dedup window;
- query scan, result, time, and concurrency limits;
- maximum funnel steps and path depth and cohort complexity;
- quarantine size and expiry.

Each default must be safe for a small self-hosted installation. An operator can
change a default. The dashboard shows the cost of the change.
