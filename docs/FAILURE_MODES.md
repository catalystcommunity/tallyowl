# Failure modes and recovery

This document uses these abbreviations: input and output (IO), and remote
procedure call (RPC). [DOCUMENTATION.md](DOCUMENTATION.md) holds the full list.

## 1. Purpose

This document owns failure and recovery for storage, consensus, the catalog,
compaction, and erasure.

[DELIVERY.md](DELIVERY.md) section 9 owns the intake path, from a browser to a
committed batch. It is complete, and this document does not repeat it. The two
documents meet at the head commit.

Each failure below names what a person sees, what the system does without help,
and what an operator does. A failure with no recovery procedure appears in
section 12 as an accepted limit.

## 2. Principles

1. **Never claim durability that the system does not have.** An acknowledgement
   is a promise. Every rule below protects that promise before it protects
   availability.
2. **A failure that hides is worse than a failure that stops.** A silent wrong
   answer costs more than a refused query, because nobody investigates a wrong
   answer.
3. **Recovery is a written procedure, not an improvisation.** An operator
   recovering a system at 3 in the morning must not design the procedure.
4. **An operator chooses the cost.** Integrity checking, catalog snapshots, and
   slow-node handling all cost something. TallyOwl gives the choice and states
   the consequence. It does not force a cost that an installation does not
   want.
5. **A system that knows why it is unwell says why.** "Unknown reason" is an
   acceptable answer. A missing answer is not.

## 3. Failure domains

```text
  application ──1──> collector ──2──> head ──3──> tablet ──4──> segments
                         │                            │             │
                         5                            6             7
                         ▼                            ▼             ▼
                     Corndogs                     catalog      object store
```

| Domain | Owned by |
| --- | --- |
| 1, 2 Intake and forwarding | [DELIVERY.md](DELIVERY.md) section 9 |
| 3, 4 Commit and consensus | Section 6 |
| 5 Durable queue | [DELIVERY.md](DELIVERY.md) section 9 |
| 6 Catalog | Section 7 |
| 7 Segments at rest | Section 5 |
| Compaction across 4, 6, 7 | Section 8 |
| Erasure across 4, 6, 7 | Section 9 |

## 4. What already protects the system

These are not gaps. They appear here so that a reader knows what section 5 and
later add to, rather than replace.

| Protection | Where |
| --- | --- |
| Stable batch and event IDs, so a retry is idempotent | DELIVERY.md section 2 |
| Retry with capped jitter, backoff, and a quarantine for a poison batch | DELIVERY.md section 4 |
| Timeout sweep that returns a claimed task to the queue | DELIVERY.md section 4 |
| Torn final frame truncated, complete frames replayed after the checkpoint | STORAGE.md section 5 |
| Receipt entries reconstructed during recovery | STORAGE.md section 5 |
| Checksums on every page, index block, footer, and log frame | SEGMENT_FORMAT.md section 10 |
| Content address over the whole segment payload | SEGMENT_FORMAT.md section 10 |
| Bounded decode allocation and a decompression ratio limit | SEGMENT_FORMAT.md section 2 |
| Readiness that fails when a durable store is unreachable | CONVENTIONS.md section 3 |

**Retry design is complete.** Nothing in this document adds a retry.

## 5. Integrity at rest

Checksums existed before this document. Nothing read them until a query touched
the data, so a cold segment could be damaged for its whole retention period
without anybody knowing.

### Three levels, and an operator selects one

| Level | `integrity.mode` | What it catches |
| --- | --- | --- |
| None | `none` | Nothing. The fastest read path. |
| Verify on read | `verify-on-read` | Damage in data that a query touches, at the moment it touches it. **The default.** |
| Scrub and repair | `scrub` | Damage anywhere, whether or not a query touches it. |

`verify-on-read` is the default because a wrong answer is the failure this
project most wants to avoid, and the cost is a checksum over bytes already in
memory. BENCHMARKS.md section 7 measured xxHash3 at 9,112 MB each second, far
above the device read rate.

`none` is a legitimate choice. An installation that wants the last of the read
throughput and accepts a wrong answer over a damaged page may select it. The
dashboard shows that integrity checking is off, because an operator who
inherits the installation must not have to discover it.

### What `scrub` adds

A background pass verifies every retained segment on a configurable period,
oldest first, at a configurable rate limit. It reads at low priority and yields
to query and ingest work.

On a failed checksum:

1. the segment is marked damaged in the catalog, and stops serving reads;
2. when another copy exists and passes its own check, the tablet replicates
   from that copy and clears the mark;
