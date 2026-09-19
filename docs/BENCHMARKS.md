# Benchmark results

This document holds measured results. Every number here came from a run on
known hardware with a recorded seed. A claim without a measurement does not
belong in this document.

The prototypes live in `prototypes/`. See
[../prototypes/README.md](../prototypes/README.md).

## 1. Hardware

| Item | Value |
| --- | --- |
| CPU cores | 16 |
| Memory | 125 GiB |
| Storage under test | Samsung SSD 990 PRO 4TB, ext4, `noatime` |
| Device write cache | write back, no power-loss protection |
| Rust | 1.97.0, release profile, thin LTO |

## 2. Method rule: never measure storage on tmpfs

The first run of the catalog benchmark used the system temporary directory. On
this machine `/tmp` is tmpfs, which is memory.

| Measurement | On tmpfs | On ext4 and NVMe | Error |
| --- | --- | --- | --- |
| `fsync` of 4 KiB | 410,612 each second | 186 each second | 2,208x |
| redb durable commit | 48,830 each second | 621 each second | 79x |

The tmpfs run reported a durable commit rate that the hardware cannot reach. It
would have produced a wrong D3 conclusion and a wrong capacity envelope.

Every storage benchmark must state its filesystem and must run on real storage.
The catalog benchmark now refuses to default to a temporary directory.

## 3. The fsync ceiling

One `fsync` costs 5.4 milliseconds on this device. That is 186 each second.

This is the single most important number measured so far. It bounds every
durability boundary in the design.

Consequences:

- group commit is mandatory, not an optimization. STORAGE.md section 3.1
  already requires it. This measurement shows that the requirement is load
  bearing.
- a design that calls `fsync` once for each batch reaches about 200 batches
  each second, whatever the core count.
- Corndogs `group` mode is the only sensible file-backend setting. Its `always`
  mode pays this cost for every task.

## 4. D3: embedded catalog, redb 3.1.3

Workload from STORAGE.md section 3.3: receipt writes, deduplication lookups,
and manifest prefix scans, using the documented ordered key prefixes.

### Commit path

| Durability | Receipts per commit | Commits each second | Receipts each second |
| --- | --- | --- | --- |
| Immediate | 1 | 621 | 621 |
| Immediate | 16 | 214 | 3,422 |
| Immediate | 256 | 182 | 46,656 |
| None (unsafe) | 1 | 48,971 | 48,971 |
| None (unsafe) | 256 | 2,758 | 706,055 |

Commits each second converge on the `fsync` ceiling. The catalog is not the
limit. The device is.

`None` is 79 times faster at one receipt for each commit. That gap is the cost
of durability, and it is not negotiable.

### Read path

| Measurement | Result |
| --- | --- |
| Deduplication point lookup | 1,098,930 each second |
| Mean lookup latency | 0.91 microseconds |
| Manifest prefix scan | 2,913,447 rows each second |
| Reopen after close | 1 millisecond |

The deduplication lookup on the ingest hot path is free. It costs about one
microsecond against a device commit that costs 5,400 microseconds.

### Size

337 bytes on disk for each receipt, against roughly 104 bytes of key and value.
A copy-on-write B+tree leaves free pages behind, so this amplification follows
from the engine design. It needs watching as retention grows.

### Reading

redb meets the catalog requirement. Nothing in this workload argues for a
write-oriented alternative, because the write path is device bound rather than
engine bound.

## 5. D17: page encodings, compression, and sizing

One million rows for each column, seed 42. Column shapes model real telemetry.

### Encoding results

| Column | Winning encoding | Ratio after zstd level 1 |
| --- | --- | --- |
| `occurred_at`, near-sorted | `varint-delta` | 11.85x |
| `duration_ms`, skewed | `varint` | 8.17x |
| `service_name`, 20 distinct | `dictionary` | 18.16x |
| `route`, 500 distinct | `dictionary` | 22.99x |
| `trace_id`, 10 spans each | `dictionary` | 19.65x |
| `end_user_id`, Zipf | `dictionary` | 7.35x |
| `event_id`, unique | `offset-bytes` | 2.36x |
| `measure`, float | `plain-fixed` | 1.51x |

The encoding list in SEGMENT_FORMAT.md section 6 holds. Every encoding won a
column, and the dictionary rule correctly refused the unique column.

### Zstandard level 3 is not worth it

| Level | Total | Ratio |
| --- | --- | --- |
| 1 | 29,369 KiB | 5.28x |
| 3 | 29,753 KiB | 5.21x |

Level 3 is 1.3 percent **larger** than level 1 across the winning encodings. It
loses on six of eight columns, because an encoded column has already removed
the redundancy that a higher level would find.

The float column is the one exception. Level 3 gains 22.4 percent there.

D17 permits level 3 during cold compaction. The measurement does not support
that as a general rule. See the D17 update.

### Page size

| Page target | Ratio cost against whole-column compression |
| --- | --- |
| 16 KiB | Between +2.5 and -13.4 percent |
| 64 KiB | Between +2.0 and -4.8 percent |
| 256 KiB | Between +2.1 and -1.2 percent |

The 64 KiB target in D17 holds. The penalty is under 2.5 percent for every
column except the float column. That column loses 4.8 percent at 64 KiB and
13.4 percent at 16 KiB.

Smaller pages help a query read less. The ratio cost is small enough that
64 KiB remains the right default.

## 6. New finding: a UUID should travel as bytes

`event_id` is unique on every row, so it compresses worst and dominates the
segment. It is 16,660 KiB of the 29,369 KiB total, which is 57 percent.

`csil/types/common.csil` stores a UUID as 36 characters of text.

| Representation | Raw | After zstd level 1 | Saving |
| --- | --- | --- | --- |
| 36-character text | 39,062 KiB | 16,660 KiB | — |
| 16 raw bytes | 15,625 KiB | 9,795 KiB | 41.2 percent |
| 16 bytes, prefix and tail split | 15,625 KiB | 7,812 KiB | 53.1 percent |

The split stores the UUIDv7 time prefix and the random tail as two columns. The
prefix delta-encodes to almost nothing. The tail is random and does not
compress, which the measurement confirms: 7,812 KiB is exactly eight bytes for
each row.

A 53 percent saving on 57 percent of the segment is roughly a 30 percent
reduction in stored bytes.

**Applied.** `csil/types/common.csil` now stores every ID as
`bytes .size (16..16)`, and `SpanId` as eight bytes. A driver converts to and
from the text form at a propagation boundary such as a W3C `traceparent`
header.

Measured again after the change, with the same seed and row count:

| Total for the segment | Before | After |
| --- | --- | --- |
| Raw | 155,058 KiB | 112,089 KiB |
| After zstd level 1 | 29,369 KiB | 20,498 KiB |

That is a 30.2 percent reduction in stored bytes, which matches the prediction.

The re-run also refined the encoding rule. `split-prefix` wins for a unique
time-ordered ID such as `event_id`, at 7,812 KiB against 9,795 KiB for plain.
It loses for a repeated ID such as `trace_id`, where plain fixed-width lets
Zstandard find the repetition across spans. The writer must choose by
distribution here too.

One consequence for D17: with binary IDs the total appears to favour Zstandard
level 3 by 5.6 percent. That gain is entirely the float column. No other column
improves, so the D17 rule stands.

## 7. D44: checksum cost

| Function | Throughput |
| --- | --- |
| xxHash3-64 over 64 KiB pages | 9,112 MB each second |
| BLAKE3-256 over one buffer | 3,857 MB each second |

xxHash3 is 2.4 times faster. Both are far above the device write rate.

D44 splits the two functions: xxHash3 for a page checksum and BLAKE3 for a
content address. The measurement supports the split but the margin is smaller
than the decision implies. A single-function design using BLAKE3 everywhere
would not be a throughput problem on this hardware.

The decision stands on its stated reason, which is that a content address needs
collision resistance and a page checksum does not.

## 8. D20 and D25: exact index on high-cardinality values

One million rows for each distribution, seed 42, 20,000 probes with half hits.

### Native layouts

| Dataset | Layout | Bytes for each row | Build M rows/s | p50 ns | p99 ns |
| --- | --- | --- | --- | --- | --- |
| Single repeated value | unique-lookup | 12.0 | 54.6 | 23,730 | 4,694,416 |
| Single repeated value | term-postings | 1.0 | 66.0 | 20 | 30 |
| Unique on every row | unique-lookup | 12.0 | 23.3 | 580 | 1,680 |
| Unique on every row | term-postings | 27.0 | 2.8 | 150 | 2,390 |
| Zipf end-user IDs | unique-lookup | 12.0 | 34.6 | 890 | 9,480 |
| Zipf end-user IDs | term-postings | 4.7 | 14.5 | 70 | 790 |
| Trace IDs, 10 spans each | unique-lookup | 12.0 | 34.6 | 550 | 1,390 |
| Trace IDs, 10 spans each | term-postings | 3.6 | 33.2 | 70 | 490 |

The design promise holds. A value that is unique on every row retrieves in 580
nanoseconds at the median from a 12-byte-for-each-row index.

**A wrong layout is catastrophic in both directions.**

The unique-lookup layout on a single repeated value reaches a p99 of 4.7
milliseconds. Every row shares one fingerprint, so the probe walks all of them.

The term-postings layout on a unique column costs 27 bytes for each row. That
is more than the 16-byte data it indexes, and it builds 8 times slower.

SEGMENT_FORMAT.md section 7 already requires the writer to select a layout from
measured statistics. This measurement shows that the requirement is load
bearing rather than an optimization.

### Fingerprint collisions

No collision occurred in any dataset. A 64-bit fingerprint over one million
values is far below the birthday bound, as expected.

Full-value verification still runs on every probe, because the design forbids a
hash from deciding correctness. It costs nothing measurable.

### D25: Tantivy comparison

| Dataset | Build M rows/s | Index KiB | p50 ns | p99 ns |
| --- | --- | --- | --- | --- |
| Single repeated value | 0.51 | 4,954 | 36,830 | 1,161,024 |
| Unique on every row | 0.35 | 31,050 | 4,050 | 60,570 |
| Zipf end-user IDs | 0.41 | 13,176 | 4,470 | 19,610 |
| Trace IDs, 10 spans each | 0.42 | 18,768 | 4,440 | 26,230 |

Against the native layouts on the same data, Tantivy is:

- 7 to 130 times slower to build;
- 2.6 times larger on the unique column;
- 7 times slower at the median lookup, and 36 times slower at p99.

The reason is structural rather than a defect. Tantivy is a full-text engine whose
API takes text, so a 16-byte ID becomes 32 characters of hexadecimal before it
reaches the index. TallyOwl needs fixed-width binary exact lookup, which is
a narrower problem with a much cheaper solution.

**Reading:** do not adopt Tantivy for the exact-ID index. D25 asked for the
comparison and the comparison answers it.

## 9. D24: cold tiering, range reads, and the page cache

Segment 256 MiB, page 64 KiB, injected object-store first-byte latency 15 ms.

Apache OpenDAL performs the ranged read that the design depends on. A 64 KiB
ranged read from a 256 MiB object returned the correct page.

### Round trips dominate, not bytes

| Pattern | Pages | MiB fetched | Requests | Latency cost |
| --- | --- | --- | --- | --- |
| Point lookup | 2 | 0.1 | 1 | 0.0 s |
| One column aggregate | 256 | 16.0 | 256 | 3.8 s |
| Four column scan | 1,024 | 64.0 | 256 | 3.8 s |
| Whole segment | 4,096 | 256.0 | 1 | 0.0 s |

A one-column aggregate fetches only 6.2 percent of the segment and still costs
3.8 seconds. The pages sit at a stride and do not coalesce, so each page needs
its own request.

**This is a layout finding.** SEGMENT_FORMAT.md organizes pages by row group,
so one column is every sixteenth page in a sixteen-column segment. The row-group
count therefore sets the cold round-trip count directly:

| Row groups in a segment | Requests for one column |
| --- | --- |
| 256 | 256 |
| 16 | 16 |
| 1 | 1 |

SEGMENT_FORMAT.md says only that a row group holds "a bounded count of rows".
That value is load bearing for cold reads and needs an explicit choice. A
larger row group costs more decode memory and gives coarser statistics pruning.

### Bounded page cache

