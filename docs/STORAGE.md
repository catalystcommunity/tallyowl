# Native storage design

This document uses these abbreviations: write-ahead log (WAL), key-value (KV),
Concise Binary Object Representation (CBOR), and CBOR Service Interface
Language (CSIL). [DOCUMENTATION.md](DOCUMENTATION.md) holds the full list.

## 1. Goals

The storage system must have one understandable path from a tiny installation
to a large replicated cluster:

- one binary and one data directory for a home lab;
- low idle CPU and memory with no mandatory external database;
- safe acknowledgement and crash recovery;
- sustained append throughput and bounded write amplification;
- time-range and dimensional queries over telemetry;
- immediate logical deletion and bounded physical reclamation;
- horizontal write sharding and read replication;
- exact high-cardinality correlation and filtering;
- hot and warm local data with optional cold object storage;
- online backup, export, tiering, and tablet movement;
- a compact documented native format, with optional export to Parquet for
  DuckDB and other tools;
- no single implementation library as the permanent data-format contract.

Simplicity is the governing rule. The design uses an append log, immutable
segments, a small manifest catalog, tombstones, and compaction. A feature needs
strong justification if these terms cannot explain it.

## 2. Non-goals

- General relational transactions across telemetry rows
- In-place mutation of immutable telemetry records
- A distributed SQL dialect
- Making Corndogs the retained data store
- Pretending arbitrary field and value sizes are free
- Requiring object storage, a consensus cluster, or a separate catalog service
  for a single-node installation

## 3. Storage layers

### 3.1 Append log

The append log is the final storage acceptance boundary.

Each record is a length-delimited, checksummed frame containing:

- storage format version;
- tablet and virtual-shard IDs plus a monotonically assigned tablet log
  position;
- authenticated workspace, project, and source;
- stable batch ID;
- canonical batch payload or a content-addressed payload reference;
- event ID summary;
- receive time and ingest policy version.

The single-node path fsyncs the frame and its required metadata before it sends
an acknowledgement. A tablet acknowledges only after it satisfies the
configured receipt policy. The policy determines the failure-survival
guarantee.

Concurrent appends use bounded group commit: one fsync may make many complete
batch frames durable, while each caller receives only its own receipt. This
keeps the durability contract intact without forcing one physical sync per
event or collector.

Group commit is not an optimization. A measurement put the device ceiling at
186 fsync operations each second. Group commit with a 2 millisecond linger then
reached 16,923 durable frames each second from 134 of them. The same path
without a linger reached 503. See [BENCHMARKS.md](BENCHMARKS.md) section 13.

The committer therefore obeys three rules:

- it releases the lock while it writes and calls fsync, so other writers
  accumulate into the next group;
- it waits a configurable linger before it seals a group, defaulting to
  2 milliseconds;
- it bounds a group by bytes and by count, so a large group cannot exhaust
  memory or stall a caller past its deadline.

A longer linger is worse, not better. At 10 milliseconds the same benchmark
lost half its throughput and doubled its latency.

TallyOwl does not retain the log forever. A segmenter seals committed log
ranges into segments. The manifest records those segments. Required replicas
confirm them. The segmenter then checkpoints and removes old log ranges.

### 3.2 Immutable segments

A segment contains a bounded and self-describing data unit. Its usual size is
tens or hundreds of megabytes, not a full table. Configure the target size so a
small installation does not wait for large batches.

A segment object contains:

- segment manifest in canonical CBOR;
- format and schema version;
- tablet, virtual shard, workspace and project, telemetry kind, and time range;
- first and last source log position;
- row and event count and uncompressed and compressed sizes;
- column statistics and optional Bloom filters;
- payload checksum and manifest checksum;
- projector and rollup version and source lineage when derived;
- one or more native column pages;
- segment-local exact-value indexes and optional sidecar indexes.

TallyOwl Segment is the native physical representation. Arrow and Parquet are
not dependencies of the always-on storage or query path. A segment contains:

