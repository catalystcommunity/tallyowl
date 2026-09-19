# Phase 8 report: product behavior

What was built, what was tested, and what was not built. Read
[IMPLEMENTATION_LOG.md](IMPLEMENTATION_LOG.md) L097 to L112 beside this: it
holds the reasoning and this holds the state.

**Phase 7's revisit work came first, and it is in this report too.** The
implementation prompt's section 0 said the sealed-segment copy was "the single
highest-value piece of work left" and "worth doing before Phase 8 rather than
after". It was done first, and section 1 below is what it changed.

**All four Phase 8 exit criteria pass.** Section 4 states each one and names
where it is proved.

**Collection policy and saved analyses are durable.** The first version of this
report named that as the one thing outranking everything else, because both were
in the head's memory and a restart lost them. The owner asked for it and L112
built it: three record kinds in the control catalog, read back at start-up,
written before they are applied, and checked through the running loop rather
than only in a test.

**Two defects were found by writing the tests rather than by reading the
code**, and both would have shipped:

- **a funnel counted a converting person twice** — once as the anonymous visitor
  who vanished and once as the customer who appeared — because it correlated by
  event-time identity. Every sign-up funnel would have shown nobody converting.
  L104;
- **four query forms bypassed authorization.** `projects_named` read the trace
  form and the node tree and returned an empty list for anything else, and the
  authorization loop over an empty list permits everything. A funnel naming
  another tenant's project would have been answered. L109.

A third was found by reading two functions side by side while building a third:
**the locator was built in two places and they had gone out of step**, so a
compacted segment was pruned out of every trace and request lookup that another
segment also named. The answer came back smaller with nothing marked
incomplete. L102.

## 1. Phase 7's revisit work, which came first

`docs/PHASE7_REPORT.md` section 8 listed seven entries marked `Revisit: yes` and
named three of them as one item wearing three hats. All three are now built.

| Entry | What it asked for | State |
| --- | --- | --- |
| L087 | Build the sealed-segment copy | **Built.** `crates/tallyowl-cluster/src/transfer.rs`. `list-segments` and `fetch-segment` are answered, a copy is driven, every segment is proved from the bytes that arrived, and a snapshot seals first so that everything it covers is in a segment the copy can carry |
| L095 | Reclaim the consensus log | **Built.** A group snapshots every 4,096 entries and keeps 512, so the log is bounded rather than growing with the installation. Each entry is stored compressed. Section 5 holds the measurement |
| L090 | Build restore | **Built.** `snapshot-cluster` now writes the tablet's own store rather than the state machine's marks — the snapshot was not a backup — and `restore` verifies every segment, refuses onto live data and names the offline procedure, and otherwise adopts every segment through the path L087 built |
| L089 | The controller acts in one mode | **Left**, as the recommendation said. `recommendation-only` until a cluster has run |
| L093 | What Phase 7 does not cover | **Unchanged.** D15's two open items are still open: a partition that splits a group other than by isolating one node, and recovery from a corrupt or truncated log |
| L094 | The generation fence | **Left**, as the recommendation said |
| L083 | Whether an alpha ships with Phase 7 | **The owner's decision**, unchanged |

Two more items from that report's section 4 are also built:

| Item | State |
| --- | --- |
| **The head's query executor does not fan out** (ranked first) | **Built.** `crates/tallyowl-cluster/src/fanout.rs` asks every readable tablet through `partial-aggregate` and merges, and `ReplicatedStore` delegates its reads to it, so the head's executor is unchanged. A tablet that did not answer makes the result incomplete and is **named**. L101 |
| **An erasure is not replicated** | **Built.** `Store::erase` is on the contract and a replicated tablet proposes it. The command carried a generation and applying it advanced a number; it now carries the whole predicate, because a predicate is what hides a row. L098 |

**Nothing drives the segment copy on a timer yet.** A copy is a call rather than
a background task: `copy_tablet` for a new replica, `drive_movement` for a
tablet that is moving, and `restore_from` for a restore. That is what the
movement procedure and the restore command need, and a replica that joins and
falls behind still needs an operator to start its catch-up. See section 6.

## 2. What was built for Phase 8

| Path | Holds |
| --- | --- |
| `crates/tallyowl-head/src/identity.rs` | The identity graph: `identify`, `alias`, group association, traits, and both resolutions. Derived from the rows and nothing else |
| `crates/tallyowl-head/src/analysis.rs` | Funnel, retention, path, and timeline, and the cost guards each one checks before it works |
| `crates/tallyowl-head/src/erasure.rs` | Per-user erasure across every identifier a person is known by |
| `crates/tallyowl-head/src/policy.rs` | Collection policy: five levels, one compiled snapshot, and the event and property filtering it applies |
| `crates/tallyowl-head/src/saved.rs` | Saved analyses and the dashboards made of them |
| `crates/tallyowl-store/src/control.rs` | Where both of those live: three record kinds in the control catalog, beside the workspaces, the projects, and the keys |
| `crates/tallyowl-head/src/query.rs` | The four new query forms, and the scan-and-identity pass under them |
| `csil/tallyowl-control.csil` | `PolicyDocument`, `CompiledPolicy`, `SavedAnalysis`, `SavedDashboard`, and six new operations at wire IDs 17 to 22 |
| `csil/tallyowl-cluster.csil` | `SegmentList` and `list-segments`, at wire ID 6 |
| `packages/driver-go/item.go` | `Identify`, `Alias`, `Group`, `WithEndUser`, and `WithAnonymous` |
| `testbed/simulator`, `testbed/ledger` | The identity journey: one person, three client surfaces, and the funnel, retention, and timeline the ledger predicts |