Dashboard pattern: a hot set of 64 pages repeated 20 times, with a different ad
hoc 64-page query in each round.

| Cache size | Hit rate | MiB fetched |
| --- | --- | --- |
| 4 MiB | 1.3 percent | 157.9 |
| 16 MiB | 48.8 percent | 82.0 |
| 64 MiB | 48.8 percent | 82.0 |
| 256 MiB | 48.8 percent | 82.0 |

The hot set is exactly 4 MiB, and a 4 MiB cache thrashes. Each ad hoc query
evicts the whole hot set before the next round reads it. A 16 MiB cache
captures everything that a cache can capture. More cache adds nothing, because
the ad hoc tail never repeats.

**Reading:** size the cache to several times the hot working set, not to a
fraction of the segment. A larger cache gives nothing past that point.

## 10. D15: consensus

openraft 0.9.24, three voters, in-memory storage and an in-process network.

**Scope.** This measures the library and the algorithm. It does not measure
TallyOwl's durable storage or its transport. A real deployment calls fsync on
every append, and sections 3 and 13 measure that separately. Read this section
together with those.

### Formation and change

| Operation | Time |
| --- | --- |
| Initialize to elected leader | 6 ms |
| Add a learner | 4 ms |
| Promote a learner to voter | 4 ms |
| Build a 256 KiB snapshot | under 1 ms |

### Commit throughput

| Entry size | Entries each second | MiB each second | p50 | p99 |
| --- | --- | --- | --- | --- |
| 256 B | 60,576 | 14.8 | 0.01 ms | 0.03 ms |
| 4 KiB | 49,284 | 192.5 | 0.02 ms | 0.04 ms |
| 64 KiB | 12,543 | 783.9 | 0.07 ms | 0.22 ms |

**Consensus is not the limit. The device is.** Section 13 puts group commit at
about 16,900 durable frames each second on this hardware. openraft commits
49,284 entries each second at a comparable size, with no durability at all. The
algorithm has ample headroom over the storage beneath it.

### Failure behaviour

| Event | Result |
| --- | --- |
| Isolate the leader | A new leader in 985 ms |
| Write on the majority side | Commits, 10.9 ms |
| Write on the isolated node | Blocked for the full 3 second timeout, never commits |
| Rejoin the isolated node | Caught up in 528 ms |

The isolated node never committed. A commit there means split brain, so the
benchmark checks for it explicitly rather than assuming.

### Many groups on one process

STORAGE.md section 7 puts hundreds of three-voter tablet groups on one storage
node. This is the TallyOwl-specific risk, because a general Raft library targets
a few large groups.

| Groups | Raft instances | Build | RSS added | Groups with a leader |
| --- | --- | --- | --- | --- |
| 10 | 30 | 1 ms | not measurable | 10 of 10 |
| 50 | 150 | 6 ms | 5.0 MiB | 50 of 50 |
| 200 | 600 | 23 ms | 21.2 MiB | 200 of 200 |

Every group elected a leader. A group without a leader cannot accept a write,
so that count is the property that matters.

The 10-group row reported a negative RSS change, which is allocator noise
rather than a measurement. The 50-group and 200-group rows are consistent with
each other at roughly 0.1 MiB for each group.

**Reading:** the multi-group design holds with a real Raft implementation. Six
hundred Raft instances cost 21 MiB and 23 milliseconds to build.

### What this does not answer

- behaviour against durable storage, where every append pays an fsync;
- behaviour over a real network with loss, reordering, and delay;
- a partition that splits the group other than by isolating one node;
- recovery from a corrupt or truncated log;
- openraft 0.10, which is at alpha and carries breaking changes.

## 11. D33: Corndogs as the collector durability boundary

Corndogs 27 MiB binary, file backend, `group` fsync mode, audit log disabled,
data directory on ext4 and NVMe.

### SubmitTask, which is the collector acknowledgement path

| Payload | Connections | Tasks each second | MiB each second | p50 | p99 |
| --- | --- | --- | --- | --- | --- |
| 1 KiB | 1 | 134 | 0.1 | 7.6 ms | 16.1 ms |
| 1 KiB | 8 | 725 | 0.7 | 9.6 ms | 20.0 ms |
| 1 KiB | 32 | 2,475 | 2.4 | 13.0 ms | 21.8 ms |
| 64 KiB | 1 | 150 | 9.4 | 5.8 ms | 11.8 ms |
| 64 KiB | 32 | 1,258 | 78.7 | 25.4 ms | 35.7 ms |
| 512 KiB | 1 | 103 | 51.4 | 8.9 ms | 17.7 ms |
| 512 KiB | 8 | 232 | 116.2 | 33.9 ms | 44.0 ms |
| 512 KiB | 32 | 393 | 196.4 | 80.7 ms | 109.4 ms |

One connection accepts about 103 to 150 batches each second whatever the
payload size. That matches the fsync ceiling. Concurrency lifts the rate,
because Corndogs group commit coalesces across connections.

One app driver connection therefore reaches roughly 26,000 to 38,000 events
each second. That assumes the D19 seal of 256 events for each batch.

The p50 of 80.7 milliseconds at 32 connections and 512 KiB matters for D19. The
driver permits 8 MiB of unacknowledged data on one connection. That is 16
batches of this size, so the budget and the acknowledgement latency interact.

### CleanUpTimedOut is not usable at the specified interval

D33 gives the forwarder this call on a one-second interval.

| Backlog in the swept queue | Sweep time | Fraction of a one-second interval |
| --- | --- | --- |
| 2,000 | 21.0 s | 2,104 percent |
| 10,000 | 21.5 s | 2,153 percent |
| 40,000 | 22.5 s | 2,254 percent |

| Payload in the swept queue, 2,000 tasks | Sweep time |
| --- | --- |
| 4 KiB | 22.6 s |
| 64 KiB | 25.8 s |
| 512 KiB | 30.3 s |

The database held about 83,000 live tasks across 13 queues at the end of the
run.

**Measurement flaw.** The depth series ran against a database that already held
tens of thousands of tasks from the submit phases. The three depths are
therefore confounded, and this table does not give a clean curve of sweep time
against live task count. Section 11a gives that curve.

Two facts still hold. A twentyfold growth of the swept queue, from 2,000 to
40,000 tasks, moved the time by only 7 percent. A larger payload in that queue
moved it by 34 percent.

**The sweep cost follows the whole database, not the queue that the caller
names.** The file backend walks every live task and decodes it. It applies the
queue filter after that decode, and payload bytes are part of the decode.

The cost works out near 0.25 to 0.5 milliseconds for each live task.

That rate matters more than the absolute times above, because it says where the
mechanism holds and where it fails:

- at a few thousand live tasks the sweep fits inside a one-second interval;
- at tens of thousands it does not.

**The failure mode is the problem, not the rate.** Live task count grows during
a head outage. That is exactly when retry and dead-worker recovery matter, so
the sweep slows down under the condition that it exists to handle.

Everything that depends on the sweep degrades together: backoff, retry,
dead-worker recovery, and forwarder readiness.

The design already anticipated the shape of this risk and recorded it in D33.
The measurement shows a worse result. The cost does not follow the queue that
TallyOwl sweeps. It follows everything else in the same Corndogs deployment.

Options, none of them yet chosen:

1. Keep the batch payload out of Corndogs. Store it in a collector-local
   content-addressed spool and put only a reference in the task. This attacks
   the payload term directly. STORAGE.md section 3.1 already permits a
   content-addressed payload reference in a WAL frame, so the shape exists.
   It reintroduces a second local journal, which D4 currently forbids.
2. Give TallyOwl its own Corndogs deployment, so no other workload adds to the
   scan. Then accept a sweep interval of tens of seconds.
3. Ask Corndogs for an indexed timeout sweep, which is a change request under
   the process in AGENTS.md.

## 11a. Sweep cost against live task count, measured cleanly

Fresh database, one queue, nothing else present. 256-byte payloads, which is
the size of a content-addressed reference rather than a batch.

| Live tasks | Sweep | Microseconds for each task | Fits a 1 second interval |
| --- | --- | --- | --- |
| 1,000 | 6.1 ms | 6.1 | yes |
| 5,000 | 22.0 ms | 4.4 | yes |
| 20,000 | 82.6 ms | 4.1 | yes |
| 50,000 | 205.4 ms | 4.1 | yes |

The curve is linear at about 4.1 microseconds for each task.

**This overturns the rate in section 11.** That section put the cost near 0.25
to 0.5 milliseconds for each task. The clean measurement puts it at 4
microseconds, which is 60 to 120 times lower.

The difference is the payload. Section 11 ran against a database holding 64 KiB
and 512 KiB payloads, and the file backend decodes every live task during a
sweep. Task count barely matters. Payload bytes are almost the whole cost.

At 4 microseconds for each task, a 50,000-task outage backlog sweeps in 205
milliseconds. That fits the one-second interval with five times the headroom.

### The same curve with a full batch payload

Identical benchmark, 512 KiB payloads, on its own fresh database.

| Live tasks | 256-byte payload | 512 KiB payload | Ratio |
| --- | --- | --- | --- |
| 1,000 | 6.1 ms | 3,272.5 ms | 536 times |
| 5,000 | 22.0 ms | 16,791.9 ms | 763 times |

Cost for each task: 4.1 microseconds against 3,358 microseconds. That is a
factor of 800.

A thousand tasks already fail the one-second interval when the payload rides in
the task. Fifty thousand tasks fit inside it when the payload does not.

### Why the payload costs so much

The file backend stores a task as JSON, and `Payload` is a `[]byte` field. Go
encodes a byte slice in JSON as base64, so a 512 KiB payload occupies about
700 KiB in the stored record.

Every sweep calls `json.Unmarshal` on every live task, which base64-decodes
that payload only to discard it. The sweep needs the timeout and the state. It
decodes the bytes anyway.

**Reading:** the sweep is not expensive. Decoding batch payloads during the
sweep is expensive. Keep the payload out of the task and the problem
disappears.

## 11b. Corndogs with a group-commit linger

Same benchmark as section 11, with `CORNDOGS_FILESTORE_GROUP_MAX_DELAY=2ms`.

| Payload | Connections | Default, 0s | With 2 ms linger | Change |
| --- | --- | --- | --- | --- |
| 1 KiB | 1 | 134 | 133 | none |
| 1 KiB | 8 | 725 | 869 | +20 percent |
| 1 KiB | 32 | 2,475 | 2,618 | +6 percent |
| 64 KiB | 1 | 150 | 113 | −25 percent |
| 64 KiB | 8 | 388 | 639 | +65 percent |
| 64 KiB | 32 | 1,258 | 1,474 | +17 percent |
| 512 KiB | 1 | 103 | 75 | −27 percent |
| 512 KiB | 8 | 232 | 256 | +10 percent |
| 512 KiB | 32 | 393 | 442 | +12 percent |

Rates are tasks each second.

**A linger helps a busy collector and hurts a quiet one.** With eight or more
connections it gains 6 to 65 percent. With one connection it costs 25 to 27
percent, because that connection waits 2 milliseconds for a batch that will
never grow.

This matters for the TallyOwl shape. D5 gives each app driver one persistent
connection. A collector serving one application therefore sees the regression,
and a collector serving many applications sees the gain.

**Reading:** leave the default at zero. Set a linger only on a collector that
serves many applications, and measure it there. Corndogs' own test uses 200
microseconds rather than 2 milliseconds, which is worth trying before 2
milliseconds.

Note also that 442 tasks each second at 512 KiB is 221 MiB each second. At that
point the bbolt write path and the device, not the fsync count, set the limit.

## 11c. The collector accept path, both ways

D4 moves the batch payload out of the Corndogs task and into a spool. That adds
a second durable write to the accept path, so the pair needed measuring against
the single write that it replaces.

Path A submits the whole payload as the task. Path B appends the payload to a
group-committed spool and then submits a 256-byte reference task. Both complete
before the collector may acknowledge.