3. when no good copy exists, the segment stays damaged, and an alert fires that
   names the segment, its project, and its time range;
4. a query that would have read a damaged segment returns `incomplete-result`
   and names what it could not read. It never silently returns a smaller
   answer.

**At one copy there is no repair.** The home profile stores one copy, so
`scrub` there is detection only. That is still worth having: knowing on the day
it happens beats knowing during an incident. The alert says so in plain
language rather than implying a repair that cannot happen.

### Restore is the repair of last resort

A damaged segment with no good copy is recovered from a snapshot. See
section 11 procedure 6.

## 6. Consensus and node failures

BENCHMARKS.md section 10 measured the first four of these against a real
openraft cluster.

| Failure | Behavior | Measured |
| --- | --- | --- |
| Leader loss | A remaining voter wins an election and writes continue | New leader in 985 ms |
| Minority partition | The minority accepts no write and commits nothing. No split brain. | Blocked 3 s, no commit |
| Voter rejoins | It catches up from the leader's log | 528 ms |
| Membership change | A learner is added and promoted without a write outage | 4 ms each |
| Voter disk exhausted | The voter fails readiness and stops accepting appends. Quorum continues without it. | Not measured |
| **Node alive but slow** | Section 6.1 | Not measured |
| **Quorum lost permanently** | Section 6.2 | Not measured |

### 6.1 A node that is alive but slow

A dead node is easy. A node that answers every request slowly holds up writes
while looking healthy, and a health check that only asks "are you there" says
yes.

**Detection.** A node reports its own append and fsync latency. The controller
compares each node against the group and raises a slow state when a node
exceeds a configurable multiple of the group median for a configurable
duration. Both values are configurable, and the default is deliberately
forgiving, because a brief spike is not a sick node.

**Action.** `placement.slowNode.action` selects it:

| Value | Behavior |
| --- | --- |
| `alert` | Raise the health state and alert. An operator decides. **The default.** |
| `demote` | The controller demotes a slow voter and promotes a learner, and steps down a slow leader. |

`alert` is the default because automatic demotion during a network-wide
slowdown can cascade: every node looks slow relative to a moving median, and
membership churns while the real fault is elsewhere.

**The alert must say why.** A slow node that reports only "slow" sends an
operator to look at the wrong thing.

The node reports the most specific cause it can establish:

| Cause | How the node knows |
| --- | --- |
| `storage-errors` | The device returned errors, or retried reads |
| `storage-saturated` | Device queue depth and service time are at their limit |
| `storage-slow` | Latency rose with no error and no saturation, which suggests a failing device or a noisy neighbour |
| `write-volume` | This node's accepted bytes each second grew, so the load changed rather than the node |
| `compaction-pressure` | Background rewrite work is consuming the device |
| `memory-pressure` | The process is reclaiming or swapping |
| `network-latency` | Peer round trips grew while local IO stayed flat |
| `unknown` | None of the above matched |

**`unknown` is a valid answer and must be reported as one.** A node that
guesses a cause it cannot establish sends an operator down a wrong path, which
is worse than sending them nowhere. The alert then carries the raw evidence:
append latency, fsync latency, queue depth, and accepted bytes, against the
group median for each.

### 6.2 A tablet that loses quorum permanently

Two of three voters destroyed with no recovery leaves one replica holding a log
that may be behind the last committed entry. There is no safe automatic answer,
so TallyOwl gives two deliberate ones.

**Restore from a snapshot is the default and the documented path.** It never
loses an acknowledged write that the snapshot covers, and it loses everything
written after the snapshot. See section 11 procedure 3.

**Unsafe recovery exists behind an explicit flag.** It forces a single-voter
membership from the surviving replica's log. The tablet returns in minutes
rather than in the time a restore takes.

It can lose an acknowledged write. A write committed by the two lost voters but
not yet replicated to the survivor is gone, and no component can tell which
writes those were.

Therefore:

- the command requires an explicit confirmation that names the tablet;
- it writes an audit record in the `audit` retention class;
- it marks the affected time range degraded in the catalog, and every query
  that overlaps that range reports the degradation in its result and in its
  explain output;
- the mark never expires on its own. An operator clears it deliberately, which
  records who accepted the loss.

A fast path that hides its cost becomes the habitual path. This one cannot hide
its cost.

## 7. The catalog

STORAGE.md section 3.3 says a repair command rebuilds the segment catalog by
scanning manifests. That is true, and it covers one of the twelve things the
catalog holds.

