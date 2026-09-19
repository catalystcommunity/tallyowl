# Deployment and Helm charts

This document uses these abbreviations: certificate authority (CA) and mutual
Transport Layer Security (mTLS). [DOCUMENTATION.md](DOCUMENTATION.md) holds the
full list.

## 1. Purpose

Two Helm charts install TallyOwl. The same charts serve a home lab and a
multi-region installation.

A multi-region installation is several releases of the same charts in several
clusters, and those releases must coordinate. That case is not an afterthought
in this design. Section 5 gives it first-class treatment, because a chart that
only works for one cluster is not useful to the people who need cells.


### One installation, one device

`storage.reserveBytes` holds space back so that recovery can still write when a
device fills. **The reserve belongs to the device and one process enforces it**,
so two installations sharing a device each hold back the same bytes and each
treats them as its own. Both can spend the reserve at once.

No data is at risk from this: a write is still refused when the device cannot
take it, because that check reads the real free space. What is at risk is the
recovery the reserve exists for.

An installation reports its device at start-up and in
`tallyowl_storage_device_info`. Two installations reporting the same number are
sharing a reserve. Give each one a device, or accept that the reserve is
advisory. See FAILURE_MODES.md section 10.

## 2. Charts

| Chart | Installs |
| --- | --- |
| `tallyowl` | Head roles, query, storage, dashboard, controller, and workers |
| `tallyowl-collector` | Intake, forwarder, and compatibility receiver |

A collector holds no durable state. Corndogs owns the queue and the payload, so
a collector stays a stateless Deployment.

Each chart can reference an existing Corndogs service or install a tightly
scoped local one.

A chart never puts a role token, a key, or any other secret in a command line,
a log, or rendered manifest output.

## 3. Profiles

A profile selects placement and replication. It never changes the stored format
or the application integration.

| Profile | Shape |
| --- | --- |
| `home` | One head pod, one volume, one tablet, one voter, `local-one` receipts |
| `home-collector` | One collector, one durable Corndogs copy |
| `home-limits` | No read replica, no cold object store, no separate projector |
| `replicated` | One cell, three controllers, three tablet voters, stateless gateways, separate workers |
| `scaled` | Several cells, a global directory, read and export replicas, role-specific pools |

`home` is the default.

A home installation runs three containers: the head, one collector, and one
Corndogs. The head and the collector share that Corndogs and use separate
queues.

A profile change is an online change. It moves data. It does not rewrite it.

## 4. Values that matter

### Installation identity

| Value | Meaning |
| --- | --- |
| `installation.id` | Identifies one logical TallyOwl installation |
| `installation.caBundle` | The trust root for every node certificate |
| `installation.caFingerprint` | The value a new node verifies before enrollment |

### Cell identity

| Value | Meaning |
| --- | --- |
| `cell.id` | Identifies this cell inside the installation |
| `cell.region` | The region label for placement and for write ownership |
| `cell.controllers` | Three or five; the controller quorum size |
| `globalDirectory.endpoints` | The global directory, when one exists |

### Storage and durability

| Value | Meaning |
| --- | --- |
| `storage.receiptPolicy` | `local-one`, `local-quorum`, `remote-one`, or custom |
| `storage.tabletVoters` | Voting replicas for each tablet |
| `storage.coldTier.enabled` | Object storage for cold segments |
| `corndogs.durableCopies` | Copies required before a collector acknowledgement |
| `corndogs.backend` | `file` or `postgres` |
| `corndogs.maxPayloadBytes` | Corndogs payload limit; 16 MiB by default |
| `corndogs.maxDeliveryAge` | How long a batch may keep being retried; 24 hours by default |
| `storage.deduplicationWindow` | How long the head remembers a batch ID; 72 hours by default |

### Replication and placement

A home installation leaves `replication.listen` empty and none of the rest
applies. An operator turns replication on by giving the node an address that
peers can reach: no address, no listener, no cluster.