| Payload | Connections | A, in task | B, spooled | Change | A p50 | B p50 |
| --- | --- | --- | --- | --- | --- | --- |
| 64 KiB | 1 | 163 | 101 | −38 percent | 5.2 ms | 9.8 ms |
| 64 KiB | 8 | 538 | 375 | −30 percent | 14.6 ms | 17.1 ms |
| 64 KiB | 32 | 1,256 | 1,060 | −16 percent | 24.8 ms | 27.9 ms |
| 512 KiB | 1 | 85 | 59 | −31 percent | 11.5 ms | 14.9 ms |
| 512 KiB | 8 | 235 | 193 | −18 percent | 33.2 ms | 29.2 ms |
| 512 KiB | 32 | 419 | **662** | **+58 percent** | 75.5 ms | **35.9 ms** |

Rates are batches each second.

**The spool loses at low load and wins at high load.** The crossover sits
between 8 and 32 connections at the 512 KiB seal size. At 32 connections the
spooled path moves 331 MiB each second against 209, and it halves the median
latency.

The reason is the storage engine underneath each path. Corndogs stores a task
in a copy-on-write B+tree, which pays write amplification and page splits on a
512 KiB value. The spool is an append-only log, which does not. Path B sends
512 KiB to the cheap structure and 256 bytes to the expensive one.

**Reading:** the cost lands where there is headroom, and the gain lands where
there is not. A collector with one application still reaches 59 batches each
second at 512 KiB. That is roughly 15,000 events each second at the D19 seal. A
collector under real load gains 58 percent and half its latency.

Combined with the 800-fold sweep improvement in section 11a, the D4 amendment
holds.

### An open thread

The spool group size stayed small: 1.0, 2.1, and 8.2 at one, eight, and
thirty-two connections. Each worker serializes its spool append against its own
task submit, so the spool never sees full concurrency. A different linger, or a
spool append that returns before the task submit begins, might raise the group
size and the win with it. Not measured.

## 11d. Corndogs after the payload change

Corndogs commit `b8c10b0` moved payloads out of the task record and added a
deadline index. TallyOwl reported the measurements in sections 11 and 11a, and
the Corndogs project answered both problems at once.

What it changed:

- a payload lives in its own bbolt bucket as raw bytes, with no JSON and no
  base64. The postgres backend uses `bytea` with `STORAGE EXTERNAL`, and
  metadata queries do not select the column;
- a deadline index maps a deadline to a task key, so `CleanUpTimedOut` seeks to
  the first expired entry and stops at the first live one;
- the wire contract separates `Task` metadata from a `TaskDelivery`, so payload
  bytes travel only in a claim response;
- a startup migration converts an existing database, and it resumes after a
  restart.

### The sweep

Same benchmark, same hardware, 512 KiB payloads, fresh database.

| Live tasks | Before | After |
| --- | --- | --- |
| 1,000 | 3,272.5 ms | 2.3 ms |
| 5,000 | 16,791.9 ms | 2.3 ms |

The cost is now flat, because the sweep is a function of expired tasks rather
than live ones. Nothing expired in this run, so it did constant work.

At 1,000 tasks that is 1,423 times faster. The shape matters more than the
factor. The old cost grew with the backlog. The new cost does not.

### The accept path, and the end of the TallyOwl spool

Section 11c measured a collector spool against a payload in the task. That
comparison was against the old Corndogs. Repeated against the new one:

| Payload | Connections | In task | Spooled | Winner |
| --- | --- | --- | --- | --- |
| 64 KiB | 1 | 212 | 76 | in task, by 179 percent |
| 64 KiB | 8 | 511 | 388 | in task, by 32 percent |
| 64 KiB | 32 | 1,727 | 1,157 | in task, by 49 percent |
| 512 KiB | 1 | 116 | 88 | in task, by 32 percent |
| 512 KiB | 8 | 334 | 278 | in task, by 20 percent |
| 512 KiB | 32 | 614 | 313 | in task, by 96 percent |

Corndogs alone improved 30 to 47 percent on the payload-in-task path:

| Payload and connections | Before | After |
| --- | --- | --- |
| 64 KiB, 32 | 1,256 | 1,727 |
| 512 KiB, 1 | 85 | 116 |
| 512 KiB, 8 | 235 | 334 |
| 512 KiB, 32 | 419 | 614 |

The one case where the spool used to win, 512 KiB at 32 connections, now loses
by a factor of two. The B+tree no longer carries the payload, so the reason for
an external spool is gone.

**TallyOwl therefore does not build a payload spool.** The collector keeps the
payload in the Corndogs task. See D4, D33, and D48.

## 12. D10: a preliminary capacity envelope

**The storage half is now measured. The end-to-end envelope is not.** D10 says
the reference application produces the real envelope, and that remains true.

`prototypes/segment-bench` writes a whole segment in the SEGMENT_FORMAT.md
layout, writes it to an ext4 disk, reads it back, verifies the trailer, the
footer checksum, and the BLAKE3 payload hash, and answers 20,000 point lookups
from the index. Earlier versions of this section added component measurements
together. That addition was wrong, and this section records by how much.

### Bytes for each event: the addition against the measurement

| Part | Added, earlier | Measured, whole segment |
| --- | --- | --- |
| Column data after zstd level 1 | 21.0 | 27.3 |
| Exact indexes | 20.3 | 20.3 |
| Catalog receipt | 1.3 | 1.3 |
| Format overhead | not counted | 0.0 |
| **Total** | **42.6** | **48.9** |

Two findings, and they point in opposite directions.

**Format overhead is free.** The prologue, the header, the row group
directory, the footer, the trailer, the page headers, and the null bitmaps
together cost less than 0.005 bytes for each event at one million rows. A
64-byte prologue over a 45 MiB file does not show up. The addition was right to
ignore it.

**Column data costs 30% more than the sum of the parts.** Section 5 measured
each encoding on its own column with its own value distribution. In a real
segment the same eight columns carry correlated, higher-cardinality values, and
zstd finds less to remove. The addition was wrong here.

### Where the bytes go

At one million events, 65,536 rows for each row group:

| Part | Bytes for each event | Share |
| --- | --- | --- |
| `event_id` exact index | 12.00 | 25% |
| `event_id` column | 8.13 | 17% |
| `end_user_id` column | 7.85 | 16% |
| `measure` column | 5.45 | 11% |
| `end_user_id` postings | 4.74 | 10% |
| `trace_id` postings | 3.60 | 8% |
| `trace_id` column | 1.83 | 4% |
| `route` column | 1.48 | 3% |
| `duration_ms` column | 1.11 | 2% |
| `occurred_at` column | 0.80 | 2% |
| `service` column | 0.68 | 1% |

**One index on one column is a quarter of the segment.** That is the largest
single line item in the format, and section 12a shows it does not have to be.

### Cost for each event grows with segment size

| Rows in the segment | Bytes for each event |
| --- | --- |
| 250,000 | 44.7 |
| 1,000,000 | 47.7 |
| 4,000,000 | 50.3 |

Roughly 2.8 more bytes for each four-fold increase in rows. The cause is
cardinality: a larger segment holds more distinct end-user IDs and trace IDs,
and a compressor removes less from a wider value space.

**A capacity estimate must therefore state the segment size it assumes.** A
single "bytes for each event" number without that qualifier is wrong at one end
of the range or the other.

The small-segment case runs the other way and is cheap: at 1,000 rows a segment
costs 43.9 bytes for each event, of which 0.9 is fixed format cost. The home
profile seals microsegments at one second, so this case is common there, and it
carries no penalty worth designing around.

### What a disk holds

Segment plus catalog receipt, because both sit on the disk. The
one-million-row measurement gives 48.9 bytes for each event. Section 12a gives
41.1.

| Disk | Events, today | Events, with section 12a |
| --- | --- | --- |
| 100 GiB | 2.2 billion | 2.6 billion |
| 1 TiB | 22.5 billion | 26.8 billion |
| 4 TiB | 90 billion | 107 billion |

### Retention at a sustained rate

| Sustained rate | Each day | 1 TiB lasts | With section 12a |
| --- | --- | --- | --- |
| 1,000 events each second | 3.9 GiB | 260 days | 310 days |
| 10,000 events each second | 39.3 GiB | 26 days | 31 days |
| 50,000 events each second | 196.7 GiB | 5 days | 6 days |

These ignore the WAL, the raw CBOR tier, rollups, and free space that
compaction needs. Treat them as an upper bound on retention rather than a
promise.

### Build and read rates

| Operation | Rate |
| --- | --- |
| Build a segment from events in memory | 4.0 million events each second |
| Write and fsync to ext4 | 1,700 MiB each second |
| Read a whole segment | 1,650 MiB each second |
| Point lookup through the exact index | 5.2 million each second |

The segment writer is not a bottleneck. At 4 million events each second it is
two orders of magnitude above the ingest ceiling in the next table.

## 12a. Two changes the whole-segment measurement asks for

Both are measured together, not added, because adding is what produced the
42.6 estimate.

### The `event_id` index should be a filter, not a sorted list

`event_id` is unique by construction. A postings list serves a value that
repeats. A unique value needs the answer to one question: which row group holds
this ID, if any? A block filter answers that question, and a page scan turns a
maybe into an exact answer.

| Form | Bytes for each event | False positive rate | Lookups each second |
| --- | --- | --- | --- |
| Sorted, 12 bytes for each row | 12.00 | 0 | 6.9 million |
| Block filter, 4 bits for each key | 0.51 | 16.06% | 28.1 million |
| Block filter, 8 bits for each key | 1.02 | 3.08% | 23.6 million |
| **Block filter, 12 bits for each key** | **2.00** | **0.53%** | **21.1 million** |
| Block filter, 16 bits for each key | 2.03 | 0.50% | 20.6 million |

12 bits for each key saves 10.00 bytes for each event, which is 21% of the
whole segment. The cost is about one wasted page read for every twelve lookups.

16 bits buys nothing over 12, because a filter is sized to a power of two
words and both round to the same size. Take the 12.

Correctness holds. A filter says maybe or no. A no is exact. A maybe reads the
`event_id` page and gets an exact answer there. Deduplication and point lookup
both keep their meaning.

`trace_id` and `end_user_id` keep their exact postings. Those values repeat,
and a query for a whole trace wants the row list, which a filter cannot give.

### A page must be sized in bytes, not in rows

SEGMENT_FORMAT.md asks for 64 KiB pages and 65,536-row row groups. The two
cannot both hold: a 16-byte column at 65,536 rows is 1 MiB before compression.
Measured, the largest page is **520 KiB, eight times the target**.

Splitting the page costs compression ratio, because the compressor sees less
history:

| Rows for each page | `event_id` | `end_user_id` | `trace_id` | Largest page |
| --- | --- | --- | --- | --- |
| 4,096 | 8.14 | 9.20 | 1.84 | 37.4 KiB |
| 8,192 | 8.13 | 8.58 | 1.83 | 69.4 KiB |
| 16,384 | 8.13 | 8.17 | 1.83 | 131.8 KiB |
| 65,536 | 8.13 | 7.85 | 1.83 | 520.1 KiB |

Only `end_user_id` cares, and it costs 1.35 bytes for each event to come inside
the target. `event_id` and `trace_id` do not change at all, because their bytes
are already incompressible.

### The two together

| Configuration | Bytes for each event | Largest page |
| --- | --- | --- |
| As written today | 47.66 | 520.1 KiB |
| 4,096-row page only | 49.75 | 37.4 KiB |
| Filter only | 37.66 | 520.1 KiB |
| **Both** | **39.75** | **37.4 KiB** |

Both changes, measured together: **39.75 bytes for each event, and every page
inside the 64 KiB target.** That is 17% smaller than the format as written, and
below the 42.6 that the earlier addition predicted.

The saving holds at every scale tested. The filter saves exactly 10.00 bytes
for each event at 250,000, 1 million, and 4 million rows, because its size does
not depend on the value distribution.

**Action:** SEGMENT_FORMAT.md needs both changes. See D17 and D20.

### Ingest ceiling on this hardware

| Boundary | Rate | Source |
| --- | --- | --- |
| Device fsync | 186 each second | Section 3 |
| Corndogs accept, one connection | 103 to 150 batches each second | Section 11 |
| Corndogs accept, 32 connections | 393 batches each second at 512 KiB | Section 11 |
| Catalog commit | 182 to 621 each second | Section 4 |

A batch crosses **two** fsync boundaries in the home profile: Corndogs accepts
it, and then the head commits it. Both must group-commit, or the two costs add
in series and single-stream throughput halves.

