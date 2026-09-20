# Release notes

Each release has one section. The most recent release is first.

A version number here comes from the conventional commits in the release. The
first published release is 0.2.0, because the version calculator every
repository here uses starts an untagged repository at 0.1.0.

This is a first release. Read the limits before you install it.

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
