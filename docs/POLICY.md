# Collection policy and retention

This document gives the compiled policy snapshot, the property rules, and the
retention classes. [DESIGN.md](DESIGN.md) section 7 gives the scope inheritance.

## 1. Scope inheritance

A policy compiles from five levels. A narrower level overrides a wider one.

```text
installation defaults
  → workspace overrides
    → project overrides
      → environment overrides
        → source overrides
```

The head compiles the result into one snapshot with one version number. A
collector fetches the snapshot, caches the last valid version, and reports the
version that it applies.

The head activates a snapshot atomically. Invalid policy never replaces valid
policy. A collector that cannot reach the head keeps its last known good
snapshot and reports staleness.

**Where a policy is stored.** Every level is in the control catalog under
`control/policy/<scope>/<scope-id>`, and the version is
`control/policy-generation` beside it, raised in the same durable write. The
head reads them back at start-up, compiles them, and applies the result at
ingest. `put-policy` and `get-policy` on `TallyOwlControl` are the surface an
operator edits through.

**Both ends apply the policy.** The head applies it at the commit, and a
collector applies the same compiled snapshot at intake. Section 7 says how the
snapshot travels and why each end has a different reason to hold it. See L112
and L113.

## 2. Snapshot contents

The compiled snapshot holds these groups. `tallyowl-collector.csil` holds the
normative types.

### Collection

- enabled telemetry kinds;
- head sampling rate;
- tail sampling rules and the decision window;
- the late-span grace period;
- always-keep expressions;
- session maximum lifetime.

### Properties

- protected key names and their required origin;
- properties that the collector stamps;
- redaction rules;
- the maximum property count for one item;
- the maximum property name size and value size.

### Limits

- maximum event bytes;
- maximum batch bytes;
- maximum attribute and measurement counts;
- metric label and series budgets.

### Retention

- the duration for each retention class;
- a per-kind override where a project needs one.

### Control

- privacy and consent behavior;
- an emergency kill switch.

## 3. Property rules

One typed property namespace carries every descriptive value. See D38.

Each property records an origin of `client`, `driver`, or `collector`.

A protected key names a property that an application cannot set. The collector
refuses a client value for a protected name and counts the refusal in a metric.

A protected key is one of two kinds. For an operator key such as `region`,
`env`, `cell`, or `installation`, the collector stamps its own value after the
refusal. For a correlation name — `request_id`, `session_id`, `trace_id`, or
`event_id` — there is no replacement, because the row's own column carries
that value. A client property with one of these names would be shadowed by the
column, and no filter could reach it. See L153.

That refusal is visible on purpose. A silent overwrite would hide a
misconfigured application.

A property never grants access and never selects a destination. Tenancy comes
from the credential. See D32.

## 4. Retention classes

A telemetry kind maps to a class. A project can override the duration for one
kind. See D43.

| Class | Holds | Bound |
| --- | --- | --- |
| `provisional` | Spans inside an open tail-sampling decision window | The decision window plus the grace period |
| `raw` | Accepted interchange CBOR | Short, and optional |
| `detailed` | Events, spans, error occurrences, metric points | The main analytic range |
| `rollup` | Aggregates and downsampled series | Longer than `detailed` |
| `audit` | Control-plane, deletion, and export records | Longest |

Rules:

- `provisional` never outlives its decision window and grace period;
- `rollup` must be at least as long as `detailed`, because a rollup that
  expires first leaves a gap that no query can fill;
- `audit` is never shorter than the deletion horizon;
- a query result names the class that bounded its range when data expired
  inside the requested window.

## 5. Retention couplings

Three couplings are correctness rules, not preferences. A violation of any one
of them produces a wrong answer that looks like a working system.

**Attribution lookback and touch retention.** A lookback window longer
than touch retention moves credit to later touches, because the early
touches aged out. TallyOwl refuses the query. See D40.

**Deduplication window and outage buffer.** The head must remember a batch ID
for longer than a collector can retry. Otherwise a recovered collector creates
duplicates that no query can remove. See D36.