At the D19 seal of 256 events for each batch, one app driver connection reaches
roughly 26,000 to 38,000 events each second. That is the number to design the
home profile against until the reference application replaces it.

## 12b. D20 at scale: the tablet locator with 100 million users

Every benchmark above section 12b measures inside **one** segment. That proves
a lookup is cheap once the right segment is open. It says nothing about finding
a value across a retention window, which is the tablet locator's job.
[HIGH_CARDINALITY.md](HIGH_CARDINALITY.md) section 4 defines the locator, and
until now nothing had built one.

`prototypes/locator-bench` builds real locator runs at full user cardinality.
100 million users is not a memory problem: the locator is a few GiB.

### The workload

| Item | Value |
| --- | --- |
| Registered users | 100 million |
| Daily active users | 10 million |
| Servers | 100,000 |
| Events each day | 1 billion, which is 11,600 each second |
| Retention | 30 days |
| Segment target | 256 MiB, which is 6.8 million events at 39.75 bytes |
| Segments each day | 149 |
| Segments retained | 4,470 |
| Stored bytes retained | 1.08 TiB |

### The load-bearing quantity is not the user count

The locator holds one segment reference for each **(user, segment) pair**. The
pair count, not the user count, sets its size. The pair count depends on how a
user's events scatter across segments, which is a placement decision.

| Layout | Segments for each user | Pairs retained | Of all segments |
| --- | --- | --- | --- |
| Scattered, as designed today | 100 | 30 billion | 67.1% |
| Sharded by end user, 64 shards | 2.3 | 698 million | 1.6% |
| Sharded by end user, 1024 shards | 1.0 | 300 million | 0.7% |

100 million users costs nothing on its own. A user who appears in 100 of the
day's 149 segments costs 100 entries, and that is where the size goes.

Sorting rows by end user **inside** a segment changes none of this. It reorders
rows. It does not change which segment holds them. Only routing does.

### Bytes for each pair is not flat, and density is why

| Pairs | Users | Groups | Run bytes | Bytes for each pair |
| --- | --- | --- | --- | --- |
| 10 million | 5 million | 4.3 million | 50.1 MiB | 5.257 |
| 50 million | 20 million | 18.4 million | 219.5 MiB | 4.605 |
| 200 million | 50 million | 49.1 million | 696.9 MiB | 3.655 |
| 500 million | 100 million | 99.3 million | 1.52 GiB | 3.274 |
| 1 billion | 100 million | 100.0 million | 2.28 GiB | 2.451 |

**Bytes for each pair moved 53 percent across this series.** Taking any one
figure and multiplying it by a target pair count would repeat the section 12
mistake in a new place.

What drives it is density, meaning segments for each user. A fingerprint costs
the same whether one segment reference follows it or three hundred:

| Segments for each user | Bytes for each pair | Bytes for each user |
| --- | --- | --- |
| 1 | 7.074 | 7.1 |
| 3 | 4.428 | 13.3 |
| 10 | 2.530 | 25.3 |
| 30 | 1.679 | 50.2 |
| 100 | 1.136 | 112.4 |
| 300 | 1.031 | 299.1 |

Every size below reads bytes for each pair off this curve at its own density.

### Probe cost

| Measurement | Result |
| --- | --- |
| Probes each second | 867,000 |
| Median probe | 1.2 microseconds |
| Mean candidate segments | 10.0 |
| Worst candidate segments | 27 |

A locator probe is not the cost. What follows it is.

### A time range prunes linearly

Runs are partitioned by day, so a query reads only the runs it overlaps.
Measured over 30 daily runs holding 20 million pairs each:

| Query range | Runs read | Bytes touched | Candidate segments | Probe |
| --- | --- | --- | --- | --- |
| 1 day | 1 | 81.2 MiB | 2.0 | 0.6 microseconds |
| 7 days | 7 | 568.1 MiB | 13.9 | 5.3 microseconds |
| 30 days | 30 | 2.38 GiB | 59.6 | 26.7 microseconds |

**An unbounded lookup on a high-cardinality value reads the whole retention
window.** That is the shape of the cost, and a query surface must show it.

### The answer at target scale

| Layout | Density | Bytes for each pair | Pairs | Locator size | Candidate segments |
| --- | --- | --- | --- | --- | --- |
| Scattered, as designed today | 3,000 | 1.031 | 30 billion | 28.81 GiB | 3,000 |
| Sharded by end user, 64 | 69.8 | 1.298 | 698 million | 864.7 MiB | 70 |
| Sharded by end user, 1024 | 30.0 | 1.679 | 300 million | 480.4 MiB | 30 |

The locator size is survivable in every row. **The candidate count is not.**
An unbounded lookup against the layout as designed today opens 3,000 segments.

### Without a locator at all

| Measurement | Result |
| --- | --- |
| Segments to probe for one lookup | 4,470 |
| Block filter memory, all segments | 42.21 GiB |

The locator replaces 4,470 filter probes with one. It is not an optimization at
this scale. Without it, the working set for a single point lookup is 42 GiB.

### What compaction fixes, without changing ingest routing

Ingest must not wait to sort, so hot data stays scattered. Compaction already
rewrites cold segments and already merges locator runs. Grouping rows by end
user while it does so lowers density for the retained majority:

| Tier | Density | Bytes for each pair | Locator size | Candidate segments |
| --- | --- | --- | --- | --- |
| Hot, 2 days, scattered | 200 | 1.070 | 1.99 GiB | 200 |
| Cold, 28 days, user-grouped | 28 | 1.732 | 462.6 MiB | 28 |
| **Total** | | | **2.44 GiB** | **228** |

Against 28.81 GiB and 3,000 candidate segments for the scattered layout: a
**13-fold** reduction in candidate segments and a 12-fold reduction in locator
size.

This changes no routing, so it does not trade away trace locality the way
sharding by end user would. Sharding by end user helps an end-user lookup and
hurts trace assembly, because a trace's spans then scatter across shards. A
system cannot shard by both.

### Fingerprint width

| Width | Expected collisions at 100 million values |
| --- | --- |
| 32-bit | 1.2 million |
| 48-bit | 18 |
| 64-bit | 0 |

A collision costs one wasted segment open. It never costs correctness, because
the segment index verifies the full typed value. A 32-bit fingerprint is still
disqualified at this scale: it would send every lookup to more than a million
extra segments.

### What this does not measure

The locator runs are real and built at full cardinality. These are not:

- the segment opens that follow a probe. The candidate counts above are the
  input to that cost, not the cost itself;
- locator merge cost during compaction;
- the locator on disk, or its cold-start read;
- fan-out across tablets, which multiplies the candidate count by the tablet
  count for a project;
- write amplification from user-grouped compaction.

## 12c. D10: the ingest half, measured against a running installation

> **Superseded by section 16**, which re-ran this on 2026-08-04 against a build
> where both drivers pipeline and the append log is reclaimed. Two conclusions
> below did not survive: the disk explanation in point 3, and the reading of the
> sustained rate as the system's ceiling rather than the collector's. The
> measurements themselves stand; the run is kept because it is what the
> comparison is against.

**Measured, 2026-08-03.** This closes the half of D10 that no prototype could
answer: the earlier numbers came from component benchmarks, and this one comes
from the whole product with the reference application's driver in front of it.

**Hardware.** AMD Ryzen 7 5800X, 8 cores and 16 threads, 125 GiB of memory.
**Filesystem.** ext4 on NVMe, `/dev/nvme1n1p2`. Not tmpfs; see section 2.
**Build.** `--release`. **Profile.** `home`, all on one machine.
**Harness.** `testbed/cmd/load`, seed 20260803, through the Go app driver.

| Measure | Result |
| --- | --- |
| Sustained events each second | **36,525**, and 38,927 achieved at a 40,000 target with nothing refused |
| One synchronous producer | **5,405** each second |
| Bytes for each sealed row, in segments | **32.0** |
| Bytes for each event, whole data directory | **223.3** |
| Point lookup, p50 and p99 | 51.8 and 54.0 milliseconds over 387,177 events |
| Aggregate, p50 and p99 | 51.0 and 53.6 milliseconds |
| Behaviour at the ceiling | Refusal. 238 batches offered with the durable store gone, 238 refused, 0 accepted |
| Recovery after `kill -9` under load | **462 milliseconds**, watermark and row count identical |
| Candidate segments for a high-cardinality lookup | 1, over 2 sealed segments. **Not usefully measured**; see 12b |

**The derived envelope held.** Section 12 said one app driver connection would
reach roughly 26,000 to 38,000 events each second on this hardware, and eight
concurrent producers reached 36,525. That is the first estimate in this file
that a whole-system measurement confirmed rather than overturned.

**32.0 bytes for each sealed row beats the 39.75 of section 12a**, and on harder
rows: every event here carried a unique `request_id`, which is the
high-cardinality column that dominates a segment.

Three findings, and the first is the useful one:

1. **One synchronous producer reaches 5,405 events each second, and that is
   what most applications get.** A driver flush waits for its durable
   acknowledgement, a batch seals at 256 items, and the round trip is about 47
   milliseconds. Eight producers reach 36,525 because eight round trips
   overlap. DELIVERY.md section 3 already permits pipelining and neither
   maintained driver does it. **The ceiling in this table is a property of
   concurrency, not of one connection.**

2. **The burst multiplier could not be measured**, for the same reason. The
   harness offers four times the sustained rate and the system took all of it
   without refusing any, so the harness could not offer faster than the system
   took. A real burst measurement needs a producer that does not block on its
   own acknowledgements.

3. **The whole data directory costs seven times what the segments do**, because
   this run never reached a steady state: the append log is the durable record
   until a segment replaces it and nothing reclaims a log range, and the catalog
   had just taken 273,257 unique request IDs into the locator. An operator
   sizing a device today uses 223 bytes for each event, not 32.

   **This explanation was wrong.** Reclamation exists now and the whole-directory
   figure barely moved, from 223 to 216. The cost is the tablet locator, which no
   pass reclaims, and not the append log. See section 16.

**A point lookup and an aggregate cost the same**, because the query executor
materialises the rows in a range and then filters them. `lookup_correlated` uses
the tablet locator and does not scan; the executor does not use it.

Reproduce with the commands in `docs/ALPHA_REPORT.md` section 9.

## 13. WAL group commit: the answer to the fsync ceiling

Section 3 measured the ceiling. STORAGE.md section 3.1 answers it with bounded
group commit. That answer needed its own measurement, because it carries the
whole write path.

4 KiB frames, appended to a growing file on ext4 and NVMe.

| Mode | Writers | Frames each second | Fsyncs each second | p50 | p99 | Mean group |
| --- | --- | --- | --- | --- | --- | --- |
| One fsync for each frame | 1 | 78 | 78 | 13.8 ms | 14.2 ms | 1.0 |
| One fsync for each frame | 32 | 91 | 91 | 10.0 ms | 8.77 s | 1.0 |
| One fsync for each frame | 128 | 150 | 150 | 5.8 ms | 23.34 s | 1.0 |
| Group, no linger | 32 | 228 | 186 | 5.5 ms | 1.13 s | 1.2 |
| Group, no linger | 128 | 503 | 183 | 38.3 ms | 1.53 s | 2.8 |
| Group, 2 ms linger | 8 | 1,101 | 138 | 7.5 ms | 9.2 ms | 8.0 |
| Group, 2 ms linger | 32 | 4,440 | 139 | 7.4 ms | 9.5 ms | 32.0 |
| Group, 2 ms linger | 128 | **16,923** | 134 | 7.5 ms | **9.4 ms** | 126.7 |
| Group, 10 ms linger | 128 | 7,721 | 60 | 16.6 ms | 18.6 ms | 128.0 |

Three findings.

**A linger converts the ceiling into throughput.** At 128 writers it reaches
16,923 durable frames each second from 134 fsyncs. The naive path reaches 150.
The device still does about 134 syncs each second, and each one now carries 127
frames.

**A linger is not optional.** Without one, a group holds only what arrived while
the previous fsync was in flight. That gives a mean group of 2.8 even at 128
writers, and 503 frames each second. The linger is what makes the group large.

**More linger is worse.** At 10 milliseconds the throughput falls to 7,721 and
the latency doubles. A longer wait leaves the device idle between syncs. Two
milliseconds was the best of the values tried.

