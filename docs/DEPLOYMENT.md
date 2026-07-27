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

## 2. Charts

| Chart | Installs |
| --- | --- |
| `tallyowl` | Head roles, query, storage, dashboard, controller, and workers |
| `tallyowl-collector` | Intake, forwarder, and compatibility receiver |

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

## 6. Placement and disruption

- spread tablet voters across failure domains with anti-affinity rules;
- a disruption budget preserves intake capacity and query capacity;
- a disruption budget never overrides safe draining;
- a storage pod keeps its node ID on its persistent volume, so a replacement
  pod requests a certificate for that same node ID;
- a stateless pod gets a new node ID for each start.

## 7. Upgrade order

1. Upgrade the head ingest and query roles first, because the head accepts the
   current and the previous protocol version.
2. Upgrade storage nodes one failure domain at a time.
3. Upgrade collectors last, so no collector speaks a version the head does not
   accept.
4. Upgrade cells one at a time in a multi-region installation.

Before a release candidate there is no client compatibility window. See D31.

Head ingest stops readiness before it drains. Collectors stop intake, finish or
release claimed tasks, and leave queued data durable.

## 8. Required tests

1. Chart rendering for every profile, with no cluster.
2. A refused `local-one` on a multi-voter tablet.
3. A refused `durable_copies` value that the backend cannot satisfy.
4. A refused duplicate `cell.id`.
5. A refused installation with a mismatched CA fingerprint.
6. An install, an upgrade, and a rollback in a disposable cluster.
7. A second cell joining an existing installation.
8. A cell that continues data operations while the global directory is down.
9. A rolling upgrade with live traffic and adjacent protocol versions.
