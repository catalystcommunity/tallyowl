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