The naive path also has a latency pathology worth naming. Its p99 reaches 23
seconds at 128 writers, because every writer queues behind a serialized fsync.
Group commit at 2 milliseconds holds p99 under 10 milliseconds at the same
concurrency.

### A prototype bug worth recording

The first version of this benchmark reported a mean group of 1.0 and no
throughput gain. That result was a false negative against the design.

The cause was in the prototype, not in the idea. The committer held the mutex
across the write and the sync. No other writer could then add to the next
buffer while a sync was in flight. Releasing the lock across the expensive part
is the whole mechanism.

### Consequence for Corndogs

Corndogs exposes `CORNDOGS_FILESTORE_GROUP_MAX_DELAY` and defaults it to zero,
which is the no-linger case above.

**The prediction from this section was wrong.** It said that a small linger
should raise the Corndogs accept rate substantially. Section 11b measures it and
the gain is modest, with a regression on a single connection.

The reason is that Corndogs already coalesces without a linger. Its committer
collects everything queued behind the first operation, so concurrency alone
fills a batch. A minimal append path has no such queue, which is why the linger
mattered so much here and so little there.

A synthetic benchmark predicted the wrong thing about a real system. That is the
normal outcome, and it is why section 11b exists.

## 14. Not yet measured

| Item | Decision | Blocked on |
| --- | --- | --- |
| Consensus election, partition, membership, and snapshot transfer | D15 | A storage and network implementation |
| Capacity envelope, measured end to end | D10, D23 | The reference application |
| Segment cost at real value distributions | D10 | The reference application |
| Segment opens that follow a locator probe | D20 | A read path |
| Locator merge cost during compaction | D20, D49 | A compaction path |
| Locator fan-out across tablets | D20, D26 | A tablet implementation |
| Provisional retention cost at the tail decision window | D35 | The reference application |
| Deletion and cold-object lookup | D20 | A deletion path |
| Cold-tier upload limits and bucket-outage behaviour | D24 | An object-store deployment |
| Cryptographic erasure of cold data | D28 | The segment key design |
| Query budgets, caps, and fan-out limits | QUERY.md section 18 | The capacity envelope |

Sections 8 through 13 cover what earlier versions of this table listed as
pending. The index layouts, the Tantivy comparison, cold tiering round trips,
Corndogs throughput, the sweep, and group commit are all measured.

Section 12 measures a whole segment, but with generated values. The shape of a
real distribution changes the compression ratio, and section 12 shows that
column data is the part the component measurements got wrong. Treat 39.75 bytes
for each event as a floor for this column set, not as a promise.

## 15. Reproducing

```
cd prototypes
cargo build --release
./target/release/page-bench 1000000 42
./target/release/catalog-bench 200000
./target/release/index-bench 1000000 42
./target/release/tier-bench 15
./target/release/wal-bench 4096
./target/release/consensus-bench
./target/release/segment-bench 1000000 42
./target/release/locator-bench 42
```

`locator-bench` builds locator runs at 100 million users and needs about
20 GiB of memory at its largest case. It takes roughly three minutes.

The catalog and segment benchmarks write to `~/.cache/tallyowl-bench` by
default. `TALLYOWL_BENCH_DIR` selects another directory, and the catalog
benchmark takes one as its second argument. That directory must be on real
storage. Both refuse a memory-backed path. See section 2.

## 16. Measured, 2026-08-04 — the ingest half, re-run

`testbed/cmd/load` against the `home` profile on this build. Hardware: AMD
Ryzen 7 5800X, 8 cores and 16 threads, 125 GiB of memory. Filesystem: ext4 on
`/dev/nvme1n1p2`, an NVMe device. **Not tmpfs.** Build: `--release`. Seed
20260803. The harness sends through the maintained Go app driver, and it now
uses the driver's pipelined `submit` rather than `flush`.

| Measure | 2026-08-04 | 2026-08-03 |
| --- | --- | --- |
| Sustained events each second, collector acceptance | **67,624** | 36,525 |
| One synchronous producer | **19,534** | 5,405 |
| Head commits to final storage | **17 batches each second**, about 3,600 events | Not measured |
| Bytes for each event, whole data directory | **215.8** | 223.3 |
| Bytes for each event, catalog alone | **157.2** | 111.8 |
| Bytes for each event, append log alone | **25.8** | 88.8 |
| Query p50, point lookup and aggregate | **103.0** and **102.0** ms | 51.8 and 51.0 |
| Events sent, accepted, and committed | 543,994 / 543,994 / **543,994** | 387,177 |

**Three findings, and two of them contradict what this document said.**

**One producer moved 3.6 times.** L055 gave both drivers a pipelined `submit`
and gave a connection the ability to serve correlated requests at the same time.
This is the number D10 cared about, because an application with one telemetry
worker gets it.

**The published ingest ceiling has always been the collector's, not the
system's.** A collector acknowledges when Corndogs is durable; the head drains
afterwards, at a flat 60 milliseconds for each batch. 67,624 events each second
holds while the queue has room. The steady-state rate is the head's 3,600, and
the cost is one durable catalog transaction for each batch, which grows with the
catalog. Section 12's derivation was for the collector path and remains correct
for it.

**Section 12a's disk explanation was wrong.** The gap between 32 bytes for a
sealed row and 223 for the directory was attributed to a missing reclamation
pass. The pass exists now, the append log fell from 88.8 bytes to 25.8, and the
directory figure barely moved. The cost is the tablet locator: one entry for
each value and segment pair, and every event in this run carries a unique
`request_id`. That is the price of the exact high-cardinality lookup `AGENTS.md`
requires, and no pass reclaims it.

**A regression this run introduced and fixed.** The first re-run measured 37,135
because the reclamation copied the whole append-log tail for each seal, which is
quadratic in the backlog. Reclaiming only when the prefix is half the file makes
it amortised. See L060.

## 17. Measured, 2026-08-04 — Phase 6 on the same path

> **Section 18 supersedes this one.** Six defects were found after it, four of
> them in the foundation, and four of the runs below inherited a delivery queue
> that outlived every reset (section 17.4). This section is kept because how
> each number was wrong is worth as much as the number: read 17.4 and 17.6
> before using anything here.

`testbed/cmd/load` against the `home` profile on the Phase 6 build. Hardware:
AMD Ryzen 7 5800X, 8 cores and 16 threads, 125 GiB of memory. Filesystem: ext4
on `/dev/nvme1n1p2`, an NVMe device. **Not tmpfs.** Build: `--release`. Seed
20260803.

**Read section 17.4 before any number here.** The first four runs in this
section inherited a Corndogs queue that outlived thirteen hours of resets, so
their sustained figures and their disk figures describe a system that was being
handed work nobody offered it. The final run started from a Corndogs this
session started, and it is the one to use.

**The sustained figure is still not one number.** Four runs of the same harness
on the same machine produced 77,382, 49,776, 80,000, and **37,874** events each
second. Section 16 said the published ceiling was the collector's rather than
the system's; this shows what that costs in practice. The collector's rate is
bounded by **how much room the Corndogs queue has**, and the queue's room is
bounded by how far behind the head is. A run against a drained head reaches the
top of the ramp; a run that starts behind reaches a third of it. The inherited
queue was one more way to start behind.

The clean run, from an empty `data/` and a Corndogs this session started:

| Measure | Phase 6, clean | Phase 5 |
| --- | --- | --- |
| Sustained events each second, collector acceptance | **37,874** | 67,624 |
| One synchronous producer | **19,566** | 19,534 |
| Head commits to final storage | **17 batches each second**, unchanged | 17 |
| Batches accepted, delivered | **2,624 and 2,624** | Not correlated |
| Items accepted, events committed | **438,866 and 438,866**, a ratio of **1.0000** | 543,994 and 543,994 |
| Query p50, point lookup and aggregate | **98.0** and **97.0** ms | 103.0 and 102.0 |
| Query p99, point lookup and aggregate | **115.1** and **115.2** ms | Not separated |
| Bytes for each accepted event, whole data directory | **243.9**, and see below | 215.8 |
| Recovery after an abrupt kill under load | **314 milliseconds**, no loss and no refusal | 462 ms |
| Burst multiplier absorbed without loss | **1.03**, still not the real answer | 0.71 |

**Accepted equals committed exactly.** That is the number the first four runs
could not produce, and it is the one that says the durable path neither loses
nor duplicates.

### 17.1 One synchronous producer did not move, and that is the finding

19,518 to 19,578 across three runs, against 19,534 before Phase 6. **Phase 6
added a series ledger and a merge pass to the same intake path and neither is
measurable here.** The ledger charges only metric points, the merge returns
immediately for a batch with one metric point or none, and this harness sends
events. An installation that sends no metrics pays nothing for the feature.

### 17.2 Query latency doubled again, and the range is wider than the middle

211 milliseconds at p50 against 103. But the second run of the same query
against the same store measured **42** milliseconds at p50 and **357** at p99.
The p50 moves by five times depending on whether the head is committing, and the
p99 moves the other way. Reporting one number for query latency has been wrong
in both directions now.

The cause is the one L045 named and section 3.5 of the alpha report measured:
the executor materialises the rows in the range rather than using the locator.
Everything else — the backlog, the catalog size, the page cache — moves the
number around that.

`LOAD_QUERIES_ONLY=1` now measures the query half on its own, so the next run can
separate the two rather than reporting three causes as one.

### 17.3 The disk cost is 243.9 bytes for each event, and the append log is most
of it while the backlog is unsealed

After 438,866 accepted events, fully drained, the data directory held
107,019,010 bytes:

| Part | Bytes | For each accepted event |
| --- | --- | --- |
| Catalog, mostly the tablet locator | 26,521,600 | 60.4 |
| Segments | 7,435,580 | 16.9 |
| Append log | 73,061,830 | **166.5** |
| The whole data directory | 107,019,010 | **243.9** |

**Only 196,946 of the 438,866 rows had been sealed into segments**, which is 45
percent, and a sealed row costs **37.8 bytes**. The rest are still in the append
log, which is why the log dominates. So 243.9 is a snapshot of a store that is
behind on sealing rather than a steady state, and the honest reading is that
**this project has never measured a steady state**: every run has stopped while
the head was still catching up.

That is the measurement to design next. A run that offers load and then waits
for sealing and compaction to settle would give the number an operator actually
needs, and no run so far has.

### 17.4 The forwarder delivered more batches than intake accepted, and it was
the measurement rather than the system

The first Phase 6 run reported 7,925 batches accepted, 9,998 delivered, and 1.61
rows in the store for each accepted event. **It was a defect in the development
loop, not in TallyOwl.** The system behaved correctly throughout, and the
finding is recorded here because the way it was found is worth keeping.

`./tools.sh dev up` starts Corndogs as `go run main.go run` when there is no
binary on the path. `go run` compiles to a temporary executable and runs it as a
**child**, so `dev.py` recorded the wrapper's process identifier and `dev down`
stopped the wrapper. **The server survived every stop.** Its open file was

```
data/corndogs/corndogs.bolt (deleted)
```

so every `rm -rf data/` unlinked the path while the process kept the inode. One
Corndogs held the delivery queue for **13 hours and 15 minutes** across four
supposed resets.

So the queue handed each new collector tasks that an *earlier* collector had
accepted. This collector never accepted them, which is why they were missing
from its count; the forwarder delivered each exactly once, which is why no batch
identifier repeated; and the freshly wiped head committed them as new, which is
why nothing deduplicated. Every component was right.

**How it was found**, because the method transfers:

| Step | What it ruled out |
| --- | --- |
| Counted every distinct message in the collector log | Showed 3,979 delivery failures in one 35-second window, all "connection refused" — the deliberate kill |
| Correlated batch identifiers between the acceptance and delivery lines | 9,998 distinct, each delivered exactly **once**. No redelivery, no duplicate commit. The head was exonerated here |
| Ran a short ramp with no kill | 408 accepted, 408 delivered. Clean |
| Ran a short ramp **with** a kill | 472 accepted, 472 delivered. Clean, so a kill alone does not do it |
| Ran the full ramp | 3,292 accepted, all correlated, and the metric matched the log line count exactly. Clean |
| Read `run/processes.json` against `ss -lptn` | The recorded Corndogs identifier was dead and a **different** process held the port |

