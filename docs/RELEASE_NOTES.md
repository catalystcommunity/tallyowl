# Release notes

Each release has one section. The most recent release is first.

A version number here comes from the conventional commits in the release. The
first published release is 0.2.0, because the version calculator every
repository here uses starts an untagged repository at 0.1.0.

This is a first release. Read the limits before you install it.

## Not released yet — changes after 0.2.1

An antagonistic review on 2026-09-20 examined the scalability, the stability,
and the ease of use of TallyOwl. These are its results.
[IMPLEMENTATION_LOG.md](IMPLEMENTATION_LOG.md) L191 gives the reasons. The
release that contains these changes gets its version number from its commits.

### Read this before you upgrade

Each item below changes what an installation does with a configuration that
worked before.

- **A setting that does nothing now refuses a value that is not its default.**
  The settings are `integrity.mode: scrub`, `integrity.scrub.*`,
  `catalog.snapshots.*`, `storage.coldTier.enabled`,
  `placement.slowNode.factor`, `placement.slowNode.duration`, and
  `retention.audit`. Run `config check` before you upgrade.
  `integrity.mode: none` now stops verification. Before, it only wrote a log
  line.
- **Validation refuses more values.** It refuses a negative duration, a host
  name in a `*.listen` setting, `retention.detailed: 0s` together with a
  `retention.rollup` above zero, a `linkkeys.sessionLifetime` below one
  minute, and a `sampling.tail.keepPercent` that is not from 0 to 100. It
  refuses a `dashboard.callbackPath` that is different from the path of
  `linkkeys.callbackUrl`.
- **An unknown setting writes a warning at start.** The service still starts.
  An unknown `--a.b` flag is now an unknown setting. Before, the head read it
  as a verb.
- **Every network connection uses TLS (D62).** A listener on a loopback
  address or a unix socket can stay plaintext. A listener on a network address
  needs TLS material, and `config check` refuses it without that material. On
  one host, the services need no certificates. For a network installation:
  - Make an authority with `tallyowl-head ca create <directory>`. Give its
    intermediate to each head (`installation.signingCertificate`,
    `installation.signingKey`) and its root to each service
    (`installation.authorities`).
  - Give each collector a certificate for applications
    (`tls.certificateDirectories`) and a role token (`enrollment.roleToken`).
    A collector enrolls for its node certificate at each start.
  - `transport.allowPlaintext: true` permits plaintext on a network address.
    Use it only on a network that something else protects.
  - The hop to Corndogs uses TLS. TallyOwl needs Corndogs release 0.7.6 or
    later. Set `corndogs.tls.caFile` to the authority of the Corndogs
    certificate. The head chart needs `corndogsDeployment.tlsSecret` for its
    Corndogs sidecar.
  - A dashboard on a network address needs `dashboard.allowPlaintext: true`,
    because the Gateway ends TLS in front of it.
  - A renewal, a `commit-batch`, and a consensus message each need the proved
    identity of the node that sends it. `enrollment.allowUnverifiedRenewal`
    is removed.
- **The charts need their security values.** Neither chart renders with its
  default values. The head chart needs `deployment.tls.signingSecret` and
  `deployment.tls.authorities`. The collector chart needs
  `deployment.tls.certificateSecrets`, `deployment.tls.authorities`, and
  `deployment.tls.roleTokenSecret`. Set `transport.allowPlaintext` instead
  only on a network that something else protects. The environment variables
  that carry secrets are now `SECRET_TALLYOWL_API_KEY` and
  `SECRET_TALLYOWL_ROLE_TOKEN`, because the loader reads each `TALLYOWL_*`
  variable as a setting.
- **The native alert callback is signed.** A rule with a `csil-callback`
  target needs a `secret_ref`. The receiver implements
  `TallyOwlAlertReceiver.notify` from the ingest contract and verifies each
  call with `VerifyAlertCallback` (Go) or `verify_alert_callback` (Rust). The
  request shape changed: it was a raw JSON payload.
- **The Corndogs sidecar has its own pull policy.**
  `corndogsDeployment.imagePullPolicy` in the head chart. Empty, the default,
  means `image.pullPolicy`, as before.
- **`tallyowl-head token create|list|revoke`** makes, lists, and stops the
  role tokens that collectors enroll with.
- **The app drivers use TLS by default.** The Go and the Rust app drivers use
  TLS with the trusted authorities of the operating system for a network
  address, and plaintext for a loopback or `unix:` address. A collector whose
  certificate comes from a private authority needs that authority in the
  driver's `Transport` setting. Plaintext to a network address needs
  `AllowPlaintext`.
- **csilgen moved to 0.2.9.** A generated decoder reserves at most 1,024
  elements from a length on the wire. Before, a small frame could reserve 32
  times its own size.