| Catalog contents | Rebuildable from segments? |
| --- | --- |
| Segment manifests and generations | Yes |
| Log checkpoints and projector watermarks | Yes, conservatively |
| Durable batch receipts and deduplication windows | **No** |
| Tombstones and compaction state | **No** |
| Workspace, project, environment, and source configuration | **No** |
| API key and role-token hashes, scopes, and revocation | **No** |
| Node certificate identity, role, and fencing state | **No** |
| LinkKeys identity mappings, sessions, and memberships | **No** |
| Saved dashboards, queries, cohorts, funnels, and alerts | **No** |
| Backup and export snapshots and audit records | **No** |

Two of those are not merely inconvenient:

- **lost tombstones resurrect erased data.** An erasure that a person was told
  had happened stops having happened;
- **lost receipts duplicate on retry.** A collector retrying an in-flight batch
  finds no receipt and commits it a second time.

### Catalog snapshots

`catalog.snapshots.enabled` turns on periodic catalog snapshots. **It is off by
default.** A home installation runs without it, and the default recovery for a
lost catalog is a restore from the ordinary backup.

`catalog.snapshots.keep` sets how many snapshots to retain. The default is 2,
and it applies whenever snapshots are on. Two is enough to survive a snapshot
that is itself damaged, which is the failure that one snapshot cannot cover.

A snapshot uses the versioned canonical CSIL encoding, not the engine's byte
format, so a snapshot outlives an engine change. See STORAGE.md section 3.3.

This is deliberately small. Continuous change shipping and a recovery point
measured in seconds are a later feature, and nothing here prevents adding them.

### What a rebuild does and does not restore

Procedure 5 in section 11 gives the steps. A rebuild without a snapshot
restores the segment catalog and leaves the system able to serve queries. It
does not restore the nine rows above marked no, and the operator is told so
explicitly rather than discovering it.

**A rebuild without a snapshot is not sufficient for a project that permits
erasure.** Tombstones are gone. Section 9 requires an erasure ledger that
survives a rebuild for exactly this reason.

## 8. Compaction races

Compaction reads segments, writes replacements, and updates the catalog. Four
things run concurrently with it, and each is a correctness question rather than
a performance one.

### 8.1 Compaction against a running query

A query resolves a manifest generation and reads segments from it. Compaction
publishes a new generation and wants to delete the old segments.

Rules:

1. A query pins the generation it resolved. A pinned generation's segments are
   never deleted.
2. Compaction publishes the new generation atomically in one catalog
   transaction. A query sees the old generation or the new one, never a mixture.
3. A deletion waits for a garbage-collection grace period after the last pin on
   that generation is released. The grace period is configurable and exceeds
   the query budget's maximum runtime, so a long query cannot outlive its own
   inputs.
4. A pin that outlives the grace period, because a process died holding it,
   expires. Expiry is bounded, so a leaked pin cannot retain storage forever.

### 8.2 Compaction against erasure

**This is the dangerous one.** Compaction reads a segment, spends time
rewriting it, and publishes the result. An erasure that lands during that
window applies to the segment that compaction is replacing. If the replacement
is written from the pre-erasure snapshot, erased data comes back.

Rules:

1. Compaction records the tombstone generation it started from.
2. Before it publishes, it re-reads tombstones and applies every tombstone
   committed since that generation.
3. The publish transaction verifies that the tombstone generation has not moved
   again. If it has, compaction applies the new tombstones or restarts.
4. A tombstone is a standing predicate, not a one-time action, so a tombstone
   applied to the wrong generation still hides the data on read. This is the
   second line of defence, not the first.

Rule 4 is what keeps a bug in rules 1 to 3 from becoming a privacy incident.
Both exist on purpose.

### 8.3 Compaction against a cold-tier upload

An upload in flight can name a segment that compaction has obsoleted.

Rules:

1. A segment is identified by its content address, so an obsolete upload cannot
   overwrite a live object.
2. An upload that completes for an obsoleted segment is discarded by the
   catalog transaction, and the object is collected under the same grace period
   as section 8.1.
3. Compaction never deletes the only valid copy of a segment before the
   replacement is durable and confirmed. See STORAGE.md section 13.

### 8.4 Compaction against locator runs

STORAGE.md now has compaction group cold rows by correlation value, which
lowers query cost. See BENCHMARKS.md section 12b. That makes the locator runs
and the segments they describe mutually dependent.

Rules:

1. A locator run is published in the same catalog transaction as the segments it
   describes. A generation never holds a locator run that disagrees with its
   segments.
