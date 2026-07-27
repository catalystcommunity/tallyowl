# Delivery and failure semantics

## 1. Contract

TallyOwl provides durable at-least-once delivery after a declared durability
boundary. Stable event and batch IDs make retries logically idempotent.
TallyOwl does not claim exactly-once network delivery.

There are three distinct promises:

| Mode | Success means | Intended use |
| --- | --- | --- |
| Browser event | The host application's CSIL carrier accepted the frame | Ordinary behavior events |
| Durable collector receipt | Corndogs persisted the item or batch to the configured number of durable copies | Errors, conversions, backend events, spans, metric snapshots |
| Final storage receipt | The tablet satisfied its configured `local-one`, `local-quorum`, `remote-one`, or custom policy | Collector task completion |

The app driver uses the durable collector receipt for every batch. A helper that
returns `Ok` must mean the collector durably accepted it into Corndogs, not
merely that an in-process channel or socket send accepted bytes. Browser helpers
remain constrained by the browser-to-app carrier and cannot make that backend
durability claim unless the host deliberately exposes a correlated
critical-event operation.

`durable` means that the configured boundary has stored the data. It does not
give a fixed outage-survival guarantee.

`durable_copies` gives the necessary number of Corndogs copies. The default is
`1`. This value means one fsynced copy and no redundancy.

The Corndogs clustered file backend counts follower acknowledgements. TallyOwl
sets `ack_count = durable_copies - 1` for this backend.

A higher value needs more healthy replicas. It can increase latency and decrease
write availability.

`durable_copies` depends on the configured Corndogs backend:

| Backend | Supported values | Note |
| --- | --- | --- |
| file, single replica | `1` | The shipped default. One fsynced copy. |
| file, clustered | `1` to replica count | The clustered backend is a Corndogs design. It is not implemented. |
| postgres | `1` | PostgreSQL supplies redundancy. `ack_count` does not apply. |

TallyOwl refuses a value that its backend cannot satisfy. It fails at startup.
It does not accept the configuration and then acknowledge a weaker guarantee.

The file backend must use the `group` or `always` fsync mode. The `interval` and
`never` modes acknowledge writes that a power loss can destroy. TallyOwl
refuses to start against those modes.

## 2. Stable identifiers

- `event_id`: UUIDv7 generated once at the original producer.
- `batch_id`: deterministic or persisted UUID generated once when an app driver
  seals a batch. The collector preserves it through Corndogs and final storage
  retries.
- `source_id`: head-issued stable identity for a collector and source.
- `sequence`: monotonic per producer process and session when available, used to
  diagnose gaps but not as a global ordering guarantee.
- `attempt`: Corndogs workflow metadata, not part of logical event identity.

Retries preserve `event_id` and `batch_id`. Rebatching after a permanent partial
rejection creates new batch IDs but preserves event IDs.

A browser supplies an untrusted `event_id`. A client that sends an ID early can
suppress a later legitimate event with the same ID. Therefore:

- deduplication uses project scope, never installation scope;
- batch deduplication uses `(source_id, batch_id)`;
- the app driver namespaces or replaces a browser-supplied `event_id` before it
  seals a batch;
- a server-generated `event_id` is authoritative when both exist.

## 3. Intake sequence

```text
App driver               Collector intake          Corndogs
    │ persistent TCP          │                        │
    │ submit(batch, id)       │                        │
    ├────────────────────────►│ validate + limit       │
    │                         ├─ submit task ─────────►│
    │                         │◄──── durable ack ──────┤
    │◄─ accepted(batch, id) ──┤                        │
    │ discard local batch     │                        │
```

The collector never returns a durable receipt before Corndogs acknowledges the
task at the configured `durable_copies`. For the single-node file backend this
also requires a Corndogs fsync mode that makes acknowledged writes durable.
The collector applies validation and hard limits before enqueue so poison
payloads do not consume the delivery queue.

Corndogs is the intermediate durable handoff. The collector does not need a
second journal for the same payload.

The collector completes the task after the final storage receipt. Corndogs can
remove the task according to queue retention.

Interim queue data is not permanent analytics history.

The app keeps one reconnecting CSIL-RPC byte-stream connection open to the
collector and may pipeline a configured number of correlated batch calls. Each
batch keeps the same stable ID across connection loss and retry. The collector
acknowledgement gives that ID, the accepted count, and the queue acceptance
time.

A lost acknowledgement can cause a duplicate Corndogs task. Corndogs does not
have a TallyOwl batch-id idempotency key.

Final storage deduplicates the stable batch ID. Thus, the result has one logical
commit.

The first app-driver defaults are:

