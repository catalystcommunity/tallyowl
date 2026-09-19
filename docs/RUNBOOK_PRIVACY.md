# Privacy runbook

This runbook tells an operator how to answer a privacy request and what the
system does on its own. [POLICY.md](POLICY.md) owns collection policy;
[DATA_MODEL.md](DATA_MODEL.md) owns what a row can hold.

## 1. What is never recorded

TallyOwl never records secrets, credentials, request bodies, claim values, or
raw personal data by default. Key names that look like secrets never travel:
the scrub refuses them by substring, so `api_key` and `authorization_header`
are both caught. An end-user ID appears in logs at debug level only, and a
project policy can forbid that too.

## 2. Erase one end user

An erasure request names a project and an end-user ID.

1. Submit the erasure. A tombstone becomes visible immediately: queries stop
   returning the rows before any byte is rewritten.
2. The deletion pass rewrites only the affected bounded segments and
   physically reclaims the data.
3. A tombstone is a standing predicate. Matching data that arrives after the
   request is hidden too, so a late batch cannot resurrect a person.

The erasure ledger records each request durably, independent of the catalog.

## 3. Erasure and recovery

The interactions an audit will ask about:

- **a restore does not resurrect anybody.** A snapshot carries the erasure
  ledger, and the restore applies it. The restore prints "Nobody who asked to
  be removed has come back" only after that is true.
- **a catalog rebuild keeps erasures.** The ledger survives outside the
  catalog. The rebuild recovers it and says how many records it applied.
- **a replayed batch cannot bring rows back.** The tombstone predicate hides
  matching rows whenever they arrive.
- **the cold tier erases by destroying key material.** Until cold-tier
  encryption exists, cold tiering must not carry erasable data. See D28.

The disaster-recovery drill (`./tools.sh drill dr`) exercises the restore and
the rebuild paths.

## 4. Retention

A telemetry kind maps to a retention class: `raw`, `detailed`, `rollup`, and
`audit`. The retention pass expires data past its class. `rollup` must be at
least as long as `detailed`, and the configuration is refused when it is not,
because a rollup that expires first leaves a gap no query can fill.

Raw accepted telemetry stays append-only for its retention period, and every
derived projection is reproducible from it.

## 5. Consent

Collection policy is authored on the head and reaches every collector within
`collector.policyInterval`. A policy can stop collection by property, by
kind, or entirely — the kill switch. A collector that has never fetched a
policy collects everything, which is what an installation with no policy
means; a collector that cannot reach the head keeps applying the last good
policy rather than turning a control-plane outage into a data-plane one.

## 6. Export and access

An operator with export rights can export their project, including to
Parquet. Audit records show each export. Nothing prevents an authorized
export: that is the accepted risk THREAT_MODEL.md section 8 records, and the
audit trail is the control.