- **A webhook to a private, loopback, or link-local address is refused.** Put
  the host in `alerts.allowedPrivateTargets`. An installation on one host that
  sends a webhook to a local receiver must do this.
- **The head refuses a batch from a source that it did not issue**, and rejects
  an item whose project is not the project of its source.
  `ingest.requireKnownSource: false` restores the old behavior.
- **Intake refuses a frame larger than `collector.maxFrameBytes` (4 MiB).**
  Before, the limit was `corndogs.maxPayloadBytes` (16 MiB). Intake closes a
  connection that is idle for `collector.idleTimeout` (5 minutes), and the app
  driver connects again.
- **A scraped series has a new identity.** Each scraped series gets an
  `instance` label with the host and port of its target. A counter from an
  OpenMetrics target keeps its `_total` suffix. Without the label, two replicas
  of one application were one series, and each scrape looked like a restart.
  Queries on old scraped series and on new scraped series give two series.
- **`metrics.maxBytesForEachMetric` is now the size of the active series.**
  Before, it was the bytes since the collector started. A constant metric
  filled it in some hours, and then each new series was refused.
- **`split-tablet`, `move-tablet`, and `assign-project` refuse on a node that
  runs consensus groups.** No code did the work after the topology changed.
- **The store refuses to open an append log that has damage before
  acknowledged frames.** Before, it removed all frames after the damage and
  wrote nothing. The message gives the file, the position, and the procedure.
- **The collector chart does not render with its default values.** Set
  `corndogs.endpoint` and `head.endpoint`. The loopback defaults made a pod
  that stopped at each start.
- **Both services stop on SIGTERM.** They fail readiness, stop their
  listeners, complete the requests in progress, and exit with code 0. The
  charts set `terminationGracePeriodSeconds`.
- **The Go app driver and the Rust app driver keep a batch that was not
  acknowledged.** `MaxBatchAttempts` changes from 3 to 5 and counts only an
  attempt with an unknown result. `Flush` can send more than one batch and
  returns one receipt for all of them. In the browser package,
  `attachUnloadFlush` now returns a function that removes the listeners.

### Data that could be lost, and is not lost now

- An acknowledged batch that was not in a segment was lost when the append log
  was empty at a restart. The log started its positions at 0 again, below the
  checkpoint.
- A seal that failed for a reason other than disk space lost its rows.
- A segment from a different node moved the local checkpoint, and the next
  seal removed local frames that no segment held.
- A replica that was behind the purged log installed a snapshot with no rows
  and reported that it was current. It now copies the segments first. A
  snapshot also carries the erasures.
- A restart lost the committed controller commands, because the node kept the
  applied position on disk and the state in memory.
- A follower with a full disk skipped an entry permanently. It now stops.
- A voter that returned with an empty disk could vote, and a majority could
  then remove an acknowledged write. It now holds its vote until it is
  current.
- An app driver discarded a sealed batch when a send failed, and `Shutdown`
  then reported 0 items.
- A scrape lost most of its points. The event IDs of the compatibility edge
  repeated each 16 counts, and the head removes a repeated event ID.

### Isolation between tenants

- `put-policy` authorizes on the scope. Before, a person with no role could
  set the installation kill switch.
- An alert rule can read only its own project.
- An export writes a plain file name below `exports/<project>/` and does not
  replace a file. Before, the caller gave a path, and the head truncated that
  file.
- `list-notifications` returns the notifications of one project.
- The role-token rate limit is enforced. A revocation is not lost when an
  enrollment occurs at the same time.

### One stalled peer, and one bad value

- Each RPC client has a read deadline and a write deadline of 30 seconds.
  Before, a peer that accepted a connection and did not answer held the caller
  with no limit, and readiness stayed correct.
- The collector uses a pool of Corndogs connections, and a separate head
  connection for delivery and for key checks. A watchdog fails readiness when
  the delivery loop does not turn.
- The forwarder waits from 1 to 30 seconds between probes of a head that does
  not answer. Before, it claimed, failed, and wrote each batch again with no
  wait.
- A retried batch keeps its priority.
- A panic in a request handler, a background loop, or a scrape is contained,
  counted, and written to the log.
- A decimal with an exponent outside -38 to 38 rejects one item. Before, the
  head tried to allocate the zeros, stopped, and received the same durable
  batch again after each restart.
- A query checks its deadline inside its loops. Each project has a limit on
  concurrent queries. A query tree has a depth limit in the executor.
- An alert rule with `sustained_ms` or `for_ms` above zero now fires. Before, it
  could not.
- Two projects can each have a rule with the same name. Before, one of them
  was not evaluated.