- Seal a batch at 256 items.
- Seal a batch at 512 KiB.
- Seal a batch after 100 milliseconds.
- Use a 1 MiB maximum ordinary frame.
- Use a 4 MiB maximum exceptional frame.
- Permit 8 MiB of unacknowledged data on one collector connection.
- Seal the current batch when a critical event occurs.
- Use a shutdown flush deadline of 2 seconds.

All values are configurable. Retries use capped jitter while the process is
alive.

For browser events, the application's generated route performs only cheap
validation and hands the value to the app driver. The driver may batch
many such events before the collector call, but it cannot report them durable
until the Corndogs-backed collector ack. The host decides whether its browser
operation waits for that ack or remains best effort. It never blocks on the
remote head.

## 4. Collector Corndogs workflows

The native intake path uses one durable task, avoiding a second handoff between
an intake queue and a delivery queue.

### `tallyowl-delivery`

```text
queued → normalizing → sending → committed → complete
  ▲          │           │
  │          │           ├── transient failure ─────┐
  │          │           └── permanent invalid ──► quarantined
  └──────── timeout/retry/backoff ──────────────────┘
```

Collector intake creates this task and returns the app acknowledgement only
after Corndogs durably accepts it in `queued`. A forwarder claims and advances
the same task; it never completes an intake task merely because it created a
second delivery task.

`normalizing` and `sending` have timeouts and target state `queued`. Thus, a
failed worker releases the task.

### Backoff and the timeout sweep

Corndogs expresses a delay with the task timeout and the state swap. To wait
before a retry, the worker parks the task and names the ready state as the
automatic target state:

```text
UpdateTask(uuid,
           new_state = "backoff",
           auto_target_state = "queued",
           timeout = <backoff seconds>)
```

Corndogs moves the task back to `queued` when the timeout expires. No worker
holds a claim during the wait, and no component enumerates tasks. The worker
does not make a hot loop.

Corndogs evaluates a timeout only when a caller invokes `CleanUpTimedOut`.
Nothing happens without that call. Therefore:

- the forwarder role owns the sweep for each queue it serves;
- the default sweep interval is one second and is configurable;
- forwarder readiness fails when the sweep stops;
- a stopped sweep stops retry, backoff, and dead-worker recovery at the same
  time, so the health metric for the sweep is a required indicator.

The sweep cost is backend-specific. The file backend examines every live task.
Its cost grows with queue depth, and queue depth grows during a head outage.
Measure the sweep at the outage-buffer depth that the capacity work selects.
See D33.

A forwarder may coalesce compatible tasks into one final-storage batch. The
final batch ID is stable for that attempt and every source event ID remains
stable. All contributing Corndogs tasks complete only after the head receipt.
If a crash changes coalescing on retry, event-level deduplication still prevents
logical double counting.

### `tallyowl-quarantine`

Limits control quarantine size. Operators can see quarantine entries. Entries
contain safe diagnostics, schema version, and source.

An entry contains the rejected payload only when policy permits it.
Quarantine expiry is finite. Releasing an entry returns the same task to
`queued`, or creates a replacement before completing the original, while
preserving event IDs.

Corndogs currently provides task state, atomic claims, timeout state swaps,
priority, and durable backends. TallyOwl must implement retry counts, next
attempt time, and batch lineage in its task payload until Corndogs grows a
native scheduling contract. That does not require changing CSIL.

## 5. Head commit sequence

```text
Collector forwarder   Head ingest             Storage tablet
    │ submit(batch_id)     │                         │
    ├─────────────────────►│ authenticate/scope      │
    │                      │ validate/decompress     │
    │                      ├─ append batch ─────────►│
    │                      │  satisfy receipt policy │
    │                      │◄──── durable result ────┤
    │◄─ committed receipt ─┤                         │
    │ complete task        │                         │
```

The receipt includes:

- batch ID;
- accepted and rejected event counts;
- committed timestamp;
- server protocol and projector version;
- explicit per-event rejection entries for bounded partial failure;
- retry classification for the whole response.

The collector completes a Corndogs task only after a valid committed receipt.
If the connection drops after the storage tablet commits but before the receipt
arrives, the collector retries the same batch ID. The durable receipt index
returns the prior commit without creating a second logical batch.

## 6. Deduplication reality

Physical duplicates can exist after pathological retries, token-window expiry,
manual replay, or disaster recovery. Therefore:

- primary queries count logical `event_id`, not physical rows, where duplicates
  would alter results;
- retain batch IDs for at least the maximum retry and replay period, where
  `dedup_window >= max_outage_buffer + max_replay_window + safety margin`;
- checkpoint batch receipt records with the storage log;
- automated retries stop before the dedup window expires and require an
  explicit replay workflow afterward;
