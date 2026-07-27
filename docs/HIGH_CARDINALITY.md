# High-cardinality storage and correlation

## 1. Contract

TallyOwl's native store must handle both:

- aggregate scans over common dimensions and time buckets;
- exact retrieval by values that are unique on nearly every row.

Examples include request, event, trace, span, session, actor, order, deployment,
and arbitrary application correlation IDs. High cardinality is not an ingest
error and does not justify silently dropping, hashing away, or coalescing data.
Correctness and resource use remain explicit.

The storage engine is a telemetry-independent Rust module. Telemetry schemas,
dashboards, and collectors sit above its typed storage and query API.

## 2. Typed field registry

Each workspace and project has a schema registry. The registry maps a
normalized field name and namespace to these values:

- a stable numeric `field_id`;
- a value type;
- a sensitivity policy;
- a physical access class.

The physical access classes are:

- `lookup`: exact equality lookup;
- `facet`: equality lookup plus efficient grouping and aggregation;
- `stored`: retained typed value with scan access.

Built-in correlation fields are always `lookup`. Dynamic scalar fields default
to `lookup`, so an unexpected application-specific request ID remains useful
without advance administration. An operator can promote a field to `facet` or
demote it to `stored`. The change creates a new schema generation and can start
a background index build.

The initial type set contains:

- null and Boolean values;
- signed and unsigned integers;
- floats and decimals;
- timestamps and durations;
- UTF-8 text and bytes;
- UUIDs and fixed identifiers;
- IP addresses.

Arrays and objects require explicit bounded semantics. TallyOwl does not keep
them as opaque query data. Project policy rejects type conflicts or creates
visible schema variants. TallyOwl never silently converts them to text.

The default type-conflict policy keeps typed variants. A query selects one type
or requests an explicit conversion. The dashboard shows the conflict.

The first limits are:

- 128 properties for one item;
- 64 bytes for a property name;
- 4 KiB for one indexed string or byte value;
- 64 KiB for all dynamic properties in one item.

All limits are configurable. A project can store a larger permitted value
without an exact index.

## 3. Segment representation

A segment contains native envelope and typed columns and only the sparse dynamic
columns present in that segment. It does not reserve a physical column for every
field ever seen.

Each exact-indexed field chooses a segment-local layout from measured
statistics:

- **term postings:** sorted and prefix-compressed term dictionary plus compressed
  row-ID postings for repeated values;
- **unique lookup:** sorted fixed-width fingerprints and row IDs for
  mostly-unique UUIDs and random identifiers, with full-value verification;
- **ordered values:** sorted value and row blocks and skip data for range queries;
- **facet column:** typed values or ordinals suitable for grouping and local
  partial aggregation.

The writer selects a layout for each distribution. A random UUID must not create
a large posting object for each row. Repeated service names use compressed
postings.

Every format includes checksums, versioning, bounded decode allocations, and
enough statistics to select a query path without opening all data pages.

## 4. Tablet locator

Segment-local indexes alone would still require checking every retained
segment. Each tablet therefore maintains immutable, time-partitioned locator
runs:

```text
(field_id, typed-value fingerprint, time bucket)
    -> candidate segment generation(s)
```

Locator runs cover both local and cold segments. Compaction combines locator
runs incrementally. Recent heads and high-level cold routing data stay local.
Fingerprints only prune and route. The segment index verifies the full typed
value, so collisions cannot create incorrect results.

An exact query:

1. resolves project, field ID and type, and optional time range;
2. routes to the project's virtual shard and tablet set;
3. probes tablet locator runs;
4. opens only candidate local, cached, and cold segment indexes;
5. verifies the full value and obtains matching row IDs;
6. fetches selected columns and applies visible tombstones;
7. merges exact rows in deterministic event-time and commit-time order.

Cross-system request or actor timelines issue the same lookup across relevant
telemetry kinds and merge their common envelopes. Trace parent and child assembly
uses trace and span indexes rather than a scan.

## 5. Aggregation and materialized views

High-cardinality detailed data remains the retained source for its configured
period. Lower-cardinality projections accelerate predictable dashboards:

- per-minute event, error, and service counts;
- service-operation duration and error histograms;
- active actor and session buckets;
- campaign and conversion summaries;
- metric downsampling tiers.

Each materialized segment generation has a version and a source watermark.
They never replace exact ID lookup while detailed retention remains active.
Queries select a rollup only when its dimensions, filters, exactness, deletion
generation, and freshness satisfy the request.

Distinct counts and other expensive long-window aggregates may use an explicit
mergeable sketch. The result declares the algorithm and error bound. Exact
lookup, event detail, trace assembly, and user erasure never use approximate
membership.

## 6. High-cardinality metrics

A stable hash of the metric identity and canonical typed labels identifies each
metric series. TallyOwl stores and verifies the complete label set. An exact
index stores series metadata. Metric samples use time and value pages that use the
series ID.

Request, session, and actor IDs are usually cheaper as event or span attributes
or metric exemplars. However, they remain valid labels. Configure limits for
bytes, active-series memory, index work, write work, and retained storage. A
limit causes visible backpressure or rejection according to project policy.
TallyOwl never silently merges unrelated series.

## 7. Tiering and deletion

Hot locator heads, WAL, and recent segments stay local. Warm local segments and
cold object-storage bundles retain the same field IDs, indexes, and checksums.
Cold movement includes the data pages and exact indexes; local locator metadata
continues to route queries without listing a bucket.

An actor-erasure request resolves the actor ID through the exact locator,
publishes a tablet-wide tombstone generation, and immediately hides matching
rows in every tier.

Compaction rewrites affected local segments and rebuilds their indexes and
rollups. The cold tier uses cryptographic erasure instead of an object rewrite,
because one actor can touch thousands of cold objects. See
[STORAGE.md](STORAGE.md) section 11 and D28.

The tombstone is a standing predicate. Late telemetry for an erased actor can
still be in a collector queue when the erasure lands. That data must not become
visible when it arrives. A replay carrying an older deletion generation cannot
resurrect rows.

## 8. Reuse versus invention

The prototype must compare:

- Tantivy's MIT-licensed immutable segments, term dictionaries, postings, fast
  fields, merge policy, and delete machinery;
- a small TallyOwl-specific adaptive index for mostly-unique fixed-width IDs;
- the selected embedded transactional KV engine for catalog and locator heads;
- Apache OpenDAL for optional local and bucket object access.

Tantivy is an index library. It is not the TallyOwl durability and query
database. Reuse requires:

- deterministic snapshots;
- cold range reads;
- bounded memory;
- deletion generations;
- compatibility with the native WAL and segment contract.

Adopt a component only when it fits. Do not let an unsuitable abstraction
control the data model.

## 9. Required benchmark matrix

Benchmarks must publish ingest bytes and CPU, index amplification, merge cost,
lookup latency, aggregation latency, and cold bytes fetched for:

- one repeated value across millions of rows;
- one random request ID per row;
- Zipf-distributed actor and session IDs;
- trace IDs shared across multi-service spans and events;
- dynamic field counts and sparse field presence;
- high-cardinality metric series;
- exact lookup with no time bound across local and cold retention;
- actor deletion touching few rows across many segments;
- rollup queries with and without a matching materialized view;
- cache-cold and cache-warm object-store queries.

Claims such as “fast at high cardinality” require measured p50/p95/p99 results
at home, single-large-node, and distributed tablet scales.
