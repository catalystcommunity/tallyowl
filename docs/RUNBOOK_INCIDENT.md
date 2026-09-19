# Incident runbook

This runbook tells an operator what to do when a TallyOwl installation is
wrong. Each section starts from a signal, because an incident starts from one.

## 1. Readiness failed

`/readyz` answers 503 and names the check that failed. Each check has one
meaning:

| Check | Meaning | First action |
| --- | --- | --- |
| `storage` | The store cannot serve its job | Read the head log for the store's own message |
| `ingest-listener` | The ingest listener stopped | Read the head log; the port may be taken |
| `disk-space` | The device is at or inside the reserve | Free space, or grow the volume. Section 4 |
| `append-log` | The append log refused a write | Section 3. This does not clear on its own |
| `retry-sweep` | The timeout sweep stopped | Nothing retries until it runs. Check the Corndogs connection |
| `durable-store` | Corndogs is unreachable | Intake refuses rather than accepting what it would lose. Restore Corndogs first |

A collector that lost the head stays ready on purpose: the last good policy
is in force and the queue holds the batches. That is an outage of the head,
not of the collector. Section 5.

## 2. The stall signature

L131 records an append-log stall that was found two days late because the
state was inside a lock. The state is published now. The signature is:

- `tallyowl_wal_commit_in_flight_count` is 1 across several samples, and
- `tallyowl_wal_durable_position_count` does not move across the same samples.

The head also writes one warning when the signature holds for about twenty
seconds: "A group commit has been in flight without making the append log
durable...". The log field beside it carries the whole append-log state.

Do this, in order:

1. Do not restart anything. A restart destroys the evidence and the next
   occurrence starts the search again.
2. Save the warning line and the `append_log` field from the head log.
3. Save `/metrics` from the stalled head.
4. Take every thread's stack. Yama permits a tracer that is an ancestor, so
   attach as root, or start the suspect binary under `gdb --args` next time.
   `docs/IMPLEMENTATION_LOG.md` L132 records the method.
5. Then recover: restart the head. Nothing acknowledged is lost — recovery
   replays the append log.

The signature means a committer took a group and has not come back. L132
proved the wait cannot be inside `wal.rs`, so the stacks are the evidence
that finds the lock outside it.

**Move fast: the observed episodes clear themselves.** The soak recorded
three episodes on one follower voter in one night, 60 to 210 seconds each,
and every one recovered on its own (L163). Attach while the durable position
is still frozen, because a cleared episode leaves nothing to attach to. No
data was lost in any observed episode: the other voters kept the quorum.

**The signature has a second face, and it fooled this runbook's own soak.**
The gauges above are written by one watcher thread. When that thread itself
stalls, every sampled gauge freezes at its last value — sometimes with
`commit_in_flight` at one — while the head keeps committing. Tell the two
apart with `tallyowl_commit_watermark_count`, which the commit path writes
inline: a rising watermark under a frozen durable position means the
observer is stalled, not the append log. Both are worth stacks; they are
different defects. L164 and L165 record the day this happened on all three
heads at once, and the cause: a query-cost defect in the locator, not a
lock. The gauges' freshness is itself a signal.

## 3. The append log refused a write

A refused append-log write stops the log on purpose. Every later write is
refused, readiness carries the reason, and no caller is told a refused write
was durable. See [FAILURE_MODES.md](FAILURE_MODES.md) section 10.

The cause is the device: full, read-only, or failing. Fix the device. Then
restart the head, and recovery replays the complete frames.

## 4. The device is filling

`storage.reserveBytes` holds space back so recovery can still write. When the
`disk-space` check fails, the device is inside the reserve.

1. Read `tallyowl_segment_bytes`, `tallyowl_wal_bytes`, and the Corndogs
   directory size, so you know which of the three is growing.
2. A growing delivery queue means the head is behind or away. Section 5.
3. A growing segment store is retention doing its job. Shorten the retention
   classes, or grow the volume.
4. Do not delete files by hand. A segment the catalog names and cannot open
   becomes a damaged segment.

## 5. The head is away and the queue is growing

The collectors keep accepting: acknowledgement means the queue took the
batch durably. The bound is time, not depth — a batch older than
`corndogs.maxDeliveryAge` goes to quarantine rather than being retried past
the head's deduplication window.

1. `tallyowl_delivery_queue_depth_count` says how much is waiting, and
   `tallyowl_delivery_oldest_waiting_ms` says for how long.
2. Restore the head before the oldest batch reaches
   `corndogs.maxDeliveryAge`. The default is 24 hours.
3. After recovery the queue drains with jittered backoff, so the head does
   not meet the whole backlog in one second.
4. Credential resolution has its own window: a key answer the collector
   already held keeps working for `collector.keyCacheGrace`. An outage longer
   than that refuses new batches from sources whose key answers expired. Set
   the grace to cover the outage window you intend to hold.

## 6. Quorum

A cell with three voters survives one. `docs/FAILURE_MODES.md` section 11
gives the procedures:

- one voter lost, quorum survives: replace the node and let it catch up.
  Automatic.
- quorum lost permanently: restore from a snapshot (procedure 3), or unsafe
  recovery from the survivor (procedure 4). Unsafe recovery can lose
  acknowledged writes and marks the range degraded.

A follower that takes writes forwards them to the leader, so ingest works on
every voter. Two nodes that disagree about the leader produce a retryable
refusal, and the forwarder's next attempt asks whoever leads by then.

## 7. Integrity failures

`tallyowl_integrity_failures_total` above zero means a checksum failed. With
another copy, the tablet repairs from it under `scrub`. With one copy, the
segment is damaged: queries over its range answer `incomplete-result` and
name it. Restore the segment from a snapshot. See
[FAILURE_MODES.md](FAILURE_MODES.md) procedure 6.

## 8. After every incident

Write down the signal, the cause, and the fix. If a signal was missing —
a state you needed and could not see — that is a defect in the system's
reporting, and it goes in the implementation log with the incident.