- test projection and rollup designs with duplicate raw rows;
- operators can inspect dedup hits and suspected duplicate rates.

A materialized rollup must not count duplicate insert attempts. Use a versioned
projection or a deduplicated aggregate state.

The dedup window and the collector outage buffer are one decision. A collector
that can hold a longer outage than the head can remember creates duplicates
that no query can remove. See D36.

## 7. Ordering

TallyOwl does not promise global order. It preserves:

- timestamps from the producer;
- receive and commit timestamps from trusted infrastructure;
- sequence within a producer session when supplied;
- parent and child relationships for traces;
- touch and conversion IDs for attribution.

Queries select event-time or receive-time semantics. TallyOwl measures and
marks clock skew.

Late events remain in raw storage. They can require rollup correction.

## 8. Backpressure and overload

Each boundary has limits:

- browser memory buffer: count and byte cap, oldest and newest drop policy by kind;
- app driver queue: count and byte cap and short send deadline;
- collector intake: frame, event, connection, and source quotas;
- Corndogs: disk and row budget and oldest-age alarms;
- sealed batch: compressed and uncompressed byte and event caps;
- head: per-key rate, concurrent batch, and decompression limits;
- query: time, rows, bytes, memory, and concurrency limits.

Priority defaults:

1. conversions and explicitly critical business events;
2. unhandled errors and sampled root traces;
3. metric snapshots and handled errors;
4. ordinary behavior events;
5. high-volume diagnostic spans.

When storage pressure becomes critical, collectors reject new durable intake.
They do not acknowledge data that they cannot retain.

Best-effort paths can sample or drop data according to policy. Each service
counts its local drop reasons.

When the app driver reaches its unacknowledged byte and count bound, durable sends
block until their deadline or return an explicit typed backpressure error. They
never silently become best effort and never return success for discarded data.

## 9. Failure matrix

| Failure | Required behavior |
| --- | --- |
| Browser closes | Unsent browser-buffered events may be lost; no false durable claim |
| App process dies | Durable receipts already returned remain safe at collector; in-process handoffs may be lost |
| Collector intake replica dies | Client retries durable RPC; stable event IDs prevent logical duplicates |
| Collector loses head connectivity | Corndogs retains tasks; workers retry with jitter |
| Collector disk nearly full | Shed best effort, reject new durable intake, emit health alert |
| Worker dies while sending | Corndogs timeout returns task to `queued` |
| Head rejects auth | Permanent failure; quarantine safe metadata and stop retry storm |
| Head rate limits or unavailable | Retryable receipt or status with backoff |
| Head commits then connection drops | Retry same batch ID; dedup and return receipt |
| Some events invalid | Commit valid subset only under explicit partial policy; split or quarantine invalid IDs |
| Unsupported schema version | Quarantine and surface upgrade requirement |
| Storage tablet unavailable | The head must not acknowledge until the tablet satisfies its receipt policy |
| Projector unavailable | Raw ingest continues; lag alert; projections catch up |
| Control catalog unavailable | Existing collector auth may use a very short safe cache; dashboard mutations fail closed |
| Policy fetch fails | Collector uses last-known-good policy and reports staleness |
| Corndogs unavailable at intake | Intake readiness fails; refuse new batches; never acknowledge |
| Corndogs disk budget exhausted | Reject new durable intake; alert; keep queued data |
| Timeout sweep stops | Forwarder readiness fails; retry and recovery stop together |
| Erasure lands while data is queued | The erasure predicate stays active and hides the late arrivals |
| Late batch older than its retention | Accept, then apply retention at projection; never acknowledge and discard |
| Producer clock ahead of the collector | Keep producer time; mark skew; queries can select receive time |

## 10. Shutdown and upgrades

- App drivers stop accepting, drain bounded queues until deadline, then report
  unsent counts.
- Collectors stop intake, finish or release claimed tasks, flush receipts, and
  leave queued data durable.
- Head ingest stops readiness before draining in-flight commits.
- Rolling upgrades support adjacent protocol versions and mixed projector
  versions.
- Schema migration order is expand → deploy readers and writers → backfill → verify
  → contract.
- Helm disruption budgets must preserve intake and query capacity; they do not
  override safe draining.

## 11. Delivery tests

The implementation is not complete without automated tests for:

- process kill before and after every acknowledgement boundary;
- commit success and receipt loss duplicate retry;
- Corndogs claim timeout and worker replacement;
- head unavailable for longer than normal retry intervals;
- partial rejection and deterministic batch splitting;
- disk-full and queue-budget behavior;
- poison payload quarantine without retry loops;
- protocol version skew during rolling upgrades;
- duplicate events through every projection and rollup;
- clock skew and late-event correction;
- API key overlap, expiry, and immediate revocation.
