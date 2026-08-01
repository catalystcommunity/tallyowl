# Cell architecture and growth

## 1. Purpose

The cell architecture supports one-node and 10,000-node installations. It uses
the same protocol, storage format, and query model at all sizes.

A cell is a regional control and data boundary. A cell contains controllers,
gateways, workers, and storage nodes.

## 2. Control hierarchy

The hierarchy has two control levels:

```text
Global directory
  ├── Regional cell A
  │     ├── cell controllers
  │     └── tablet groups
  ├── Regional cell B
  │     ├── cell controllers
  │     └── tablet groups
  └── Regional cell C
        ├── cell controllers
        └── tablet groups
```

The global directory maps a project to one or more cells. It stores global
policy and cell identity. It does not store telemetry or tablet placement.

Each cell controller quorum stores tablet placement for its cell. The quorum
also stores node identity, fencing epochs, and cell policy.

## 3. Home profile

The home profile uses one head process and one data directory. That process
contains the global directory role, cell controller role, gateway, query
service, and storage.

The home installation runs three containers: the head process, one collector,
and one Corndogs deployment. The head and the collector share that Corndogs
deployment and use separate queues.

The home profile has one tablet. It has one voter. The `local-one` receipt
policy acknowledges a write after a local fsync.

The home profile does not require object storage. It does not require a
controller quorum.

The home profile still enrolls its processes against a local installation
certificate authority. One identity path exists at every size. See
[NODE_IDENTITY.md](NODE_IDENTITY.md).

## 4. One-cell cluster

An operator can add nodes to make one regional cell. The protocol and storage
format do not change.

The cell can have:

- three or five cell controllers;
- stateless ingest gateways;
- stateless query coordinators;
- independent projector and workflow workers;
- storage nodes with many tablet groups;
- read and export replicas.

The controller moves the embedded tablet to a managed tablet group. The move
uses a snapshot and WAL catch-up. A fencing epoch prevents two writers.

## 5. Tablet routing

A project has stable virtual shards. Many virtual shards can use one tablet.

The primary affinity key depends on the telemetry type:

- A span uses its trace ID.
- A behavior event uses its session ID or end-user ID.
- A metric point uses its series ID.
- Other data uses a stable event key.

Exact indexes store the other correlation fields. A query can find related data
that is in different tablets.

The ingest gateway caches this route:

```text
project -> cell -> virtual shard -> tablet -> leader
```

Each response includes a route generation. A stale gateway follows a fenced
redirect and updates its cache.

## 6. Automatic tablet control

The cell controller measures these conditions:

- stored hot and warm bytes;
- sustained ingest rate;
- query processor load;
- compaction debt;
- storage-node disk pressure;
- replica lag.

The controller can split, merge, or move a tablet. Hysteresis prevents frequent
changes. Limits control concurrent changes.

An operator can select one of these modes:

- automatic;
- recommendation only;
- paused.

The normal production mode is automatic. The first releases can use
recommendation-only mode until tests prove safe automatic control.

## 7. Multi-region installation

A multi-region installation has one or more cells in each region. The global
directory assigns projects to cells.

Each tablet has one write region at one time. Different tablets can have
different write regions.

Other regions can contain:

- read replicas;
- export replicas;
- recovery replicas;
- cold object copies.

A regional failover uses a fenced control operation. The operation selects a
current replica and changes the write-region epoch.

## 8. Receipt policies

TallyOwl supports these final-storage receipt policies:

- `local-one`: one local voter and one fsynced copy;
- `local-quorum`: a local tablet quorum;
- `remote-one`: a local quorum and one remote durable copy;
- a custom failure-domain policy.

TallyOwl does not acknowledge an uncommitted entry in a multi-voter group.
This rule has no exception.

Therefore `local-one` is legal only for a tablet with one voter. It is the
default in the home and embedded profiles. A tablet with more than one voter
uses `local-quorum` by default.

The controller refuses `local-one` for a multi-voter tablet. It does not
silently change the policy.

Each receipt gives its satisfied policy and commit watermark.

A policy change is an online configuration change. TallyOwl first builds the
necessary replica set. The controller then activates the new policy.

The collector has a separate Corndogs durability setting. Its default is also
one durable copy.

## 9. Global-directory failure

An existing cell continues data operations during a global-directory failure.
It uses its current project assignments and policy generation.

The failure can prevent these operations:

- new project placement;
- project movement between cells;
- global policy changes;
- new cell registration.

The failure does not add the global directory to a tablet write path.

## 10. Cell failure

A cell-controller failure does not stop a healthy tablet group. It prevents
tablet placement changes until the controller quorum returns.

A tablet-quorum failure stops writes for that tablet. Other tablets continue
data operations.

A region failure starts the configured failover procedure. The possible data
loss depends on the receipt policy.

## 11. Project movement

The global directory records a project-movement intent. The source and target
cells then do this procedure:

1. Create target tablet groups.
2. Copy sealed segments and exact indexes.
3. Copy the applicable tombstone and policy generations.
4. Catch up the committed WAL.
5. Verify checksums and watermarks.
6. Stop the source writer with a fencing epoch.
7. Publish the target route generation.
8. Keep the source readable until old requests stop.
9. Remove the source data after the safety period.

The app and collector configuration does not change during the movement.

## 12. Automated node enrollment

An operator creates a role token. The token can permit repeated enrollment for
one role, cell, and region.

The token can include:

- an expiration time;
- a maximum use count;
- network restrictions;
- project restrictions;
- rate limits.

A new node generates its private key. It sends a certificate request and the
role token. The controller returns a short-life certificate.

The node uses mutual Transport Layer Security (mTLS) after enrollment. It uses
its current certificate for renewal.

A role token can enroll stateless nodes without manual approval. A storage token
can enroll a storage process. The controller controls data placement.

A role token cannot add a controller voter. It cannot change a tablet voter
set.

## 13. Growth sequence

An installation can use this sequence:

1. Start one home process and one Corndogs process.
2. Add stateless ingest and query nodes.
3. Create a three-controller regional cell.
4. Add storage nodes and move the embedded tablet.
5. Permit automatic tablet splits and moves.
6. Enable object storage for cold data.
7. Add read or export replicas.
8. Add another regional cell.
9. Move projects or tablets to the new region.
10. Add more cells without a global tablet-control group.

Each step is an online change. No step changes the app protocol or segment
format.

## 14. Scale tests

The scale prototype must include:

- one-process home tests;
- one-cell failure tests;
- multi-cell route and failover tests;
- a 400-node cell simulation;
- a 10,000-node multi-cell simulation;
- global-directory outage tests;
- tablet split and movement tests;
- regional failover tests for each receipt policy;
- role-token enrollment and revocation tests.

The 10,000-node test must show bounded global-directory work. Tablet placement
must stay in each applicable cell.