```text
fixed prologue
  magic, format version, segment ID, header/footer offsets
canonical CBOR header
  schema, tablet/virtual-shard, project, kind, time/log ranges
row groups
  row count, per-column page directory
  null bitmap + encoded/compressed pages + page checksums
footer
  column statistics, dictionaries, optional Bloom filters
  row-group offsets, payload hash, footer checksum
```

Column IDs are stable numeric schema IDs. Dynamic properties use typed, sparse
native columns with a schema-registry field ID. The projector validates unknown
fields and assigns a type and field ID. TallyOwl does not retain unknown fields
indefinitely in a general payload blob. Common envelope fields, correlation IDs,
and known payload fields use dedicated native columns.

Version 1 starts with simple encodings:

- fixed-width little-endian values for booleans, floats, and IDs;
- unsigned varints plus optional delta encoding for integers and timestamps;
- offsets plus byte regions for text and bytes;
- dictionary encoding only when it measurably reduces a page;
- one independently checksummed compression block per page.

The first writer defaults are:

- Seal a microsegment at 1 second or 8 MiB.
- Use a 32 MiB to 64 MiB segment target in the home profile.
- Use a 256 MiB segment target in a cluster.
- Use a 64 KiB compressed page target.
- Use Zstandard level 1 for hot and warm data.
- Use Zstandard level 3 only for a float column. A measurement showed that
  level 3 is larger than level 1 on every other encoded column. See D17.
- Use a 2 millisecond group-commit linger on the append log. See D47.

These values are configurable. A reader accepts all permitted sizes and codecs.

Benchmarks must justify more elaborate bit packing and XOR float encoding.
Exact-value indexing is a first-release requirement for correlation fields.
Query code decodes only selected pages into reusable typed buffers. It evaluates
filters and aggregates directly. It does not materialize an Arrow table.

Transport and committed WAL frames use CSIL CBOR. Thus, replay preserves the
accepted value. The writer then writes and checks the segment for that log
range. The catalog publishes the segment. The required replica and tier policy
must also cover it. The WAL checkpoint can then advance and remove the source
CBOR with the old log range.

An additional raw-CBOR tier is an explicit, time-bounded operator option. It is
not the default database representation.

Parquet lives in an optional exporter crate or process. Small installations do not
load or compile that dependency unless they enable exports. An export replica
may enable it without adding the same memory and dependency footprint to ingest
or normal dashboard queries.

Each segment has a content address or an immutable unique ID. The writer uses a
temporary name, fsyncs the segment, and renames it atomically. The writer then
publishes the segment in a catalog transaction. A crash can leave a complete
segment without a reference. Recovery verifies the segment and adopts or
removes it. The catalog must never refer to a partial file.

### 3.3 Transactional catalog

The catalog uses a small ordered key-value database. It stores:

- active segment manifests and generations;
- durable batch receipts and deduplication windows;
- log checkpoints and projector watermarks;
- tombstones and compaction state;
- workspace, project, environment, and source configuration;
- API key and role-token hashes, scopes, limits, and revocation;
- node certificate identity, role, serial, expiration, and fencing state;
- LinkKeys identity mappings, sessions, and memberships;
- saved dashboards, queries, cohorts, funnels, and alerts;
- node, shard, replica, and lease metadata;
- backup and export snapshots and audit records.

The initial spike must test a copy-on-write B+tree engine such as `redb`. This
engine provides a Bolt-like operational model. Crash, concurrency, backup, and
workload benchmarks determine the decision. The spike can also measure
write-heavy LSM alternatives.

The catalog's byte format is not TallyOwl's recovery contract. Every retained
segment has a self-contained manifest, and control and catalog snapshots use a
versioned canonical CSIL encoding. A repair command can rebuild the segment
catalog by scanning manifests.

Illustrative ordered key prefixes:

```text
format/<component>
control/workspace/<workspace-id>
control/project/<project-id>
control/source/<source-id>
auth/api-key/<key-id>
receipt/<source-id>/<batch-id>
tablet/<tablet-id>/log-checkpoint
segment/<tablet-id>/<generation>/<segment-id>
tombstone/<tablet-id>/<generation>/<tombstone-id>
projector/<projector>/<tablet-id>/watermark
cluster/node/<node-id>
cluster/placement/<virtual-shard>
```