| Value | Meaning |
| --- | --- |
| `replication.listen` | Where this node answers other storage nodes. Empty in the home profile |
| `replication.peers` | The peers a first bootstrap starts with. A cell learns its peers from its controller quorum afterwards |
| `replication.writeTimeout` | How long a write waits for its tablet group to commit; 10 seconds by default |
| `node.name` | What the control plane calls this node. Empty means the head assigns one at enrollment |
| `node.failureDomain` | The rack, zone, or host this node is in |
| `placement.mode` | `automatic`, `recommendation-only`, or `paused`; recommendation-only by default |
| `placement.splitAbove` | Split a tablet larger than this; 64 GiB by default |
| `placement.mergeBelow` | Merge two adjacent tablets smaller than this; 8 GiB by default |
| `placement.concurrentChanges` | Splits, merges, and movements that may run at once in one cell; one by default |
| `query.maxFanOut` | Tablets one query may ask at once; 256 by default |

`replication.listen` must have an address when `storage.tabletVoters` is
greater than one, and TallyOwl refuses to start when it does not. A node with
peers and no address to be reached at elects nothing and refuses every write,
which reads as a storage fault rather than as a missing setting.

`placement.mergeBelow` must be below half of `placement.splitAbove`, and
TallyOwl refuses to start when it is not. Two merged tablets that were
immediately over the split threshold would make a cell rewrite the same data for
ever. See CELLS.md section 6 on hysteresis.

`placement.mode` is `recommendation-only` by default. The controller decides in
every mode and the mode selects whether it acts, so the automatic path is
exercised at every installation rather than being code nobody ran until the day
it was switched on.

`corndogs.maxPayloadBytes` must exceed the batch seal size in D19, which is
512 KiB.

`storage.deduplicationWindow` must exceed `corndogs.maxDeliveryAge`, and
TallyOwl refuses to start when it does not. D36 makes the two one decision: a
retry that arrives after the head has forgotten the batch ID commits a second
logical batch, and no query can remove it afterwards.

### Dashboard

| Value | Meaning |
| --- | --- |
| `dashboard.enabled` | Serve the dashboard; on by default |
| `dashboard.listen` | Where the dashboard serves its document, its bundle, and the browser carrier |
| `dashboard.assets` | Where the built dashboard bundle is |
| `dashboard.callbackPath` | The path a LinkKeys sign-in returns to |

`dashboard.listen` is the origin `linkkeys.callbackUrl` must name, and
`dashboard.callbackPath` must be that URL's path. A sign-in returns to an
address nothing serves when the two disagree.

The dashboard carries `TallyOwlControl` and refuses every other service, so it
is not an ingest surface. Telemetry reaches a collector over CSIL.

### Integrity and recovery

| Value | Meaning | Default |
| --- | --- | --- |
| `integrity.mode` | `none`, `verify-on-read`, or `scrub` | `verify-on-read` |
| `integrity.scrub.period` | How long one full pass takes | 7 days |
| `integrity.scrub.rateLimit` | Read bandwidth the scrub may use | 16 MiB each second |
| `catalog.snapshots.enabled` | Periodic catalog snapshots | `false` |
| `catalog.snapshots.period` | Time between snapshots | 1 hour |
| `catalog.snapshots.keep` | Snapshots retained | 2 |
| `placement.slowNode.action` | `alert` or `demote` | `alert` |
| `placement.slowNode.factor` | Multiple of the group median that counts as slow | 4 |
| `placement.slowNode.duration` | How long it must hold before the state changes | 5 minutes |
| `compaction.gcGrace` | Wait before deleting an unpinned generation | 1 hour |
| `storage.reserveBytes` | Space held back so recovery can still write | 1 GiB |

`integrity.mode: none` turns off every integrity check. The dashboard shows
that state, because an operator who inherits an installation must not have to
discover it. See D57.

`compaction.gcGrace` must exceed the maximum query runtime the budget permits.
The chart refuses a value that does not. See
[FAILURE_MODES.md](FAILURE_MODES.md) section 8.1.

The chart refuses `storage.receiptPolicy: local-one` when
`storage.tabletVoters` is greater than one. `local-one` is legal only for a
single-voter tablet. See D27.

