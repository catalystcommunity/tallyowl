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
refuses a client value for a protected name, counts the refusal in a metric,
and stamps its own value.

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

**Attribution lookback and touchpoint retention.** A lookback window longer
than touchpoint retention moves credit to later touches, because the early
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

## 8. Required tests

1. An invalid snapshot never replaces a valid one.
2. A collector without head connectivity keeps its last good snapshot and
   reports staleness.
3. A protected key refuses a client value and counts the refusal.
4. Configuration refuses a `rollup` duration shorter than `detailed`.
5. The query service refuses an attribution window longer than touchpoint
   retention.
6. A policy change applies at a batch boundary and not inside a batch.
7. A kill switch stops collection at the next fetch.
