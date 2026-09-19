# Phase 7 report: replicated storage

> **Overtaken in part, 2026-08-05.** Everything below describes Phase 7 as it
> stood when the phase was reported. **Three of the seven items in section 8 are
> now built** — the sealed-segment copy, the consensus-log reclamation, and
> restore — and so are the first and the fourth items in section 4.
> [PHASE8_REPORT.md](PHASE8_REPORT.md) section 1 is the current state of all of
> them, and L097 to L102 hold the reasoning. This document is kept as it was,
> because it is what the state was when the decisions in it were taken, and
> because section 4's ranking is still the right ranking of what was left.

What was built, what was tested, and what was not built. Read
[IMPLEMENTATION_LOG.md](IMPLEMENTATION_LOG.md) L083 to L096 beside this: it
holds the reasoning and this holds the state.

**Phase 7 was built because the owner asked for it.** The implementation prompt
says "Do not start Phase 7", and it gives the reason: replicated storage adds
nothing to an alpha, because an alpha targets the home profile, which is one
node. The owner's instruction is the later and more specific one. L083 records
the disagreement rather than arguing it.

**A home installation is unchanged.** It has one node, one tablet, one voter,
`local-one`, and an empty `replication.listen`, so no group starts, no listener
opens, and no consensus message reaches any socket. The store the head uses is
the same local store it was before this phase.

**Seven of the eight exit criteria pass. One passes in part**, and section 3
says exactly which part and why. The one that passes at one voter rather than
three is the reference application, and the table says so.

**The measured cost is disk.** The first measurement said a replicated tablet
used **sixteen times** the disk of an unreplicated one, and asking why rather
than accepting it found a defect: serde encodes a `Vec<u8>` as a sequence, so
every batch was written as a CBOR array of integers instead of a byte string,
twice over. L096 fixed it and the log fell 4.2 times. **A replicated tablet now
needs about five times the disk**, 1,034.2 bytes for each event against 214.6,
and the excess is still the consensus log rather than the data: a raft entry is
a whole batch, and nothing purged it because TallyOwl's tablet snapshot
deliberately carries no rows. `docs/BENCHMARKS.md` section 19 holds the
measurement and L095 holds what is left to do. **It is still the first thing to
look at.**

The head also commits **18 percent fewer batches each second** at one voter,
which is one extra durable append and fsync for each batch and no network round
trip at all; a three-voter number will be worse and is not measured. The
collector accept path, the point lookup, and the aggregate did not move.

## 1. What was built

| Path | Holds |
| --- | --- |
| `csil/tallyowl-cluster.csil` | The contract: `TallyOwlReplication` (wire ID 4) and `TallyOwlCluster` (wire ID 5). 82 rules |
| `crates/tallyowl-cluster` | The whole phase: topology, directory, routing, consensus, groups, health, controller, replicated store, distributed query, movement, recovery, both services, and the scale simulations. 7,130 lines of source and 2,365 of tests |
| `crates/tallyowl-head/src/cluster.rs` | Where the head meets it. A home installation returns from the first branch |
| `crates/tallyowl-rpc/src/duplex.rs` | The duplex TLS carrier L058 asked for, fixed before anything measured replication over it |
| `charts/tallyowl/values.yaml` | Failure-domain spread, anti-affinity, and a disruption budget |

**972 Rust tests, 52 Go tests, and 32 TypeScript tests pass**, against 875 at
the alpha gate. Lint is clean in all three languages, `cargo fmt --check`
passes, and `./tools.sh gen-check` passes. Phase 7 added 97: 85 in
`tallyowl-cluster` plus one that decomposes its disk cost, five for the head
over a replicated store, one that runs the reference application against one,
two for the duplex TLS carrier, and three configuration rules.

**Eleven of the 85 run real consensus** over real loopback sockets against real
durable storage, with each node holding its own store, its own log, and its own
listener, and **eleven more reach `TallyOwlCluster` over a real socket** the way
an operator does.

**The contract grew by one file and no existing file changed.** Every existing
generated package regenerates byte for byte.

## 2. Deliverable by deliverable

`docs/PLAN.md` Phase 7 lists fourteen.