The gap never reproduced because every controlled cycle started from a Corndogs
this session had started. The first run inherited one that nothing had stopped
since morning.

**Two things were changed as a result.** `dev up` now starts each service in its
own process group and `dev down` stops the group, so the wrapper's child cannot
outlive it; and `dev down` checks each service address afterwards and names any
process still holding one. That check reports the exact identifier in one line.

**The batch identifier was missing from the acceptance log line**, so the two
halves of the path could not be correlated at all until it was added. It is
there now, with the producer that enqueued the batch, because `Intake::submit`
has two callers and only one of them logged.

### 17.4a The settled store, and the page guard that was hiding it

Everything above was measured on a store that was still catching up. The first
measurement of a **settled** store — every batch delivered, every row sealed,
nothing in flight — needed a defect fixed first, and then said something the
project had not seen.

`LOAD_QUERIES_ONLY=1` against the drained store answered `incomplete-result`
every time. The reason, once the refusal was made to name it:

> A stored page claims to expand far more than real data does. We did not
> expand it.

**The page was intact.** It had already passed its checksum; a decompression
ratio limit rejected it. A page where every row holds the same release or
service name compresses to almost nothing, so a high ratio is what telemetry
looks like rather than what an attack looks like. The limit had already been
raised once for the same reason, and 438,866 events passed the raised one too.
It is removed; see L077.

With every page readable, the settled store answers:

| Measure | Settled store, 438,866 events |
| --- | --- |
| Point lookup, p50 and p99 | **1,063** and **1,775** ms |
| Aggregate, p50 | **1,057** ms |
| Stable across runs | Yes: 1,066 then 1,063 at p50 |

**That is ten times the last published p50, and it is the honest number.** Every
query figure this project has published was taken against a store that was
either mid-backlog or about to refuse a page. A point lookup on a unique
`request_id` should touch one segment through the locator and instead costs a
second, which is exactly what L045 predicted and nothing had yet measured with
the other variables removed.

### 17.5 An abrupt kill under load lost nothing and refused nothing

The head was killed with `SIGKILL` twelve seconds into a run. The producers kept
going and **not one batch was refused**, because the collector's durability
boundary is Corndogs and not the head: 610,423 offered, 610,423 accepted, zero
refused. The head restarted and answered `Ready.` in **314 milliseconds**,
opening a store that held eight more rows than the last reading before the kill.

This is the durability claim working exactly as `docs/DELIVERY.md` section 3
states it, and it is the second time it has been measured under real load.

### 17.6 A damaged segment is now named

A store that had accumulated several overlapping load runs reached a state where
`segment.rows()` failed for at least one segment, and every query over its range
then answered `incomplete-result` for the rest of the process's life. The
refusal was correct and unactionable: nothing said **which** segment.

`SegmentedStore::unreadable()` now reports the reasons, the head logs them at
start-up, and a lookup that finds damage records the reason rather than only
setting the flag. FAILURE_MODES.md procedure 6 requires the naming and it was
missing.

### 17.7 Reproducing

```sh
./tools.sh dev up
cd testbed && go run ./cmd/load 127.0.0.1:5100 127.0.0.1:5110 \
    "$(cat ../data/collector.key)" "$(cat ../data/operator.session)"

# the query half alone, against a store that already holds data
LOAD_QUERIES_ONLY=1 go run ./cmd/load 127.0.0.1:5100 127.0.0.1:5110 \
    "$(cat ../data/collector.key)" "$(cat ../data/operator.session)"
```

**Start from an empty `data/`.** Two of the three runs above did not, and the
spread between them is most of what section 17 reports.


## 18. Measured, 2026-08-04 — after the six fixes

Everything section 17 could not answer, answered. Same machine, same
filesystem, same seed, `--release`, from an empty `data/` with a Corndogs this
session started.

**Six things changed between section 17 and this**, and four of them were
defects rather than tuning: the decompression ratio limit (L077), the missing
background segmenter (L078), the locator pushdown (L079), retention expiry
(L080), the burst measurement pacing itself (L082), and the ramp aborting on
scheduler jitter (L082).

| Measure | Section 18 | Section 17 | Section 16 |
| --- | --- | --- | --- |
| Sustained events each second | **37,464** | 37,874 | 67,624 |
| One synchronous producer | **19,547** | 19,566 | 19,534 |
| **Burst offered, unpaced** | **39,714 each second, 0 refused** | Never offered | Never offered |
| Burst multiplier | **1.06, and the harness is still the slower half** | 1.03, paced | 0.71, paced |
| Point lookup, p50 and p99 | **41.0** and **42.0** ms | 1,063 and 1,775 | 103.0 |
| Aggregate, p50 and p99 | **104.0** and **120.3** ms | 1,057 | 102.0 |
| Items accepted against events committed | **440,522 and 440,522** | 438,866 and 438,866 | 543,994 |

### 18.1 The point lookup is 26 times faster and the aggregate is not

**1,063 milliseconds to 41.** The executor now asks the locator which segments
can hold an exact value instead of materialising the whole range. See L079.

The aggregate did not move and should not have: 104 milliseconds against 1,057
is the same work done on a store that is no longer fighting a backlog, and an
aggregate over a whole range genuinely reads the range.

**The finding this project carried since its first benchmark is gone.** Every
report until now said "a point lookup and an aggregate are the same latency,
because the executor scans the range either way". They are now 41 against 104,
and they were only ever the same for the wrong reason.

### 18.2 The burst multiplier, finally offered rather than paced

Eight unpaced producers offered **39,714 events each second and the system
refused none of them**. Three reports called this "not measured" and one called
it 0.71; all three measured a harness that was pacing itself. See L082.

**The multiplier is 1.06 and the honest reading is that the harness is still the
slower half**, but now the ceiling is known: 39,714 each second is what eight
unpaced Go producers generate on this machine, and TallyOwl absorbed all of it
with nothing refused. A number above the sustained rate with zero refusals is
the answer D10 asked for, and the next step for a larger one is more producer
machines rather than a different harness mode.

### 18.3 The steady state, measured for the first time

Every previous disk figure in this document described a store that had stopped
sealing when the load stopped, because nothing sealed an idle head. With the
background segmenter running, the store settled completely: **440,522 rows
accepted, 440,522 committed, 440,522 sealed, and 8 bytes left in the append
log.**

| Part | Bytes | For each event |
| --- | --- | --- |
| Catalog, mostly the tablet locator | 60,076,032 | **136.4** |
| Segments | 17,566,021 | **39.9** |
| Append log | 8 | 0.0 |
| The whole data directory | 77,642,061 | **176.3** |

**A sealed row costs 39.9 bytes, against the 39.75 the capacity envelope
predicted in section 12.** The segment format hits its design target almost
exactly, and it has now been measured end to end on a settled store rather than
derived.

**The catalog is 3.4 times the size of the data it indexes**, and that is the
number to work on. 48.6 of those bytes are the tablet locator, which holds one
entry for each value and segment pair while every event in this run carries a
unique `request_id`. Section 18.4 shows that user-grouped compaction takes the
locator down by an order of magnitude. The rest is receipts and manifests, and
receipts expire on the deduplication window.

**Against 243.9 in section 17.3**, which was the same workload measured while 45
percent of its rows were still in a 73 MiB append log. The difference is not a
saving; it is the difference between a settled store and one that had stopped
working.

### 18.4 Candidate segments for a high-cardinality lookup

`prototypes/locator-bench`, run at target scale. This is the row three reports
have carried as "not usefully measured", and it was measurable the whole time in
the prototype written for it.

| Layout | Density | Bytes for each pair | Locator size | Candidate segments, 30 days |
| --- | --- | --- | --- | --- |
| Scattered | 3,000 | 1.031 | 28.81 GiB | **3,000** |
| Sharded by user, 64 shards | 69.8 | 1.298 | 864.7 MiB | **70** |
| Sharded by user, 1,024 shards | 30.0 | 1.679 | 480.4 MiB | **30** |
| Hot 2 days scattered, cold 28 days user-grouped | — | — | 2.44 GiB | **228** |

**The last row is the one to build.** Grouping rows by end user during
compaction, which already rewrites cold segments, takes the locator from 28.81
GiB to 2.44 and the candidates from 3,000 to 228, and it needs no change to
ingest routing — so it does not trade away the trace locality that sharding by
end user would.

A 32-bit fingerprint would send every lookup to more than a million extra
segments at 100 million values; 48-bit expects 18 collisions and 64-bit expects
none. A collision costs one wasted segment open and never costs correctness,
because the segment index verifies the full value.

## 19. Measured, 2026-08-05 — what replication costs

Phase 7 put a consensus group in front of the head's write path. This is what
that costs, measured rather than argued.

**Method.** The same machine, the same filesystem, the same harness, the same
seed, `--release`, each run from an empty `data/` with a Corndogs the run
started. **One voter**, because that is what one machine can hold honestly: the
group elects, appends, fsyncs, commits, and applies exactly as a three-voter
group does, and it does not pay a network round trip. **A three-voter number is
therefore not in this section**, and nothing here should be read as one.

**Hardware.** AMD Ryzen 7 5800X, 8 cores and 16 threads, 125 GiB of memory.
**Filesystem.** ext4 on an NVMe device, `/dev/nvme1n1p2`. Not tmpfs.

### 19.1 The collector accept path does not move, and should not

| Measure | Not replicated | Replicated |
| --- | --- | --- |
| Sustained events each second | 45,485 | 49,248 |
| One synchronous producer | 19,516 | 18,013 |
| Burst offered, unpaced | 52,347, none refused | 58,436, none refused |
| Point lookup, p50 and p99 | 41.0 and 42.0 ms | 41.0 and 42.0 ms |
| Aggregate, p50 and p99 | 105.0 and 126.1 ms | 95.0 and 128.0 ms |
| Items accepted against events committed | 479,230 and 479,230 | 488,464 and 488,464 |

**Both runs reached the ceiling of the ramp without failing**, so neither
sustained number is a saturation point and the difference between them is not a
result. What the table shows is that replication is behind the collector and the
collector does not feel it, which is the design: a collector acknowledges a
durable Corndogs write and knows nothing about a tablet.

**The point lookup is identical to a tenth of a millisecond.** A read is
answered from the local replica and never asks consensus anything.

### 19.2 The disk cost, which is the finding

**The first measurement said sixteen times, and part of that was a defect.**
L096 found it: serde encodes a `Vec<u8>` as a sequence, so a derived
`Serialize` wrote one CBOR integer for each byte of every batch — and a batch
travels inside two such fields, the batch inside the command and the command
inside the log entry, so it was doubled twice. The fix is a byte string.

Both runs below are the full load harness, the same seed, `--release`, from an
empty `data/`, one voter, drained to completion before the directory was
measured.

| Measure | Not replicated | Replicated, before L096 | Replicated, after L096 |
| --- | --- | --- | --- |
| Events | 479,230 | 488,464 | 524,797 |
| Batches | 2,785 | 2,817 | 2,963 |
| Whole `data/head`, bytes for each event | **214.6** | 3,577.2 | **1,034.2** |
| Of which the consensus log | none | 3,341.4 | **798.3** |
| Of which the store | 214.6 | 235.8 | **235.9** |
| Consensus log, bytes for each batch | none | 579,388 | **141,389** |

**The log is 4.2 times smaller and the store did not move**, 235.8 against
235.9, which is what says the change was in the encoding and nowhere near the
data.

**A replicated tablet still needs about five times the disk of an unreplicated
one**, 1,034.2 against 214.6, and the excess is still the log rather than the
data. What remains is not a defect:

- **a raft entry is the whole batch.** It has to be, until a lagging replica can
  catch up some other way;
- **nothing purged it.** A raft log is purged behind a snapshot, TallyOwl's
  tablet snapshot deliberately carries the marks and not the rows, and the copy
  that would replace it — a sealed-segment transfer — is built as an interface
  and is not driven by anything. The run never reached the 8,192-entry snapshot
  threshold either, at 2,963 entries;
- **redb amplifies.** `crates/tallyowl-cluster/tests/amp.rs` decomposes it: the
  consensus directory is 2.7 times the commands it was given, from a
  copy-on-write B-tree, a durable commit for every append and every apply, and a
  file that grows and does not shrink.