2. A locator entry that names a segment which the generation does not hold is a
   detectable inconsistency, not a wrong answer: the reader verifies the full
   typed value in the segment index, so a stale entry costs a wasted open.
3. The consistency check between a generation's locator runs and its segments
   is a required test. See section 14.

## 9. Erasure durability

Nothing previously stated when an erasure becomes durable relative to when it is
acknowledged. That ordering is the whole guarantee.

**A tombstone is durable before the erasure is acknowledged.** The
acknowledgement is a statement to a person that their data is gone. A crash
after the acknowledgement and before the durable write would make that statement
false.

Rules:

1. The tombstone commits through the same durability boundary as telemetry, at
   the tablet's configured receipt policy. An erasure is not a lesser write.
2. The acknowledgement follows the commit. It never precedes it.
3. The erasure ledger is durable independently of the catalog, and it survives a
   catalog rebuild. Section 7 shows why: a rebuild loses tombstones, and an
   erasure that a rebuild can undo is not an erasure.
4. An erasure ledger travels with a snapshot and with a restore, so a restore
   cannot resurrect an erased end user. This already appears in
   THREAT_MODEL.md section 5 and is repeated here because section 11 procedure
   3 depends on it.
5. An erasure that arrives while its target data is still queued stays active
   as a standing predicate and hides the late arrival. This is already in
   DELIVERY.md section 9.

## 10. Disk exhaustion

The collector side is covered in DELIVERY.md section 9. The storage side was a
test bullet with no stated behavior.

| Point of exhaustion | Behavior |
| --- | --- |
| Append log | Stop accepting writes and fail readiness before the device is full. Never accept a write that cannot be made durable. |
| Segment publish | Keep the log range. The log is the durable record until a segment replaces it. |
| Compaction | Abandon the attempt, keep the source segments, and alert. Compaction is never required for correctness. |
| Catalog | Stop accepting control writes. Serve reads. |
| Cold-tier cache | Evict cached copies of cold objects. A cached copy is never the only copy. |
| Export | Fail the export. Never let an export displace live data. |

A reserve, configurable and non-zero by default, is held back so that the
system can still write the metadata needed to recover. A device that reaches
100 percent cannot always be recovered in place.

## 11. Recovery procedures

Each procedure names what is lost. An operator reads that first.

### Procedure 1: a process crashed

Automatic. The recovery scanner truncates a torn final frame, replays complete
frames after the catalog checkpoint, reconstructs receipt entries, and resumes
segment work. **Nothing acknowledged is lost.** See STORAGE.md section 5.

### Procedure 2: a voter was lost, quorum survives

Automatic. Replace the node, let it enrol, and add it as a learner. It catches
up from the leader's log and is promoted. **Nothing is lost.** Measured at
528 ms to catch up in BENCHMARKS.md section 10.

### Procedure 3: quorum lost permanently, restore path

1. Stop the tablet's remaining replica.
2. Identify the most recent snapshot that covers the tablet.
3. Restore it, including its erasure ledger.
4. Bring up a new voter set.
5. Replay any Corndogs tasks still queued for the affected range. Stable batch
   IDs make the replay idempotent.

**Lost: everything written after the snapshot that Corndogs no longer holds.**
Step 5 recovers more than the snapshot alone, which is why the collector queue
depth and the snapshot period should be chosen together.

### Procedure 4: quorum lost permanently, unsafe recovery

Only when the outage cost exceeds the cost of losing an unknown set of writes.

1. Confirm the command with the tablet name.
2. The surviving replica forms a single-voter membership from its own log.
3. The affected time range is marked degraded, and every overlapping query
   reports it.
4. Add voters back as learners.
5. Review the audit record, and clear the degraded mark deliberately.

**Lost: any write committed by the lost voters and not replicated to the
survivor. The set is unknown and unknowable.**

### Procedure 5: the catalog was lost or damaged

With snapshots on:

1. Restore the most recent good catalog snapshot.
2. Rebuild the segment catalog by scanning manifests, which reconciles segments
   written after the snapshot.
3. Replay queued Corndogs tasks.

**Lost: control changes made between the snapshot and the failure.**

Without snapshots:

1. Rebuild the segment catalog by scanning manifests.
2. Restore the erasure ledger, which is durable independently. See section 9.
3. Re-enrol nodes, because fencing state is gone.
4. Re-create sources and credentials.

**Lost: receipts, so an in-flight retry may duplicate; sessions, so every
person signs in again; saved dashboards, queries, and alerts.** Tombstones
survive only through the erasure ledger in step 2.