Values use versioned canonical CSIL CBOR. They do not use
implementation-language structure dumps. Prefix scans and transactions are
sufficient. The design does not require SQL semantics from the catalog.

## 4. Data directory

Illustrative single-node layout:

```text
data/
  FORMAT
  node.cbor
  catalog/
    catalog.redb
  wal/
    tablet-0000/
      0000000000000001.wal
  segments/
    tablet-0000/
      project-id/
        event/
          2026/07/26/
            segment-id/
              manifest.cbor
              data.tbs
  tombstones/
    tablet-0000/
      tombstone-id.cbor
  snapshots/
  cache/
  exports/
  quarantine/
  tmp/
```

Paths are not query semantics. Manifest contents are authoritative. All files
carry a format version, and startup refuses unknown incompatible versions rather
than guessing.

## 5. Write and recovery sequence

### Single node

1. Validate collector identity, scope, schema, quotas, and batch checksum.
2. Look up the stable batch ID in the receipt index.
3. Append a checksummed record and fsync according to durability policy.
4. Atomically record the receipt and log position in the catalog.
5. Return a committed receipt.
6. Segment committed ranges asynchronously.
7. Publish complete segments in one catalog generation.
8. Advance the segment checkpoint and eventually remove covered WAL files.

The recovery scanner truncates only a torn final frame, replays complete frames
after the catalog checkpoint, reconstructs missing receipt entries, and resumes
segment work. A batch committed before a lost response returns the same receipt
when retried.

### Multi-voter tablet

1. The tablet leader validates and assigns the next log position.
2. Voting replicas append the frame.
3. Consensus commits after the configured quorum persists it.
4. The leader gets an additional remote copy if the policy requires it.
5. The leader returns the durable receipt.
6. Segment construction and immutable segment replication proceed
   asynchronously from the committed log.

The prototype must select the consensus implementation. An existing Rust Raft
implementation must provide election, membership, log replication, and snapshot
mechanics. TallyOwl must not invent a consensus algorithm. Before selection,
evaluate its API stability and multi-group behavior.

## 6. Virtual shards, tablets, and scale

The design separates three terms:

- **virtual shard:** stable routing bucket derived from trusted
  workspace and project and a telemetry-type affinity key;
- **tablet:** placement, leadership, WAL, replication, and compaction unit
  containing many adjacent virtual shards;
- **replica set:** the small set of storage nodes holding one tablet.

A home installation maps every virtual shard into one local tablet. A large
installation has many tablets on many storage nodes. One tablet holds many
virtual shards. Therefore TallyOwl does not run a consensus group for each
project or for each small routing bucket.

Routing properties:

- a span uses its trace ID;
- a behavior event uses its session ID or end-user ID;
- a metric point uses its series ID;
- other data uses a stable event key;
- all other correlation fields use exact indexes;
- new capacity moves or splits a tablet; it does not rewrite unrelated data;
- a move copies sealed segments, catches up committed WAL positions, verifies
  checksums, then changes placement generation;
- ingest gateways route by a cached virtual-shard → tablet → leader map and
  retry fenced leader redirects;
- shard and tablet identity and placement are controller metadata, never accepted
  from clients.

Millions of events per minute scale through more independent tablet leaders,
larger collector batches, and distributed partial aggregation. Each tablet write
path remains single-leader and ordered.

Tablet split:

1. Let the controller select a virtual-shard boundary and record split intent.
2. Make the tablet group checkpoint at a committed log position.
3. Assign files that do not cross the split.
4. Compact bounded files that cross the split.
5. Let two child tablet groups catch up from that checkpoint.
6. Let the controller publish the new routing generation atomically.
7. Keep old ownership readable until active requests drain.

The merge operation reverses this sequence. It prevents many small tablet groups
in small installations.

## 7. Controller and tablet consensus

There is no cluster-wide Raft group containing every storage node.

### Global directory

A large installation has one small global directory quorum. The directory maps
projects to regional cells. It also stores cell identity and policy.

The global directory does not store telemetry. It does not manage tablets. A
home installation puts this role in the local process.

### Cell controller quorum

Each regional cell has three controllers by default. An operator can select
five controllers. The cell controllers store:

- node membership and health leases;
- virtual-shard and tablet definitions and placement generations;
- tablet split, move, and merge intents;
- schema, policy, and cluster configuration versions;
- metadata-group membership and fencing epochs.

Controllers do not carry telemetry batches or participate in every tablet write.
A 400-node cell still has only three or five controller voters.

Loss of a cell controller quorum prevents topology changes in that cell.
Existing tablet groups continue data operations with their current membership.
Loss of the global directory does not stop existing cell data operations.

### Tablet groups

Each write-replicated tablet normally has three voting storage replicas selected
across failure domains. Only those three nodes participate in that tablet's
consensus. A storage node may host many independent tablet groups, multiplexed
over shared CSIL connections and runtime threads.

Tablet consensus owns:

- leader election and fencing;
- ordered WAL replication;
- commit index and durable batch receipts;
- voting membership changes authorized by a controller placement generation;
- snapshots and checkpoints used to add or recover a replica.

This is a multi-consensus design: hundreds of storage nodes collectively host
many small three-member groups. Read and export replicas are non-voting learners and
do not enlarge write quorums.

```text
Global directory: [g1, g2, g3]                 project-to-cell map

Cell controller group: [c1, c2, c3]            cell metadata only

Tablet A voters:  [storage-007, storage-041, storage-233]
Tablet B voters:  [storage-019, storage-041, storage-388]
Tablet C voters:  [storage-002, storage-174, storage-301]
                       │
                       └─ optional read/export learners

The other storage nodes do not vote on A, B, or C unless they hold that tablet.
```

An existing consensus implementation supplies the algorithm. TallyOwl supplies
the tablet state machine, multiplexed transport, storage, placement integration,
and operational limits. Do not use a custom controller lease with an ad hoc
replication protocol. An alternative must prove equal quorum, fencing, and
recovery properties with less complexity.

## 8. Replication modes

### Embedded

- one storage node;
- replication factor one;
- acknowledgement after local fsync;
- in-process reads;
- simplest home-lab default.

### Write-replicated

- normally three voting replicas per tablet;
- acknowledgement after quorum commit;
- survives a configured number of node and failure-domain losses;
- leader and follower membership changes are explicit and auditable.

Each tablet has one write region at one time. Other regions can have non-voting
read, export, or recovery replicas. A fenced operation changes the write region.

The receipt policy is configurable:

- `local-one` requires one local voter and one fsynced copy;
- `local-quorum` requires the local tablet quorum;
- `remote-one` requires a local quorum and one remote durable copy;
- a custom policy can name failure-domain requirements.

TallyOwl does not acknowledge an uncommitted entry in a multi-voter group.
This rule has no exception.

Therefore `local-one` applies only to a tablet with one voter. It is the
default for the embedded and home profiles. A write-replicated tablet uses
`local-quorum` by default. The controller refuses `local-one` for a multi-voter
tablet.

A policy change first builds the necessary replica set. The controller then
commits the new group configuration and activates the new receipt policy.

### Read replica

- non-voting replica follows sealed segments and the required tombstone and catalog
  generation;
- can serve bounded-staleness queries and exports;
- does not slow the write quorum;
- reports its exact high-water marks in query results and health metrics.

### Export replica

- read replica optimized for scans, snapshots, and Parquet export;
- isolates large exports from dashboard latency;
- can retain a longer segment window if policy permits.

Read consistency is explicit. A `committed` read routes to a sufficiently
current voting replica or leader. A `bounded-stale` read uses a read replica
whose watermark satisfies the request.

Global mutable control state uses the global directory quorum. Cell and tablet
state uses the applicable cell controller quorum. Tablet groups refer to a cell
metadata generation.

Security changes do not depend on segment transfer. Embedded mode uses the same
local catalog without a network consensus layer.

## 9. Query execution

The query planner:

1. authorizes workspace and project scope;
2. resolves the requested logical dataset and schema and projector version;
3. snapshots a catalog generation and tombstone high-water mark;
4. prunes tablets and segments by project, kind, time, and statistics;
5. selects only required columns;
6. pushes filters and partial aggregation to storage and read nodes;
7. merges partial results with deterministic limits;
8. returns freshness, scanned bytes and segments, approximation, and warnings.