**Erasure horizon and audit retention.** An erasure predicate stays active
until its horizon ends, and the audit record must outlive it. See D28.

## 6. Numeric defaults

These are first values for measurement. The reference application confirms or
changes each one. They are not recommendations.

| Value | First value |
| --- | --- |
| Session maximum lifetime | 12 hours |
| Tail decision window | 60 seconds |
| Late-span grace period | 30 seconds |
| Maximum properties for one item | 128 |
| Maximum property name size | 64 bytes |
| Maximum indexed value size | 4 KiB |
| Maximum properties byte size for one item | 64 KiB |

The batch and frame values are in D19. The storage writer values are in D17.

## 7. Policy distribution

A collector fetches a snapshot over CSIL-RPC and passes its known version. The
head returns nothing when the version is current.

A collector applies a new snapshot at a batch boundary, never inside a batch.

A kill switch takes effect at the next fetch. It is not a substitute for
revocation, which the head enforces at the next batch.

The head applies the same policy at the commit. Both ends apply it, and each
end has a different reason:

- the **head** is the durability boundary. A rule that only the edge applies is
  a rule that an edge that missed it can break;
- the **collector** is where the cost is. An event that only the head refuses
  has already cost a batch, a durable queue write, a delivery, and a receipt.

Both ends read one compiled snapshot. A collector that removed something the
head keeps would lose data that no query can find again.

**A stale policy never fails readiness.** A collector reports how long since the
head last answered, on its health record, and keeps accepting telemetry. A
collector that stopped when it lost the head would turn a control-plane outage
into a data-plane one, and the last good policy is still in force. An operator
alerts on the age.

A collector names its source and never its tenancy. The head resolves the
workspace and the project from the control catalog. See D32.

A refusal by policy is not a rejected item on the receipt. A rejected item tells
an application that something is wrong and invites a retry. A policy refusal is
the installation as its operator configured it, and a driver that retried it
would retry it for ever. The collector counts the refusal in a metric.

## 7.1 Campaign linking

D30 puts consent at the point where campaign data joins an identified end user.
A session identifier is that point, because it is what links touches over time.

The policy has three levels of campaign linking:

| Level | Behavior |
| --- | --- |
| `linked` | Keep the touch and its links. Attribution works |
| `unlinked` | Keep the campaign facts. Remove the session, the end-user, and the anonymous links from a campaign touch, and remove the campaign fields from every other kind of record |
| `none` | Refuse a campaign touch, and remove the campaign fields from every other kind of record |

A narrower level can only keep less. A project cannot widen what a workspace
made narrower.

`unlinked` exists because D30 requires it. An operator who turns off
session-linked campaign data must still be able to measure a campaign.

Under `unlinked` a campaign fact lives only on a campaign touch. An application
that sends campaign parameters only on a page view therefore records no campaign
data at that level. Send an explicit campaign touch.

## 7.2 Consent and attribution

The policy has a setting for whether attribution reads a person who refused
marketing consent.

The default is off. An absent consent state is not a refusal: an application
that never sent a consent state has not refused on behalf of its person, and
TallyOwl does not guess a jurisdiction. D30.

The consent state is stored with the applicable record whatever the setting is,
so a later policy can act on data that arrived before it.

## 8. Required tests

1. An invalid snapshot never replaces a valid one.
2. A collector without head connectivity keeps its last good snapshot and
   reports staleness.
3. A protected key refuses a client value and counts the refusal.
4. Configuration refuses a `rollup` duration shorter than `detailed`.
5. The query service refuses an attribution window longer than touch
   retention.
6. A policy change applies at a batch boundary and not inside a batch.
7. A kill switch stops collection at the next fetch.
8. A campaign touch at the `unlinked` level keeps its campaign and loses its
   session, its end-user, and its anonymous links.
9. A campaign touch at the `none` level is refused, and no other kind of record
   is refused with it.