| Deliverable | State |
| --- | --- |
| Three-or-five-voter controller quorum for topology and configuration only | **Built.** `topology.rs` is the state machine and `ControllerCommand` is the whole change surface. The head starts a `cell-controller` group beside its tablet group, so a topology change goes through consensus at every size. A one-node installation runs a group of one, so there is one code path rather than two |
| Regional cell control and a small global project-to-cell directory | **Built.** `directory.rs` holds one row for each project and never tablet placement. **A one-cell installation runs the directory in process**, which is what CELLS.md section 3 describes for the home profile; the directory as its own quorum group is implemented and the head does not start one |
| Virtual-shard to tablet mapping, placement, split, merge, and movement | **Built.** 4,096 fixed virtual shards, derived from tenancy and an affinity key and never assigned. Split, merge, and placement are commands with their rules in the state machine. Movement is a stage machine that refuses to publish before the copy is proved |
| Existing Rust consensus implementation integrated as multiplexed three-voter tablet groups | **Built.** openraft 0.9, per D15 and D27. `raft/storage.rs` is a durable log on redb; `raft/network.rs` is one CSIL connection for each **peer**, not for each group; `groups.rs` is the registry and the one place threads meet the async runtime |
| Write replication with quorum receipts, without a cluster-wide storage-node consensus group | **Built.** A node starts a group when the controller places it there and not before. A test asserts that a node which does not hold a tablet refuses its messages by name |
| Non-voting read and export replicas | **Partly.** A learner joins online, follows the log, and does not enlarge the write quorum, with a test. A replica reports its exact applied watermark. **An export replica has no behaviour of its own yet**: it is a learner with a different role name, and the longer segment window STORAGE.md section 8 permits is not built |
| Distributed query planning and partial aggregation for the generic event slice | **Partly.** Planning, the partial states, the merge, and the `partial-aggregate` operation are built and tested, and a storage node computes count, trend, and exact lookup locally. **The head's query executor does not fan out yet**: it answers from its own replica. See section 4 |
| Correctness-first query failure and partial-result semantics, and exact high-cardinality lookup across tablets | **Built.** A missing tablet refuses with `incomplete-result` unless partial mode was asked for; a partial result names what is missing and can never mark itself complete; the merged watermark is the smallest and the merged staleness is the largest; an exact lookup concatenates and never samples |
| Online replica addition and removal, and tablet movement | **Built** for membership, with a test that adds a learner to a live group. **Partly** for movement: the stage machine, the parity check, and the placement change are built and tested, and nothing drives the segment copy automatically. See L087 |
| One write region for each tablet, and fenced regional failover | **Built.** A failover raises the epoch, and a write from the old epoch or from another region is fenced with the reason. A failover onto a region with no replica is refused |
| Configurable `local-quorum` and `remote-one` receipts, and refusal of `local-one` on a multi-voter tablet | **Built.** The refusal is in the state machine, in the configuration check, and in the store, and it says what to use instead. `remote-one` waits for a copy outside the write region and refuses rather than degrading |
| Replicated tombstone and compaction generation safety | **Partly.** `TabletCommand::Tombstone` and `TabletCommand::CompactionGeneration` are replicated and applied idempotently, and the generations are part of a tablet snapshot. **The head still applies an erasure to its local store** rather than proposing it, so a multi-voter tablet would not carry a tombstone to its followers today |
| Cluster snapshot, bootstrap, and restore | **Partly.** Snapshot and bootstrap are built; a snapshot is checksummed and a damaged one is refused before it is used. **`restore` refuses**, names the snapshot, and points at the documented path. See L090 |
| Failure-domain-aware Helm placement and disruption rules | **Partly.** The values exist and are documented: topology spread across zones and then nodes, required anti-affinity, and a disruption budget as a count rather than a percentage. **The charts still have no templates**, which was true before this phase |

## 3. Exit criteria