**1,021 Rust tests, 52 Go tests, and 38 TypeScript tests pass**, against 972 at
the end of Phase 7. Lint is clean in all three languages, `cargo fmt --check`
passes, and `./tools.sh gen-check` passes.

## 3. Deliverable by deliverable

`docs/PLAN.md` Phase 8 lists seven.

| Deliverable | State |
| --- | --- |
| Sessions, identify, alias, group association, and traits | **Built.** `identify` links from a point, `alias` is a merge edge that does not rewrite a row, a group association carries the time it began, and traits are the properties an `identify` carried beside the identity. An alias loop terminates rather than looping |
| End user and session timelines | **Built.** A timeline is asked for by end-user identifier or by session, resolves by latest-known identity so that what somebody did before they signed in is on it, and paginates with a cursor that carries the occurred time and the event ID |
| Per-user erasure across detailed data, derived state, local segments, cold objects, and caches | **Built.** One standing predicate for each identifier a person is known by, so a person who used three devices is removed from all three. The predicate keeps hiding what arrives late, and a repeated request is one erasure because the predicates have stable identifiers. **The bytes go when compaction next passes**, which is what `AGENTS.md` asks for: an erasure is visible immediately because a read applies the predicate |
| Funnel, retention, path, and cohort query operations | **Partly.** Funnel, retention, path, and timeline are built and each has fixtures a person can work out by hand. **A cohort operation is not built**: `docs/QUERY.md` section 12 does not define one, and the cohort key it does define is the retention matrix's first period, which is built |
| Late event and identity-merge semantics | **Built.** A late `identify` binds the anonymous events that came before it under latest-known resolution and does not under event-time resolution, and both are tested. A merge is followed at read time and the stored rows are untouched |
| Saved analyses and dashboard composition | **Built.** An analysis stores its request and the algebra version it was written for, and one written for a newer version is refused with both numbers. A dashboard references analyses rather than embedding queries, a panel that names nothing is refused, and removing an analysis a dashboard shows is refused with the dashboard named. **Both are durable**, in the control catalog beside the workspaces and the keys. L112 |
| Collection policy UI for event and property filtering | **Partly.** The five levels compile, a narrower level overrides a wider one, a refusal accumulates and cannot be undone by a narrower level, invalid policy never replaces valid policy, the head applies the result at ingest, and **it is durable**. **The distribution to collectors is not built.** L107 and L112 |

## 4. Exit criteria

| Criterion | State |
| --- | --- |
| Anonymous-to-known conversion works without cross-project leakage | **Passes.** `an_anonymous_visitor_becomes_a_known_end_user_from_the_moment_they_identify` covers both resolutions, and `two_projects_that_use_the_same_anonymous_identifier_are_two_different_people` proves the isolation: the graph counts the rows it set aside, so a test can assert the filter did work rather than that nothing happened to be there |
| Funnel and retention fixtures have explainable exact results | **Passes.** Ten fixtures, each small enough to work out by hand, and each one's comment says the answer and why before the assertion says what it is. Ordered against unordered, a window that closes, an exclusion that voids a sequence, a person counted once however many times they did a step, a person who returns twice in one period, and first-time semantics |
| Query cost guards reject pathological analysis safely | **Passes.** `a_funnel_with_more_steps_than_the_guard_allows_is_refused_and_the_guard_is_named`, `a_retention_matrix_longer_than_the_guard_allows_is_refused`, and `a_path_deeper_than_the_guard_allows_is_refused`. Each is a typed `budget-exceeded` that names the budget and the limit, and each is checked **before** the work rather than after |
| The reference application end user uses the web, rich, and mobile clients, and the resulting funnel, retention, and timeline results match the ledger | **Passes.** `the_reference_application_end_user_uses_three_clients_and_the_analyses_match_the_ledger`. The simulator writes what it expects before it sends anything: four people, three surfaces each, three return days. The test then asks TallyOwl the same questions through the head's own query executor and compares |

## 5. What the Phase 7 fixes gave, measured

`docs/BENCHMARKS.md` section 20 holds the method and the whole table. Both runs
are the full load harness, the same seed, `--release`, from an empty `data/`,
one voter, drained to completion.