L095 holds the three possible answers and why none is taken yet. Compression is
the next lever and it is not measured on real telemetry: on the deliberately
uniform fixture in `amp.rs` one batch compresses 36.8 times at zstd level 1,
**which is an upper bound and not a prediction**.

### 19.3 The head commits about a fifth fewer batches each second

The number that says what consensus costs the write path is the head's
steady-state commit rate while a backlog drains. It is sampled from
`tallyowl_commits_total{outcome="committed"}` over a 90-second window.

**The first attempt did not resolve it**, and that is recorded rather than
hidden: two baseline samples taken while the machine was also running the Rust
test suite came back at 16.75 and 12.29 batches each second, which disagree with
each other by more than either disagreed with the replicated run. The pair below
was then taken on an otherwise idle machine, one run after the other, each from
an empty `data/`.

| Run | Batches each second | Events each second | Events for each batch |
| --- | --- | --- | --- |
| Not replicated | **16.19** | 2,403 | 148.4 |
| Replicated, one voter | **13.30** | 1,478 | 111.1 |

**About a fifth fewer batches each second: 13.30 against 16.19, which is 18
percent.** A second replicated sample taken earlier on a quiet machine gave
13.22, so the two replicated samples agree to within 0.6 percent, and the two
uncontended baseline samples — 16.19 and 16.75 — agree to within 3.5 percent.
The difference between the conditions is larger than the spread inside either
one, which is what makes it a result rather than noise.

**Read the batch rate and not the event rate.** Section 18.3 established that
the head's cost is one durable catalog transaction for each batch, so batches
each second is the measure that tracks the bottleneck. The two runs happened to
carry different numbers of events in each batch — 148.4 against 111.1 — so the
event rate moves further than the underlying cost does and would overstate what
replication costs.

**What the 18 percent is.** One extra durable append and fsync for each batch,
in the consensus log, before the store's own durable commit. At one voter there
is no network round trip in it at all, so **a three-voter number will be worse
than this and is not measured here**.

### 19.4 Reproducing

```sh
cargo build --release --workspace
rm -rf data run && ./tools.sh dev up          # not replicated
# or, replicated:
rm -rf data run && TALLYOWL_REPLICATION__LISTEN=127.0.0.1:5299 ./tools.sh dev up

cd testbed && go run ./cmd/load 127.0.0.1:5100 127.0.0.1:5110 \
    "$(cat ../data/collector.key)" "$(cat ../data/operator.session)"

# wait for the forwarder to drain, then
du -sb ../data/head ../data/head/consensus
```

The head's steady-state commit rate is sampled from
`tallyowl_commits_total{outcome="committed"}` and
`tallyowl_events_committed_total` on `127.0.0.1:5111/metrics`, over a 60-second
window while the backlog is draining.

## 20. Measured, 2026-08-05 — what bounding and compressing the log gave

Section 19 measured a replicated tablet at about five times the disk of an
unreplicated one and named the cause: the consensus log was a second full copy
of every batch, and **nothing reclaimed it**. L087 built the sealed-segment copy
that lets a lagging replica catch up another way, which made a purge safe; L099
then bounded the log and compressed each entry. This is what that gave.

**Method.** The same machine, the same filesystem, the same harness, the same
seed, `--release`, each run from an empty `data/` with a Corndogs the run
started, and each drained to completion before the directory was measured. **One
voter**, for the same reason section 19 gives: it is what one machine can hold
honestly, and it pays no network round trip. **A three-voter number is therefore
not in this section.**

**Hardware.** AMD Ryzen 7 5800X, 8 cores and 16 threads, 125 GiB of memory.
**Filesystem.** ext4 on an NVMe device. Not tmpfs.

### 20.1 The consensus log, which is what changed

| Measure | Replicated, section 19 | Replicated, now |
| --- | --- | --- |
| Events | 524,797 | 524,805 |
| Batches | 2,963 | 2,964 |
| Consensus log, bytes for each event | **798.3** | **36.6** |
| Consensus log, bytes for each batch | **141,389** | **6,480** |
| Whole `data/head`, bytes for each event | **1,034.2** | **259.2** |

**The log is 21.8 times smaller, and the whole data directory is 4.0 times
smaller.** A replicated tablet needed about five times the disk of an
unreplicated one; it now needs **1.27 times**, 259.2 against the 204.1 the
unreplicated run of the same harness measured on the same day.

### 20.2 What this run measures, and what it does not

**This is compression. The purge is not exercised by this run**, and saying so
matters more than the number does.

A group snapshots every 4,096 committed entries and keeps 512 after it, so a
tablet's log is bounded at about 4,608 entries however long the installation
runs. **This run reached 2,964 batches**, which is below the threshold, so no
snapshot was taken and nothing was purged. Every byte of the 21.8 times above
came from compressing the entry.

The bound is what changes the *shape* of the problem, and it is proved by a test
rather than by this run: `a_snapshot_seals_first_so_everything_it_covers_can_be_copied`
asserts that a snapshot seals the store first, which is the sentence that makes
a purge safe. Before it, a purge would have taken the only copy of entries a
lagging replica still needed.

**What that means for an operator.** A short-lived installation gets the
compression. A long-lived one gets the compression *and* stops the log growing
with its history, which is the larger of the two and is the one this measurement
cannot show.

### 20.3 The store did not change, and the two runs' sealing states differ

| Measure, bytes for each event | Not replicated | Replicated |
| --- | --- | --- |
| Whole `data/head` | 204.1 | 259.2 |
| Of which the consensus log | none | 36.6 |
| Of which the catalog | 164.8 | 114.5 |
| Of which the segments | 31.0 | 25.7 |
| Of which the append log | 8.3 | 82.4 |

**Do not read the last three rows as a difference replication caused.** The two
runs were stopped at different points in their sealing cycle: the replicated one
still held 82.4 bytes for each event in the append log against 8.3, and the
catalog and segment figures move against it because a sealed row moves from one
to the others. The sum of the three is 204.1 against 222.6, an 8 percent
difference that is sealing state rather than a cost.

**The row that is a result is the consensus log**, because it is the one that
exists in one run and not the other, and 36.6 bytes for each event is what
replication now costs in disk.

### 20.4 The rest of the load run, unchanged

Neither the collector accept path nor the point lookup moved, which is what
section 19.1 established and what this confirms.

| Measure | Not replicated | Replicated |
| --- | --- | --- |
| Sustained events each second | 58,899 | 58,397 |
| One synchronous producer | 19,497 | 19,508 |
| Burst offered, unpaced | 48,157, none refused | 56,754, none refused |
| Point lookup, p50 and p99 | 40.99 and 42.01 ms | 40.99 and 42.03 ms |
| Aggregate, p50 and p99 | 104.0 and 120.2 ms | 167.0 and 185.0 ms |
| Items accepted against events committed | 519,182 and 519,182 | 524,805 and 524,805 |

**Both runs reached the ceiling of the ramp without failing**, so neither
sustained number is a saturation point and the difference between them is not a
result. **The point lookup is identical to a hundredth of a millisecond**: a read
is answered from the local replica and asks consensus nothing.

**The aggregate is 60 percent slower and that is the sealing state again.** An
aggregate reads its range, the replicated run held ten times as much of that
range in the append log rather than in segments, and an append-log read is the
least compact form the store has. Section 18.1 established the same thing from
the other direction. It is not a replication cost and it should not be read as
one.

**Nothing was lost and nothing was duplicated in either run**: 519,182 accepted
against 519,182 committed, and 524,805 against 524,805.

## 21. Measured, 2026-08-08 — what a busy append log reclaims

This is not a throughput number. It is the answer to one question: **does the
append log shrink while the installation is taking writes?** Before this run the
answer was no, and nothing measured it, because every earlier run measured a
settled store.

**Method.** `cargo run --release -p tallyowl-store --example wal_reclaim_measure
-- <seconds> <callers> <linger-us>`. Callers append a 192-byte payload in a tight
loop with no pause. One thread asks the log to reclaim everything it covers, in a
loop, which is what a seal does. `min_reclaim_bytes` is zero, so the amortisation
floor is out of the way and this measures the handover alone.

**Hardware.** AMD Ryzen 7 5800X, 8 cores and 16 threads, 125 GiB of memory.
**Filesystem.** ext4 on an NVMe device. Not tmpfs. The driver prints the
filesystem it ran on.

| Callers | Reclamations | Bytes taken | Still in the log |
| --- | --- | --- | --- |
| 6, before | **0** | 768,288 | 768,296 — **100 percent** |
| 6, after | 415 | 449,440 | 432 — **0.1 percent** |
| 12, after | 513 | 1,036,256 | 1,492 — **0.1 percent** |

**Zero is the result, not a rounding.** A reclamation stepped aside whenever a
group commit was in flight, and a committer keeps that role for as long as
callers keep arriving. On a log that never goes quiet there was no moment at
which a reclamation could run, so the file kept every byte the installation had
ever written, for as long as it stayed busy. Section 20 measured the consensus
log doing the same thing for a different reason and called it the whole question;
this is the same failure one layer down.

**What changed.** A reclamation says it is waiting instead of giving up, a
committer stands down as soon as its own frame is durable, and a new caller waits
rather than taking the role. See L132 and STORAGE.md section 5.

**The throughput column is deliberately absent.** This driver runs a reclaimer in
a tight loop with the amortisation floor removed, so it rewrites the whole file
hundreds of times in a run and the append rate it reports is a property of the
driver rather than of the store. A seal reclaims once for each segment it
publishes. Section 18 holds the numbers that mean something.

**What is still not measured.** How much of this reached the numbers in sections
18 and 20. Both ran against a head committing about 17 batches each second, which
leaves the log quiet often enough that reclamation got in, so the effect there is
somewhere between none and small. A long soak at the collector's rate would say,
and nothing has run one.

## 22. Measured, 2026-08-09 — Phase 11: the replicated path end to end, and alert scale

Hardware and filesystem: the same development machine as sections 18 to 21,
ext4 on real storage, with every process of the soak topology — one Corndogs,
three head voters, two collectors, and the driver — sharing one device. That
sharing is the caveat on every number here: a real cell spreads the fsync load
these numbers pay in one place.

The topology is the cross-cluster soak from `./tools.sh soak up`: three voters
under `local-quorum`, collectors delivering to the first head, and the
proposal forward from L149 carrying writes from whichever voter takes them to
whichever voter leads.

| Measure | Measured | What it means |
| --- | --- | --- |
| Replicated end-to-end delivery | **about 7 batches each second** | Collector claim, forward to the leader, a quorum commit, and the task completed. Measured from the delivery counters over a minute while the queue was level. One machine's number; see the caveat above |
| Sustained soak rate | **500 events each second, queue depth level at 2 to 4** | With the driver's linger at two seconds, so a paced producer fills real batches. The same rate under the default 100 ms linger produced about 80 small batches each second and the queue grew without bound: the ceiling is batches, not events |
| Alert evaluation, one permit, under the soak's load | **0.6 evaluations each second** (80 in 132 s) | Each evaluation is a whole trend query, about 1.6 seconds under this load. A thousand one-minute rules ask for 16.7 each second, so one permit sustains about thirty-six such rules. L154 |
| Ingest while alerting was saturated | **unchanged**: 500 events each second, delivery depth 4 | The L144 pool bounding what alerting takes, observed from the other side |
| Snapshot of a settled 6,000-event store | **0.03 s** | `./tools.sh drill dr`, release build, verified run |
| Restore, plus head start to ready | **0.2 s + 0.1 s** | The same drill. The store held everything acknowledged before the snapshot, the queue replayed 340 of the post-snapshot events, and the 1,280 lost are exactly procedure 3's documented loss |

**A correction to sections 17 and 18.** The point-lookup latencies there were
measured against a harness that sent `request_id` as a client property, and a
field reference on that name reads the envelope's request column — which those
events left empty. The 41 ms p50 is therefore the cost of a locator miss, not
a hit. The harness now writes the envelope column (L153); the lookup numbers
need re-measuring before anything cites them.

**What the soak adds that a table cannot.** The numbers above are its first
hours. The thing it exists for is days at this concurrency against L131, with
a journal, an outage schedule, and per-window reconciliation; its report is
`./tools.sh soak report`.