| Criterion | State |
| --- | --- |
| Acknowledged batches survive the agreed number of storage-node losses | **Passes.** `three_voters_commit_a_write_and_it_survives_losing_one_of_them`: three voters over real sockets, a committed write, one voter lost, and the next write still commits |
| Leader loss during write returns either a prior receipt or one logical retry | **Passes.** `losing_the_leader_during_a_write_costs_one_retry_and_never_two_commits`: the leader goes away with the write committed, the survivors elect a new one, and the retry with the same batch ID comes back deduplicated with one row in the store |
| Read replicas expose exact freshness and bounded-stale behaviour | **Passes.** A replica reports its exact applied watermark and how old its newest data is; `replica_satisfies` refuses a `committed` read against a non-voter or a replica behind the requested watermark, and refuses a `bounded-stale` read outside the caller's tolerance. **The freshness number is "how old my newest data is" and not "how far behind the leader I am"**, which a follower cannot know without asking; the type says so where it is defined |
| Tablet movement completes online with checksummed parity | **Partly.** The parity check is real and tested in both directions: a damaged segment and a short segment each stop the move and leave the source owning the data, and `publish` refuses before the copy is proved. **Nothing drives the copy automatically**, so a movement is a procedure with a verified gate rather than one command |
| A multi-node topology test proves that only controllers and each tablet's replica set participate in their respective consensus groups | **Passes.** `a_node_that_does_not_hold_a_tablet_refuses_its_messages_by_name`, and `many_groups_on_one_node_share_one_connection_for_each_peer` for the transport half |
| Existing cells continue data operations during a global-directory outage | **Passes.** `a_cell_keeps_working_when_the_global_directory_is_gone`: the cached assignments still answer, placement changes refuse, and the refusal says the data path is unaffected |
| A reusable role token enrols permitted nodes but cannot change a voter set | **Passes**, from Phase 4 and unchanged. `NodeRole` has no name for a voter, so it is a property of the contract rather than a check somebody could forget, and `a_role_token_has_no_way_to_ask_for_a_voter` asserts it. Creating a voter set is `bootstrap-group` on `TallyOwlCluster`, which a token cannot reach |
| The reference application runs unchanged against the replicated installation and produces identical ledger results | **Passes at one voter.** `the_reference_application_produces_the_same_ledger_against_a_replicated_installation` runs the same scenario against the same collector, head, and query service, with the head's store behind a real tablet consensus group with a durable log, and compares against the ledger the simulator wrote before it sent anything: the logical event count, the breakdown by kind, the breakdown by name, the deliberate duplicates, and every exact lookup. **That is one voter, not three.** The application against a three-node installation was not run; three voters over real sockets are covered in `tallyowl-cluster`'s own tests |

## 4. What is not built, and where it would go

Ranked by what they cost.

0. **The consensus log is a second full copy of every batch and nothing
   reclaims it.** L095 and BENCHMARKS.md section 19. It is listed above the
   others because it is measured rather than predicted, and because the fix for
   it is item 2 below. The encoding defect inside it is fixed (L096); what is
   left is the entry itself and redb's amplification.
1. **The head's query executor does not fan out.** `query.rs` in
   `tallyowl-cluster` plans, merges, and holds every partial-result rule, and
   `crates/tallyowl-head/src/query.rs` still answers from the local store. The
   join is one function in `QueryService` that asks every readable tablet
   through `partial-aggregate` and merges. Until it is there, a multi-tablet
   installation would answer from one tablet and **would not know it was
   short**, which is the failure FAILURE_MODES.md section 2 ranks worst. It is
   the first thing to build next.
2. **Nothing drives the segment copy.** L087. A replica added to a tablet whose
   log has been purged past its position will not catch up on its own, and a
   movement needs an operator to move the segments. The interface, the chunking,
   and the parity check exist; the background task does not.
3. **`restore` refuses.** L090. The documented path — restore the data
   directory from the ordinary backup and start the node — works and the
   command does not, and the refusal says so rather than acknowledging.
4. **An erasure is not replicated.** The command exists and the head does not
   propose it. A single-voter tablet is unaffected, which is every installation
   that exists today.
5. **The charts have no templates.** True before this phase and still true. The
   placement values are declared and documented and nothing renders them.
6. **An export replica has no behaviour of its own.**
7. **The generation check on consensus messages does not fire between healthy
   nodes.** It is deliberate and L094 says why: a node that is behind on
   topology learns the generation through the transport that would be refusing
   it. The fence that protects a write is the epoch check on the write path, and
   that one is always on and is tested.

## 5. Failure work

`docs/PLAN.md` Phase 7 names four, from FAILURE_MODES.md section 6.

| Failure work | State |
| --- | --- |
| A node that is alive but slow, asserting detection, the reported cause, and `unknown` when no cause can be established | **Built and tested.** A node is slow when it is much slower than its group's median for long enough, so a busy hour does not make every node slow and fast hardware does not hide a sick one. The cause table is ordered most specific first, and every row has a test. `unknown` is returned and reported as a cause, and the condition still carries every raw reading beside the group median. One node on its own is never called slow |
| Quorum lost permanently, recovered by restore | **Partly.** `Recovery::recommended` is `Restore`, a snapshot is checksummed, and a damaged snapshot is refused. The command refuses; see L090 |
| Quorum lost permanently, recovered by unsafe recovery, asserting the audit record, the degraded mark, and the mark reaching query and explain output | **Built and tested.** All four requirements of section 6.2 in one test, because they are one intention: the tablet must be named twice, a blank reason is refused, the audit record carries the survivor's watermark and the reason, the range is marked, and the mark never expires on its own. A merge that swallows a degraded range keeps the mark. `merge` in the query layer carries the mark into the result and the warning names the tablet |
| A voter with an exhausted disk, and quorum continuing without it | **Built and tested.** A node that reports it cannot make a write durable becomes `read-only` rather than `slow` — a different failure needing a different answer — and the recorded action says the quorum continues without it |

