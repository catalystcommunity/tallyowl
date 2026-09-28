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

This release has no command and no dashboard view that submits an erasure. The
operation is `request-deletion` on the `TallyOwlControl` service of a running
head. Call it with a generated control client:

- Rust: `TallyOwlControlClient::request_deletion(DeletionRequest)` in
  `generated/rust/tallyowl-control-api`.
- Go: `(*TallyOwlControlClient).RequestDeletion(ctx, DeletionRequest)` in
  `generated/go/tallyowl-control-api`.

The connection must carry a session token of a person who has the `owner` role
in the workspace of the project. `RUNBOOK_OPERATIONS.md` section 3 tells you how
to make that session. The `reason` field is mandatory. The erasure ledger keeps
the reason.

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
`audit`. The retention pass expires data past its class. This release does
not build the `audit` class: TallyOwl keeps control-plane, deletion, and export
records without limit, and it refuses a `retention.audit` value other than the
default. `rollup` must be at
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

A person who has the `admin` role in a workspace can export a project of that
workspace to Parquet.

This release has no command and no dashboard view that starts an export. The
operation is `run-workflow` on the `TallyOwlControl` service of a running head.
Call it with a generated control client:

- Rust: `TallyOwlControlClient::run_workflow(RunWorkflowRequest)`.
- Go: `(*TallyOwlControlClient).RunWorkflow(ctx, RunWorkflowRequest)`.

Set these fields:

- `kind`: `export`.
- `project_id`: the project.
- `range`: mandatory. The head refuses an export that has no time range.
- `destination`: optional. It is one plain file name, for example
  `orders-2026-08.parquet`. The head refuses a directory, an absolute path, and
  `..`.

The head writes the file on its own volume, in
`<head.dataDir>/exports/<project-id>/`. It writes a description of the export
beside the file, with the extension `.manifest.txt`. The head does not replace
a file. If the name is in use, the export stops and the workflow log gives the
reason.

Nothing sends the file to you. Copy it from the volume of the head. In a chart
installation, use `kubectl cp` from the head pod.

The workflow log of the head shows each export. Nothing prevents an authorized
export: that is the accepted risk THREAT_MODEL.md section 8 records, and the
record of the export is the control.