**The overload run** (`./tools.sh drill overload`, home profile, same
machine): four unpaced producers offered about 43,000 events each second for
sixty seconds — 2.57 million events — and intake absorbed every one. Peak
resident memory across the head and the collector: 284 MB. Disk for the run:
115 MB. The backlog peaked at 9,409 batches on the queue's disk and drained
at about fifteen batches each second once the offer stopped. No refusal was
reachable: intake outruns four local producers, so the drill's verdict is the
absence of silent loss rather than the presence of a refusal. L159.

## 23. Measured, 2026-08-10 — the locator merge, against a soak-aged catalog

**Filesystem: ext4 on NVMe, the same device the soak writes.** The subject is
a byte-identical copy of head-3's catalog, taken while the soak's own outage
schedule held that voter down, after 24 hours of three-voter load at 500
events each second. The copy holds **1,455 stored locator runs, 86,264,831
entries, 2.76 GB** across two day buckets. The probe is
`measure_locator_against_an_aged_catalog` in `crates/tallyowl-store`; run it
with `AGED_CATALOG_DIR` pointing at such a copy.

| Measurement | Result |
| --- | --- |
| Decode every stored run | 5.3 s |
| **Combine, sealing once per bucket** (`Locator::from_all_runs`, L168) | **14.2 s** |
| Combine, one merge per run (what every build before L168 did) | **251 of 1,455 runs at the 180 s cap** |
| The old combine, extrapolated to completion | quadratic in runs: roughly **1.7 to 2.8 hours** |
| `Catalog::locator_bytes()`, the sampler's new read | 1.25 s, cold cache |

The extrapolation matches what the soak observed directly: watcher threads
and exact lookups wedged inside `Catalog::locator()` for hours on all three
heads at once (L164, L165), because every caller re-ran the quadratic
combine from scratch. After L168 a call costs one 14-second combine, the
store caches the result per manifest generation so repeated lookups pay
nothing, and the metrics sampler no longer combines at all.

**What this does not measure.** The 14.2 s is still the price of a cache
miss on a catalog this size, paid by the first lookup after a generation
change; cold consolidation shrinks the entry count itself and its effect at
this scale is not yet measured. The 1.25 s sampler read repeats every ten
seconds; if that duty cycle matters on smaller machines, a slower cadence
for this one gauge is the obvious lever.

### 23.1 Measured, 2026-08-12 — the same catalog after consolidation, and the pass that had to be bounded first

The "after" column, without waiting for wall-clock age: a lab copy of this
section's exact subject — the aged-catalog before-image plus hard links of
the sealed segments it names — consolidated in place by the
`consolidation_probe` in `crates/tallyowl-store`, with the age threshold at
12 h because the copy's rows were 19 to 43 hours old. The threshold decides
when the pass is due, never what it does.

**The first attempt took the machine down, and that is the most important
measurement in this section.** The pass budget (`cold_group_batch_bytes`)
broke only *between* bucket groups, and a soak-aged day is one group: the
probe loaded it whole, reached **69.6 GB** of resident memory, and the
kernel's OOM killer ended it — and, through the session unit that also held
the running soak's processes, ended the 38.9-hour soak with it. The live
heads would have run the same unbounded load themselves at hour 48. The
budget now bounds what a pass takes from inside a group — smallest segments
first, converging over maintenance intervals — and a regression test holds
it. L171 carries the full account.

Bounded, the probe converged in about twenty minutes of passes:

| Measurement | Before (section 23) | After consolidation |
| --- | --- | --- |
| Stored locator runs | 1,455 | **2** (one per day bucket) |
| Locator entries | 86,264,831 | 84,998,630 |
| Segments | 999 | **168** at the 8 MiB target, 1.3 GB |
| **Combine** (`Locator::from_all_runs`) | **14.2 s** | **1.55 s** |
| Combine, one merge per run (pre-L168) | 251 of 1,455 runs at the 180 s cap | completes in 3.0 s |
| `Catalog::locator_bytes()`, cold cache | 1.25 s (warm) | 7.1 s (cold) |

**What the numbers say.** The combine — the cost that starved every
observer in L165 — fell another ninefold, because two sealed runs merge in
one pass of already-sorted data. The entry count barely moved, and honestly
so: the soak's rows carry a unique `request_id` each, and 29.2 million
unique values cannot merge; what consolidation removes is the repeated
(value, segment) pairs of the reused session values, about 1.3 million. The
grouped rewrite also compressed better than the scattered originals — the
same rows in 1.3 GB instead of their share of 2.1 GB. The `locator_bytes`
row is not a regression: it reads the stored run values end to end, 2.7 GB
either way, and the before-number was taken against a warm page cache.

**The trade this section leaves standing.** Section 24.1 measures the other
side: fewer, larger segments make a cold exact hit cost the target-size
read. `compaction.coldGroupTarget` and `compaction.coldGroupBatch` are
configuration now, so an operator tunes the locator's win against the
lookup's price. The owner set the defaults on 2026-08-12 from these two
sections together: a 2 MiB target, so a cold hit costs a 2 MiB read instead
of an 8 MiB one, and a 32 MiB pass budget, because a 64 MiB bite measured
12 to 22 GB resident. Section 24.2 re-measures the lookups at the 2 MiB
target.

## 24. Measured, 2026-08-10 — the point lookups, re-measured as hits

This is the re-measurement L153 and section 22 required before anyone cites
the alpha point-lookup latencies again. Same machine and filesystem as
sections 18 to 23, home profile, release binaries with the L168 locator
program, from an empty `data/`. One caveat: the soak — three head voters at
500 events each second — ran on the same device throughout.

The harness now writes the correlation value through the envelope column
(`WithRequest`, L153), and it reports
`point_lookups_that_found_nothing`, so a lookup that finds no rows can never
again pose as a fast one. Two probe controls select what a lookup targets:
`LOAD_LOOKUP_OFFSET` moves the probe past the counters every ramp step
reuses, and `LOAD_LOOKUP_WORKER` pins it to the one producer whose counter
tail was written exactly once.

The store under measurement: one full ramp, 286,658 events accepted, six
sealed segments, settled — `LOAD_QUERIES_ONLY=1`, run after the head's
commit log went quiet.

| Lookup target | p50 | p99 | Found nothing |
| --- | --- | --- | --- |
| A value stored **once** (offset 25,000, worker 0) — the shape the measure is for | **93.0 ms** | **103.0 ms** | 0 of 50 |
| A value stored in **about ten segments** (no offset; every ramp step reuses low counters) | 503 to 719 ms | 744 to 833 ms | 0 of 50 |
| A value stored **nowhere** (offset 90,000) — what the alpha report actually timed | 55.0 ms | 59.0 ms | 50 of 50 |

**What the numbers say.** The alpha report's 41 ms was a miss on an idle
machine; the same miss costs about 55 ms beside a running soak. A real
point lookup on one exact value costs about **93 ms** at p50 on this store.
And the cost of a hit grows with the number of segments that hold the
value — about ten segments cost about ten times what one does — which is
the per-lookup restatement of what user-grouped cold consolidation exists
to bound (L168): the locator names the candidate segments cheaply now, and
the segment reads are what remain.

The settled aggregate — a count by minute over the whole range — measured
734 to 863 ms at p50 over the same store. The 85 ms the same query answered
directly after the ramp is not a contradiction: most of the 287,000 rows
were still in the delivery queue, so the early query counted a store that
was mostly not there yet. A query taken before the store settles measures
the backlog, which is exactly why `LOAD_QUERIES_ONLY` exists (section 16).

### 24.1 Measured, 2026-08-11 — the same lookups, after cold consolidation

This is the "after" column L169's revisit asked for: the same table, the
same store, the same seed and harness, release binaries carrying the L169
maintenance-log fix, beside the same running soak. The store's segments
were about fifteen hours old, so `compaction.coldGroupAfter` was lowered
from its 48 h default to 12 h for this run. The threshold decides only when
the pass becomes due, never what it does. One pass ran at head start-up and
logged `consolidated_sources: 4, consolidated_outputs: 2` — the first
observed cold consolidation, and the first proof the L169 log line fires.
Six segments became two: 9.3 MB and 0.9 MB, against the 8 MiB
`cold_group_target_bytes` target.

| Lookup target | Before, p50 (section 24) | After, p50 | After, p99 | Found nothing |
| --- | --- | --- | --- | --- |
| A value stored **once** | 93.0 ms | **976 to 985 ms** | 1,325 to 1,382 ms | 0 of 50 |
| A value stored in **about ten segments** | 503 to 719 ms | **1,021 ms** | 1,452 ms | 0 of 50 |
| A value stored **nowhere** | 55.0 ms | 66 ms | 79 ms | 50 of 50 |

**What the numbers say.** Consolidation made the hit costs converge —
upward. A miss costs what it did, so the fixed overhead did not move. Both
hit shapes now cost about one second, which is the price of reading the one
9.3 MB segment nearly every row now lives in. Section 24 measured a hit at
roughly 90 ms per roughly-1 MB segment read; the per-hit cost is linear in
the **bytes** of the segments a lookup reads, not only in how many segments
there are. Consolidating to the 8 MiB target therefore traded the
ten-segment lookup's 503 to 719 ms for about one second, and raised the
stored-once lookup tenfold. The settled aggregate moved from 734 to 863 ms
to about 1,151 ms; the miss and the aggregate drifted upward together,
which is the ambient cost of the post-roll soak beside the measurement, and
neither drift approaches the tenfold hit change.

**What this means for L168's argument.** The locator shrink is real and
section 23 measures it; the combine time and the entry count fall with the
segment count. But per lookup, consolidation helps only if the read path
can read less than a whole segment. Today it cannot: a hit decompresses
every candidate segment end to end, so fewer, larger segments cost a hit
more, not less. Until reads gain sub-segment granularity — or the target
shrinks — `compaction.coldGroupAfter`'s default should be judged by both
sides of this trade, not by the locator alone. L170 carries the open item.

### 24.2 Measured, 2026-08-12 — the lookups at the 2 MiB target, and the store that refused to consolidate

The owner set `compaction.coldGroupTarget` to 2 MiB and
`compaction.coldGroupBatch` to 32 MiB on 2026-08-12 (L172). The re-measure
ran against a fresh full ramp on the new defaults: 462,830 events, five
segments of 1.7 to 7.8 MB holding 15.7 MB, settled, with nothing else on
the machine — the soak retired the same morning, so this run is quieter
than sections 24 and 24.1 were.

**The store never consolidated, and that is the design working.** Its
15.7 MB need eight segments at the 2 MiB target and it holds five, so the
bucket is not due; the pass merges scatter and never splits an oversized
segment. A smaller target therefore also makes consolidation rarer: an
ordinarily-sealed store already sits at what its bytes need, and only a
genuinely scattered cold bucket — the soak's day of one-megabyte segments —
is ever rewritten.

| Lookup target | p50 | p99 | Found nothing |
| --- | --- | --- | --- |
| A value stored **once** (offset 35,000, worker 0) | **215 ms** | 232 ms | 0 of 50 |
| A value stored in **every segment** (no offset) | 1,207 ms | 1,270 ms | 0 of 50 |
| A value stored **nowhere** (offset 45,000) | 63 to 70 ms | 74 ms | 50 of 50 |

**One probe control aged out, and the empty count could not catch it.**
Section 24's once-stored offset, 25,000, measured 1,199 ms here — because
this ramp ran without a soak throttling it, its steps reused counters far
past 25,000, and that value now sits in most segments. The
`point_lookups_that_found_nothing` guard catches a probe that finds
nothing; it cannot catch one that finds too much. The once-stored band on
any store is a fact of its write history: here it is the prelude's
counters above every ramp step's reach (about 30,000 to 40,000), verified
by the misses starting at 45,000.

**The model, across every measurement so far.** A hit costs the bytes of
the segments it reads: 93 ms at about 1 MB (section 24), 215 ms at about
2.5 MB (here), about 980 ms at 9.3 MB (section 24.1), and about 1.2 s
reading all 15.7 MB (here) — 80 to 105 ms per megabyte throughout. The
2 MiB target prices a cold once-stored hit at roughly 200 to 250 ms,
which is the trade the owner chose; sub-segment read granularity remains
the lever if that ever needs to fall further.