## 6. The scale simulations

`docs/PLAN.md` calls these a milestone and not a gate, and says not to block the
phase on hardware the project does not have. They count control work and measure
nothing; no number here is a benchmark.

| Simulation | Result |
| --- | --- |
| A 400-node cell, 600 tablets | Three controller voters. No consensus group larger than three. Every tablet's voters span at least two failure domains. Losing the controller quorum leaves all 600 tablets writable and stops placement changes |
| 10,000 nodes across 25 cells, 5,000 tablets, 500 projects | 500 directory rows — one for each project and none for each tablet. 75 controller voters across 25 cells, which is three each. No group larger than three |
| One 20-node cell, 200 tablets | The busiest node holds more than ten tablet groups, which is the multi-group design D27 measured at 600 instances and 21 MiB |

The two properties these exist to show are that the controller quorum does not
grow with the cell and the directory does not grow with the tablets. Both would
be visible here if they broke.

## 7. What was changed outside this phase

- **`crates/tallyowl-rpc`**: the TLS carrier is duplex. L058 said this belonged
  before replication was measured and it was done first. Two tests cover it: a
  fast call answered while a slow one is still running, and sixteen large calls
  at once on one session. See L084.
- **`crates/tallyowl-store`**: two lint failures that a newer clippy found in
  files this phase never touched, in `segment/format.rs` and `space.rs`. Both
  were cosmetic and both are fixed.
- **`crates/tallyowl-config`**: ten new settings, three new cross-setting rules,
  and the three values documents. A multi-voter node with no replication address
  is refused at startup, a merge threshold that would make a cell oscillate is
  refused, and a fan-out of nothing is refused.
- **`docs/DECISIONS.md`**: D15 carries a "Built" section saying which of the
  four things the prototype left open are now closed. Two are still open and the
  selection stays open on those two, exactly as D15 says.

## 8. Everything marked `Revisit: yes`, with a recommendation

Seven of the fourteen Phase 7 log entries are marked for revisit. This is that
list, in order. `docs/ALPHA_REPORT.md` section 5 holds the same list for
Phases 1 to 6 and does not cover these.

> **L087, L090, and L095 are built.** They were the three that were one item
> wearing three hats, and building the first unblocked the other two exactly as
> this section said it would. [PHASE8_REPORT.md](PHASE8_REPORT.md) section 1
> holds what each one now does, and section 5 of that report holds what the
> reclamation cost. The recommendations below are kept because they are what was
> recommended and they turned out to be right.

| Entry | What | Recommendation |
| --- | --- | --- |
| L083 | Phase 7 was built although the implementation prompt said not to | **Decide whether an alpha ships with it.** If not, `crates/tallyowl-cluster` leaves the default workspace members and a home build stops compiling openraft entirely. Nothing above the `Store` contract knows it exists |
| L087 | A tablet snapshot carries its marks, not its rows | **Build the segment copy.** It is the largest gap in this phase and it is what unblocks L090 and L095 as well. A replica added to a tablet whose log was purged past its position cannot catch up without it |
| L089 | The controller decides in every mode and acts in one | **Leave `recommendation-only` until a cluster has run**, which is what CELLS.md section 6 asks for. The automatic path is exercised by every test, so switching it on later is one setting rather than untested code |
| L090 | Restore is refused rather than half done | **Build it after L087**, which it needs. Until then the refusal naming the documented path is the honest behaviour; an acknowledgement would be worse than the refusal |
| L093 | What Phase 7 does not cover | **Two of D15's four open items are still open**: a partition that splits a group other than by isolating one node, and recovery from a corrupt or truncated log. Neither is large. D15's selection stays open on them and should |
| L094 | The generation fence is on the write path, not the consensus path | **Leave it.** Moving it onto consensus messages has to answer the deadlock first: a node behind on topology learns the generation through the transport that would be refusing it. The epoch check on the write path is what protects a write and it is always on |
| L095 | The consensus log is a second full copy of every batch | **First thing to look at.** Compression is the cheap lever and needs a number from real telemetry rather than the uniform fixture; the real fix is L087, after which the log can be purged |

Three of these are one item wearing three hats: **L087 unblocks L090 and L095.**
Building the sealed-segment copy is the single highest-value piece of work left
in this phase.

## 9. Anything blocked

Nothing. No credential, no external service, and no csilgen capability was
needed. The contract validated and generated for Rust, Go, and TypeScript on the
first attempt, and L085 records the three CSIL shapes that were considered and
rejected before the one that was used.