The chart refuses `corndogs.durableCopies` greater than one on a backend that
cannot satisfy it. See D4.

## 5. Multi-cluster and multi-region

A large installation is several chart releases that must agree about identity
and disagree about location. Getting that split wrong is the most common way to
break a distributed installation.

### Values that must match everywhere

Every release in one installation uses identical values for these:

- `installation.id`;
- `installation.caBundle` and `installation.caFingerprint`;
- the segment format version range that the release supports;
- the protocol version range that the release supports;
- `globalDirectory.endpoints`, once a global directory exists.

A mismatch in any of these produces a node that enrolls and then cannot join.
The chart fails the installation rather than starting a node that will not
work.

### Values that must differ

Each cell release sets its own:

- `cell.id`, unique in the installation;
- `cell.region`;
- storage volume classes and sizes appropriate to that cluster;
- collector endpoints local to that cluster.

The chart refuses a duplicate `cell.id` when it can reach the global directory.

### Bootstrap order

1. Install the first cell with `globalDirectory.bootstrap: true`. That release
   creates the installation ID and the CA.
2. Export the CA fingerprint and a role token from that release.
3. Install each additional cell with the same installation identity values and
   its own cell identity values.
4. Each new cell enrolls its nodes against the existing CA.
5. Register the new cell with the global directory.
6. Assign or move projects to the new cell.

Step 2 is the only manual handoff. A role token is a reusable credential with
its own expiry, use count, and network restrictions. See
[NODE_IDENTITY.md](NODE_IDENTITY.md).

### Cross-cluster connectivity

- a collector needs to reach its head over TCP with TLS;
- a storage node needs to reach the other voters of its tablets;
- a cell controller needs to reach the global directory, but a cell continues
  data operations without it;
- a collector never needs to reach another cluster's collector.

A storage node does not need to share a Kubernetes cluster with a collector.

### Write ownership

Each tablet has one write region at one time. A fenced control operation
changes it. Different tablets can have different write regions, so an
installation writes in many regions without a multi-writer conflict for one
tablet. See D27.

The chart does not decide write ownership. The controller does.

## 5a. Infrastructure requirements

TallyOwl states what it needs from infrastructure. It does not restate another
product's durability numbers as its own objective. See D53.

| Requirement | Why |
| --- | --- |
| A volume that honours `fsync` | Every durability claim rests on it. A volume that acknowledges a flush without one makes every receipt a lie. |
| A storage class with the redundancy the operator needs | TallyOwl acknowledges at the configured receipt policy. Surviving the loss of the volume itself is the infrastructure's job. |
| An object store with its own durability guarantee | The cold tier holds retained telemetry. Its durability is the bucket's. |
| A backup target that the operator tests | A restore that nobody has run is not a backup. |

A process crash needs no recovery procedure. Writes are atomic, and a restart
loses nothing that TallyOwl acknowledged.

A region loss is a convergence problem rather than a recovery one. A surviving
region continues, and CELLS.md gives the fenced failover and the watermarks
that show convergence.

## 6. Placement and disruption

- spread tablet voters across failure domains with anti-affinity rules;
- a disruption budget preserves intake capacity and query capacity;
- a disruption budget never overrides safe draining;
- a storage pod keeps its node ID on its persistent volume, so a replacement
  pod requests a certificate for that same node ID;
- a stateless pod gets a new node ID for each start.

The head chart carries three groups of values for this. A home installation has
one replica and none of them applies to it.

| Value | Meaning |
| --- | --- |
| `topologySpread.constraints` | Spread replicas across zones and then across nodes, with a skew of one |
| `affinity.antiAffinity` | `required` by default: two voters of one tablet never share a node |
| `disruption.minAvailable` | How many replicas must stay during a voluntary disruption; two of three by default |

`disruption.minAvailable` is a count and not a percentage. A percentage of a
shrinking set rounds in the wrong direction, and this number has to hold when
the set is already smaller than it should be.

A node reports its own domain in `node.failureDomain`, and the controller places
at most one voter of a tablet in each domain. When a cell has fewer domains than
a tablet has voters, the controller still places the tablet and reports that one
domain's loss can end its quorum. It does not refuse, because refusing to place
data is worse than placing it and saying what the risk is.

