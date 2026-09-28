# Incident runbook

This runbook tells an operator what to do when a TallyOwl installation is
wrong. Each section starts from a signal, because an incident starts from one.

## 1. Readiness failed

`/readyz` answers 503 and names the check that failed. Each check has one
meaning. The head and the collector have different checks.

| Service | Check | Meaning | First action |
| --- | --- | --- | --- |
| Head | `storage` | The store cannot serve its job | Read the head log for the store's own message |
| Head | `ingest-listener` | The ingest listener did not start | Read the head log; the port may be taken. This check does not fail after a successful start |
| Head | `disk-space` | The device is at or inside the reserve | Free space, or grow the volume. Section 4 |
| Head | `append-log` | The append log refused a write | Section 3. This does not clear on its own |
| Head | `workflow-queue` | The head has not reached Corndogs yet. It fails until the first connection, and is degraded when the sweep fails later | Check Corndogs. The head connects again on its own |
| Head | `loop-<name>` | A background loop did not finish a pass in time. This is degraded, not failed | Read the head log for that loop |
| Head | `accepting-work` | The head is stopping | Nothing. It is a SIGTERM |
| Collector | `intake-listener` | The intake listener did not start | Read the collector log; the port may be taken |
| Collector | `retry-sweep` | The timeout sweep stopped | Nothing retries until it runs. Check the Corndogs connection |
| Collector | `durable-store` | Corndogs is unreachable | Intake refuses rather than accepting what it would lose. Restore Corndogs first |
| Collector | `delivery-loop` | The delivery loop did not turn | A peer did not answer. Section 5 and section 8 |
| Collector | `role-threads` | A role thread ended | Restart the collector. The check also stops liveness |

Your release can have more collector checks than this table. `/readyz` names
each one and says what it means.

**The head reaches Corndogs in the background.** If Corndogs does not answer
when the head starts, the head connects again with a growing wait, and
`workflow-queue` fails until it connects. Alert evaluation, notifications, and
the projector passes wait for that connection. This occurs most often at a
first install, when the head container starts before the Corndogs container
listens. It needs no action.

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

`tallyowl_integrity_failures_total` above zero means a checksum failed. The
segment is damaged: queries over its range answer `incomplete-result` and name
it. Restore the segment from a snapshot. See
[FAILURE_MODES.md](FAILURE_MODES.md) procedure 6.

**Nothing repairs a damaged segment automatically in this release.** The
design repairs it from another copy under `integrity.mode: scrub`. No scrub
runs in this release, and `integrity.mode` has no effect. See DEPLOYMENT.md
section 4.

## 8. Transport security

Section 7c of DEPLOYMENT.md gives the design. Each symptom here starts from a
signal.

### 8.1 A certificate for applications is about to expire

**Signal.** `tallyowl_tls_certificate_expiry_seconds` is low. The `listener`
label names `intake` or `otlp`. The rule `TallyOwlApplicationCertificateExpiring`
fires at 14 days.

**Action.** Put the new certificate in a second directory, and add it to
`tls.certificateDirectories` (on Kubernetes, to
`deployment.tls.certificateSecrets`). DEPLOYMENT.md section 7c gives the steps.
When the certificate expires, each app driver refuses the connection, keeps
its data, and reports "no trusted authority" or "not valid now".

### 8.2 A changed certificate file was not used

**Signal.** `tallyowl_tls_reload_failures_total` increases. The collector log
says "A certificate file changed and could not be used. The listener keeps the
certificate it had." The `reason` field gives the file and the cause.

**Action.** A certificate and a key that do not match is the usual cause. Put
the matching pair in the directory. The collector reads it on its next pass.

### 8.3 A node certificate is not renewed

**Signal.** `tallyowl_node_certificate_expiry_seconds` stays below one third of
`enrollment.certificateLifetimeHours`. `tallyowl_enrollment_failures_total`
increases. Its `reason` label gives the cause:

| Reason | Meaning | Action |
| --- | --- | --- |
| `unreachable` | The head did not answer | Section 5. The node continues until the certificate expires |
| `refused` | The head refused the role token or the renewal | The token is revoked, expired, used up, or outside its hourly rate, or the node was revoked. Make a new token, or remove the revocation |
| `handshake` | The TLS handshake failed | The head certificate does not chain to `installation.authorities`, or the clocks disagree. Compare the authority files on the head and the node |
| `invalid` | The head refused the request as not valid | The requested role, cell, or region is outside the token policy |
| `other` | Any other cause | Read the head log |

The node also writes a warning for each failed attempt, with the reason and
the message from the head: "This node could not get a certificate. It tries
again after a wait." The waits grow to five minutes, so a long outage writes
about twelve lines each hour.

**When the certificate expires.** A collector continues to accept batches into
Corndogs and stops delivery to the head. When it enrolls again, delivery
continues from Corndogs. A head whose certificate expired cannot reach its
peers, so its consensus groups lose that voter.

### 8.4 A peer is refused during the TLS handshake

**Signal.** A client reports one of these messages. The server counts the
refusal in `tallyowl_tls_handshakes_refused_total`. The `listener` label is
`intake` on a collector and `head-ingest` on a head. The server writes no log
line for it, because a stranger could write one line for each connection.

| Message at the client | Cause |
| --- | --- |
| "showed a certificate that no trusted authority signed" | The client does not trust the authority of the server. Add the root to `installation.authorities`, or to the app driver |
| "showed a certificate that is not for the name" | The server name does not match. A collector verifies a head as `head.tallyowl.internal`. An application verifies the collector as the address that it dials |
| "showed a certificate that has expired" | Section 8.1 or 8.3 |
| "showed a certificate that is not valid yet" | The clocks disagree. Check NTP on both hosts |
| "refused this connection during the TLS handshake" | The server did not accept the client certificate. The node has no certificate yet, or an authority that the server does not trust signed it |
| "closed the connection during the TLS handshake" | The listener serves plaintext on that address. Give it certificates, or set `transport.allowPlaintext` on both sides |

A handshake failure sends nothing, so an app driver keeps the batch and uses
no attempt.

### 8.5 A service cannot reach Corndogs over TLS

**Signal.** A collector stops at start with "We could not reach the durable
store at `<endpoint>` over TLS." and the reason from the Corndogs client. A
head writes the same reason each time it tries to connect, and its
`workflow-queue` check stays failed.

| Reason contains | Cause | Action |
| --- | --- | --- |
| `invalid peer certificate` (for example `UnknownIssuer` or `BadSignature`) | The Corndogs certificate does not chain to `corndogs.tls.caFile`, or to the system authorities when that setting is empty | Sign the Corndogs certificate with the installation authority, or point `corndogs.tls.caFile` at the authority that signed it |
| `certificate not valid for name` | The certificate does not carry the name that the service checks | The name is the host of `corndogs.endpoint`, or `corndogs.tls.serverName`. Add the name to the certificate, or set `corndogs.tls.serverName` |
| `tls handshake` with `Connection reset by peer` or a closed connection | Corndogs serves plaintext on that port | Give Corndogs a certificate (`corndogsDeployment.tlsSecret`), or set `transport.allowPlaintext` on the service |
| `timed out` | Corndogs did not answer within `corndogs.callTimeout` | Section 1. Check that Corndogs runs |

## 9. After every incident

Write down the signal, the cause, and the fix. If a signal was missing —
a state you needed and could not see — that is a defect in the system's
reporting, and it goes in the implementation log with the incident.