### Procedure 6: a segment is damaged

1. With another copy, the tablet replicates from it. Automatic under `scrub`.
2. With no other copy, restore the segment from a snapshot.
3. With neither, the segment stays damaged. Queries over its range return
   `incomplete-result` and name it. **Lost: that segment's events.**

Step 3 is a real outcome on the home profile, which holds one copy.

## 12. Accepted limits

Each of these is a deliberate choice.

**`integrity.mode: none` accepts a wrong answer.** An operator who selects it
has chosen read throughput over correctness. TallyOwl shows the state and does
not prevent the choice.

**One copy cannot be repaired.** The home profile detects damage under `scrub`
and cannot fix it. Only a restore or a second copy fixes it.

**Unsafe recovery can lose an acknowledged write, and cannot say which.** The
degraded mark says the range is affected. It cannot say more.

**A catalog rebuild without snapshots loses control state.** Snapshots are off
by default, so this is the default outcome of a catalog loss.

**Slow-node detection compares against the group.** A slowdown that affects
every node equally is not detected as a slow node, because there is no fast
node to compare against. Absolute latency alerts cover that case.

**Cold-tier encryption is not designed.** D28 defers it, and cold tiering must
not carry erasable data until it exists. This is repeated from
THREAT_MODEL.md section 8 because procedures 3 and 5 restore cold data.

## 13. What the system reports

An operator cannot act on a failure that produces no signal, and an alert
cannot exist without a measurement. Each state below has an instrument.

| Instrument | Reports |
| --- | --- |
| `tallyowl_integrity_pages_verified_total` | Pages checked, labelled by mode |
| `tallyowl_integrity_failures_total` | Failed checksums, labelled by tier |
| `tallyowl_segments_damaged` | Segments currently marked damaged |
| `tallyowl_segments_repaired_total` | Segments repaired from another copy |
| `tallyowl_scrub_progress_ratio` | Fraction of one pass completed |
| `tallyowl_catalog_snapshot_age_seconds` | Age of the newest catalog snapshot |
| `tallyowl_tablet_degraded` | Tablets marked degraded by unsafe recovery |
| `tallyowl_node_slow` | Nodes in the slow state, labelled by cause |
| `tallyowl_generation_pins` | Manifest generations pinned by a running query |
| `tallyowl_generation_pin_age_seconds` | Age of the oldest pin |
| `tallyowl_compaction_restarts_total` | Compactions restarted by a tombstone move |
| `tallyowl_storage_reserve_bytes` | Space held back for recovery |

Two are the early warnings that matter most, because each one predicts a
failure rather than reporting one:

- **`tallyowl_generation_pin_age_seconds`** rising means a query is holding
  storage that compaction wants. A leaked pin retains data until it expires.
- **`tallyowl_catalog_snapshot_age_seconds`** is the recovery point for
  procedure 5. When snapshots are off, it reports nothing, which is itself the
  answer.

A metric never carries an end-user ID as a label. See CONVENTIONS.md section 6.

## 14. Required tests

1. A torn write at every structural boundary, and a crash before and after
   every fsync and catalog boundary.
2. A damaged page, index block, and footer, each detected under
   `verify-on-read` and under `scrub`, and each undetected under `none`.
3. A damaged segment with a second copy, repaired automatically.
4. A damaged segment with no second copy, producing `incomplete-result` that
   names it.
5. A query holding a pinned generation while compaction publishes and tries to
   delete it.
6. An erasure that lands mid-compaction, proving the rewritten segment does not
   resurrect it.
7. A tombstone applied against a stale generation, proving the standing
   predicate still hides the data on read.
8. A locator run and its generation's segments checked for agreement.
9. A crash between an erasure commit and its acknowledgement, proving the
   erasure survives.
10. A catalog rebuild with snapshots and without, each asserting exactly what
    section 7 says survives.
11. Unsafe recovery, asserting the audit record, the degraded mark, and the
    mark's presence in query output and explain output.
12. A slow node, asserting detection, the reported cause, and `unknown` when the
    cause cannot be established.
13. Disk exhaustion at each point in section 10.
14. A restore that must not resurrect an erased end user.
15. A leaked generation pin expiring, proving storage is not retained forever.

## 15. Review

Review this document when any of these changes:

- a durability boundary moves;
- a new background process rewrites or deletes data;
- the catalog gains a content that cannot be rebuilt;
- a recovery procedure gains or loses a step;
- an accepted limit stops being acceptable.