| Measure | Section 19 | Now |
| --- | --- | --- |
| Consensus log, bytes for each event | **798.3** | **36.6** |
| Consensus log, bytes for each batch | **141,389** | **6,480** |
| Whole `data/head`, bytes for each event | **1,034.2** | **259.2** |

**The log is 21.8 times smaller and the whole data directory is 4.0 times
smaller.** A replicated tablet needed about five times the disk of an
unreplicated one; it now needs **1.27 times**, 259.2 against the 204.1 that the
unreplicated run of the same harness measured on the same day.

**Say what this run measures, and what it does not.** A group snapshots every
4,096 entries and this run reached 2,964 batches, so **no snapshot was taken and
nothing was purged**. Every byte of the 21.8 times came from compressing the
entry. The bound is the larger of the two changes and it is proved by a test
rather than by this run: a snapshot seals the store first, which is what makes a
purge safe, and a tablet's log is bounded at about 4,608 entries however long
the installation runs. This measurement cannot show that, and section 20.2 says
so rather than letting the number imply it.

**The collector accept path and the point lookup did not move**, which is what
Phase 7 established and this confirms: the point lookup is identical to a
hundredth of a millisecond, because a read is answered from the local replica
and asks consensus nothing.

## 6. What is not built, and where it would go

Ranked by what they cost.

1. **The collection policy is not distributed to collectors.** L107. The head
   applies it, so nothing wrong is stored; a blocked event still costs a batch,
   a queue write, and a delivery. `docs/POLICY.md` section 7 describes the fetch
   and the cache, and the collector's `fetch-policy` already exists and answers
   "there is nothing to apply". **This is the first thing to build next.**
2. **Nothing drives a segment copy on a timer.** A copy is a call rather than a
   background task, so a replica that joins and falls behind past the purge
   point needs an operator to start its catch-up. The interface, the parity
   check, the install, and the movement stage machine are all built and tested.
3. **The general aggregate is not pushed down.** L101. A count and a trend are
   computed on the tablet and merged; the head's own measures are computed over
   rows, so a scan that feeds one moves rows. Correctness is preserved and the
   design's intent is not.
4. **A retention month is 28 days.** L110. `docs/QUERY.md` section 12.2 asks for
   calendar periods in a supplied timezone and this build carries no timezone
   database. The result names its period, so nothing is claimed that is not
   true.
5. **Attribution is refused by name.** The operator shape is in the contract and
   Phase 9 owns the models.
6. **A copy onto a target that already holds overlapping data is refused rather
   than reconciled.** L097. Two replicas that applied the same entries build
   differently shaped segments, and nothing in the segment format tells a copied
   one from a locally built one.
7. **The charts still have no templates.** True before Phase 7 and still true.

## 7. Everything marked `Revisit: yes`, with a recommendation

Nine of the sixteen entries added in this run are marked for revisit, and one of those nine was closed the same day.
`docs/ALPHA_REPORT.md` section 5 holds the same list for Phases 1 to 6 and
`docs/PHASE7_REPORT.md` section 8 for Phase 7.

| Entry | What | Recommendation |
| --- | --- | --- |
| L097 | A copy onto a target that already holds data is refused | **Leave it.** Reconciling needs a way to tell two replicas' segments apart, and neither the segment format nor the manifest carries one. Both cases this exists for start from nothing |
| L099 | The snapshot threshold and the log to keep are constants | **Make them settings once a cluster has run** long enough to show what a real tablet's batch size is. A setting nobody has measured is a constant with more places to be wrong |
| L101 | The general aggregate is not pushed down | **Build it when a cell has more than one tablet in earnest.** It is the performance half of the fan-out and it needs measures and dimensions mapped onto partial states, which not every measure has |
| L103 | The identity lookback grows with the installation's age | **Materialise the graph** when it costs enough to notice. This module stays as the thing that rebuilds it |
| L104 | Event-time resolution is implemented and nothing asks for it | **Put it on the contract.** A cohort question genuinely wants it, and the code is already there |
| L106 | Policy and saved analyses are in memory | **Done.** L112 built it: three record kinds in the control catalog, read back at start-up, written before they are applied |
| L107 | The policy is not distributed to collectors | **Build it next.** The durable half is done, and this is the other end of the same piece of work: `docs/POLICY.md` section 7's fetch and cache |
| L110 | A retention month is 28 days | **Add a timezone database** and use the `timezone` field the contract already carries |
| L095 → L099 | The consensus log | **Closed.** Kept here because the entry still reads `Revisit: yes` and a reader following the log would look for an open question that is closed |

## 8. Anything blocked

Nothing. No credential, no external service, and no csilgen capability was
needed. Both contract changes validated and generated for Rust, Go, and
TypeScript on the first attempt.

One contract change is worth naming because it broke a rule the project holds:
`get-policy` changed from `ListRequest -> Empty` to
`PolicyRequest -> CompiledPolicy` rather than being added beside. L108 says why
— nothing answered it and nothing called it, and `Empty` is the shape of a
placeholder rather than of a contract — and records the rule it stepped around.