- The downsample pass follows a durable watermark and does not skip a window.
  It no longer counts a rollup row a second time.

### Easier to use

- README has an installation path and a list of the ways to monitor an
  application.
- Each app driver and the browser package has a README with a complete first
  program, a table of failures, and the defaults.
- The Go app driver has `Run`, `Stats`, `OnError`, `DryRun`, `StartSpan`,
  `Middleware`, `ErrorFrom`, and `FromBrowser`. The Rust app driver has the
  equivalent functions, a quick-start example, and the constructors that only
  the Go app driver had.
- The head has `member add`, `member remove`, and `member list`. `provision`
  gives the operators the new workspace. The dashboard accepts a session
  token.
- The head chart has a maintenance Job for the administration verbs, Secret
  and environment values, resource requests, a container security context,
  and an optional NetworkPolicy and ServiceMonitor. A replicated installation
  gives each pod its own `replication.advertise` address.
- `deploy/monitoring/` has alert rules for TallyOwl itself. A test fails when
  a rule names an instrument that no service registers.
- A scrape target that is down gives an `up` series with the value 0. The
  OpenTelemetry receiver refuses a body that it cannot read. Before, it
  answered 200.

### What is not in these changes

- **Unix sockets on Windows.** A `unix:` address works on Linux and macOS. On
  Windows it is refused with a message.
- A control operation for a key, a member, an erasure, or an export on a head
  that runs. Each of these needs a new CSIL operation.
- A limit on the memory of the segment cache. An opened segment stays in
  memory.
- Authentication, TLS, and service discovery for scrape targets.
- A multiplexed connection between consensus peers.
- A second roll-up of a window when a point arrives after
  `metrics.downsampleLatenessGrace`.

## 0.2.0 — 2026-08-12

The first published release. It is the first build of TallyOwl with package
coordinates, a container image, and a client compatibility window.

### Collection behavior changes

There is no earlier release, so each change below is a change from a build of
TallyOwl from before 2026-08-10. Read this section if you run one.

**TallyOwl now refuses four client property names.** An application that sends
a property with one of these names sees the property removed:

- `request_id`
- `session_id`
- `trace_id`
- `event_id`

TallyOwl removes only the property. It keeps the event, the error, the trace,
or the metric point that carried it, and it stores everything else in the item.
It counts each refusal in `tallyowl_protected_property_refused_total`.

**Why the change:** each of these names is also an envelope field. A query that
refers to one of these names reads the envelope column, so a filter could never
read the property value. The value looked like it was stored and it was not
usable. A refusal that an operator can count is better than a value that is
silently unreachable.

**What to do.** Look at your instrumentation code before you upgrade:

1. Find each property your application sends with one of the four names.
2. Put the value in the envelope field with that name, if the value is the
   correlation identifier. The app drivers do this for you.
3. Give the property another name, if the value is something else. An example
   is `checkout_request_id`.
4. Look at `tallyowl_protected_property_refused_total` after the upgrade. A
   count that goes up means an application still sends one of the four names.

TallyOwl refused four operator property names before this release: `region`,
`env`, `cell`, and `installation`. That behavior does not change.

### The compaction settings, and the defaults they ship with

These three settings are new, and each default is measured.
`docs/BENCHMARKS.md` sections 23 to 24.2 hold the measurements.

| Setting | Default | What it does |
| --- | --- | --- |
| `compaction.coldGroupAfter` | `48h` | How old data must be before the cold pass groups it by end user |
| `compaction.coldGroupTarget` | `2MiB` | The size the cold pass builds each group to. It is the price of one cold exact lookup, which is about 215 ms |
| `compaction.coldGroupBatch` | `32MiB` | How many stored bytes one pass holds at a time. The rows of a batch cost many times their stored bytes in memory |

Increase `compaction.coldGroupBatch` with care. The setting counts stored
bytes, and the rows those bytes become are much larger.

### The log privacy filter

The log privacy filter refuses a field name by whole word rather than by
substring. A field such as `files_reclaimed` prints its value now, because
`claim` is no longer found inside `reclaimed`. A field such as `claim_type`,
`x-api-key`, or `client_ip` is still refused.

### What this release contains

- the collector, the head, storage, query, and the dashboard;
- replicated storage with per-tablet voter groups, and a controller quorum;
- product behavior: identity, funnels, retention, paths, timelines, per-user
  erasure, saved analyses, and dashboards;
- campaigns, six attribution models, and consent-aware collection;
- alert rules, schedules, and workflows;
- compatibility receivers for Prometheus, OpenMetrics, and OpenTelemetry. Each
  one stays closed until an operator opens it;
- export to Parquet.

### Where the artifacts are

