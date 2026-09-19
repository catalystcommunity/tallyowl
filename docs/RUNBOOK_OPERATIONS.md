# Operations runbook

This runbook tells an operator how to run a TallyOwl installation day to day.
[RUNBOOK_INCIDENT.md](RUNBOOK_INCIDENT.md) covers the moments when something
is wrong.

## 1. The processes

A home installation runs three processes: one Corndogs, one head, and one
collector. A replicated cell adds head nodes as storage voters. The charts in
`charts/` install both shapes. See [DEPLOYMENT.md](DEPLOYMENT.md).

| Process | Data port | Health and metrics port |
| --- | --- | --- |
| Corndogs | 5080 | — |
| Head | 5110 | 5111 |
| Collector | 5100 | 5101 |
| Dashboard | 5120 | — |

The health and metrics port serves three paths: `/livez`, `/readyz`, and
`/metrics`. It serves nothing else.

## 2. Daily checks

1. Read `/readyz` on the head and on each collector. A ready process answers
   200.
2. Read `/metrics` and compare these values with yesterday:
   - `tallyowl_delivery_queue_depth_count` — batches that wait for delivery.
     A small number is normal. A number that grows for hours is not.
   - `tallyowl_delivery_oldest_waiting_ms` — how long the oldest batch has
     waited. Zero beside a positive depth means a collector restart lost the
     age. The next claim finds it again.
   - `tallyowl_batches_quarantined_total` — batches that need a person.
     Section 5 says what to do.
   - `tallyowl_integrity_failures_total` — must stay zero.
   - `tallyowl_storage_refusals_total` — writes the store refused, by cause.
3. Read the disk use of the data directory and of the Corndogs directory.

## 3. Keys and sessions

Make a project and its first key:

```sh
tallyowl-head --config <file> provision <project>
```

The key is printed one time. Store it as a secret. Give the collector a
reference to it (`file:` or `env:`), never the value.

Keep more than one active key for each source. This permits rotation without
a coordinated cutover. Revoke a key with `tallyowl-head key revoke <key-id>`.
A revocation reaches collectors within `collector.keyCacheTtl`.

## 4. Backups

Take a snapshot on a schedule:

```sh
tallyowl-head snapshot <directory>
```

Stop the head first. The command copies the stored files and the erasure
records. Run it again into the same directory to add only what is new.

Test the restore. A restore that nobody has run is not a backup. The drill
does the whole cycle and measures it:

```sh
./tools.sh drill dr
```

The drill reports the backup time, the restore time, and what a disaster
loses: everything committed after the snapshot that the delivery queue no
longer held. Choose the snapshot period together with the queue's
`corndogs.maxDeliveryAge`, because the queue is what recovers the gap.

## 5. The quarantine

A batch goes to quarantine when no retry can fix it: a payload that does not
decode, a permanent rejection, or an age past `corndogs.maxDeliveryAge`. The
dashboard's workflow table shows the count, and
`tallyowl_batches_quarantined_total` counts it.

Read the quarantine reason first. A quarantined batch holds safe metadata and
the reason it stopped. Replay it only when the cause is fixed, and know that a
replay after `storage.deduplicationWindow` commits a second logical batch.

## 6. Upgrades

Upgrade in this order. See [DEPLOYMENT.md](DEPLOYMENT.md) section 7.

1. Head ingest and query roles.
2. Storage nodes, one failure domain at a time.
3. Collectors, last.

Each step is a graceful restart. Head ingest stops readiness before it
drains. A collector finishes or releases its claimed tasks and leaves queued
data durable. The soak harness proves the procedure under live load:

```sh
./tools.sh soak roll
```

## 7. Scaling

A collector is stateless and scales horizontally. The collector chart carries
a `HorizontalPodAutoscaler` under `autoscaling.*`. Every replica must reach
the same Corndogs service.

The head does not autoscale. Adding a storage voter is a placement decision:
enroll the node, then add it as a learner, and the controller promotes it.
Query capacity grows with read replicas, which the `scaled` profile adds.

## 8. Watching the watchers

Alert evaluation, notification delivery, and the projector passes run on
durable queues. `tallyowl_workflow_pending_count` shows each queue's depth,
and the dashboard's workflow table shows lag, failures, and quarantine.
Nothing retries when the sweep stops, and readiness fails with it — so a head
or collector that is ready has a running sweep.