## 7. Upgrade order

1. Upgrade the head ingest and query roles first, because the head accepts the
   current and the previous protocol version.
2. Upgrade storage nodes one failure domain at a time.
3. Upgrade collectors last, so no collector speaks a version the head does not
   accept.
4. Upgrade cells one at a time in a multi-region installation.

The compatibility window opened at the first release, 0.2.0, and it is
enforced rather than promised. An app driver declares the protocol version it
speaks on every batch, and a collector declares its own when it forwards.
Collector intake and the head each accept the current version and the one
before it, from one list. A version outside the window is refused with a
message that names both ends, and `tallyowl_protocol_version_refused_total`
counts it, by version.

That is what makes the order above safe: the head is upgraded first, so every
collector still running is one version behind the head it forwards to, which is
inside the window by construction. Support for a protocol version ends one
minor release after the release that replaces it, and the release notes say so
before it ends. There is one protocol version today, so nothing a current
client sends is refused. See D31, `docs/RELEASE_NOTES.md`, and L183.

Head ingest stops readiness before it drains. Collectors stop intake, finish or
release claimed tasks, and leave queued data durable.

## 7b. Reaching an installation

### What a pod binds

The settings tree keeps the loader's own defaults, and those are loopback: a
process on a workstation must not open a port to the network because somebody
started it. A pod is the other case. A loopback bind there reaches nothing, and
the Service in front of it forwards to a port no client can use.

Both charts therefore rewrite the host part of every listening address in the
rendered configuration, from the deployment value `bindAddress`, which defaults
to `0.0.0.0`. The port always comes from the setting, so a changed port reaches
the process and the Service together. `replication.listen` stays empty when it
is empty, because an empty value means a node with no replication port.

Set `bindAddress` to `127.0.0.1` for a pod whose only client is a sidecar.

### The dashboard bundle

The container image carries the dashboard at
`/usr/local/share/tallyowl/dashboard`, and the deployment value
`dashboardAssets` points the head at it. The setting keeps the loader default,
which is the path a developer builds into. An image without the bundle serves
an empty page, which is why `helm-check` refuses a rendered configuration whose
`dashboard.assets` is a relative path.

### Gateway API

Traffic from outside the cluster reaches the dashboard through an **HTTPRoute**.
The charts render one when `gateway.enabled` is true, and refuse to render a
route with no `gateway.parentRef.name`, a dashboard route with the dashboard
turned off, or a dashboard route on the collector chart, which serves none.

```sh
helm install tallyowl oci://…/tallyowl \
  --set gateway.enabled=true \
  --set gateway.parentRef.name=platform \
  --set gateway.hostnames[0]=tally.example.com
```

**No Gateway is created.** A Gateway carries a listener and an address that
belong to the platform, and a chart that created one would be claiming both.

**Ingest gets no route.** Native TallyOwl traffic is CSIL over TLS over TCP and
an application reaches the collector directly. An HTTPRoute in front of intake
would be a generic HTTP ingest API, which `AGENTS.md` forbids.

**There is no Ingress template.** Gateway API is the interface these charts
support. Ingress can be added when somebody needs it.

## 8. Required tests

1. Chart rendering for every profile, with no cluster.
2. Chart values and the configuration loader agree on every key, every type,
   and every required value for the `home` profile. This test fails when a
   setting reaches only one of them. It is what keeps the local development
   loop matching a deployment. See PLAN.md Phase 1.
3. A refused `local-one` on a multi-voter tablet.
4. A refused `durable_copies` value that the backend cannot satisfy.
5. A refused duplicate `cell.id`.
6. A refused installation with a mismatched CA fingerprint.
7. An install, an upgrade, and a rollback in a disposable cluster.
8. A second cell joining an existing installation.
9. A cell that continues data operations while the global directory is down.
10. A rolling upgrade with live traffic and adjacent protocol versions.
11. A refused `compaction.gcGrace` shorter than the maximum query runtime.