Initial execution uses TallyOwl's own small typed page and batch interfaces:
selection bitmaps, typed slices, dictionary values, and aggregate states. The
query layer reads only needed native pages and reuses bounded decode buffers. A
general SQL or Arrow engine is not a storage dependency or public contract.

The logical plan uses these conventional relational operators: typed scan,
filter, project, aggregate, sort, bounded join, and limit. Domain
operators support functions such as ordered funnels and trace assembly. A
simple aggregate cannot correctly represent these functions. A bounded SQL or
PromQL compatibility parser can translate input into this plan. The versioned
typed CSIL query algebra remains the public and native contract.
[QUERY.md](QUERY.md) defines that algebra and the pushdown contract.

For a correctness-first distributed query, the coordinator captures control and
tombstone generations. It also captures the required commit watermark for each
tablet in the plan. Each tablet uses a snapshot that satisfies those values. By
default, a missing required tablet causes a typed incomplete-result error. A
caller must explicitly request partial mode.

The response identifies missing
tablets and time ranges. It cannot identify itself as a complete result. Storage
nodes return exact rows or mergeable partial states. Deterministic rules control
types and overflow.

An operation uses approximate states only when it permits a named algorithm and
error bound.

Detail queries use the current committed watermark. ID lookup, trace assembly,
and erasure also use this watermark.

A dashboard can use an exact watermark that is not more than five seconds old.
The response gives the watermark. All freshness limits are configurable.

The first release does not include a SQL interface. A PromQL adapter can
translate metric queries to the typed query plan.

Common queries use replayable rollup segments. Ad hoc queries scan source
segments with strict byte, memory, time, and concurrency budgets. A query can
read recent data from a sealed microsegment or committed-log read view. Thus,
dashboard freshness does not depend on the normal target segment size.

## 10. Index strategy

TallyOwl must support two complementary access patterns:

1. scans and aggregations over time ranges and dimensions;
2. exact lookup and correlation on very high-cardinality values. These values
   include event, request, trace, span, session, end user, order, and custom
   correlation IDs.

Every segment starts with indexes inherent in its organization:

- shard, project, kind, and time partitioning;
- minimum and maximum statistics;
- dictionaries where they save space;
- Bloom filters for segment rejection;
- sorted columns where a dominant query requires them.

Exact lookup uses an inverted index per tablet and segment generation:

```text
(field_id, typed value) -> compressed postings of segment_id + row_id
```

Term dictionaries use prefix-compressed or finite-state representations;
posting blocks use delta and bit-packed row IDs and skip data. Unique values are
valid: a request ID with a one-row posting must remain quickly retrievable.
Built-in correlation fields are always exact-indexed. Dynamic fields declare
one of three physical access classes:

- `lookup`: exact term-to-posting index;
- `facet`: exact postings plus an aggregable typed column;
- `stored`: typed column scan only.

String, bytes, integer, UUID, IP, Boolean, and timestamp values keep their types
in columns and index terms. Dynamic scalar fields default to `lookup`. Thus, an
unexpected correlation property remains useful. Operators or schema policy can
promote it to `facet` or demote it to `stored`. Resource quotas limit bytes,
fields for each row, value size, and indexing work. Cardinality alone is not a
rejection reason.

The tablet keeps a bounded locator layer over immutable segment indexes. Thus,
an exact term does not require a read of each segment. Checkpoint and compact
locator runs like other immutable metadata. Time constraints, value hashes,
Bloom filters, and segment generations narrow candidates. Before it returns
rows, the index verifies the full typed value. Hashes never decide correctness.

The stable event ID receipt and deduplication index can use a time window.
Long-term point lookup uses the same exact locator and index path.

Implement the storage engine as a telemetry-independent `tallyowl-store`
crate. Give it a versioned schema, segment reader and writer, exact index,
tombstones, snapshots, and query primitives. The TallyOwl telemetry model is
one consumer. A prototype compares its exact-index components with Tantivy
segment and index components. The comparison includes term dictionaries,
postings, fast fields, merges, and deletes. Reuse a component only when it obeys
the TallyOwl WAL, tiering, snapshot, and deletion contracts.

