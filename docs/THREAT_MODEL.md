# Threat model

This document uses these abbreviations: certificate authority (CA), mutual
Transport Layer Security (mTLS), and Transport Layer Security (TLS).
[DOCUMENTATION.md](DOCUMENTATION.md) holds the full list.

## 1. Scope

This document names what TallyOwl protects, who might attack it, and which
decision answers each threat. It is not a control checklist.
[DESIGN.md](DESIGN.md) section 10 holds the controls.

A threat with no answer appears in section 8 as an accepted risk. An accepted
risk that nobody wrote down is the dangerous kind.

## 2. Assets

| Asset | Why an attacker wants it |
| --- | --- |
| Telemetry of one project | Business intelligence about a competitor or a target |
| End-user IDs | They identify people, even when opaque |
| Source credentials | They inject or forge telemetry |
| Role tokens | They enrol a node into the installation |
| The CA signing key | It mints any node identity |
| LinkKeys sessions | They act as a person in the dashboard |
| Collection policy | It turns off collection, which hides an attack |
| Audit records | They show what an attacker did |
| Cold objects and backups | They hold retained telemetry outside the running system |

End-user IDs deserve their own line. D9 prohibits direct personal data, and D28
gives erasure. An opaque end-user ID is still sensitive, because it links a
person's activity across time.

## 3. Trust boundaries

```text
  browser ──1──> application ──2──> collector ──3──> head ──4──> storage
                                        │                 │
                                        5                 6
                                        ▼                 ▼
                                    Corndogs          object store

  operator browser ──7──> dashboard
  scrape target   ──8──> compatibility receiver
```

| Boundary | What crosses it | What authenticates |
| --- | --- | --- |
| 1 | Browser telemetry | The application's own session |
| 2 | Batches | A project-scoped source credential |
| 3 | Batches and policy | A collector key, optionally mTLS |
| 4 | Committed writes | Node certificates, mTLS |
| 5 | Durable tasks | A deployment secret or workload identity |
| 6 | Cold segments | Object-store credentials |
| 7 | Queries and administration | A TallyOwl session from LinkKeys |
| 8 | Scrapes and pushes | Target or network policy |

Boundary 1 is the one that matters most, because everything beyond a browser is
attacker-controlled input.

## 4. Adversaries

| Name | Capability |
| --- | --- |
| A1 Hostile browser | Runs any code in a page, sends any bytes the application accepts |
| A2 Compromised application | Holds one project credential |
| A3 Hostile tenant | Holds a valid credential for a different project |
| A4 On-path network | Reads and modifies traffic between components |
| A5 Compromised collector | Runs code on a collector pod |
| A6 Compromised storage node | Runs code on a storage node |
| A7 Curious operator | Has dashboard access to some projects |
| A8 Backup reader | Reads cold objects or backup media |
| A9 Supply chain | Controls a dependency or a generated artifact |

## 5. Threats and answers

### A1, a hostile browser

| Threat | Answer |
| --- | --- |
| Send telemetry directly to TallyOwl | A browser cannot reach TallyOwl. It uses the application's connection. The collector accepts no browser credential. |
| Claim another workspace or project | The collector resolves tenancy from the source credential and discards any tenancy value in a payload. See D32. |
| Claim a privileged end user identity | The application compares browser end user identity against its own authenticated session. |
| Forge a `collector` property origin | The collector stamps origin. A protected key refuses a client value and counts the refusal. See D38. |
| Suppress a later event by replaying its ID | Deduplication has project scope, and the app driver namespaces or replaces a browser-supplied `event_id`. See DELIVERY.md section 2. |
| Exhaust a project quota | Per-source quotas, fair scheduling, and hard frame limits. |
| Poison a decompression path | Strict frame size before allocation and a decompression ratio limit. |

The last two are throughput attacks that succeed quietly. A browser that
inflates a project's ingest bill is a real attack even when no data leaks.

### A2, a compromised application

An application holds one project credential, so the blast radius is one
project. That is the point of scoping the credential rather than the collector.

| Threat | Answer |
| --- | --- |
| Write to another project | The credential proves one project. The head verifies the destination is inside the collector's permitted set. |
| Forge history | Telemetry is append-only. A correction is a new event, never an edit. |
| Read another project | An ingest credential grants no read. |
| Hide its own compromise | Audit records live in the `audit` retention class, which is never shorter than the deletion horizon. |

### A3, a hostile tenant

| Threat | Answer |
| --- | --- |
| Query another project | The session determines scope. A refused project returns an authorization error, never an empty result, so absence does not leak existence. |
| Correlate across projects by shared ID | A cross-project query needs an explicit identity policy. Shared request and trace IDs correlate only where policy permits. |
| Infer another project's volume from shared resources | Per-project budgets and a separate alert budget pool. This is a partial answer; see section 8. |