| Artifact | Coordinate |
| --- | --- |
| Head and collector image | `containers.catalystsquad.com/public/catalystcommunity/tallyowl:0.2.0` |
| Head chart | `tallyowl` version `0.2.0`, from the `catalystcommunity/charts` repository |
| Collector chart | `tallyowl-collector` version `0.2.0`, from the same repository |
| Browser package | `@catalystcommunity/tallyowl-browser` version `0.2.0` on npmjs |
| Go app driver | `github.com/CatalystCommunity/tallyowl/packages/driver-go` at tag `packages/driver-go/v0.2.0` |
| Generated clients for Go | `github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-<name>-api` at tag `generated/go/tallyowl-<name>-api/v0.2.0` |
| Binaries | `tallyowl-0.2.0-linux-amd64.tar.gz` on the GitHub release page, with `SHA256SUMS` |
| Rust app driver | Not published. See "What is not in this release" |

The image runs as user and group 65532. Both charts set the same numbers, and
the head chart sets `fsGroup` so that its volume is writable.

**The image carries the dashboard.** The head serves it from
`/usr/local/share/tallyowl/dashboard`, and both charts point at that path. To
reach it from outside the cluster, turn on the Gateway API route:

```sh
helm install tallyowl … \
  --set gateway.enabled=true \
  --set gateway.parentRef.name=<your gateway> \
  --set gateway.hostnames[0]=tally.example.com
```

The charts attach an HTTPRoute to a Gateway you already run; they create no
Gateway, and they render no Ingress. Ingest is not routed: it is CSIL over TLS
over TCP, and an application reaches the collector directly.

**Every listening address in a rendered chart faces the network.** The settings
keep loopback defaults, which is right for a workstation and wrong for a pod,
so the charts rewrite the host from `bindAddress`.

**To run TallyOwl without Kubernetes**, take the archive from the release page.
It holds `tallyowl-head`, `tallyowl-collector`, and the license. Check it
first:

```sh
sha256sum --check SHA256SUMS
tar xzf tallyowl-0.2.0-linux-amd64.tar.gz
```

The binaries come out of the container image, so they need a glibc as new as
Debian 12 has. A build for another platform is not in this release.

**The browser package arrives staged.** npm is removing the token that
bypasses two-factor authentication, so the release pipeline stages the publish
and a maintainer approves it. `@catalystcommunity/tallyowl-browser` becomes
installable when that approval happens, which is minutes after the release
rather than at the same moment. Everything else — the image, the charts, the
binaries, and the Go modules — is available as soon as the tag is.

### The compatibility window

D31 gave no client compatibility window before the first release. This release
opens one.

- There is one protocol version, and it is version 1. The head reports it in
  `protocol_version` in every commit receipt.
- **An app driver declares the version it speaks on every batch**, and a
  collector declares its own when it forwards. TallyOwl accepts the current
  version and the one before it, at the collector and again at the head.
- A batch outside that window is refused with a message that names both ends,
  and `tallyowl_protocol_version_refused_total` counts it. The collector
  refuses before the batch becomes durable, so an application learns
  immediately.
- Nothing a current client sends is refused today, because there is one
  version. A driver that declares nothing is accepted.
- TallyOwl removes support for a protocol version one minor release after the
  release that replaces it, and these notes say so before the removal.
- An application compiles the ingest contract into its own build, so each
  application upgrades on its own schedule. Version skew between two
  applications is normal, and TallyOwl expects it.

### Upgrade

There is no earlier release, so there is no upgrade path to test against. The
rolling-upgrade procedure is in `docs/DEPLOYMENT.md` section 7. It restarts the
ingest head, then the storage voters one at a time, then the collectors.

### What is not in this release

- **The Rust app driver is not on crates.io.** Publishing it publishes the
  crates it depends on, and each of those takes a public name that cannot be
  taken back. The project owner turned crates.io off until the release pages
  have proved themselves. A Rust application depends on the driver by Git
  revision meanwhile:

  ```toml
  tallyowl-driver-rust = { git = "https://github.com/CatalystCommunity/tallyowl", tag = "v0.2.0" }
  ```

- **the generated clients for TypeScript are not on npmjs.** An application
  includes `csil/tallyowl-ingest.csil` in its own CSIL build and generates its
  own client, which is the intended path. The generated package also does not
  compile on its own today: its server dispatch throws an error with a number
  for a code, and this contract gives an error a name rather than a number. The
  browser package is not affected, and it carries the code it needs;
- **the rolling upgrade is drilled, not proven across two versions.** This is
  the first version, so there is no second version to hold a compatibility
  window against. The procedure runs under load with clean reconciliation;
- **the dashboard package is not published.** The head serves the dashboard.