Do not assume that the component is the source database. See
[HIGH_CARDINALITY.md](HIGH_CARDINALITY.md) for the full design.

## 11. Deletion and mutation

Telemetry correction is append plus supersession; deletion is tombstone plus
compaction.

1. Commit a tombstone describing event IDs, end user, project, or bounded predicate.
2. Advance the visible tombstone generation.
3. Queries at newer generations filter those rows immediately.
4. A worker identifies intersecting segments from manifests, statistics, and indexes.
5. It writes replacement segments without deleted rows.
6. One atomic manifest generation swaps old segments for replacements.
7. Remove old segment files after all readers and replicas release them.
8. Keep old files while the backup policy refers to them.

Compaction rewrites only affected bounded segments. A project and time-range
deletion can remove fully covered segments without reading them. Compaction can
combine small tombstones to keep query filtering bounded.

### A tombstone is a standing predicate

A tombstone is not only a filter over data that already exists. Telemetry for
an erased end user can still be in a collector queue when the erasure lands. That
telemetry arrives later.

Therefore a tombstone stays active until its horizon ends. The ingest path
applies active erasure predicates to newly accepted data. A late arrival that
matches an active predicate never becomes visible.

The same rule applies to a restore and to a replay. A tombstone generation
travels with the data.

### Erasure by tier

An exact index contains opaque end-user IDs. Thus, an erasure request finds
matching local and cold segment generations without a full scan. Tombstones
hide the end user immediately in every tier.

**Hot and warm tiers.** Compaction rewrites partially affected local segments
and deletes fully covered ones when snapshot and backup retention permit. This
work stays local and bounded. The first physical target is 24 hours.

**Cold tier.** Do not rewrite every intersecting cold object. One end user can
touch thousands of cold objects across a retention period. Download, rewrite,
and upload of that set cannot meet a 24-hour target, and the cost grows with
retention.

The cold tier encrypts under one project key:

- destroying the project key erases the whole project and cannot be undone;
- an object-store reader without the key reads nothing useful;
- there are no per-end-user keys, so a single end-user erasure does not destroy
  a key.

An end-user erasure is immediate and logical in every tier. The tombstone hides
the rows at once and stays active for late arrivals. Hot and warm segments
rewrite within the 24-hour target. Cold bytes physically disappear when
retention expires them.

TallyOwl does not promise immediate physical destruction of one end user's cold
data. An operator who needs that sets a shorter cold retention, or does not
enable the cold tier. See D28.

The same deletion generation invalidates or rebuilds derived profiles, rollups,
cohorts, caches, and export manifests.

The default immutable-backup horizon is 30 days. It is configurable.

An erasure ledger is part of each restore. The ledger prevents restored data
from making erased end user data visible again.

Mutable control records live directly in the transactional catalog or replicated
metadata log and do not use telemetry tombstones.

## 12. Rollups and projection

The segmenter can directly write typed canonical segments after validation
establishes their shape. More expensive derived datasets record:

- projector name and version;
- input segment IDs and log range;
- output schema and version;
- completion checksum and watermark.

Rollups use replaceable segment generations. Late data marks affected buckets
as dirty. Workers recompute only those bounded buckets. Deduplication resolves
duplicate event IDs before they contribute to a rollup.

## 13. Storage tiers, backup, restore, and export

### Hot, warm, and cold

All tiers use the same immutable TallyOwl Segment format:

- **hot:** WAL, open builders, microsegments, exact locator heads, and the
  configured most-recent window on local disk;
- **warm:** sealed local segments optimized for normal dashboard scans;
- **cold:** older sealed segment and index bundles in configured object storage,
  with local manifest and routing metadata and a bounded page and index cache.

When an operator enables object storage, the first policy uses these values:

- Keep 24 hours of hot data on local storage.
- Keep the next 7 days of warm data on local storage.
- Put older retained data in the cold tier.
- Start normal tier movement at 70 percent disk use.
- Increase tier movement at 85 percent disk use.
- Stop ingest before unsafe exhaustion at approximately 95 percent disk use.