### A4, an on-path network

| Threat | Answer |
| --- | --- |
| Read telemetry in flight | TLS on every hop that crosses a pod trust boundary. |
| Impersonate a collector or a node | mTLS with short-life certificates from the installation CA. See D22. |
| Replay a batch | Stable batch IDs and a deduplication window make a replay one logical commit. |
| Downgrade a protocol | Capability negotiation returns a permitted set, and a node must not use a capability the controller did not permit. |

### A5, a compromised collector

A collector holds a credential and a queue. It does not hold a query surface.

| Threat | Answer |
| --- | --- |
| Inject telemetry for its projects | Accepted. A collector is trusted for the projects it serves. |
| Inject telemetry for other projects | The head verifies the destination against the collector's permitted set. |
| Read stored telemetry | A collector has no read path to storage. |
| Suppress telemetry | Detectable. Ingest health, queue depth, and policy version are reported, and an absence alert fires when expected data stops. See D41. |

### A6, a compromised storage node

| Threat | Answer |
| --- | --- |
| Read the tablets it holds | Accepted. A storage node holds its own data. |
| Read other tablets | It votes only in the tablet groups it holds. There is no cluster-wide group. |
| Take tablet ownership | Placement comes from the controller. A role token cannot change a voter set. |
| Forge a commit | A tablet acknowledges only after its receipt policy is satisfied, and a multi-voter group never acknowledges an uncommitted entry. |

### A7, a curious operator

| Threat | Answer |
| --- | --- |
| Read a project outside their role | Authorization runs for each operation, not only at login. |
| Escalate through a LinkKeys claim | A claim maps to a role only when a trusted domain signed it, and only through explicit installation policy. See D7. |
| Exfiltrate through export | Export is an audited operation and respects project scope. |
| Read raw personal data | D9 prohibits it by default. Bodies, headers, cookies, and claim values are never captured. |

### A8, a backup reader

| Threat | Answer |
| --- | --- |
| Read cold objects | Segment encryption, with keys in the catalog rather than the object. |
| Read an erased end user after erasure | Cryptographic erasure destroys the key. See D28. |
| Resurrect erased data by restoring | The erasure ledger travels with a restore, and a tombstone is a standing predicate. |

The key design that D28 defers is the load-bearing part of this row. Until it
exists, cold tiering must not carry data for a project that permits erasure.

### A9, supply chain

| Threat | Answer |
| --- | --- |
| Hostile dependency | Dependency audit in the release pipeline, and Apache-2.0 compatibility review. |
| Tampered generated code | Generated output is reproducible, and CI fails on drift. |
| Secret leaked through a build | Secret references and masking, and artifact inspection before promotion. |
| Untrusted contribution running with secrets | Reactorcide's dual-source model keeps trusted pipeline code separate. |

## 6. What an attacker gains from each boundary

An attacker at boundary 1 gains one project's ingest. At boundary 2 they gain
one project. At boundary 3 they gain the projects one collector serves. At
boundary 4 they gain the tablets one node holds.

Capability grows with the difficulty of the boundary. A browser is the easiest
boundary to cross and it gains the least.

## 7. Design rules that this model depends on

These are not new. They appear here because a quiet relaxation of any one of
them breaks this threat model.

1. A browser never reaches TallyOwl.
2. Tenancy comes from a credential, never from a payload.
3. A client value is data, never a routing or authorization fact.
4. An exact-once claim is never made, so no component trusts a delivery count.
5. A node generates its own private key, and the control plane signs only a
   request.
6. A role token cannot change a voter set.
7. Authorization runs for each operation.
8. An audit record outlives the thing it describes.

## 8. Accepted risks

These have no answer today. Each is a deliberate choice.

**A hostile tenant may infer another project's activity from shared timing.**
Per-project budgets bound resource use, but a shared node leaks coarse timing.
An installation that cannot accept this uses separate deployments, which D8
already gives as the answer for separate customers.

**A compromised collector can inject telemetry for its own projects.** No
mechanism distinguishes real telemetry from telemetry that a compromised but
authentic collector produced.

**A compromised storage node reads its own tablets.** Segment encryption
protects cold objects, not a node's working set.

**An operator with export rights can exfiltrate their own project.** Audit
records show it happened. Nothing prevents it.

**Cold-tier encryption is not designed yet.** D28 defers it. Cold tiering must
not carry erasable data until it exists.

**No rate limit protects the LinkKeys login path.** TallyOwl owns its session
after login, and the login path is LinkKeys' surface.

## 9. Review

Review this document when any of these changes:

- a trust boundary moves;
- a credential gains a scope;
- a new component reaches storage or the query path;
- a compatibility edge starts listening by default;
- an accepted risk stops being acceptable.

The reference application exercises the negative cases. See
[TESTBED.md](TESTBED.md).