All values are configurable for each data class.

The tier policy combines age, retained bytes, free-disk watermarks, data class,
and minimum local replicas. TallyOwl evicts a local segment only after durable
cold storage contains its object, indexes, manifest, and checksums. Verification
must satisfy the configured cold-store copy policy. Each catalog location
change creates a generation. Thus, readers see the old or new valid location.

Cold queries read required index and column page ranges. They verify checksums
and fill a bounded cache. Query results report the cold byte count and can have
a separate deadline. Cold data remains part of the logical database. It does
not become an export. Read and export replicas can prefetch cold indexes or
segments without becoming voting replicas.

Object storage is optional. Apache OpenDAL is the initial Apache-2.0 candidate
for local files and common bucket APIs, with only selected backends compiled
into a deployment. TallyOwl does not require an operator to run a particular
bucket server, and the abstraction remains replaceable.

### Snapshot

A snapshot pins:

- catalog and control snapshot generation;
- shard log checkpoints;
- required segment IDs and checksums;
- tombstone generation;
- format and software compatibility metadata.

A worker hard-links, copies, or incrementally transfers an immutable segment.
Only new segments and catalog snapshots need copying after the first backup.
The snapshot can refer to cold objects that satisfy placement and checksum
rules. It does not have to copy them again.

### Restore

Restore verifies each manifest and payload checksum before it publishes the
snapshot. It can restore into a standalone node or seed replicas. A missing or
corrupt file causes a visible failure. Restore never silently skips a file.

### Export

Parquet export is a stable optional product feature:

- raw accepted envelopes;
- typed canonical datasets;
- selected projections and rollups;
- project, time, and predicate filters;
- manifest with schemas, policy, checksums, and deletion high-water mark.

The exporter reads native TallyOwl pages and performs a bounded conversion.
Exports apply visible tombstones before physical compaction reclaims the source
segments. The exporter can use Parquet or Arrow libraries. These libraries stay
in its feature set or a separate process. Minimal collector, storage, query, and
dashboard builds do not contain them.

DuckDB can query the resulting files directly. CSV and JSON are convenience exports
for smaller datasets, not canonical precision-preserving formats.

## 14. Capacity and self-observability

Storage metrics include:

- accepted and rejected events and bytes;
- fsync and quorum commit latency;
- WAL bytes, age, and checkpoint lag;
- open and sealed segment counts and size distribution;
- segment creation, compaction, and write amplification;
- tombstone count, rows hidden, and reclamation lag;
- per-tablet leader, term, commit and applied position, and replica lag;
- read replica freshness;
- query segments and bytes scanned, cache hits, duration, and rejection;
- export and backup bytes, lag, and failures;
- disk used and free and projected exhaustion time;
- per-project ingest and retained-byte growth.

These are available both through TallyOwl's native metric path and a
Prometheus and OpenMetrics endpoint. Capacity dashboards compare current growth and
latency with historical baselines and produce time-to-capacity alerts.

## 15. Required prototypes

Before declaring the format stable:

1. embedded KV crash, backup, and concurrency comparison;
2. WAL framing, group commit, torn-write recovery, and receipt-loss tests;
3. native page encodings, compression, segment-size, and row-group benchmarks
   across events, spans, and metrics;
4. exact high-cardinality lookup and facet benchmarks, including mostly-unique
   request IDs and end user, session, and trace timelines;
5. local-to-cold tiering, range-read queries, cache eviction, and interrupted
   upload and eviction recovery;
6. direct DuckDB query of exported Parquet;
7. point, end user, and time-range deletion across local and cold segments;
8. query pruning and rollup benchmarks at home-lab and large synthetic scale;
9. three-node quorum kill, restart, and partition tests;
10. read replica and online tablet movement;
11. snapshot and restore with corruption injection;
12. format upgrade and catalog rebuild from segment manifests;
13. cryptographic erasure of cold data, including key destruction, the erasure
    ledger, and a read attempt after destruction;
14. Corndogs write and timeout-sweep cost at the outage-buffer depth that the
    capacity envelope selects, at realistic batch payload sizes.
