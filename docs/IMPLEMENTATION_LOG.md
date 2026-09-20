# Implementation log

What the implementation decided that the design did not. This is the record of
choices, disagreements, deferrals, and design defects found while building. It
does not record routine work.

Entries are in the order they were made.

## Where the build stands

**Phases 1 through 6 are complete and the alpha gate is met. Phase 7,
replicated storage, is built with the gaps its own report names.**

`docs/ALPHA_REPORT.md` holds the state of Phases 1 to 6 and the load-test
results. `docs/PHASE7_REPORT.md` holds the state of Phase 7, deliverable by
deliverable and exit criterion by exit criterion. This file holds the reasoning
for all of them. The tables below cover Phase 3.

The header text below this point is from earlier in the build and is kept as it
was written.

One thing inside a Phase 3 deliverable is deferred and named in the table below:
cold tiering is built against the `ColdStore` boundary with a filesystem
implementation and a bounded local cache, and the bucket backends behind that
boundary are their own crate. See L032.

The status here now checks all three of the lists `docs/PLAN.md` gives for a
phase: exit criteria, deliverables, **and failure work**. Checking the first two
is what let disk exhaustion look done when none of it existed (L036).

| Phase 3 exit criterion | State |
| --- | --- |
| One binary runs the head, query service, and durable store from one directory | **Passes** |
| One binary requires no external database | **Passes** |
| Accepted data survives abrupt process and host restart | **Passes** |
| A point or end user deletion disappears immediately, rewrites only intersecting bounded local segments, and erases cold data by key destruction | **Passes** |
| A read of cold data after key destruction fails and cannot recover the value | **Passes** |
| A tombstone hides a matching event that arrives after the erasure request | **Passes** |
| Mostly-unique request IDs remain exactly retrievable without scanning every retained segment | **Passes** |
| An interrupted cold-tier upload never evicts the only valid segment copy | **Passes** |
| A clean Parquet export is queryable in DuckDB | **Passes**, against the real DuckDB command line; see L033 |

Of the fifteen required tests in `docs/FAILURE_MODES.md` section 14, twelve are
written and pass. All eleven in `docs/SEGMENT_FORMAT.md` section 16 are written
and pass. The three that remain need a second copy to repair from (test 3), an
unsafe-recovery path (test 11), and slow-node detection (test 12). All three are
properties of a replicated installation, which Phases 6 and 7 build; none can be
written against a single node.

| Phase 3 deliverable | State |
| --- | --- |
| The segment and manifest format | Built |
| Compact native column pages, no Arrow or Parquet in the always-on path | Built |
| Typed sparse dynamic columns, exact locator and posting indexes | Built |
| Checksummed framed WAL with durable receipts and crash recovery | Built |
| Embedded transactional KV catalog | Built on redb |
| Bounded segment writer and manifest generations | Built |
| Raw and typed event segments | Typed built |
| Tombstones and affected-segment compaction | Built, with all four races of FAILURE_MODES section 8 |
| Manifest snapshot query with time, project, and kind pruning | Built |
| Exact lookup on every correlation field and custom property | Built |
| Hot and warm to cold object-storage tiering, with a bounded local cache | Built against the `ColdStore` boundary, with a filesystem implementation and a bounded cache. Bucket backends are their own crate; see L032 |
| Optional Parquet exporter and DuckDB verification | Built in `tallyowl-export`, verified with real DuckDB; see L033 |
| Snapshot, restore, and catalog rebuild command | Built, with `tallyowl-head snapshot`, `restore`, and `rebuild`; see L035 |
| Per-project keys and cryptographic erasure for the cold tier | Built, per D61 |
| Storage and capacity metrics | Built, and reported by a running head; see L034 and L038 |

| Phase 3 failure work | State |
| --- | --- |
| Torn and reordered writes | Built, in `tests/segment_format.rs` |
| Crash before and after every fsync and catalog boundary | Built, in `tests/segmented_store.rs` |
| Orphan, partial, and corrupt segments | Built |
| Commit success and receipt loss retry | Built |
| Tombstone visibility during concurrent reads | Built |
| Disk full at each point of FAILURE_MODES section 10 | Built; see L037 and `tests/exhaustion.rs` |
| Format upgrade and downgrade refusal | Built |
| The three integrity levels of D57, and `incomplete-result` naming a damaged segment | Built |
| Generation pinning and the compaction grace period | Built |
| An erasure that lands mid-compaction | Built |
| A crash between an erasure commit and its acknowledgement | Built |
| A catalog rebuild with snapshots and without | Built; see L035 |

What exists: 13 hand-written Rust crates, a Go app driver, a TypeScript browser
package, the reference application in Go, the Python tooling behind `tools.sh`,
3 generated packages for each of Rust, Go, and TypeScript, 22 golden vectors,
and 484 Rust tests, 11 Go tests, and 11 TypeScript tests. Lint is clean in all
three languages and the generated-code drift check passes.

Seven findings changed a design document or a decision rather than only the
code: the contract could not be encoded in two of its three languages (L017),
the page checksum did not cover the null bitmap (L020), the decompression ratio
limit was written for what an attack looks like rather than what telemetry looks
like (L021), two concurrent commits could share a watermark (L027), a snapshot
dropped rows that were acknowledged but not yet in a segment (L035), disk
exhaustion had a configured reserve that nothing read (L036 and L037), and the
storage instruments were never declared by the binary that sampled them
(L038).

Not built, with where each goes: see L015 for the Phase 1 remainder, L032 for
the cold-tier bucket backends, and `docs/PLAN.md` for Phases 4 through 11.

## L001. Generated code is committed, and `.gitignore` changes

**Phase:** 1
**Decision:** Remove `/generated` from `.gitignore` and commit the generated
Rust, Go, and TypeScript packages.
**Why:** `docs/CI-CD.md` section 5 says the generation job compares a fresh
generation "with checked-in generated output". That cannot happen if the output
is not checked in. Committing it also means a clean clone builds without running
the generator first.
**Cost to change:** cheap. Add the path back and change `gen-check` to compare
against a checksum manifest instead of a tree.
**Revisit:** no. The two documents disagreed and only one reading works.

## L002. `TypedValue` uses `null` rather than `nil`

**Phase:** 1
**Decision:** `csil/types/common.csil` now says `TypedValue = null / bool / ...`
instead of `nil`.
**Why:** `nil` and `null` are both accepted by the csilgen validator, and
`docs/csil-spec.md` lists `nil` as the basic type name. The Rust generator maps
`null` to `()` and has no mapping for `nil`, so a `nil` arm emitted
`Variant0(nil)`, which is not a Rust type and does not compile. Go and
TypeScript were unaffected. The one-word change uses a synonym the tool already
supports.
**Cost to change:** cheap, and it changes no bytes on the wire.
**Revisit:** no, on the TallyOwl side. Worth telling the csilgen maintainer that
its Rust generator does not handle the spelling its own specification documents,
because the next project will hit it too.

## L003. A recursive CSIL type travels as encoded bytes

**Phase:** 1
**Decision:** In `csil/tallyowl-control.csil`, a node that contains a node now
holds `ExpressionRef` or `QueryNodeRef`, which are `bytes` holding the encoded
child, plus two one-field records (`ExpressionNode`, `QueryNodeBox`) that give
the encoded child a name the codec exposes.
**Why:** `Expression` and `QueryNode` were directly recursive. The csilgen Rust
generator emits no indirection for a recursive type, so `types.rs` failed with
"recursive types have infinite size". Go and TypeScript generated and compiled
fine, so this is a Rust-generator gap rather than an invalid specification.

Section 10.1 of the implementation prompt requires arguing against the tool
being wrong first. Three shapes were worked through:

1. **Bytes indirection at each recursion point.** Works. It has precedent inside
   this repository already: `TailRule.expression` in `tallyowl-collector.csil`
   is a `bytes` field holding an encoded expression, with the stated reason that
   the control specification owns the expression schema. The specification was
   inconsistent with itself, and this makes it consistent.
2. **A flat node arena with index references.** Also works, and is arguably
   better for a query budget, because a whole tree decodes once. It costs index
   validation, since a cycle or an out-of-range index becomes possible, and it
   turns each node into a record of optional fields.
3. **Bounded-depth unrolling into `Expression1`, `Expression2`, and so on.**
   Rejected: an arbitrary depth limit with no principle behind it.

Two of the three work, so the prompt's own rule says do not escalate to a
csilgen request: the burden of proof needs three shapes that *fail*. Shape 1 was
chosen as the smaller change with existing precedent.

Two properties came with it and both are wanted: a reader can bound a subtree
before decoding it, which a query budget needs; and nesting depth becomes a
decode count rather than a type, so a hostile tree cannot exhaust a stack during
parsing. The cost is one encode and decode step for each level, on a path that
is not high frequency.

**Cost to change:** moderate. It changes the wire, so it wants to happen before
beta. Moving to shape 2 later is a specification change plus one executor
change.
**Revisit:** **yes.** The owner may prefer shape 2, or may prefer to ask the
csilgen maintainer to box a recursive type in the Rust generator, which would
let the specification go back to the direct form with no wire change at all.
That last option is the only one that costs nothing in Go and TypeScript
ergonomics, and it was not pursued because 10.1 forbids escalating when a
working CSIL shape exists.

## L004. `package_name` and `go_module` were the wrong way round

**Phase:** 1
**Decision:** Each entry specification now sets `package_name` to the crate and
npm package name (`tallyowl-collector-api`) and `go_module` to the Go module
path.
**Why:** All three specifications set `package_name` to a Go import path. Per
`csilgen/docs/self-contained-packages.md`, `package_name` is the crate, gem, npm,
and pub name, and `go_module` is the Go module path. As written, the generated
npm package would have been named `github.com/CatalystCommunity/...`.

The `-api` suffix is deliberate: without it the generated Rust crate is named
`tallyowl-collector`, which collides with the collector service crate, and two
packages of one name cannot share a workspace.
**Cost to change:** cheap now, expensive after publication.
**Revisit:** yes, on the suffix only. `-api` reads well for "types, codecs, and
routing seams" but the owner may prefer `-client` or `-contract`.

## L005. Phase 1 storage is real and durable, not a stub

**Phase:** 1
**Decision:** `tallyowl-store` ships the full contract from D25 with a
directory-backed implementation that appends and calls `fsync` before it
returns, deduplicates on `(source_id, batch_id)`, and reads everything back on
open.
**Why:** `docs/PLAN.md` Phase 1 says "storage is a stub in this phase" and that
the path is not. A stub that loses data on restart would make every Phase 1
failure test meaningless, and `AGENTS.md` forbids mocking the storage interface,
so a fake would have proved only that the fake behaves like the fake. The
physical format is deliberately the simplest thing that keeps every promise the
contract makes, so Phase 3 replaces the implementation without moving the seam.
**Cost to change:** cheap. Phase 3 replaces the file format behind the trait.
**Revisit:** no.

## L006. Phase 1 resolves tenancy from the credential, not a catalog

**Phase:** 1
**Decision:** `TenancyResolver` derives a stable workspace, project, and source
ID from the credential with a non-cryptographic hash, resolves once, holds the
answer in memory, and checks revocation before the cache.
**Why:** D32 step 2 reads the mapping from the control catalog, and that catalog
arrives in Phase 4 with the scoped API key model. Everything else about the
shape is right from the first day: one resolution, held in memory, stamped on
every envelope, and a payload value discarded. Only the lookup changes.

The mapping has to be stable across a restart, or one application's data would
scatter across two projects.
**Cost to change:** cheap. Replace `derive` with a catalog lookup.
**Revisit:** no, but note that a home installation's project ID becomes a
different value when Phase 4 lands, so a home installation's existing data will
need remapping or a fresh directory.

## L007. Priority defaults are implemented at intake

**Phase:** 1
**Decision:** `priority_of` gives a batch the highest priority of its items,
using the five classes in `docs/DELIVERY.md` section 8.
**Why:** The design states the priority order and nothing said where it is
applied. Applying it at intake means the durable store already holds the
ordering when the queue is under pressure, which is when it matters.
**Cost to change:** cheap.
**Revisit:** yes. A whole batch takes the priority of its most important item,
so one conversion lifts 255 page views with it. The alternative is sealing a
critical event into its own batch, which the driver already does through
`critical()`, so this may be redundant.

## L008. The operational endpoint answers three paths and refuses every other

**Phase:** 1
**Decision:** A small threaded HTTP server serves `/livez`, `/readyz`, and
`/metrics`, returns 404 for any other path, and 405 for any other method.
**Why:** `AGENTS.md` forbids a generic HTTP ingest API and permits a service to
expose its own Prometheus and OpenMetrics endpoint. Answering exactly three
paths means the endpoint cannot grow into an ingest API by accident.
**Cost to change:** cheap.
**Revisit:** no.

## L009. `config check` fails on a key that matches no setting

**Phase:** 1
**Decision:** A key in the configuration file or a `TALLYOWL_` environment
variable that matches no setting makes `config check` exit non-zero, and the
loader reports it.
**Why:** The design is silent on an unknown key. `docs/FAILURE_MODES.md` section
2 prefers a visible failure to a silent one, and a misspelled setting that
changes nothing is the worst outcome available: the operator believes they
changed something. Note that a *service* still starts with an unknown key
present; only the check fails. Making startup fail as well would break an
upgrade where a chart carries a setting the older binary does not know.
**Cost to change:** cheap.
**Revisit:** yes. The owner may want startup to refuse as well, which would be
stricter and would make a rolling upgrade with a new setting harder.

## L010. Warnings are denied through the manifest, not the command line

**Phase:** 1
**Decision:** `[workspace.lints]` denies warnings, and each hand-written crate
opts in with `lints.workspace = true`. The generated crates do not opt in.
**Why:** `cargo clippy -- -D warnings` reaches path dependencies, so it failed
the build on a warning inside generated code. Generated code is never
hand-edited, so a lint that fails on it fails on something nobody may fix. The
manifest table scopes the denial to the crates this project writes.
**Cost to change:** cheap.
**Revisit:** no.

## L011. A network address is not personal data

**Phase:** 1
**Decision:** The log redactor no longer refuses a field named `address`. It
refuses `postal_address`, `street_address`, `home_address`, and
`billing_address` by name.
**Why:** Found by running the system rather than by reading the code. Every
service logs its own listen address on start, and the redactor was dropping it
and counting a refusal, because `address` was on the deny list as a stand-in for
a postal address. The bare word was too broad.
**Cost to change:** cheap.
**Revisit:** no.

## L012. The forwarder retries at a fixed first delay, not a full backoff curve

**Phase:** 1
**Decision:** A retryable delivery failure parks the task for one second. The
backoff table exists and only its first entry is used.
**Why:** Corndogs holds no attempt count, and `docs/DELIVERY.md` section 4 says
TallyOwl must carry retry counts and next-attempt time in its own task payload
until Corndogs grows a native scheduling contract. That payload change belongs
with the Phase 4 batch format rather than with the walking skeleton.
**Cost to change:** moderate. It needs an attempt count in the task payload,
which means the payload stops being a bare encoded batch.
**Revisit:** yes. This is a real gap: a head that is down for an hour currently
gets retried every second for that hour.

## L013. The head satisfies the receipt policy trivially, and says so honestly

**Phase:** 1
**Decision:** The head reports `satisfied_policy` from configuration, which in
the home profile is always `local-one`, and the store's `fsync` is what
satisfies it.
**Why:** `local-one` means one local voter and one fsynced copy. A home
installation has exactly one voter, so the policy is satisfied by the commit
itself. The configuration loader already refuses `local-one` on a multi-voter
tablet, so the head cannot report a policy it did not satisfy. Phase 7 makes
this a real quorum wait.
**Cost to change:** cheap.
**Revisit:** no.

## L014. Both charts carry the whole settings tree

**Phase:** 1
**Decision:** `charts/tallyowl/values.yaml` and
`charts/tallyowl-collector/values.yaml` each declare every setting, and a test
asserts that both agree with each other, with `tallyowl.example.yaml`, and with
the loader.
**Why:** `docs/DEPLOYMENT.md` section 4 puts settings at the top level of chart
values and says a rendered chart and a local file are the same document. There
is one loader, so each release needs a full configuration. The duplication is
real and the parity test is what makes it safe: a setting added to one document
and not the others fails the build.
**Cost to change:** moderate. A shared sub-chart or a library chart would remove
the duplication.
**Revisit:** yes. Three documents to update for one new setting is a papercut
that will be felt often.

## L015. Deferred from Phase 1

**Phase:** 1
**Decision:** These Phase 1 deliverables are not built, and here is where each
would go.
**Why:** Time. Each is listed so nothing is discovered missing later.

| Not built | Where it goes |
| --- | --- |
| `.reactorcide/jobs/` and `pipelines/` | `docs/CI-CD.md` section 3 gives the layout; the Python modules they would call already exist in `tools/tallyowl_tools/` |
| Compose file and `.local.example.env` | Repository root. The binary workflow needs neither, and `docs/PLAN.md` calls the container path a parity check rather than the fast path |
| Helm templates | `charts/*/templates/`. The values and the parity test exist; the templates that render them do not, so `helm template` does not run yet |
| `CONTRIBUTING.md` | Repository root, written against the `tools.sh` verbs that now exist |
| Go and TypeScript drivers reaching the collector | Phase 2 deliverable; the generated packages build |

**Cost to change:** each is additive.
**Revisit:** no, they are simply next.

## L016. The design documents now trail the code, and here is exactly where

**Phase:** 1
**Decision:** Record the drift here rather than editing the owning documents.
The owner reconciles them in one pass.
**Why:** The code is now ahead of four documents. `docs/CONVENTIONS.md` section
8 asks for the update in the same commit, and this entry is the deliberate
exception to that rule, not an oversight. Each item below is a concrete edit
somebody can make without rediscovering anything.

**`docs/DEPLOYMENT.md` section 4.** The schema holds 43 settings. The document
names 21 of them. These 22 exist in the code and not in the document:

`collector.apiKey`, `collector.listen`, `collector.maxBatchBytes`,
`collector.maxEventBytes`, `collector.maxProperties`,
`collector.operationalListen`, `collector.roles`,
`compatibility.openTelemetry.enabled`, `compatibility.openTelemetry.listen`,
`compatibility.prometheus.targets`, `corndogs.deliveryQueue`,
`corndogs.endpoint`, `corndogs.fsyncMode`, `corndogs.quarantineQueue`,
`corndogs.sweepInterval`, `head.dataDir`, `head.endpoint`, `head.listen`,
`head.operationalListen`, `installation.profile`, `log.level`,
`query.maxRuntime`.

Two of them carry a rule the document already states in prose and does not
attach to a setting name: `corndogs.fsyncMode` is what section 5a means by a
volume that honours `fsync`, and `compatibility.openTelemetry.enabled` is what
D12 means by a receiver that never listens by default.

`crates/tallyowl-config/src/schema.rs` is the authoritative list. Every setting
there carries its type, its home-profile default, a description, and a valid
example, so the table can be regenerated from it rather than retyped.

**`docs/CONVENTIONS.md` section 3.** The document names four cases where
readiness must fail. The code declares five named checks, and the names are what
an operator sees in a health response:

| Check | Service | Fails when |
| --- | --- | --- |
| `durable-store` | collector | Corndogs is unreachable |
| `intake-listener` | collector | Intake is not listening yet |
| `retry-sweep` | collector | The sweep failed, or has not run inside three intervals |
| `storage` | head | The data directory is not open |
| `ingest-listener` | head | Ingest is not listening yet |

The third is the one worth adding to the document, because it covers a case the
prose does not: a sweep that succeeded once and then stopped being called. A
check on the last result reports health while retry has been dead for minutes,
so the check is on the age of the last sweep.

No new error code was added. The twelve in section 2 are exactly the twelve the
code implements, and a test asserts the set.

**`docs/QUERY.md` section 5.** Two changes.

First, an expression and a query node now hold an encoded child rather than a
direct one. See L003 for the reasoning. Section 5 still describes a plain typed
tree, which is true of the algebra and no longer true of the encoding.

Second, the section says expression nesting has a configurable maximum depth
with a default of 16. **Nothing enforces it.** The per-node size bound in the
specification is enforced by the codec, and a depth limit is not. This is a real
gap rather than a documentation one, and it belongs with the query budget work.

**`csil/README.md`.** The package emission section does not mention that
`package_name` names the crate and npm package while `go_module` names the Go
module path, which is the mistake L004 fixed, nor the `-api` suffix that keeps a
generated crate from colliding with a service crate of the same name.

**Cost to change:** cheap for the three documentation items. The QUERY.md depth
limit is a code change and is moderate.
**Revisit:** **yes**, on the depth limit specifically. An unenforced limit that a
document promises is worse than no limit, because a reader budgets against it.

## L017. The contract could not be encoded in two of its three languages

**Phase:** 2
**Decision:** Replace every bare CSIL choice whose arms a dynamically typed
language cannot tell apart with a record that carries an explicit discriminant.
This changes `TypedValue`, `Measurement`, `TelemetryItem`, `ExpressionNode`,
`QueryNodeBox`, and `QueryRequest`.

**Why:** Phase 2 exists to make the contract trustworthy across languages, and a
measurement showed it was not. The generated Go and TypeScript clients were run
against the specification as written:

| Value | Go produced | TypeScript produced |
| --- | --- | --- |
| A `text` property | `[0, null]` | Threw |
| An `int` property | `[0, null]` | Threw |
| A page-view item | Correct | Threw |
| An error item | Correct | Threw |

Two separate causes, and only one of them is TallyOwl's to fix.

**The generator defects.** The Go generator emits `case interface{}` first for a
`null` arm, and a Go type switch matches every value against it, so every
`TypedValue` encoded as the null arm. The TypeScript generator emits `if (true)`
for the decimal arm, so a text value was cast to a decimal and threw. Both are
csilgen defects. Neither is a missing capability, so section 10.1 of the
implementation prompt says do not open a request, and `AGENTS.md` forbids
hand-editing the output. They are recorded here for the owner to pass to the
csilgen maintainer.

**The specification defect, which is ours.** Even a perfect TypeScript generator
cannot separate `int` from `uint` from `float`: all three are one JavaScript
`number`. csilgen's own wire contract says so and states the consequence —
"among several general arms that share one runtime dispatch type, the first
declared wins". By that rule a union of fifteen record types is fifteen arms
sharing the dispatch type `object`, so a TypeScript client encodes every payload
as `EventPayload`. The whole browser package, which D2 makes a maintained
deliverable, was unable to send anything but an event.

**The three shapes worked through**, as section 10.1 requires:

1. **Reorder the union so `null` is last**, keeping the bare-choice form. Fixes
   Go, because the `interface{}` arm then stops shadowing the concrete ones.
   Fails TypeScript: `int`, `uint`, and `float` still share `number`, and every
   payload record still shares `object`.
2. **Wrap each arm in its own single-field record** and keep a union of records.
   Fixes Go the same way, by giving each arm a distinct Go struct type. Fails
   TypeScript for the same reason as shape 1: every record is an `object`, so
   the collapse moves rather than disappearing.
3. **An explicit `kind` discriminant with one optional field for each arm.**
   Works in all three languages, needs no generator change, and is the shape
   section 10.1 names first among the inversions to try before an addition.

Shape 3 is what landed. `TelemetryItem` did not even need a new discriminant:
`envelope.kind` already selected the payload arm, and the specification's own
comment said so. The union simply did not use it.

**What proves it.** `golden/vectors.json` holds 22 vectors covering every value
kind, an absent optional, a present optional, an array, an enum, raw bytes, an
exact decimal, a nested record, and each discriminated shape. Rust writes the
file; Go and TypeScript build the same values and compare. All three agree byte
for byte, and a one-byte change to the file fails both other languages with a
message that names the vector.

**Cost to change:** moderate now, expensive after beta. It changes the wire, and
D31 gives no client compatibility window before the release candidate, so now is
when this costs least. It also costs bytes: a map with a `kind` key against a
two-element array. That cost falls on the wire and the append log, not on stored
telemetry, because after segment projection queryable data lives in native typed
columns rather than in CBOR. A batch is compressed and a driver hoists constant
properties into the batch envelope, so the encoded cost is smaller than the
shape suggests.
**Revisit:** **yes**, on one point only. If the csilgen maintainer fixes the two
generator defects **and** adds a way to declare a discriminant for a union, the
specification could go back to the compact form and save the bytes. Nothing else
here should change: the explicit shape is more readable, it is self-describing
on the wire, and it made three states checkable that were previously
unrepresentable — an item with no payload, an item with two, and a value whose
`kind` names a field it did not carry. `crates/tallyowl-wire` holds those checks
and each has a test.

## L018. The TypeScript transport arrives through the task runner

**Phase:** 2
**Decision:** `./tools.sh setup` clones csilgen at the pinned revision into
`.deps/csilgen`, which Git ignores, and the TypeScript packages reach the
transport by path.

**Why:** Rust pins the transport with a Git revision in `Cargo.toml`, and Go
pins the same revision with a pseudo-version in `go.mod`. npm can do neither: it
cannot depend on a subdirectory of a Git repository, and csilgen does not publish
the transport to a registry. The alternatives were a vendored copy, which drifts,
or a local path, which breaks a clean clone.

One revision is named in one place, `tools/tallyowl_tools/generate.py`, so the
generator and all three transports cannot drift apart. A workstation that already
has the sibling checkout clones from it rather than from the network.

**Cost to change:** cheap. When csilgen publishes the transport to npm, the
`.deps` step goes away and `package.json` names a version.
**Revisit:** yes. Publishing it is the right answer and it is not TallyOwl's to
make.

## L019. `./tools.sh test` runs every language, and refuses rather than skips

**Phase:** 2
**Decision:** `test` runs the Rust, Go, and TypeScript suites. A missing
toolchain fails the verb and names how to install it. There is no skip.

**Why:** The golden vectors only prove agreement when all three languages check
them. A suite that quietly did not run reports the same green as a suite that
passed, and the divergence in L017 is exactly what a silent skip would have
hidden. `test-rust`, `test-go`, and `test-ts` exist for a person working in one
language.

**Cost to change:** cheap.
**Revisit:** no.

## L020. The page checksum did not cover the null bitmap

**Phase:** 3
**Decision:** A page checksum covers the null bitmap and the compressed values,
rather than only the compressed values. `docs/SEGMENT_FORMAT.md` sections 6 and
10 now say so.

**Why:** A test found it. `a_corrupted_page_is_detected_when_the_row_is_read`
flips one byte inside a page and expects the read to refuse; the byte it
happened to flip sat in the null bitmap, and the read succeeded.

That is the worst failure this project has: a flipped bit in the bitmap turns a
value into an absent one, and a query then returns a wrong answer rather than a
smaller one. `docs/FAILURE_MODES.md` section 2 ranks a silent wrong answer above
a stopped request precisely because nobody investigates a wrong answer.

The document said "xxHash3-64 of the compressed bytes", which is exact and
leaves the bitmap out. The fix costs one hash over bytes already in memory.

**Cost to change:** cheap now, expensive after a segment exists in the field.
It changes the bytes a page checksum holds, so an older reader verifying a newer
page would report damage. Nothing is published yet, and D31 gives no
compatibility window before the release candidate.
**Revisit:** no.

## L021. The decompression ratio limit was set for a bomb and refused real data

**Phase:** 3
**Decision:** The ratio limit is 10,000 rather than 200. The absolute bound on
an uncompressed page is what protects memory; the ratio covers the case the
absolute bound misses, which is a small page claiming a large expansion.

**Why:** Also found by a test, and it is the more interesting of the two.
`docs/SEGMENT_FORMAT.md` section 14 asks for a decompression ratio limit, and
200 sounded generous. It is not: a real column where every row holds one release
name, one batch identifier, or one service name reaches a ratio in the
thousands. A first run refused its own segments.

**Real telemetry is far more compressible than a limit written in the abstract
assumes.** The whole reason a columnar format works is that a dimension repeats,
so the shape a bomb uses and the shape ordinary data uses are the same shape.
A ratio alone cannot separate them, and only the absolute size can.

**Cost to change:** cheap. It is a reader policy, not a format field.
**Revisit:** no, but note that the same reasoning applies to any future limit
written against what an attack looks like rather than against what the data
looks like.

## L022. What Phase 3 has and has not, and where the rest goes

**Phase:** 3
**Decision:** Build the format, the append log, and the catalog first, and wire
them behind the `Store` trait as one later step rather than incrementally.

**Why:** Each of the three is independently testable against its own
specification, and each carries failure tests that would be much harder to write
through the `Store` seam. The segment format has eleven required tests in
`docs/SEGMENT_FORMAT.md` section 16; nine of them are written against the bytes,
which is where they belong.

The cost is that no Phase 3 exit criterion passes yet, because the head still
runs on the Phase 1 `DirectoryStore`. That is stated plainly at the top of this
log rather than implied.

**What is left, and where each piece goes:**

| Not built | Where it goes |
| --- | --- |
| A `Store` implementation over the segment format, the WAL, and the catalog | `crates/tallyowl-store/src/segmented.rs`. The trait in `store.rs` does not move; `DirectoryStore` stays for the tests that use it |
| The tablet locator | `crates/tallyowl-store/src/locator.rs`. HIGH_CARDINALITY.md section 4 gives the shape: `(field_id, fingerprint, time bucket) -> candidate segments`, 64-bit fingerprints, time-partitioned runs |
| Compaction, and the tombstone rules around it | `crates/tallyowl-store/src/compact.rs`. FAILURE_MODES.md section 8 gives the four races and the rules for each |
| Generation pins and the garbage-collection grace period | The catalog. Section 8.1 rules 1, 3, and 4 |
| Cold tiering through OpenDAL | A feature of `tallyowl-store`, off by default. D24 gives the thresholds |
| The Parquet exporter | Its own crate. AGENTS.md forbids it as a dependency of the always-on path |
| Cold-tier encryption and cryptographic erasure | Blocked on D28's key design, which the decision register lists as open. FAILURE_MODES.md section 12 already records that cold tiering must not carry erasable data until it exists |
| Storage and capacity metrics | `tallyowl-obs` declarations plus the counters STORAGE.md section 14 lists |
| Snapshot, restore, and rebuild as a command | `tools/tallyowl_tools/` and a `tallyowl` subcommand |

**Cost to change:** each is additive, and none of them moves the `Store` trait.
**Revisit:** no on the order. Yes on one point: the cold tier is the largest
remaining item and it is gated on a design decision the owner has not made, so
it may be worth deciding whether alpha needs it at all. The home profile does
not use object storage by default, and D24 says so.

## L023. The owner settled four scope decisions, and they widen alpha

**Phase:** 3
**Decision:** Recording what the owner decided on 2026-08-02, because each one
changes what the remaining work is rather than how it is done.

| Question | Decided |
| --- | --- |
| Cold tiering, cryptographic erasure, and Parquet export | **In alpha.** Design the keys and build all three, rather than deferring them because the home profile does not use object storage |
| Phase order | **Hold the gate.** Wire the segmented store, then the locator, then compaction, then the rest of Phase 3, then Phase 4. Sequencing inside a phase can change; nothing is skipped |
| The two csilgen generator defects | **Write request documents** in `csilgen/docs/csilgen-requests/`, for a separate session to pick up |
| Query algebra for alpha | **Sort and union as well** as trend, breakdown, filter, limit, exact lookup, and trace assembly |

The first of these is the largest single change to the remaining work, and it
closes an item the decision register had listed as open since design. D61 is the
key design it needed. The fourth adds two general nodes that no alpha deliverable
names, on the reasoning that a dashboard that cannot order a breakdown is not a
dashboard.

**Cost to change:** the first is expensive to reverse once cold objects exist,
because a segment written under one key design cannot be read under another. The
other three are cheap.
**Revisit:** no. These are the owner's calls and they are recorded, not mine.

## L024. What D61 does not decide, and what I chose

**Phase:** 3
**Decision:** D61 leaves the cipher and the nonce discipline to the
implementation. The choices are AES-256-GCM, a fresh random 96-bit nonce for each
encrypted block, and the nonce stored beside the block it belongs to.

**Why:** A deterministic nonce derived from the segment identity would be
cheaper by twelve bytes for each block and is the kind of shortcut that is safe
right up until something rewrites a segment under one key. Compaction rewrites
segments, and a repeated nonce under one key with a counter mode is a total loss
of confidentiality rather than a degradation. Twelve bytes for each block against
that outcome is not a trade worth making.

AES-256-GCM over ChaCha20-Poly1305 because the hardware this runs on has AES
instructions and the cold path is bandwidth-bound rather than latency-bound.
Both are available under a licence compatible with Apache-2.0. Nothing in the
format depends on the choice: the algorithm travels in the footer beside the key
reference, so a later segment can use another one without a major version.

**Cost to change:** cheap for a new segment, and impossible for an existing one,
which is the ordinary property of an encrypted file rather than a defect.
**Revisit:** no.

## L025. DuckDB is a toolchain addition

**Phase:** 3
**Decision:** The Phase 3 exit criterion "a clean Parquet export is queryable in
DuckDB" needs DuckDB, which is not installed and is not in the shared
catalyst-tools bundle. `./tools.sh setup` will fetch it into the same
per-user location the other toolchains use.

**Why:** The alternative is verifying an export with the same Parquet library
that wrote it, which proves the library round-trips and not that the file is
readable by an independent tool. The whole point of the export contract is that
stored data stays accessible to something TallyOwl does not control.

The test refuses rather than skips when DuckDB is absent, for the reason L019
gives.

**Cost to change:** cheap. It is one more entry in the toolchain step.
**Revisit:** yes, on one point only. This adds a network fetch of a third-party
binary to `setup`, which the shared installer already does for seven other
toolchains. If you would rather DuckDB were a documented prerequisite than an
automatic download, say so and it becomes a check with an instruction.

## L026. The Phase 1 store is gone rather than kept beside the new one

**Phase:** 3
**Decision:** `DirectoryStore` is deleted. `SegmentedStore` is the only
implementation of the `Store` trait, and every test that exercised the contract
now exercises it against the real storage.

**Why:** L005 said Phase 1 would build a real durable store with a real contract
so that "Phase 3 replaces the implementation rather than introducing the
boundary". Keeping both would have made that half true: the contract tests would
have kept passing against a directory of JSON lines while the thing the head
actually runs went untested at that level.

The trait did not move. That was the whole bet of Phase 1, and it paid.

**Cost to change:** cheap, and there is nothing to change back to.
**Revisit:** no.

## L027. Two concurrent commits could share a watermark

**Phase:** 3
**Decision:** The commit watermark is allocated once, under the lock, before the
append log is touched.

**Why:** A test found it. The first version read `watermark + 1` in one lock
acquisition and wrote it back in another, so eight threads committing eighty
batches produced a watermark of 43.

A watermark is what a query result states it applies to, and D18 builds
`committed` reads on it. Two batches under one watermark would make them
indistinguishable to a caller that asked for a specific one.

A commit that then fails leaves a gap in the sequence. That is correct rather
than a defect: a watermark is a monotonic counter and was never a dense
sequence, and the catalog holds the highest one that actually committed.

**Cost to change:** cheap.
**Revisit:** no.

## L028. One process owns one data directory, and a restart waits

**Phase:** 3
**Decision:** The catalog takes an exclusive lock. `SegmentedStore::open_waiting`
waits a bounded time for a previous owner to release it, and the head uses it
with a 30-second bound.

**Why:** The Phase 1 store let two processes open one directory, because a file
of JSON lines does not care. The catalog does, and it should: two processes
writing one directory would corrupt it, and STORAGE.md section 4 describes a
single-node layout with one owner.

Failing immediately would turn a rolling restart into a crash loop, because a
scheduler can start the replacement before the old process has finished exiting.
Waiting forever would hide a real conflict. A bounded wait, then a message that
names the directory and says what to do, is the behaviour that fits both.

**Cost to change:** cheap.
**Revisit:** no.

## L029. The csilgen revision pin was documentation, and now it is checked

**Phase:** 3
**Decision:** `generate.py` compares the revision of the repository that built
the `csilgen` on the path against `CSILGEN_REVISION`, and warns when they
differ.

**Why:** `csilgen` is normally a symbolic link into a build inside the csilgen
checkout. A rebuild there silently changes what this repository generates with,
and the pinned revision above it was a comment rather than a pin. The docstring
already promised a check that did not exist.

It warns rather than refuses, because csilgen does not publish releases yet and
a developer working on both repositories at once is the ordinary case.
`gen-check` still fails the build on any output that actually drifted, so a
warning here and a hard check there cover both.

**Cost to change:** cheap. It becomes a refusal when csilgen publishes releases.
**Revisit:** no.

## L030. What the segment format gained that the documents did not ask for

**Phase:** 3
**Decision:** Two additions worth naming, because neither is in a design
document and both change behaviour a reader might otherwise assume.

**A retired segment keeps its catalog record.** `Catalog::swap` writes a marker
at the new generation rather than removing the row. A query that pinned an older
generation can still find what it resolved, which is what section 8.1 rule 1
needs, and a rebuild can still tell a retired segment from one that was never
published.

**The locator is rebuilt rather than patched on compaction.** A swap merges the
new runs, drops references to retired segments, and writes the whole set back in
one transaction. Patching would be cheaper and would leave a window where a
probe sees a run naming a segment that no longer exists. Section 8.4 rule 2 says
that is a wasted open rather than a wrong answer, so patching would be safe — it
is simply not worth the reasoning at this size.

**Cost to change:** the second is a performance change when the locator grows
past what fits comfortably in one transaction. The first is structural.
**Revisit:** yes, on the locator rewrite, once there is a measurement of what it
costs at a realistic run count. `prototypes/locator-bench` has the shape of that
measurement already.

## L031. Segment encryption, and the one thing it does not encrypt

**Phase:** 3
**Decision:** D61 is implemented. Pages and index blocks are sealed with
AES-256-GCM under a per-project key; the prologue, header, and footer stay
readable. Keys live in the catalog wrapped by an installation root key, and
destroying them is durable in the erasure ledger before anybody is told.

**Why the details matter, beyond what D61 already says:**

**The checksum is over the ciphertext.** Damage is therefore detected before any
attempt to decrypt, and a damaged encrypted page reports damage rather than
looking like a key problem. An operator chasing a failing device must not be
sent to look at key management, and the test asserts the message says so.

**A block is authenticated under its segment and its offset.** A page cannot be
moved between segments or between positions in one segment and still open. That
costs nothing and removes a whole class of tampering.

**A failure never says whether a key existed.** "No such key" and "wrong key"
worded differently would tell a reader whether a project had been erased, which
is the fact erasure removes. Both paths give one message naming all three
possibilities.

**A key never appears in a `Debug` rendering.** `RootKey` and `ProjectKey` print
`hidden`, and a test asserts it. A key that reached a log or a panic message
would undo the whole point, and CONVENTIONS.md section 4 already forbids it.

**Cost to change:** the algorithm and the key reference travel in the footer, so
a later segment can use another algorithm without a major version. An existing
encrypted segment cannot be re-read under a different design, which is the
ordinary property of an encrypted file rather than a defect.
**Revisit:** no.

## L032. Cold tiering verifies by content address rather than by a returned put

**Phase:** 3
**Decision:** An upload is verified by reading the object back and checking its
content address, and eviction verifies again rather than trusting a flag from
the upload.

**Why:** A put that returned is not proof. An interrupted upload, a truncated
write, and a bucket that acknowledged early all report success, and each one
would evict the only valid copy of a segment. The Phase 3 exit criterion is
exactly this, so the test file is a list of ways an upload can go wrong with the
same assertion in each: the local file is still there.

The second verification at eviction is not redundant. The interval between an
upload and an eviction is precisely where a bucket can lose an object, and that
is the last moment anything can notice.

**Not built:** bucket backends. `ColdStore` is the boundary and
`FilesystemColdStore` is a real implementation of it, which is what an operator
with a network volume has. STORAGE.md names Apache OpenDAL as the initial
candidate for bucket APIs, and it is async, so it belongs in its own crate with
its own runtime rather than pulling tokio into the always-on storage path. That
crate is the remaining step and it implements one trait with four methods.

**Cost to change:** cheap. The trait is four methods and the policy above it does
not care what is underneath.
**Revisit:** yes, on one point. Verifying by reading the whole object back
doubles the bytes moved for each upload. A bucket that returns a checksum on
write could verify without the read, and OpenDAL exposes that for the backends
that support it. The read-back is the honest default; the optimisation needs a
backend that can prove the same thing more cheaply.

## L033. The Parquet export is verified by a tool TallyOwl does not control

**Phase:** 3
**Decision:** `crates/tallyowl-export` writes the files, and every test then
queries them with the DuckDB command line fetched by `./tools.sh deps`.

**Why:** The exit criterion is "a clean Parquet export is queryable in DuckDB".
An export read back with the same Arrow and Parquet crates that wrote it proves
those crates round-trip. It does not prove the file is readable by anything
else, and readable by something else is the entire reason the export exists. The
tests therefore run real SQL: `sum(p_attempts)`, `min(p_score)`, a grouped count
by day, and an exact `sum(CAST(p_value AS DECIMAL(18,2))) = 1999.00` that would
fail if a decimal had passed through a float.

Three properties are asserted rather than assumed. An erased row is not in the
export, because STORAGE.md section 13 says visible tombstones apply before
export. Two projects never appear in one file. A property whose type changes
between rows produces separate typed columns rather than one column that lies.

**Where the dependency lives:** arrow and parquet are in this crate and nowhere
else. STORAGE.md section 13 requires that minimal collector, storage, query, and
dashboard builds do not contain them, and a separate crate is the only way to
make that structural rather than a promise.

**Cost to change:** cheap. Nothing depends on this crate.
**Revisit:** no.

## L034. A metric that cannot carry an end-user ID, enforced by the registry

**Phase:** 3
**Decision:** `crates/tallyowl-store/src/metrics.rs` declares every storage
gauge and counter up front, and `declare()` panics if the registry refuses a
name.

**Why:** CONVENTIONS.md section 6 says a metric never carries an end-user ID as
a label, and the registry already enforces the naming rule that makes that hard
to break by accident. Ignoring a refused declaration would leave a metric
missing at runtime with nothing said, so a refusal stops the process at start-up
instead. A gauge that is silently absent is worse than one that never existed,
because a dashboard shows a flat line rather than a gap.

The unit suffix rule caught three names during this work. Each was a count and
none said so.

**Cost to change:** cheap.
**Revisit:** no.

## L035. A snapshot carries the log, and it carries it last

**Phase:** 3
**Decision:** `snapshot::take` copies the segments, then the catalog and the
erasure record, and then the shard logs. `snapshot::restore` puts all of them
back. `SegmentedStore::snapshot` seals the open buffer first.

**Why:** A row that TallyOwl acknowledged and has not yet written into a segment
is only in the log. A snapshot of the segments and the catalog alone would drop
that row and still report success, which is the failure FAILURE_MODES.md section
2 ranks worst: an installation that looks healthy and answers wrongly. The first
version of this module did exactly that, and a test written to prove the
opposite is what found it.

The order is a choice between two wrong answers when the directory is being
written to. If the log travels last, the copy can hold a batch whose receipt did
not travel: the rows come back and a retry of that batch counts twice. If the
catalog travels last, the receipt exists for rows that are not there and the
rows are gone. Duplicated rows can be found and removed. Lost rows cannot. The
log travels last.

**The restore that does not publish.** Section 13 says a missing or corrupt file
causes a visible failure and restore never silently skips a file. Verification
therefore runs over the whole snapshot before anything is written to the
destination, so a restore that finds one damaged segment leaves nothing behind
for somebody to start by mistake.

**The rebuild says what it did not do.** Rebuilding by scanning segments
recovers one of the twelve things the catalog holds. `RebuildReport` names the
seven an operator has to act on, in the words an operator uses: every
application needs a new key, everybody signs in again, saved dashboards are
gone. Erasure records come back, because the erasure record is durable
independently of the catalog, and an erasure a rebuild could undo would not be
an erasure.

**The command surface:** `tallyowl-head snapshot`, `restore`, and `rebuild`.
They run in the same binary because one process owns one data directory, so an
operator stops the head, runs the verb, and starts it again. `restore` refuses a
destination that already holds data: mixing two installations together cannot be
taken back.

L022 expected these in `tools/tallyowl_tools/`. That was wrong. `tools.sh` is
the development loop and is not shipped to an operator, and a recovery command
that only a developer has is not a recovery command. FAILURE_MODES.md section 11
now names the three commands inside procedures 3 and 5, so an operator reading
the procedure reads what to type.

**Cost to change:** cheap.
**Revisit:** yes, on one point. Copying `catalog.redb` while the store has it
open is consistent for a directory that is quiet and is not a guaranteed
consistent read of a live database. A snapshot taken under write load should use
a redb-level copy when redb offers one. The sealing step and the log ordering
above reduce the window; they do not close it.

## L036. Disk exhaustion has no behaviour, and a setting nobody reads said so

**Phase:** 3
**Decision:** Record this as not built rather than let "every exit criterion
passes" stand for "Phase 3 is done".

**Why:** `docs/PLAN.md` lists disk exhaustion at each point of
FAILURE_MODES.md section 10 as Phase 3 failure work, and section 10 gives six
points and one behaviour for each. None of it exists. The append log accepts a
write it cannot make durable, segment publish has no reserve, compaction does
not abandon an attempt, and the catalog does not stop control writes.

`storage.reserveBytes` is the evidence. It is in the configuration schema with a
1 GiB default and a help string that says what it is for, and nothing in the
repository reads it. A setting that is documented, defaulted, validated, and
unread is worse than a missing setting: an operator who sets it believes they
have a reserve.

This was missed because the status table checked the nine exit criteria and the
fifteen deliverables, and the failure-work list is a third list that neither
covers. Disk exhaustion is in that third list. The table now carries a row for
it.

**What it needs, and where:** a free-space check in
`crates/tallyowl-store/src/segmented.rs` before the log append and before a
segment publish, an abandon path in `compact.rs`, a catalog write guard in
`catalog.rs`, and a readiness check that fails before the device fills rather
than when it is full. The reserve comes from `storage.reserveBytes`, which the
head already resolves.

**Cost to change:** moderate. It touches the write path, and the write path has
the watermark and receipt ordering that L027 shows is easy to get wrong.
**Revisit:** no on doing it. Yes on one point: section 10 says a reserve is held
back so that recovery can still write, and it does not say who enforces the
reserve when several processes share a device. On the home profile the head is
the only writer, and the answer for a shared device is worth stating before
Phase 6.

## L037. Disk exhaustion, and the reserve rule that shapes it

**Phase:** 3
**Decision:** `crates/tallyowl-store/src/space.rs` splits every write into a
bulk write and a recovery write. A bulk write may use the free space above the
reserve and never the reserve. A recovery write may use the reserve and is
refused only when nothing is left at all.

**Why:** FAILURE_MODES.md section 10 gives six points of exhaustion and one
behaviour for each, and one sentence that explains all six: the reserve exists
so the system can still write the metadata needed to recover. That sentence only
means something if some writes are held out of the reserve and others are let
in. Nothing else in the section says which, so this decides it and section 10
now records the split.

The six points and what each does:

| Point | What it does now |
| --- | --- |
| Append log | Refuses the commit before the watermark is allocated, so a refused batch takes no number and leaves no receipt |
| Segment publish | Puts the rows back in the open buffer and keeps the log range. The commit still succeeds, because the log is the durable record |
| Compaction | Builds every replacement, checks once, and abandons before it writes or retires anything |
| Catalog | Refuses control writes once the device is inside the reserve. Reads are untouched |
| Cold-tier cache | Throws the whole cache away. It is the one point with no refusal, because every byte of it exists in cold storage |
| Export | Refuses before it writes, and never displaces live data |

An erasure is a recovery write. It is small, it is an obligation, and section 9
already makes its record durable independently of the catalog. An installation
that could not accept a deletion request because a disk was full would be the
wrong way to be correct.

**A refusal names its point.** "The disk is full" does not tell an operator
whether ingest stopped or a compaction gave up, and those need different
actions. Each refusal carries the behaviour for its point in the words an
operator reads in a log line.

**Readiness fails early.** Section 10 says to fail readiness *before* the device
is full. The head samples the device every ten seconds and fails the
`disk-space` check while there is still a segment's worth of room above the
reserve, so a load balancer stops sending work while somewhere else can take it.

**`storage.reserveBytes` is now read.** It was configured, defaulted,
validated, documented, and read by nothing. See L036.

**Testing without a full device.** The tests pretend, through
`Space::pretend_free_bytes`. Filling a real device would need a device to fill,
would take minutes, and would leave a workstation in a state a failed test could
not undo. The write paths are what is under test. `statvfs` itself has its own
test that calls it for real, so the pretence cannot hide a broken reading.

**Cost to change:** moderate. The checks are in the write paths, and the write
paths hold the watermark and receipt ordering L027 shows is easy to get wrong.
Two of them changed shape to make this safe: a seal and a compaction now build
every file before writing any of it, so a refusal cannot leave half a publish on
disk for a later rebuild to adopt.
**Revisit:** yes, on one point. The reserve is per data directory and it is
enforced by one process. Two TallyOwl installations sharing a device would each
think it had the whole reserve. On the home profile there is one writer and
this is exact; a shared device needs an answer before Phase 6.

## L038. Storage metrics existed and no running process reported them

**Phase:** 3
**Decision:** `tallyowl_head::declare_metrics` declares the ingest instruments
and the storage instruments together, and a test reads the exposition endpoint
rather than the source.

**Why:** L034 built the storage instruments and asserted them in their own
tests. The binary called `Ingest::declare_metrics` and never
`tallyowl_store::metrics::declare`. Setting a gauge that nothing declared is
dropped without a word, which is by design at that call site, so `sample` ran
every ten seconds and wrote every value into nothing. A running head reported no
segment count, no log bytes, and no pin age.

This is the failure L034 was written to prevent, one level up. A gauge that is
absent looks like a system with nothing to say.

The test is what makes it stay fixed, and it works by rendering the metrics text
and looking for the names. A test that asserted `declare_metrics` calls both
functions would pass while the binary called neither.

**Cost to change:** cheap.
**Revisit:** no.

## L039. Credentials are issued, and the head is the only place one is stored

**Phase:** 4
**Decision:** A source credential is `tow_<key-id>_<secret>`, issued by
`tallyowl-head provision <project>` and printed once. The catalog holds a keyed
digest and never the secret. Collector intake resolves a credential through a
new `resolve-key` operation on `TallyOwlCollector`, which the head answers from
the control catalog, and holds the answer for a short period.

**Why:** L006 derived a tenancy from a credential with a hash, because Phase 1
had no control catalog. Every property of the shape was already right — one
resolution, held in memory, stamped on every envelope, a payload value
discarded — and only the lookup was missing. This replaces the lookup.

The credential travels whole rather than split into an identifier and a secret,
so the head owns the credential format and a change to that format reaches one
component instead of two.

**Three timings, and each bounds a different thing.** The cache period the head
returns bounds how long a revocation takes to reach an application. A grace
period bounds how long ingest survives a control-plane outage, and it extends
only what already worked: a credential the collector never resolved cannot start
during an outage, so an outage is never a way in. A refusal period bounds how
often a wrong credential reaches the head, so a client retrying a bad key cannot
turn a collector into a request amplifier.

**Cost to change:** moderate. It changes the wire by adding one operation, and
it changes what a home installation has to do before it can send anything.
`./tools.sh dev up` makes the key, so the loop still reaches a round trip with
one command.
**Revisit:** yes, on the default cache period. Thirty seconds is the whole of
revocation latency. A shorter period costs a round trip more often; a longer one
leaves a revoked application sending for longer. The right number depends on how
the owner expects a revocation to be used.

## L040. The Rust driver held a credential and never sent it

**Phase:** 4
**Decision:** `tallyowl_rpc::Client` carries a credential on the connection and
sends it on every call. The Rust driver sets it from its settings.

**Why:** Found by the key model, not by reading the code. `Client::call` passed
`None` for the authentication field, so every batch from the Rust driver arrived
with no credential and collector intake fell back to the connection default. The
Go driver had always sent one, so the two maintained drivers did not agree on
the thing the whole tenancy model rests on.

Nothing caught it because the fallback was the same credential the tests used.
The defect only became visible when two credentials existed and one of them was
revoked: the revoked key kept working and the replacement was refused, both for
the same reason.

**The credential belongs to the connection, not the call.** One application
holds one key. A service that read a key from each request body would let one
connection speak for two tenants, which is the shape D32 exists to prevent.

**Cost to change:** cheap.
**Revisit:** no. Two tests now assert it: one that a credential travels on every
call of a connection, and one that an empty credential travels as no credential
rather than as a distinct identity.

## L041. `dev up` asks the head for a key rather than inventing one

**Phase:** 4
**Decision:** `./tools.sh dev up` runs `tallyowl-head provision local` before it
starts the head, writes the printed credential to `data/collector.key`, and
`collector.apiKey` points at that file.

**Why:** TallyOwl issues keys, so the loop has to ask for one. The alternative
was a development-only credential that the home profile accepted and a real
installation did not, which is the development code path Phase 1 exists to
prevent.

It runs before the head starts because one process owns one data directory and
`provision` needs it to itself. It is the same command an operator types; the
loop only saves the typing.

**Cost to change:** cheap.
**Revisit:** no.

## L042. Corndogs never started from the task runner, and nothing said so

**Phase:** 4
**Decision:** `_service_command` returns the directory to run in, and Corndogs
runs from its own checkout.

**Why:** `go run main.go` was invoked with this repository as the working
directory, so it reported `stat main.go: no such file or directory` and the
loop reported that Corndogs did not start listening. Every test that needed a
durable queue used the in-memory stand-in, so no test noticed, and the failure
only appears when somebody actually runs the loop.

**Cost to change:** cheap.
**Revisit:** no.

## L043. The queue payload is a delivery task, not a bare batch

**Phase:** 4
**Decision:** The Corndogs task payload is a `DeliveryTask`, declared in
`csil/tallyowl-collector.csil`. It holds the encoded batch, its compression, its
uncompressed size, the attempt count, the acceptance time, the next attempt
time, and the last failure.

**Why:** `docs/DELIVERY.md` section 4 already said this had to happen:
"TallyOwl must implement retry counts, next attempt time, and batch lineage in
its task payload until Corndogs grows a native scheduling contract." L012
deferred it and named the cost, which was that a head down for an hour was asked
once a second for that hour.

**It is a contract rather than a private structure.** Intake and the forwarder
are independently deployable roles, so during a rolling upgrade an old forwarder
reads a new intake's payload. A forwarder that meets a task version it does not
know quarantines it and says which version it reads, instead of guessing.

**The batch travels compressed**, and only when compression helps. A batch is
CBOR full of repeated keys and repeated dimension values, so it usually helps a
lot: a 60-item batch with 12 dimensions each shrinks by more than half in the
test. Every byte saved is a byte the durable queue does not write, fsync, and
read back, and the queue is the part of this path bounded by a device.

**The uncompressed size is declared, and then checked.** The declaration is what
lets a reader refuse a payload before it produces one; the check afterwards is
because a declaration is a claim. A ratio bound is deliberately absent, for the
reason L021 gives: the shape that makes a columnar format work and the shape a
bomb uses are the same shape, and only the absolute size separates them.

**Cost to change:** moderate. It changes the bytes in the queue, so an upgrade
across it needs the version field that is now there. Nothing is published.
**Revisit:** no.

## L044. Retry stops at an age, and the age is half of a decision nobody has made

**Phase:** 4
**Decision:** `corndogs.maxDeliveryAge` defaults to 24 hours. A batch that has
been retried for longer goes to quarantine with a message that says automatic
retry stopped and why.

**Why:** D36 makes the deduplication window and the collector outage buffer one
decision: `dedup_window >= max_outage_buffer + max_replay_window + safety`. An
automatic retry that outlives the head's memory of a batch ID commits a second
logical batch, and no query can remove it afterwards.

**The other half is not built.** The head keeps a receipt for ever, so today
`dedup_window` is unbounded and any retry age satisfies D36 trivially. When
receipt expiry lands, these two numbers have to be chosen together rather than
separately, which is exactly what D36 says.

Backoff also gained jitter. The curve mattered less than the jitter: without it
every batch a head outage parked returns at the same moment, and the head meets
the whole queue on the second it recovers.

**Cost to change:** cheap for the age. Moderate when receipt expiry lands,
because the two numbers stop being independent.
**Revisit:** **yes.** 24 hours is a guess at how long an owner wants a collector
to hold an outage. D10 was supposed to select the outage buffer, and the load
test in the alpha report is where that number should come from.

## L045. The query algebra, and the three rules that shaped it

**Phase:** 4
**Decision:** `crates/tallyowl-head/src/expr.rs` and a rewritten `query.rs`
answer scan, filter, project, aggregate, sort, limit, and union, over the
`events` dataset, with count, sum, min, max, avg, and `count_distinct`. Every
other operator and measure is refused **by name**.

Three things the design did not settle, decided here:

**A comparison against a value that is not there is unknown, not false.**
`price > 10` over a row with no price does not mean the price is ten or less.
QUERY.md does not say which logic to use. Two-valued logic over sparse columns
makes `filter(not(price > 10))` and `filter(price <= 10)` return different sets,
and both look right, which is the silent wrong answer FAILURE_MODES.md section 2
ranks worst. Three-valued logic costs one extra state and removes the whole
class.

**A row with no value for a dimension is its own group, not a dropped row.** A
breakdown that quietly dropped them would total less than a trend over the same
range, and nobody could reconcile the two numbers.

**A sum of exact decimals stays exact.** The accumulator holds an integer
mantissa and a scale while every value it saw was exact, and falls back to a
float only when a float arrives. Three payments of 19.99 total 59.97, not
59.97000000000001.

**The depth limit is now enforced.** L016 recorded that QUERY.md promised a
configurable maximum nesting depth and nothing enforced it.
`query.maxExpressionDepth` defaults to 16 and the decoder refuses a deeper tree
before it evaluates one.

**Cost to change:** moderate. The executor materialises rows, which is right at
home-profile sizes and is not the shape a distributed query wants. The store's
`trend` push-down stays on the trait for that reason.
**Revisit:** yes, on one point. Every operator here runs over materialised rows,
so a query over a large range holds the whole range in memory. `query.maxRuntime`
and the row budget bound it; a byte budget would bound it better, and
`ResultMetadata.scanned_bytes` is reported as zero because nothing measures it
yet.

## L046. Two silent wrong answers the query work found

**Phase:** 4
**Decision:** Both fixed, and both recorded because the shape repeats.

**A scan dropped the incompleteness flag.** `Store::scan` returned
`Vec<EventRow>` while `Store::trend` returned a flag saying whether the store
could read all of it. The old executor used `trend`, so the flag reached a
caller; the new one uses `scan`, and the flag had nowhere to go. A query over a
damaged segment would have returned a smaller number and called it complete.

`scan` now returns `Scanned { rows, incomplete }`. The flag travels **with** the
rows rather than beside them, because a caller who has to ask for it separately
will forget. The Parquet exporter now refuses rather than writing a file that
holds fewer rows than the range it names.

**An unsupported measure over an empty range answered.** The measure kind was
checked when the first row reached its accumulator, so a query over a range with
no rows returned an empty result rather than saying the measure does not exist
here. Measures are now checked before any row is read.

Both are the same shape: a check that only runs on the path where there is data,
over a path where there is not.

**Cost to change:** cheap, and both are done.
**Revisit:** no.

## L047. The scrubber names what it removed

**Phase:** 5
**Decision:** `crates/tallyowl-wire/src/scrub.rs` removes credentials from free
text, and collector intake applies it to every error message, every route, every
referrer, and every property value. A removed value becomes `<removed>`.

**Why:** `AGENTS.md` says never to record secrets, credentials, request bodies,
claim values, or raw personal data by default. An error message is where those
arrive, because an error message is written by whoever wrote the code that
threw, and a connection string in an exception is the ordinary case.

**It names the removal rather than hiding it.** `password=<removed>` tells a
person the field was there and was taken out. An empty value looks like a defect
in the producer, and somebody spends an afternoon on a producer that is working.

**A value ends where the next field starts.** `password=x, user=y` keeps the
user. A scrubber that ran to the end of the line would remove the one piece that
helps somebody diagnose.

**A path stays and a query string goes.** A path is how somebody finds the line
that threw and it is not personal data. A query string is where a token ends up.

**It runs at the collector, and a driver may run it too.** The collector is the
trust boundary and `AGENTS.md` says to normalize there. A driver's scrubbing is
a courtesy, because an application that does not use a maintained driver still
reaches the collector.

**Cost to change:** cheap. It is a pattern matcher, and
`CollectionPolicy.redact_keys` already carries a per-project list for names it
does not know.
**Revisit:** yes. This catches the shapes that carry a secret and it does not
understand the text around them. A message that says "the key is hunter2" in
prose passes through. Widening it is a trade against mangling ordinary messages,
and a scrubber that mangles ordinary messages gets turned off.

## L048. A tail decision needs one durable record, and a tombstone is the other half

**Phase:** 5
**Decision:** A dropped trace gets a tombstone naming `trace_id`, and every
decision is recorded in the catalog under `projector/tail/`.

**Why:** D35 gives six steps and the last two are "a kept trace moves to its
normal retention class" and "a dropped trace gets a tombstone". Working through
what each state actually needs:

**A dropped trace needs nothing but the tombstone.** A tombstone is a standing
predicate, so it already hides the late spans of that trace. That is exactly
what a dropped trace needs and it comes free.

**A kept trace needs a record for one reason only**: a span that arrives after
the grace period must not change a decision that was already applied. Without
the record, a late span would make a kept trace look droppable at the next
sweep.

**The tombstone gained an exclusion.** D35 says an always-keep error survives
even when the tail rules drop its trace. The exclusion is part of the predicate
(`except_kinds`) rather than a list of event IDs, because a predicate also
covers a late arrival that no list could have named. `Tombstone.property` now
also resolves a row's own correlation columns, because a trace ID is a column
rather than a property.

**A decision is reproducible rather than random.** The share is taken from the
trace ID, so the same trace decides the same way on every node and after every
restart. A random source would make a sampled result unexplainable.

**Cost to change:** moderate. The open-trace registry is in memory, so a restart
loses the traces that had not been decided. That is safe rather than lossy: an
undecided trace is a kept trace. It does mean a restart during an outage leaves
some traces undecided for ever.
**Revisit:** **yes**, on two points. The registry should be rebuilt at start-up
by scanning for spans with no decision, and it is not. And the rules here are
built in rather than compiled from `CollectionPolicy.tail_rules`, which D45 says
should use the query expression tree; the expression evaluator that would run
them now exists, so wiring the two together is a small step that was not taken.

## L049. What Phase 5 has, and where the rest goes

**Phase:** 5
**Decision:** Record the remainder plainly rather than leave it to be
discovered.

| Not built | Where it goes |
| --- | --- |
| Regression detection as an operation | The release is a column and a breakdown groups by it, so the query exists. What is missing is the stored "resolved in release" mark and the rule that fires when a resolved group returns |
| Merge and split overrides | D39 records an override against the set of fingerprints, not a group ID. It needs a control operation and a catalog record under `control/` |
| Source maps and debug symbols | PLAN.md Phase 5 makes this conditional on the first consumers requiring it, and neither has |
| Tail rules compiled from a policy | See L048 |
| The browser package's error capture | `packages/browser` sends typed items; a global error handler and an unhandled-rejection handler are not wired to it |

**Cost to change:** each is additive.
**Revisit:** no on the order.

## L050. Authorization, and the three leaks it closes

**Phase:** 4
**Decision:** TallyOwl's half of D7 is built: sessions, workspace membership,
three ordered roles, and a check on every control operation.

**The LinkKeys binding is built too, and an earlier version of this entry said
it was blocked. That was wrong; L053 records what the mistake was.**

**The operator session, and why it is not a way past authorization.**
`tallyowl-head session create <name>` writes a real membership and issues a real
session, both visible to `session list` and endable with `session revoke`. It is
an operator credential rather than human authentication, and D7 says the
DNS-less local mode is a supported fallback for an installation with no stable
domain, so it does not replace the LinkKeys path.

**Three rules the authorization holds, and each removes a leak:**

**A source key is not a session.** One connection carries one credential, and
the head tells them apart by prefix. A key presented to a control operation is
refused as an authentication failure rather than a permission one: it is not
that this key lacks a role, it is that a key is not a person. NODE_IDENTITY.md
section 1 says do not use one credential as a replacement for another.

**A query names a project, and every project it names is checked.** A union can
read two, so checking the first would let one query read a project the caller
may see and one they may not.

**Not a member and a role too low read the same.** Telling them apart would say
whether a workspace exists, and existence is a fact a caller has not
authenticated for.

**Cost to change:** cheap for the seam. The verifier is one trait.
**Revisit:** yes, on the roles. Three is a guess: viewer, admin, owner. A
project-scoped role rather than a workspace-scoped one may be what an
installation with many projects wants, and that is a wider change.

## L051. The load test found that one producer gets a seventh of the ceiling

**Phase:** 4
**Decision:** Record it as the load test's most useful finding, and as a defect
rather than a measurement.

**Why:** the ramp reached 36,525 events each second sustained with nothing
refused, which lands inside the 26,000 to 38,000 that BENCHMARKS.md section 12
derived. That is the first estimate in this project that a whole-system
measurement confirmed rather than overturned.

**But that number is a property of concurrency.** One synchronous producer
reaches 5,405 each second. A driver flush waits for its durable acknowledgement,
a batch seals at 256 items, and the round trip is about 47 milliseconds. Eight
producers reach 36,525 because eight round trips overlap.

`docs/DELIVERY.md` section 3 already says an application "may pipeline a
configured number of correlated batch calls". Neither maintained driver does;
both flush and wait. An application with one telemetry worker therefore gets a
seventh of the ceiling and nothing in the system says so.

**The burst multiplier is absent for the same reason.** The harness offers four
times the sustained rate and the system took all of it, so the harness could not
offer faster than the system took. A real burst measurement needs a producer
that does not block on its own acknowledgements, which is the same change.

**Cost to change:** moderate. Pipelining means a driver holds several
unacknowledged batches, which is what `docs/DELIVERY.md` section 3's 8 MiB of
unacknowledged data on one connection already budgets for.
**Revisit:** **yes.** This is the highest-value remaining item in the whole
build.

## L052. Two bytes-for-each-event numbers, and the difference is a missing feature

**Phase:** 4
**Decision:** Report 32.0 bytes for each sealed row and 223.3 for each event
across the whole data directory, and treat the gap as a defect rather than as
overhead.

**Why:** 32.0 beats the 39.75 that BENCHMARKS.md section 12a measured, on harder
rows: every event in the load run carried a unique `request_id`, which is the
high-cardinality column that dominates a segment. The format is doing better
than the design expected.

**The whole directory is seven times that because the run never reached a steady
state.** The append log is the durable record until a segment replaces it and
nothing reclaims a log range. The catalog holds one locator entry for each value
and segment pair and had just taken 273,257 unique request IDs across two
segments. Neither retention nor reclamation exists.

An operator sizing a device today uses 223, not 32, and D23 now says so.

**Cost to change:** moderate. Reclaiming a log range needs the catalog to know
which ranges every live segment covers, which it already records.
**Revisit:** no on reporting both. Yes on building the reclamation, which is
what makes the two numbers converge.

## L053. A limitation I asserted instead of testing

**Phase:** 4
**Decision:** The LinkKeys SDK is a Git dependency, pinned by revision, exactly
like `csilgen-transport` and `corndogs`. The binding in
`crates/tallyowl-head/src/linkkeys.rs` is built and tested.

**Why this entry exists:** I reported the binding as blocked, and it was not.
The reasoning was that the SDK lives in a subdirectory of the LinkKeys
repository and path-depends on three crates beside it, so it could not be a Git
dependency. **Cargo clones the whole repository and resolves those paths inside
the checkout**, which is what it does for every Git dependency and always has.
`liblinkkeys`, `linkkeys-rpc-client`, and `csilgen-transport` all compile from
the LinkKeys checkout.

**The counter-example was already in this workspace.** `csilgen-transport` is a
Git dependency on `transports/rust`, a subdirectory of the csilgen repository,
and it inherits `serde` and `thiserror` from that repository's workspace. Every
build in this project has been doing the thing I said was impossible.

**What it cost:** an hour, and a blocked item in a report the owner would have
acted on. The failure was not the wrong belief; it was reporting a belief as a
finding without running the four-line test that settles it. A claim of the form
"the tooling cannot do this" is cheap to check and expensive to get wrong, and
the implementation prompt already says as much in section 10.1 about csilgen:
start from the position that the tool is right.

**One real consequence of the shape**, now that it is in: `csilgen-transport`
compiles twice, once from the csilgen repository as TallyOwl's own dependency
and once from the LinkKeys checkout as the SDK's. Cargo permits it because they
are different sources. Nothing crosses between them, because the SDK returns
plain verified facts and no transport type reaches TallyOwl. It costs build time
and nothing else.

**Cost to change:** cheap. The dependency is one line.
**Revisit:** no on the dependency. The habit is worth keeping: test a claim
about a tool before reporting it.

## L054. Signing in is not authorization

**Phase:** 4
**Decision:** A LinkKeys sign-in issues a session and grants no membership. A
person who signs in and belongs to nothing sees nothing until an administrator
gives them a role.

**Why:** D7 says an installation administrator selects the trusted domains and
maps each claim to a role. A default role for a trusted domain would collapse
those two into one: trusting a domain would mean trusting everybody at it, and
`example.com` has more people at it than any installation means to admit.

The cost is one manual step for the first person at a new domain, and that step
is the one somebody should be thinking about.

**Three checks around it, and each closes something:**

**The callback must be the one the installation configured.** Without that check
a caller could begin a login that returns to an address they chose, and the
token would arrive there.

**The domain is checked twice**: before the redirect, and again on the verified
assertion. The first is about where a login was sent and the second is about who
came back, and an assertion can name a different domain than the request did.

**A pending login is taken rather than read.** The SDK says plainly that it
cannot enforce single use and that replay protection is the application's job.
The record is removed before the completion is attempted, so a completion that
fails leaves nothing to try again with a different token.

**Cost to change:** cheap for the mapping. A claim-to-role mapping is additive
and the settings are already there to hold it.
**Revisit:** yes. D7 says an administrator maps each claim to a role, and this
maps no claims at all: it verifies the assertion, records the subject, and stops.
A mapping from a claim such as a verified employer domain to a role is the next
step, and it is the part that makes a large installation stop granting
memberships by hand.

## L055. Pipelining needed the server half, not only the driver half

**Phase:** 4
**Decision:** Both maintained drivers gained a `submit` and a `drain` beside
`flush`, and `tallyowl-rpc` gained a pipelining client and a connection that
serves correlated requests at the same time. `flush` keeps its exact meaning: it
returns the receipt of the batch it sealed.

**Why:** L051 and the alpha report section 3.3 named the driver as the defect.
The driver was only half of it. A connection served one request at a time, so a
client that sent four batches without waiting would still have had each batch
queue behind the durable write of the batch in front of it, and would have
gained the network latency and nothing else. `docs/DESIGN.md` section 4.2 calls
this hop "pipelined RPC", and that word needs both ends.

The server keeps the strict one-at-a-time path for a request that carries no
correlation ID, because a client that did not ask for correlation cannot tell
two replies apart.

**What the change cost, measured in the crate's own tests:** four batches
against a collector that takes 80 milliseconds to make each one durable finish
in little more than 80 milliseconds instead of 320.

**The retention rule is what makes it safe.** D5 says the app "retains the
stable batch until that acknowledgement, then moves on". The driver now does: it
holds the encoded frame of every outstanding batch, and a connection failure
sends each one again under its original batch ID rather than losing it. Final
storage deduplicates that ID, so a resend stays one logical commit. A batch that
fails `maxBatchAttempts` times is reported, never silently dropped.

**Two numbers had no source and I chose them.** The window is 4 correlated
batches for a client and 8 for a connection. `docs/DELIVERY.md` section 3 says
"a configured number" and names none. Four covers the measured 47-millisecond
round trip at the D19 batch defaults.

**Cost to change:** cheap. Both are one setting, `maxInFlightBatches` on the
driver and the argument to `serve_with_in_flight` on the service.
**Revisit:** yes, on the defaults. The load test in section 11 is what should
select them, and it now can, because the harness finally has a producer that
does not block on its own acknowledgements. That producer is also what the burst
measurement needed, which is the second of the two rows the alpha report could
not fill.

## L056. The append log was reclaimed by deleting all of it, including frames nobody had segmented

**Phase:** 4
**Decision:** `Wal::reclaim_through` removes the covered prefix of the append log
and keeps everything above it. `seal` calls it instead of `truncate_to_empty`,
and no longer moves `open_from` to the log's next position.

**Why: this was a durability bug, not only a disk one.** `seal` captured the
open rows and the log position `to` under the state lock, released the lock,
wrote and published the segments, and then removed **the whole log file**. A
commit that landed inside that window has a frame above `to` and its rows in the
open buffer. Removing the whole file destroyed the only durable copy of a batch
TallyOwl had already acknowledged, and the process would then have lost it on an
abrupt kill. `open_from` was also moved past those rows, so the next segment's
`log_range` claimed a range it did not hold.

The window was narrow while a connection served one batch at a time. L055 made
a connection serve eight, so it is much wider now. I found it by reading the
code after L055 rather than by reproducing a loss: the stress test in
`segmented_store.rs` runs a committer and a sealer against each other and
passed, because its sealer drains everything at the end and a later seal always
recovered the rows a previous one had orphaned. **The test is kept as a guard
and is not evidence that the bug was harmless.**

The rewrite is atomic: a new file beside the old one, fsync, rename, fsync the
directory. A crash before the rename leaves the old log, which replays frames a
segment already holds, and deduplication makes that harmless.

**Receipt expiry is the other half, and it closes D36.** `storage.deduplicationWindow`
defaults to 72 hours, `Catalog::expire_receipts` removes what is older, and the
maintenance pass runs it. `crates/tallyowl-config/src/validate.rs` now refuses a
configuration where `corndogs.maxDeliveryAge` is not shorter, which is the
pairing D36 states and L044 recorded as unenforced. Until this existed the
window was unbounded and any retry age satisfied D36 trivially.

**Nothing ran the maintenance pass.** `compact` existed, `reclaim_retired_files`
existed, and the only callers were tests. The head now runs the pass every five
minutes. That is a defaulted number with no measurement behind it.

**Cost to change:** cheap for both windows and the interval; each is one
setting. Moderate for the reclamation itself, which is now on the seal path.
**Revisit:** yes, on the three numbers. 72 hours, 24 hours, and five minutes were
each chosen rather than measured, and D36 ties the first two to the outage buffer
that D10 was supposed to select. The load test in section 11 is where that comes
from.

## L057. Role tokens, and the two rules the type system holds rather than a check

**Phase:** 4
**Decision:** Role tokens, node enrollment, certificate signing, and renewal are
built. `crates/tallyowl-store/src/identity.rs` holds the records,
`crates/tallyowl-store/src/certificates.rs` holds the installation authority,
and `crates/tallyowl-head/src/enrollment.rs` holds the intersection rule. Six
operations went onto `TallyOwlControl` at wire IDs 11 to 16.

**Why the shape follows the API key model:** the alpha report said it would, and
it was right. One line of text, a public identifier and 32 random bytes, a keyed
digest stored and never the value, many active credentials so a rotation needs
no cutover, and one refusal sentence whatever the reason. A role token differs in
its prefix, `towr_` against `tow_`, so a credential pasted in the wrong place is
refused as the wrong kind rather than as an unknown one.

**Two rules from NODE_IDENTITY.md section 3 are held by the type rather than by a
check.** A role token cannot create a controller voter, cannot create a global
directory voter, and cannot change a tablet voter set. `NodeRole` has no name for
any of them, in the CSIL contract and in the store. A policy cannot ask for what
the type cannot express, so there is no check to forget and no code path to miss.
A test asserts that the set stays at ten names and that none of them says
"voter", so adding one to the contract fails there rather than passing unnoticed.

**The node keeps its private key, and the signature proves it.** `sign_request`
takes a PKCS#10 request and returns a certificate. `rcgen` verifies the request's
own signature while parsing it, which proves the requester holds the key it
carries. That is not what authorizes the enrollment; the token is. The subject is
built by the head from the node ID it assigned, never taken from the request, so
a node that asks to be called `i-am-the-controller` gets a certificate that says
`node-<random>`. A test asserts exactly that.

**Two things I had to decide.**

**Installation authority.** A role token belongs to the installation and not to a
workspace, and an installation enrolls its collectors before it has a workspace
for anybody to own. Requiring an owner role would have made the bootstrap order
impossible. `SignedIn` now carries its issuer, and an operator session or an
owner of any workspace may manage tokens. L050 already records that the role
model is workspace-scoped; this is the first place that pinched.

**Certificate lifetime is clamped, not trusted.** A token policy may ask for a
shorter certificate life and cannot ask for a longer one than the 24 hours
NODE_IDENTITY.md section 6 sets. A policy that could raise it would let whoever
holds a token decide how long a stolen certificate stays good.

**Cost to change:** cheap for the lifetimes and the authority rule. Expensive for
the wire IDs, which are permanent.
**Revisit:** yes, on installation authority. An installation-scoped role is the
right answer and L050 names it as the wider change; accepting an operator session
is the narrow one that unblocks enrollment today.

## L058. TLS went under the carrier exactly as predicted, and I blamed rustls for my own shortcut

**Phase:** 4
**Decision:** `crates/tallyowl-rpc/src/tls.rs` carries mutual TLS under
`StreamCarrier`. Both ends verify against the installation authority, the server
requires a client certificate, and a peer without an enrolled identity never
reaches a decoder. **A TLS connection currently serves one request at a time,
and that is a property of the shortcut I took rather than of rustls.**

**Why the prediction held:** the alpha report said "`StreamCarrier` is generic
over any `Read + Write`, so TLS goes under it without a csilgen change". A rustls
stream is `Read + Write`, so the framing, the envelopes, and every generated
codec are unchanged and do not know TLS is there. No contract changed.

**The claim I got wrong, and the correction.** The first version of this entry
said a rustls session "cannot be split the way a TCP socket can" and that the
synchronous API "does not expose one". **Both statements are false.** I used
`rustls::StreamOwned`, which is a convenience wrapper that bundles the connection
and the socket and needs `&mut self` for each direction, and then wrote up the
wrapper's limitation as the library's.

rustls 0.23 exposes exactly what a duplex carrier needs:
`Connection::read_tls`, `process_new_packets`, `reader`, `writer`, and
`write_tls`. The pattern is to clone the TCP handle, hold the `Connection` behind
a mutex, and **never hold the lock across a blocking socket read**: read raw
bytes into a buffer first, then take the lock briefly to feed them in. TLS keeps
separate keys and sequence numbers for each direction, so this is sound rather
than a trick, and it is how synchronous duplex TLS servers are normally built.

The deadlock I talked myself into only exists for `StreamOwned`, because that
type does the socket I/O inside the borrow.

**What this cost:** nothing yet, and it would have cost Phase 7 the replicated
write path's overlap. What it nearly cost is worse: a recorded, confident,
wrong reason that the next person would have believed. The rule this project
already paid for applies exactly here — **distrust a negative result about your
own design** — and I did not apply it.

**Cost to change:** moderate and localised. A duplex carrier over a split rustls
connection sits behind the same `FrameCarrier` seam, so nothing above it moves.
**Revisit:** **yes, and it is now a defect rather than a limitation.** It belongs
before the replicated write path is measured, because measuring replication over
a serial carrier would measure the carrier.

## L059. The dashboard's carrier is one frame in a POST, and it refuses every service but one

**Phase:** 4
**Decision:** `packages/dashboard/` is the dashboard and
`crates/tallyowl-head/src/dashboard.rs` is the surface that serves it: the
document, the built module tree, the sign-in callback route, ingest health, and
a same-origin browser carrier at `POST /api/rpc`.

**Why a POST carrier.** `docs/DESIGN.md` section 4.2 requires a same-origin
browser carrier for this hop and a browser cannot open a TCP socket. One CSIL-RPC
request frame in the body and one response frame back is the smallest thing that
carries a CSIL envelope without inventing a second protocol: the envelopes, the
codecs, and the correlation IDs are the ones every other hop uses, and the
generated TypeScript client is unchanged.

**The rule that keeps it from becoming an ingest API.** `AGENTS.md` says "Do not
add a generic HTTP ingest API". `carry` refuses every service except
`TallyOwlControl`, so a frame naming `TallyOwlIngest` or `TallyOwlCollector` is
rejected with a transport status before it reaches a decoder. A test asserts it
for all three names. Telemetry reaches a collector over CSIL and never here.

**Three more decisions worth recording.**

**The session comes from the `Authorization` header, never from the frame.** A
frame that carried its own credential would let a document choose an identity the
browser never presented. The header becomes `RpcRequest.auth`, which is where
every other hop puts a credential, so the head's authorization runs unchanged.

**The query is a tree the dashboard builds, never text.** `docs/QUERY.md` says
TallyOwl never parses a query string from a browser. `packages/dashboard/src/queries.ts`
builds typed nodes and there is nothing in the head that would read a string.

**The dashboard does not fail the head.** A dashboard that could not bind logs a
warning and the head keeps taking telemetry. A head that refused to start over a
browser surface would stop ingest for a page nobody was looking at.

**Cost to change:** cheap. The carrier is one function and the views take data
and return elements, so a WebSocket carrier or a different chart is a local
change.
**Revisit:** yes, on two things. The carrier is one call for each frame, so a
dashboard that polls pays a request each time; CSIL-Events over a WebSocket is
the shape that fixes it and nothing above the carrier would move. And the
dashboard picks the first project the session can read, because there is no
project selector yet.

## L060. The load test caught a regression I had just written

**Phase:** 4
**Decision:** `Wal::reclaim_through` rewrites the log only when the reclaimable
prefix is at least half the file, or has reached `min_reclaim_bytes`, which
defaults to 4 MiB.

**Why: the first run after L056 was 2.4 times slower than the run before it.**
Sustained ingest fell from 36,525 events each second to 37,135 — which looked
fine — but the head was committing about 15 batches each second while the
collector was taking 37,000 events each second, and the append log grew to
58 MiB before the head caught up. The collector's queue was absorbing a backlog
the head could not drain.

**The cause was the reclamation itself.** A seal reclaims the range it just
published, and that range is small next to whatever is queued behind it.
Rewriting the file for each seal therefore copies the whole tail each time, and
the total work is quadratic in the backlog. `truncate_to_empty` had been O(1)
because it was `set_len`, which is exactly why it was fast and exactly why it
was wrong.

Waiting until the prefix is half the file makes the copy amortised: each byte
moves at most once for each doubling, so the total work is linear. The cost is
bounded and worth stating plainly: **the log holds at most twice what it needs.**

**After the fix, sustained ingest is 67,624 events each second**, against 36,525
before this run started. The threshold is a setting rather than a constant, and
the tests that are about what a rewrite keeps set it to zero so they stay about
that.

**This is the third time a measurement in this project overturned something that
looked right.** L056 was a correctness fix and it was correct; it was also a
performance fault that no unit test could have shown, because the fault only
appears when a backlog exists.

**Cost to change:** cheap. It is one setting, `min_reclaim_bytes`.
**Revisit:** yes. A segmented append log — several files, delete a whole file —
removes the copy entirely rather than amortising it, and it is the shape Phase 7
wants anyway because a replica ships log ranges. This is the cheap fix, not the
right one.

## L061. A meter aggregates in process and the driver never owns a timer

**Phase:** 6
**Decision:** Both app drivers gained a `Meter` that aggregates counters,
gauges, and histograms in process. A host calls `publish_metrics` on its own
period; the driver starts no timer of its own.

**Why:** an application calls `add` a million times and one point for each
series leaves the process in each period, which is what stops a metric from
putting the application's own request rate on the ingest path. The timer stays
with the host because a host that already has one would then have two, and the
two would disagree about when a period ended. Every host language already has a
scheduler that fits its runtime better than one this driver could ship.

**Cumulative is the default.** A missing delta is a hole in a total that nothing
can rebuild; a missing cumulative point costs one sample of resolution. A host
that wants delta says so.

**Cost to change:** cheap. The meter is one module in each driver and nothing
above it depends on the temporality.
**Revisit:** no.

## L062. A counter reset is a pair, not a comparison

**Phase:** 6
**Decision:** Every cumulative metric point carries `start_at`, which is when
the series began counting. `rate` and `increase` read a restart as **either** a
later `start_at` **or** a smaller value.

**Why:** `docs/QUERY.md` section 12.7 defines a reset as a decrease in a
cumulative series, and a decrease alone is ambiguous: a producer that restarted
and a producer with a defect look identical. `start_at` separates them, and it
costs 8 bytes on a point that already carries two timestamps.

Both signals are read because neither is available everywhere. A driver moves
`start_at` on a restart; a Prometheus scrape target publishes no start at all,
so `crates/tallyowl-compat/src/scrape.rs` holds the previous value and moves
`start_at` itself when a value falls. Reading only the value would miss a
producer that restarted between two equal readings, and reading only the start
would miss a target that has none.

**Cost to change:** moderate. The rule is in one accumulator, and the stored
`start_at` is in the projection.
**Revisit:** no.

## L063. The OpenTelemetry receiver reads protocol buffers over HTTP, and
nothing else

**Phase:** 6
**Decision:** The receiver answers `POST /v1/metrics` and `POST /v1/traces` with
`Content-Type: application/x-protobuf`. It answers `415` for the OTLP JSON
encoding, naming `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, and it serves no
gRPC listener. The protocol-buffer reader is 200 lines in
`crates/tallyowl-compat/src/protobuf.rs` rather than a generated crate.

**Why:** `http/protobuf` is what every OpenTelemetry SDK supports and what many
default to, so an exporter reaches this receiver by setting one environment
variable. gRPC needs an HTTP/2 implementation and its own dependency set inside
the **always-on** collector, for a transport that carries the same bytes. JSON
needs a second decoder for every message, and its 64-bit integers arrive as
text, so it is a second set of parsing faults rather than a second encoding.

The reader is hand-written for the same reason: this is the only protocol buffer
TallyOwl reads, it reads it at one boundary, and the wire format is six wire
types and a varint. A generator and its runtime would join the collector's
dependency set for that. The reader treats every length as hostile: a length
past the end of the buffer, a varint longer than ten bytes, and a group wire
type each end the message rather than panicking or looping.

**Cost to change:** moderate. A JSON decoder is a second `otlp` entry point and
nothing above it moves. A gRPC listener is a new dependency and a new port, and
it would go beside `receiver.rs`.
**Revisit:** yes. If an exporter an operator actually runs cannot be configured
for `http/protobuf`, JSON is the cheaper of the two additions.

## L064. An exponential histogram is refused rather than converted

**Phase:** 6
**Decision:** The OpenTelemetry receiver refuses an `ExponentialHistogram` and
reports the count in the OTLP partial-success field.

**Why:** an exponential histogram has no explicit bounds, and the native
histogram is defined by its bounds. Converting one would invent bounds nobody
chose, and the result would read exactly like a histogram somebody configured.
`histogram_merge` already refuses two bucket layouts for the same reason: a
merged shape nothing observed is indistinguishable from a real one.

The refusal is visible where an exporter can act on it, which is what the
partial-success field is for.

**Cost to change:** moderate. A native exponential histogram is a CSIL change
and a second histogram shape through the executor.
**Revisit:** yes. An exporter configured for the default aggregation sends one,
and an operator who cannot change that configuration has no path today.

## L065. A summary becomes one gauge for each quantile

**Phase:** 6
**Decision:** A Prometheus summary and an OpenTelemetry `Summary` both become a
`_sum` counter, a `_count` counter, and one gauge for each quantile, carrying
the `quantile` label.

**Why:** a summary reports quantiles the producer already computed, and a
quantile somebody else computed cannot be merged with another one: two targets
each reporting a p99 have no p99 between them. A gauge keeps the number the
producer published and refuses to imply it can be combined. A histogram, which
carries buckets, does merge, and keeping the two different in the store is what
lets `histogram_merge` be honest about which one it can answer.

**Cost to change:** cheap. It is one branch in each normalizer.
**Revisit:** no.

## L066. A bucket layout travels as two canonical number lists

**Phase:** 6
**Decision:** A stored histogram point carries `histogram_bounds` and
`histogram_counts` as comma-separated text, plus `histogram_count` and
`histogram_sum` as numbers.

**Why:** a stored row has no vector value, and a bucket list is a small fixed
vector rather than a payload. Two text lists keep it readable, comparable by
equality, and mergeable, and `histogram_merge` compares the bound list by
equality precisely because it must never rebucket. A whole number renders
without a decimal point, so `1` and `1.0` are one layout rather than two that
never merge.

This is not the CBOR catch-all `AGENTS.md` forbids: it is two typed lists of
numbers whose shape the executor reads directly.

**Cost to change:** moderate. A typed vector column in the row model would
replace it, and the projection, the executor, and the rollup would each move.
**Revisit:** yes. A typed page for a bucket vector is the right long-term shape
and it belongs with the Phase 3 storage work rather than here.

## L067. A metric label reaches storage under its own name, and a reserved name
is prefixed

**Phase:** 6
**Decision:** `MetricPointPayload.labels` project into row properties under
their own names, except for the fifteen names a metric point owns, which arrive
prefixed with `label.`.

**Why:** a query groups by `route`, not by `label.route`, so the ordinary case
has to be the plain name. But a producer must not be able to change what `value`
or `series_key` means by sending a label of that name, and overwriting a field
the executor reads would be a wrong answer rather than a refusal.

The projection also stores `series_key`, computed once, so `rate` over a group
that holds more than one series never reads two series as one that jumped.

**Cost to change:** cheap. The reserved list is one constant.
**Revisit:** no.

## L068. A series budget refuses in the open and never folds

**Phase:** 6
**Decision:** `crates/tallyowl-collector/src/series.rs` counts active series
exactly, for each project and metric name. A point that would open a new series
past the budget becomes a rejected item on the receipt with
`resource-exhausted`, naming the metric. Every series already admitted keeps
counting.

**Why:** `docs/DATA_MODEL.md` section 3.4 says TallyOwl supports
high-cardinality series and "does not silently put these series in an overflow
series". Three shapes were rejected before this one:

- **an overflow series** makes a value nobody can join to a request;
- **sampling** makes a counter wrong by an amount nobody can bound;
- **a silent drop** makes a chart that reads as an outage.

An explicit refusal is the only one of the four that a producer can act on, and
it is the explicit backpressure Phase 6's exit criteria ask for. An admitted
series is never refused later, even when the byte budget has since filled: the
middle of a series is harder to read than the start of a new one.

**Cost to change:** cheap. Every bound is a setting under `metrics.*`.
**Revisit:** yes, on the numbers. 100,000 series and 64 MiB for one metric name
in one project are chosen rather than measured, and a measurement should set
them.

## L069. A collector merges within one batch, and a cumulative point supersedes

**Phase:** 6
**Decision:** Two points of one series in one batch merge. A delta adds; a
cumulative point with the later end wins and the earlier one goes. Two
histograms with different bounds never merge. Past `metrics.maxMergePoints` the
batch travels unmerged and a counter says so.

**Why:** `docs/DATA_MODEL.md` section 3.4 permits the collector to "merge
compatible snapshots before transfer". A cumulative point is a level rather than
an addend, so adding two of them would report a total nothing counted. The
receipt still reports the pre-merge count, because two snapshots that became one
point were both accepted and a smaller number would read as a loss.

The work budget exists because a batch is a caller-controlled size and a
collector must not spend unbounded time inside one. Past it the batch costs
bytes downstream and never costs correctness.

**Cost to change:** cheap.
**Revisit:** no.

## L070. Golden signals are ordinary metric points

**Phase:** 6
**Decision:** The head derives three series from the spans in each committed
batch — `tallyowl_service_operation_requests_total`, `..._errors_total`, and
`..._duration_seconds` — and commits them as ordinary metric points under a
batch identifier derived from the one that produced them.

**Why:** a golden signal that had its own storage shape would have needed its
own version of `rate`, `increase`, `histogram_merge`, `quantile`, and its own
dashboard path. As metric points they get all five for free, and an operator
charts them exactly as they chart an application's own counter.

**The derived batch identifier matters.** Sharing the original would make a
re-delivery deduplicate the signals away; a random one would double them on
every retry. Deriving one from the original gives one logical rollup however
many times the batch arrives.

**Saturation is not derived.** A span says nothing about how full a queue or a
disk was, and a signal named "saturation" that guessed would read as measured.

Every derived row carries `derived`, so a count of what an application produced
can leave it out. The reference application's ledger does exactly that.

**Cost to change:** moderate. The bucket layout is fixed and shared, which is
what makes two services' histograms merge; changing it changes what old and new
points can be compared across.
**Revisit:** no. `Dataset::Events` used to return every kind, so a trend over
"events" counted derived rows and the reference application's ledger had to
filter them by hand. **That is fixed**: the dataset now means every kind a
*producer* sent, `metric_points` still holds the golden signals because a
derived point is a metric point, and the ledger no longer filters anything.
`a_trend_over_events_does_not_count_what_tallyowl_derived` holds it.

## L071. Downsampling produces the coarser points and removes nothing

**Phase:** 6
**Decision:** The head rolls delta metric points up to
`metrics.downsampleResolution` on a period. It does **not** remove the finer
points.

**Why:** `docs/DATA_MODEL.md` section 6 says a downsample policy "removes the
raw points", and `AGENTS.md` says every derived projection must be rebuildable
from retained raw data. Removal is the deletion workflow with tombstones, and a
rollup that deleted its own inputs could not be rebuilt. Producing the coarse
points is the half that is safe now; removing the fine ones belongs with the
retention expiry that does not exist yet.

A gauge is not rolled up at all. There is no way to say which moment an hour of
gauges stands for without choosing one.

**Cost to change:** cheap.
**Revisit:** no. **Retention expiry is built**, in L080. This entry's missing
half is closed: a segment past every retention is dropped, and a segment past
`detailed` but inside `rollup` is rewritten holding only its derived rows. What
this entry decided — that a downsample pass produces coarse points and never
deletes its own inputs — is still right, because a derived projection must stay
rebuildable from retained raw data.

## L072. Self-observation suppresses recording while it publishes

**Phase:** 6
**Decision:** `Registry::begin_publishing` takes a flag that makes every
recording call a no-op until it is dropped. A push that is already in flight
makes the next period skip.

**Why:** this is the recursion guard D12 asks for. Publishing self-metrics is
work, and instruments measure work: a push that counted its own batch would
raise a counter, which the next push would report, which would raise it again.
Suppressing for the length of one push makes the cost of self-observation one
batch each period and makes the numbers describe the service rather than the act
of describing it.

The head publishes through a collector with the app driver rather than
committing straight into its own store. Committing directly would skip tenancy
resolution, the series budget, and the scrubber, so the one project an operator
reads to find out whether TallyOwl is healthy would be the one project that
never went through TallyOwl's own trust boundary. The guard is released before
the flush, because holding a suppression flag across a network call would hide
real work for as long as that call took.

**Cost to change:** cheap.
**Revisit:** yes. It is off by default, and an installation that wants it has to
turn it on in two services. One setting does turn on both, but nothing checks
that the credential resolves to a project an operator meant for it.

## L073. A scrape is refused over HTTPS rather than reached in the clear

**Phase:** 6
**Decision:** `compatibility.prometheus.targets` accepts an `http://` URL. An
`https://` target is refused at configuration time with a message that says this
build scrapes over HTTP inside a trust boundary the operator controls.

**Why:** the client is twenty lines of `GET` written beside the server half in
`tallyowl_obs::http`, and it does no TLS. An operator who wrote `https` believes
the scrape is encrypted. Silently reaching the target in the clear would break
that belief without telling anybody, and adding a TLS client to the always-on
collector for a scrape is a much larger surface than the request needs.

A redirect is also not followed, for a related reason: an operator names a
target inside a boundary they drew, and following a redirect would let the
target send the collector somewhere the operator did not name.

**Cost to change:** cheap. `rustls` is already a workspace dependency for node
identity, so a TLS scrape is a connector swap.
**Revisit:** yes. A scrape target behind a service mesh with mutual TLS is an
ordinary deployment.

## L074. The load run's discrepancy was the development loop, not TallyOwl

**Phase:** 6
**Decision:** `dev up` starts each service in its own process group, `dev down`
stops the group rather than the process, and `dev down` then checks each service
address and names any process still holding one.

**What looked wrong.** The first Phase 6 load run reported 7,925 batches
accepted, 9,998 delivered, and 1.61 rows in the store for each accepted event.
It was reported as the top defect and as the reason the alpha gate stayed open.

**What was actually wrong.** `./tools.sh dev up` starts Corndogs as
`go run main.go run` when no binary is on the path. `go run` compiles to a
temporary executable and runs it as a **child**, so `dev.py` recorded the
wrapper's identifier. `dev down` stopped the wrapper and the server kept
running. Its open file was `data/corndogs/corndogs.bolt (deleted)`: every
`rm -rf data/` unlinked the path while the process held the inode. One Corndogs
carried the delivery queue for **13 hours** across four supposed resets, so each
new collector inherited tasks an earlier one had accepted.

**Every component was correct.** The collector did not accept those batches, so
it did not count them. The forwarder delivered each exactly once, so no batch
identifier repeated. The head committed them as new because its receipt store
had been deleted with everything else. The extra rows were real data from
earlier runs.

**What the investigation taught, which is the part worth keeping.** The
acceptance log line carried no batch identifier and the delivery line did, so
the two halves of the path could not be correlated at all. `Intake::submit` also
has two callers — the RPC handler and the compatibility edge — and only the
first logged, so a task could enter the queue with nothing recording which
producer made it. Both are fixed: every acceptance now logs its batch identifier
and its producer.

**The rule this cost a day to relearn:** when a number does not add up, make the
two ends correlatable before theorising about either end. Five hypotheses were
argued from aggregate counters and every one was wrong. The first correlated
query answered it.

**Cost to change:** cheap. Three edits in `tools/tallyowl_tools/dev.py` and two
log fields.
**Revisit:** no.

## L075. A damaged segment had to be nameable

**Phase:** 6
**Decision:** `SegmentedStore::unreadable()` reports why a store cannot read
part of what it holds. The head logs the reasons at start-up, and a correlated
lookup that finds damage records the reason rather than only setting the
incomplete flag.

**Why:** a store that had accumulated several load runs reached a state where
one segment would not read, and every query over its range answered
`incomplete-result` for the rest of that process's life. The refusal was correct
and completely unactionable, because nothing said which segment. Ten minutes
went into finding out that the answer was "we do not record it".

`docs/FAILURE_MODES.md` procedure 6 already required the naming: "with no other
copy the segment stays damaged, and a query over its range returns
`incomplete-result` and names it." The comment in the code said so and the code
did not do it.

**Cost to change:** cheap.
**Revisit:** no.

## L076. The self-observation recursion guard suppressed other threads

**Phase:** 6
**Decision:** The guard is a thread-local flag rather than a process-wide one.
`Registry::begin_publishing` suppresses recording on the calling thread only.

**Why:** the guard exists so a self-observation push does not measure its own
work. The first version set an `AtomicBool` on the registry, and
`Registry::add` checked it, so **every thread** stopped recording for the length
of a push.

A collector accepting batches on eight threads during a push would have had
every one of those increments dropped. The numbers the push then reported would
be lower than the truth **because it was reporting them**, and nothing would say
so: a dropped increment is silent by construction, since the whole point of the
metric path is that it never fails a caller.

That is a worse fault than the one the guard prevents, and it is the same class
as the fault that made the load run unreadable: a count that is quietly wrong.
Per thread, the push suppresses its own work and nothing else's.

`a_push_suppresses_its_own_thread_and_no_other` holds it: the publishing thread
adds 100 and another thread adds 7, and the counter reads 7.

**Found by** a targeted reading of the metric path during the load-run
investigation, not by a test. It would have shipped.

**Cost to change:** cheap.
**Revisit:** no.

## L077. A decompression-ratio limit refused TallyOwl's own data, twice

**Phase:** 6
**Decision:** Remove `MAX_DECOMPRESSION_RATIO`. A page is bounded by
`MAX_UNCOMPRESSED_PAGE_BYTES` before any expansion, and a header that lies about
its expanded size is caught by the length check that already existed.

**Why: it made every query over an affected range answer `incomplete-result`
for the life of the process, on ordinary data.** The message was "A stored page
claims to expand far more than real data does. We did not expand it." The page
was intact: it had already passed its checksum. A heuristic rejected it.

**A high ratio is what telemetry looks like.** A page where every row holds the
same release, service name, or batch identifier compresses to almost nothing.
That is the ordinary case rather than the exotic one, and **the better the
column encoding gets, the more often a real page trips the limit**. The guard
fires hardest on the best-compressed data, which is precisely backwards.

The comment on the constant already recorded that a first limit "refused real
segments" and had been raised to 10,000 after that. A load run of 438,866 events
passed 10,000 as well. A limit that has been wrong twice, in the same direction,
for the same reason, is the wrong kind of check.

**It also protected nothing.** `MAX_UNCOMPRESSED_PAGE_BYTES` is validated before
any expansion and is passed to the decoder as its capacity, so a hostile header
can make a reader allocate that much and no more, whatever ratio it claims. The
ratio only ever added false refusals, and a false refusal here is the worst
failure class `docs/FAILURE_MODES.md` section 2 names: an answer smaller than
the truth.

What replaces it is a fact rather than a guess: the decompressed length must
equal the declared length. A lying header is caught by what it lied about, and
honest data is never refused.

**How it was found.** The refusal named nothing, so the first attempt to
diagnose it produced only "some of it". `Store::unreadable` and the reasons now
travel into the refusal message, and the very next run named the page in one
line. That naming is the reason this was found at all.

**What it cost while it was there.** Every query latency figure this project has
published was taken against a store that was either mid-backlog or about to
refuse a page. The settled store now answers a point lookup in **1,063
milliseconds** over 438,866 events, which is ten times the last published p50
and is the honest number. See L045: the executor materialises the rows in the
range rather than using the locator, and this is what that costs when nothing
else is in the way.

**Cost to change:** cheap, and it changes no stored bytes. A reader that still
had the limit would refuse pages a newer writer produces, so this is a read-side
relaxation and needs no format version.
**Revisit:** no. Do not add a ratio limit back.

## L078. An idle head never sealed, and a background segmenter needed a gate first

**Phase:** 6
**Decision:** `crates/tallyowl-head/src/main.rs` runs a background segmenter
that calls `SegmentedStore::seal_if_due` on a quarter of `max_open_ms`. Adding
it needed an `append_gate` in the store first; see below.

**What was observed:** after the clean load run drained completely and the
system sat idle, **196,946 of 438,866 rows were sealed into segments and the
other 241,920 were still in the append log.** The log was 73 MiB, which is
166.5 of the 243.9 bytes each event cost on disk. Waiting did not change it.

**Why:** `SegmentedStore::commit` decides whether to seal and seals inline, and
nothing else calls `seal`. `crates/tallyowl-head/src/main.rs` runs a maintenance
loop, a downsample loop, and a disk watcher, and none of them seal. So the seal
condition is only ever evaluated when a batch arrives: **traffic stopping is
exactly when sealing stops.**

The inline seal is a recorded deferral — the comment at that call site says
"STORAGE.md calls it asynchronous, and a background segmenter is a later change
that moves no contract." What was not recorded is this consequence, and the
consequence is the one an operator sees: an installation that goes quiet keeps
its most recent data in the least compact form it has, indefinitely, and pays
for it in disk and in recovery time.

**It also means no disk figure this project has published is a steady state.**
243.9 bytes for each event describes a store that stopped sealing when the load
stopped. A sealed row costs 37.8 bytes. The real number is somewhere between,
and nothing can measure it until something seals on a timer.

**The gate the segmenter needed.** A seal computes the log range it covers from
the log's own position, and a commit is not instantaneous: it appends its frame,
records its receipt, and only then puts its rows in the open buffer. A seal that
drained inside that window would publish a segment claiming to cover the frame
while holding none of its rows, and advance the checkpoint past it. The rows
would still be in memory, so nothing would look wrong — but a process that died
before the next seal would lose an **acknowledged** batch.

That hazard existed already between two concurrent committers. A background
sealer makes it far more likely, so `SegmentedStore::append_gate` closes it: a
commit holds it for reading across the window, a seal holds it for writing
across its drain alone. The lock order is always the gate, then the state lock.

**The existing race test could not catch it.** Its sealer keeps sealing after
its committer stops, so everything reaches a segment before the reopen.
`a_seal_never_covers_a_log_frame_whose_rows_it_did_not_take` reopens with rows
still in flight, and runs four committers against one sealer.

**What it moved.** The append log settled at **8 bytes** against 73 MiB, every
row sealed, and D23's disk figure settled for the first time at 176.3 bytes for
each event. See BENCHMARKS.md section 18.3.

**Cost to change:** cheap. The segmenter is one thread and the gate is one
lock.
**Revisit:** no.

## L079. A point lookup asks the locator instead of reading the range

**Phase:** 6
**Decision:** A filter directly over a scan whose predicate pins one exact
value is answered through `Store::lookup_correlated`, which the locator prunes,
instead of materialising the whole time range. **1,063 milliseconds to 66.**

**Why:** L045 recorded that the executor materialises the rows in the range, and
section 17.4a measured what that costs once nothing else was in the way. A point
lookup on a unique `request_id` read 438,866 rows and threw all but one away.
The locator already knew which two segments could hold the value and nothing
asked it.

**What is pushed down, and what deliberately is not:**

- `request_id`, `session_id`, `trace_id`, `event_id`, and **every property**,
  because D20 gives a dynamic scalar field exact `lookup` indexing by default;
- an `and`, on any one of its branches, because every branch must hold, so
  pruning on one keeps every row the whole predicate would keep;
- **not** an `or` or a `not`: either can match a row that carries a different
  value entirely, and pruning would lose it;
- **not** `kind`, `name`, `service_name`, or `release`. They hold few distinct
  values, so the locator would name every segment and charge an index read for
  nothing.

**The predicate still runs on whatever comes back.** A locator prunes and never
answers, so nothing here decides which rows match. It decides which rows are
read.

**The dangerous half is tenancy.** A correlated lookup reads across the whole
store, so the project filter is applied on the way back, along with the time
range, the kind, and the deduplication the range path applies.
`a_lookup_never_reaches_another_projects_rows` holds it.

**An aggregate did not move**, and should not have: 1,055 milliseconds before
and after. An aggregate over a whole range genuinely reads the range. The
finding that "a point lookup and an aggregate are the same latency" — carried in
this project's benchmarks since the first run — is now gone, because they were
only ever the same for the wrong reason.

**Cost to change:** cheap. It is one branch in the filter operator, and turning
it off restores the previous behaviour exactly.
**Revisit:** no.

## L080. Retention expiry, and the prune that comes before the read

**Phase:** 6
**Decision:** `compact` expires by retention class before it does anything else.
A segment whose newest row is past the longest retention is dropped **without
being read**. A segment past `detailed` but inside `rollup` is read once and
rewritten holding only its derived rows.

**Why:** the four classes were settings with their coupling validated at
startup, and nothing deleted a row. A home installation grew without bound,
which is the defect L071 recorded and could not close.

**The newest row decides.** A segment is dropped only when everything in it is
past the cutoff, never when part of it is. That is what makes the prune safe
without a read, and it is the same shape the tombstone path already uses.

**Zero keeps everything.** An installation that has not chosen a retention must
not lose data because a default expired it, so an unset retention expires
nothing rather than expiring on a built-in number.

**A rollup outlives what it summarises.** POLICY.md section 4 requires it and
`validate` refuses the other order, and this takes the larger of the two anyway:
a misconfiguration that reached this far still cannot delete a rollup early.

**Cost to change:** cheap. It is one pass, before the tombstone work, using the
same swap the tombstone path uses.
**Revisit:** yes, on one point. A segment is the unit, so a row inside a segment
whose newest row is recent lives past its retention until that segment is
rewritten. That is bounded by the segment's time span and it is not exact.
Exactness needs a rewrite for every segment that straddles a cutoff, which
costs a read of nearly everything.

## L081. The reserve belongs to the device and one process enforces it

**Phase:** 6
**Decision:** State the limit and make it visible rather than pretend to solve
it. `Device` now carries the device identifier, the head logs it beside the
reserve at start-up, and `tallyowl_storage_device_info` reports it.

**Why:** L037 asked for an answer before Phase 6 and this is it. Two
installations sharing a device each hold back the same bytes and each believes
those bytes are its own, so both may spend the reserve at once and neither gets
what it was promised.

**The hard guarantee is unaffected**, and that is the part that matters. A write
is refused when the device cannot take it, and that check reads real free space,
so no installation ever writes past a full device however many are sharing it.
What cannot be enforced across processes is the **soft** guarantee that recovery
will find the reserve unspent.

Three shapes were considered for enforcing it and all three are worse than
stating it:

- **a lock file on the device** needs a writable path outside every
  installation's own directory, which an operator has not granted and should
  not have to;
- **dividing the reserve by an installation count** needs the count, and nothing
  can discover it;
- **refusing to start when another installation shares the device** needs to
  find the other installation, and a process cannot enumerate data directories
  it was never told about.

What is left is to report the device, so an operator comparing two installations
sees one number that tells them. Nothing else can.

**Cost to change:** cheap if a coordination mechanism ever exists.
**Revisit:** no. The constraint is real and it belongs in DEPLOYMENT.md rather
than in code.

## L082. The burst measurement was pacing itself

**Phase:** 6
**Decision:** A rate of zero means do not pace. The burst step offers as fast as
every producer can go.

**Why:** the burst multiplier has come back below or near 1 for four runs, and
three reports have called it "not demonstrated". The reason was in the harness:
`hold` divides a target rate across its producers and clamps the result to at
least one, so a burst asking for no pacing paced **at one event each second**.
The first unpaced run reported a burst of 7.5 events each second and nobody
could have read that as anything but broken.

A paced producer sleeps between events and therefore cannot outrun the
collector, which is the whole reason a burst multiplier needs a different mode
rather than a bigger target.

**A refusal is now the answer rather than a failure.** An unpaced producer fills
the driver's unacknowledged bound and `Capture` returns backpressure, which is
exactly the boundary the multiplier is about. The report carries what was
offered beside what was absorbed, because absorbing everything says nothing when
the offer was small.

**The ramp had a second fault beside it.** A step that missed its target aborted
the ramp, so a run that achieved 444 against a target of 500 — one percent low,
at the very bottom of the ramp, from scheduler jitter — published 444 as the
sustained rate. A ceiling is a system that stopped going faster, so the rule now
also requires the step to fail to improve on the best rate achieved so far.

**Cost to change:** cheap, and it changes what past numbers mean. Every burst
multiplier this project has published measured a paced producer.
**Revisit:** no.

## L083. Phase 7 was built because the owner asked for it, and the prompt said not to

**Phase:** 7
**Decision:** Build Phase 7, replicated storage, in full.

**Why:** the implementation prompt says "Do not start Phase 7", and it gives the
reason: "Replicated storage adds nothing to an alpha, because alpha targets the
home profile, which is one node." The owner then asked for Phase 7 directly.
The owner's instruction is the later and more specific one, so it wins, and the
prompt's reasoning is recorded here rather than argued.

**What the prompt was right about, and what it costs.** A home installation
gains nothing from this phase. Every module in `tallyowl-cluster` is unused or
degenerate at one node: no controller quorum, one tablet, one voter,
`local-one`, no replication listener, and no consensus messages on any socket.
The cost is not zero, though, and it is worth naming: the workspace gained
openraft and a tokio runtime, and `tallyowl-cluster` is 7,130 lines of source
and 2,365 of tests that a home installation compiles and never runs.

**Cost to change:** expensive to remove and cheap to leave. Nothing above the
`Store` contract knows this exists.
**Revisit:** **yes.** If the alpha review decides an alpha ships without it, the
crate can be excluded from the default workspace members and a home build stops
compiling openraft entirely.

## L084. The TLS carrier was fixed before anything measured replication over it

**Phase:** 7
**Decision:** `crates/tallyowl-rpc/src/duplex.rs` replaces `rustls::StreamOwned`
with a connection behind a mutex that nobody holds across a blocking socket
call. Two handles read and write at the same time, so the server loop and the
pipelining client are now one piece of code for the plain path and the secure
one.

**Why now:** L058 said this belonged "before the replicated write path is
measured, because measuring replication over a serial carrier would measure the
carrier". A tablet group multiplexes every group it holds over one connection
for each peer, so a carrier that served one request at a time would have made
the multiplexing a fiction.

**What the first version got wrong, and how it was found.** Feeding raw bytes to
rustls in a loop looked obviously right and rustls answered `received plaintext
buffer full`, which closed the session under a pipelining client. rustls holds
decoded plaintext until the caller reads it and refuses more input while that
store is full, so the pump now offers **one** read's worth at a time and keeps
whatever was not taken. The test that found it sends sixteen 40 KiB calls at
once; a smaller payload would have passed and the defect would have appeared
under load instead.

**Cost to change:** moderate and localised. It sits under `FrameCarrier` and
nothing above it moved.
**Revisit:** no. L058 is now closed.

## L085. A consensus payload travels as `bytes`, and the envelope around it does not

**Phase:** 7
**Decision:** `csil/tallyowl-cluster.csil` declares `ConsensusMessage` with the
group, the sender, the placement generation, and the algorithm's own encoding in
a sized `bytes` field. TallyOwl does not re-declare openraft's message types.

**Why:** D15 says use an existing consensus implementation and never invent the
algorithm, so its message types are that library's contract rather than
TallyOwl's. Re-declaring them would make TallyOwl responsible for keeping two
encodings in step across a library upgrade, for a payload no other language
reads. The part TallyOwl owns is the routing envelope, and that is declared.

**The three shapes considered before this one**, per section 10.1 of the
implementation prompt:

1. **Declare each openraft type in CSIL.** It works and it is wrong: `Entry`,
   `Vote`, `Membership`, and `SnapshotMeta` are generic over the type
   configuration and change between library versions, so the specification would
   track a dependency rather than a contract.
2. **One operation for each message kind.** Three operations rather than one and
   a `kind` field. It gives nothing: the payload would still be opaque, and the
   routing envelope would be repeated three times.
3. **A separate transport for consensus.** A second carrier beside CSIL. It
   fails the requirement it exists for: `AGENTS.md` puts every native
   server-to-server hop on CSIL over TLS over TCP.

No csilgen request was opened and none was needed. The specification validates
and generates for Rust, Go, and TypeScript unchanged.

**Cost to change:** cheap. A later version can declare the types if openraft's
own shapes stabilise.
**Revisit:** no.

## L086. One type configuration, three state machines

**Phase:** 7
**Decision:** openraft binds one application request type to one type
configuration. TallyOwl has three kinds of group — the global directory, a cell
controller quorum, and a tablet — so the request type is an encoded command and
`raft::machine::GroupMachine` decodes it behind each group.

**Why:** three type configurations would have meant three transports, three
registries, and three of every operational limit, for three types that travel
one wire. The cost is that consensus cannot type-check a command against its
group, so a command sent to the wrong group decodes as a refusal rather than
failing to compile. `GroupKey` makes that hard to do by accident and the state
machine reports it rather than applying something it half understood.

**Cost to change:** moderate. It would touch the registry and the storage
implementation and nothing above them.
**Revisit:** no.

## L087. A tablet snapshot carries its marks, not its rows

**Phase:** 7
**Decision:** `TabletMachine::snapshot` encodes the tombstone generation, the
compaction generation, and the applied index. It does not encode the rows.

**Why:** `docs/STORAGE.md` section 6 says a move "copies sealed segments,
catches up committed WAL positions, verifies checksums, then changes placement
generation". Segments move as segments, over `fetch-segment`, in bounded chunks
with a BLAKE3 digest each. A consensus snapshot that carried a whole tablet's
rows would make one message as large as the tablet, and openraft holds a
snapshot in memory while it installs it.

**What this means for a new replica, stated plainly.** It catches up from the
log for anything the log still holds, and from a segment copy for anything
purged behind a snapshot. **The segment copy path is built and its parity check
is tested; nothing drives it automatically yet.** A replica added to a tablet
whose log has been purged past its position will not catch up on its own. See
the alpha report's "what is not built".

**Cost to change:** moderate. It is a background task over an interface that
exists.
**Revisit:** **yes.** It is the largest gap in this phase.

## L088. A write is bounded, because a leader without a quorum waits for ever

**Phase:** 7
**Decision:** `GroupRegistry::propose` waits `replication.writeTimeout` for a
commit and then refuses with a retryable error.

**Why:** consensus is right to keep an append outstanding while a partition
lasts, because the entry may yet commit. A caller cannot wait that long. The
first version of the quorum-loss test found this the direct way: it hung, and
the whole suite hung with it.

The refusal is retryable and says the group may have no quorum, and the batch ID
makes the retry one logical commit whichever way the original went. That is the
only honest answer available: `AGENTS.md` forbids claiming exactly-once
transport, and this is exactly the case that would tempt somebody to.

**Cost to change:** cheap, and it is a setting.
**Revisit:** no.

## L089. The controller decides in every mode and acts in one

**Phase:** 7
**Decision:** `placement.mode` defaults to `recommendation-only`. The controller
produces the same recommendations in all three modes; the mode decides only
whether they run.

**Why:** `docs/CELLS.md` section 6 says the first releases can use
recommendation-only "until tests prove safe automatic control". A mode that also
skipped the decision would leave the automatic path as code nobody ran until the
day it was switched on. This way every test and every installation exercises the
decision, and only the acting is gated.

**Cost to change:** cheap. It is one setting and one match.
**Revisit:** **yes.** The owner may want `automatic` once a cluster has run.

## L090. Restore is refused rather than half done

**Phase:** 7
**Decision:** `snapshot-cluster` takes and checksums a cluster snapshot.
`restore` refuses, names the snapshot, and points at the documented path.

**Why:** a restore replaces a tablet's data and this build does not move
segments between nodes on its own, so a `restore` that acknowledged would tell
an operator their data was back when it was not. `docs/FAILURE_MODES.md` section
6.2 makes restore the **default** answer to a permanent quorum loss, which makes
a false acknowledgement here worse than anywhere else in the system.

The refusal names the procedure that does work: restore the data directory from
the ordinary backup and start the node.

**Cost to change:** moderate. It needs the same segment transfer L087 needs.
**Revisit:** **yes.** It is the second largest gap in this phase.

## L091. `remote-one` refuses rather than quietly becoming `local-quorum`

**Phase:** 7
**Decision:** A `remote-one` write waits for a durable copy outside the write
region and refuses if none arrives inside the timeout. It never acknowledges on
the local quorum alone.

**Why:** `docs/CELLS.md` section 8 says each receipt gives its satisfied policy,
and a policy that degraded silently under a slow link would make the receipt a
lie at exactly the moment somebody reads it. A tablet with no replica outside
its write region refuses every `remote-one` write and says so at the first
attempt rather than at the first outage.

**Cost to change:** cheap.
**Revisit:** no.

## L092. The scale simulations count control work and measure nothing

**Phase:** 7
**Decision:** `simulate.rs` builds a real topology and a real directory at 400
nodes in one cell and 10,000 nodes across 25 cells, and counts what the
controller and the directory hold. It does not simulate the data path and
publishes no rate.

**Why:** `docs/PLAN.md` calls the scale simulations "a milestone, not a gate"
and says not to block the phase on hardware the project does not have. The two
properties they exist to show are countable rather than measurable: a controller
quorum that does not grow with the cell, and a directory that does not grow with
the tablets. Both would be visible here if they broke, and neither is a
throughput claim.

Section 9 of the implementation prompt forbids adding component measurements and
calling the sum an answer. These are counts of records rather than measurements
that interact, and the module says so where it sums across cells.

**Cost to change:** cheap.
**Revisit:** no.

## L093. What Phase 7 does not cover, written down before somebody assumes it does

**Phase:** 7
**Decision:** Record the gaps here rather than letting a passing suite imply
more than it proves.

The consensus tests run real openraft groups over the real CSIL transport, on
real loopback sockets, against real durable storage. D27 named four things its
own measurement left open, and this closes one and part of another:

| D27 left open | State |
| --- | --- |
| Durable storage, where every append pays an fsync | **Covered.** `raft/storage.rs` on redb with immediate durability, and a restart test |
| A real network with loss, reordering, and delay | **Partly.** Real sockets and a real codec; loopback does not lose, reorder, or delay |
| A partition that splits a group other than by isolating one node | **Not covered** |
| Recovery from a corrupt or truncated log | **Not covered** |

Also not covered: the reference application against three heads. It runs against
a replicated store with one voter and a real consensus group, which is what
proves the seam did not move, and it has not been run against a three-node
installation.

**Cost to change:** the first two need a network fault injector; the third needs
a corrupted-log fixture. Neither is large.
**Revisit:** **yes.** D15's selection stays open on these, exactly as it says.

## L094. The generation fence is on the write path, and deliberately not on the consensus path

**Phase:** 7
**Decision:** A consensus message carries a placement generation and a receiver
refuses one that is older than its own. **A running node sends zero**, which
means "no generation asserted", so the check never fires between two healthy
nodes. The fence that actually protects a write is the epoch check in
`routing::check_fence`, on the write path.

**Why, and this is the part worth keeping:** a node learns the placement
generation from its cell controller quorum, and the controller quorum talks over
the same transport. A node that fell behind on topology would have its
controller-group messages refused for being behind, so it could never catch up
on the thing it was behind on. The fence would wedge exactly the node it was
meant to redirect.

Refusing only tablet-group messages avoids the deadlock and keeps a smaller
version of the same hazard: a lagging node's tablet appends get refused, and it
catches up through the controller group instead. That is defensible and it is
still more moving parts than the write-path epoch check, which needs none of it.

**What is actually protected, and by what.** Two writers in two regions is what
a fence exists to stop, and `check_fence` stops it: a write carrying an old
epoch, or arriving from a region that no longer owns the write, is refused with
the reason and the current write region. That check is on the write and does not
depend on a peer's opinion of the generation.

**What the consensus-path check is for.** It stays, it is tested, and it is what
an operator gets if a tool sends a message with a generation asserted. It is a
diagnostic, not a safety property, and this entry exists so nobody later reads it
as one.

**Cost to change:** cheap. Making it fire is a shared counter and one rule about
which group kinds it applies to.
**Revisit:** **yes.** If a later phase wants the fence on consensus messages, it
has to answer the deadlock first.

## L095. The consensus log is a second full copy of every batch, and TallyOwl's own snapshot cannot replace it

**Phase:** 7
**Decision:** Report the measurement and do not paper over it. The snapshot
policy stays as it is until the segment-copy path L087 defers exists.

**What was measured**, on the same machine, the same filesystem, the same seed,
`--release`, from an empty `data/`, one voter:

| Measure | Not replicated | Replicated |
| --- | --- | --- |
| Events | 479,230 | 488,464 |
| Batches | 2,785 | 2,817 |
| Whole `data/head`, bytes for each event | **214.6** | **3,577.2** |
| Of which the consensus log | none | **3,341.4**, which is 579 KB for each batch |
| Of which the store | 214.6 | 235.8 |

**These are the numbers before L096.** After it the whole directory is 1,034.2
bytes for each event and the log is 798.3, which is about five times an
unreplicated installation rather than sixteen. The rest of this entry stands:
what remains is a raft entry that is the whole batch, and nothing that reclaims
it.

**The consensus log is sixteen times the size of everything else put
together.** Part of that **was** a defect, and L096 found and fixed it: every
batch was encoded as a CBOR array of integers rather than a byte string, twice
over, which doubled it twice. The numbers in the table above are from before
that fix. What remains after it is a raft entry that is the whole batch, plus
redb's own amplification.

**Why it was not reclaimed, and this is the part that matters.** A raft log is
purged behind a snapshot. TallyOwl's tablet snapshot deliberately carries the
marks and **not the rows** (L087), because a snapshot holding a whole tablet's
rows would make one message as large as the tablet. So the log cannot be purged
without losing the only copy of entries a lagging replica might still need, and
the copy that would replace it — a sealed-segment transfer — is built as an
interface and is not driven by anything.

The run never reached the 8,192-entry snapshot threshold either, at 2,817
entries, so nothing was even attempted.

**The three answers, and why none of them is taken today:**

1. **Lower the snapshot threshold.** One line. It purges the log and leaves a
   lagging replica with no way to catch up, because the thing that would let it
   is L087. It trades a disk problem for a correctness problem.
2. **Compress the entry.** A batch already travels compressed on the delivery
   queue, and the same codec would apply here for the same reason. It reduces
   the number and does not change its shape.
3. **Build the segment copy, then purge.** The right answer, and it is L087.

**What this means for an operator today.** A replicated tablet needs roughly
sixteen times the disk of an unreplicated one for the same data, and the excess
is the consensus log rather than the data. `storage.reserveBytes` is unchanged
and the disk-exhaustion path still refuses a write it cannot make durable, so
this fills a disk safely rather than unsafely. It is still the largest single
cost this phase added.

**Cost to change:** the real fix is L087, which is moderate. Compression is
cheap and would help immediately.
**Revisit:** ~~**yes, and this is the first thing to look at.**~~ **Done.** L087
built the segment copy and L099 took both levers: the log is bounded at about
4,608 entries and each entry is compressed. `docs/BENCHMARKS.md` section 20
measured the log 21.8 times smaller and the whole data directory 4.0 times
smaller, and a replicated tablet at 1.27 times an unreplicated one rather than
five times.

## L096. Every replicated batch was doubled twice, because serde thinks `Vec<u8>` is a list of numbers

**Phase:** 7
**Decision:** `crates/tallyowl-cluster/src/raft/mod.rs` holds a `byte_string`
module that serializes a `Vec<u8>` with `serialize_bytes`, and the two fields
that carry a batch use it.

**What was wrong.** serde treats `Vec<u8>` as a sequence, so a derived
`Serialize` writes CBOR major type 4 — an array — with a head for every element.
A byte below 24 costs one byte and every byte above it costs two, so a thousand
bytes of telemetry encode to 2,003. Told that they are bytes, the same thousand
encode to 1,003 as a byte string.

**It applied twice.** A batch travels inside `TabletCommand::Commit`, and that
whole command then travels inside `GroupRequest.payload`, which is another
`Vec<u8>`. Both were sequences, so the batch was doubled, and then the doubled
form was doubled again.

**How it was found.** Not by reading the code. The load run said a replicated
tablet used sixteen times the disk of an unreplicated one (L095), and sixteen is
not what "store the batch twice" should cost. `crates/tallyowl-cluster/tests/amp.rs`
decomposes it, and the first line of that decomposition was a replicated command
1.74 times the size of the batch it carried, for an envelope that holds a group
reference and a variant tag.

**Measured, before and after**, 34,600 events in 200 batches on the same
fixture:

| Measure | Before | After |
| --- | --- | --- |
| Replicated command against the batch it carries | 1.74x | **1.001x** |
| Consensus directory, bytes for each event | 2,015.4 | **1,007.8** |

**Exactly half on that fixture, which is the arithmetic working out.** The full
load run gives the compound figure, because the doubling applied twice: the log
fell from 3,341.4 bytes for each event to **798.3**, which is 4.2 times, and the
whole data directory fell from 3,577.2 to **1,034.2**. The store did not move —
235.8 against 235.9 — which is what says the change was in the encoding and
nowhere near the data.

**What is left, and it is not a defect.** The consensus directory is still 2.7
times the commands it was given. That is redb: a copy-on-write B-tree, a
durable commit for every append and every apply, and a file that grows and does
not shrink. L095's option 2 — compressing an entry — is the next lever, and on
this fixture one batch compresses 36.8 times at zstd level 1. **That number is
an upper bound and not a prediction**: the fixture repeats the same event name,
route, plan, region, service, and release on every row, and real telemetry does
not. The delivery queue already compresses a batch for the same reason, so the
precedent and the codec both exist.

**Cost to change:** cheap, and it is done. A log written before this reads back,
because the deserializer accepts a sequence as well as a byte string.
**Revisit:** no. Compression is L095.

## L097. The sealed-segment copy, and the seal that makes a snapshot mean something

**Phase:** 7 (revisited)
**Decision:** `crates/tallyowl-cluster/src/transfer.rs` copies sealed segments
between nodes, over `list-segments` and `fetch-segment`, and
`TabletMachine::before_snapshot` seals the store before a consensus snapshot is
built.

**Why:** L087 left the copy as an interface nothing drove, and named that as the
largest gap in Phase 7. Two things were missing rather than one:

1. **Nothing served the segments.** `fetch-segment` and `fetch-snapshot-chunk`
   were declared in the contract and answered by no operation. A declared
   operation nobody answers is worse than one nobody declared, because a caller
   reads the contract.
2. **A snapshot did not cover what it claimed to.** A tablet snapshot carries
   the marks and not the rows, so purging the log behind it would take the only
   copy of rows that were applied and not yet sealed. Sealing first is the
   sentence that closes it: everything a snapshot covers is now in a segment,
   and a segment is what the copy carries.

A failure to seal fails the snapshot, so the log grows rather than losing what
it holds. Growing is a disk problem and losing is a data problem.

> **Read [L147](#l147-l097-wanted-a-marker-in-the-segment-format-and-the-marker-is-on-every-row) after this.** The paragraph below is the one that was
> carried through three phases as needing a change to the segment format. It
> does not: a segment was the wrong unit to reconcile on, and the identity the
> reconcile needs has been on every row since Phase 3. A copy onto a node that
> already holds part of the tablet reconciles now.

**What the copy refuses.** A copy onto a node that already holds sealed segments
for the tablet. Two replicas that applied the same entries build differently
shaped segments, so their content addresses do not match and there is no honest
way to tell a copied segment from one the target built itself; a merge would
count the overlap twice. The two cases this exists for — a new replica and a
tablet movement — both start from nothing, and the refusal says so.

**Nothing the sender says is trusted.** The manifest does not travel:
`SegmentedStore::install_segment` derives it from the segment, which is
self-describing, exactly as `snapshot::rebuild` derives one from a file on disk.
The digest travels and is recomputed from the bytes that arrived.

**Cost to change:** moderate. One new CSIL operation and one new store method.
**Revisit:** **yes**, on one point: a copy onto a target that already holds
overlapping data is refused rather than reconciled. Reconciling it needs a way to
tell two replicas' segments apart, and neither the segment format nor the
manifest carries one today.

## L098. An erasure is proposed, and the predicate travels rather than a generation

**Phase:** 7 (revisited)
**Decision:** `Store::erase` is on the contract. `ReplicatedStore::erase`
proposes `TabletCommand::Tombstone` through the tablet group, and the command
carries the whole predicate in the store's own encoding.

**Why, and this is the part worth keeping:** the command already existed and it
carried a column, a value, and a generation, and applying it only advanced the
generation. **A follower bumped a number and hid nothing.** A predicate is what
hides a row, so a predicate is what has to arrive. The old shape would have
passed a test that asserted the generations agreed, and a query on a follower
would still have returned the rows a person asked to have removed.

`tallyowl_store::catalog::encode_tombstone` is now public for the same reason the
replicated commit carries the append log's own row frame: one codec, so a leader
and a follower cannot read an erasure differently.

**Applying it twice is applying it once.** The erasure ledger is keyed by the
tombstone identifier, so a replayed entry replaces the same record rather than
adding a second one, which is what a state machine fed from a log requires.

**Cost to change:** moderate. It touched the `Store` contract, which D25 keeps
small on purpose, and this is the second time Phase 7 has had to widen it.
**Revisit:** no. An erasure that reached one replica is not an erasure.

## L099. The consensus log is bounded and compressed, and the bound is what mattered

**Phase:** 7 (revisited)
**Decision:** A group snapshots every 4,096 entries and keeps 512 after it, and
each log entry is stored zstd-compressed behind a four-byte frame.

**Why:** L095 measured a replicated tablet at about five times the disk of an
unreplicated one and named the cause: the log was a second full copy of every
batch and **nothing reclaimed it**, because a snapshot could not be purged behind
without losing the rows a lagging replica needed. L097 gives that replica the
other copy, so the purge is now safe and the log is bounded at about 4,608
entries rather than at the number of batches the installation has ever taken.

**Why 4,096 and not a smaller number.** A tablet snapshot seals, and a seal makes
a segment. A snapshot every few hundred entries would make many small segments,
and the catalog — 3.4 times the size of the data it indexes — is what pays for
those. `docs/BENCHMARKS.md` section 18 measured about 173 events for each batch,
so 4,096 entries is roughly 710,000 rows, near the 800,000 the store's own
sealing policy targets. The two policies agree rather than fight.

**Compression is the smaller half and it is honest about that.** L095 called it
option 2 and said it reduces the number without changing its shape. The bound is
what changes the shape. Both are in, because the constant is large.

**The frame.** `TOL1` and a form byte. An entry written before this frame existed
is bare CBOR, and an `Entry` encodes as a CBOR map or array, so it can never
begin with 0x54 — a byte string head. An installation that upgrades reads its own
log rather than discarding it.

**What it gave, measured.** `docs/BENCHMARKS.md` section 20: the log fell from
798.3 bytes for each event to **36.6**, and the whole data directory from 1,034.2
to **259.2**. A replicated tablet needs 1.27 times the disk of an unreplicated
one rather than five times.

**The load run measures the compression and not the bound**, and the section
says so rather than letting the number imply otherwise: 2,964 batches is below
the 4,096-entry threshold, so nothing was purged. The bound is the larger of the
two changes and it is what stops the log growing with the installation's
history; a test proves it and this run is too short to reach it.

**Cost to change:** cheap. Two constants and one function.
**Revisit:** **yes.** The two numbers should be settings once a cluster has run
long enough to show what a real tablet's batch size is. They are constants today
because a setting nobody has measured is a constant with more places to be wrong.

## L100. A restore puts the rows back, and refuses onto live data

**Phase:** 7 (revisited)
**Decision:** `snapshot-cluster` writes the tablet's own store into
`<data>/snapshots/<id>/<tablet>` and digests the marks and the watermark
together. `restore` verifies every segment, refuses onto a node that already
holds data for the tablet, and otherwise adopts each segment through the path
L097 built.

**Why the old refusal was right and incomplete.** L090 refused rather than half
doing it, and that was the correct call at the time. But the deeper problem was
one layer down: **`snapshot-cluster` took the state machine's marks and none of
the rows.** The one thing a restore needed was the one thing the snapshot did not
hold. A refusal to restore an unrestorable snapshot is honest and it leaves an
operator with a backup that is not a backup.

**What it still refuses, and why that is not a gap.** Restoring on top of live
data can lose an acknowledged write and there is no way to take that back, so the
online command refuses and names `tallyowl-head restore <directory>`, which is
`docs/FAILURE_MODES.md` section 11 procedure 3 and which already worked. The
online path is for the case restore exists for: a tablet whose quorum is gone,
coming back onto a node that holds none of it.

**Cost to change:** moderate.
**Revisit:** no. Both halves now do what they say.

## L101. Reads fan out as a store, not as a second path in the head

**Phase:** 7 (revisited)
**Decision:** `crates/tallyowl-cluster/src/fanout.rs` asks every readable tablet
through `partial-aggregate` and merges, and `ReplicatedStore` delegates `scan`,
`trend`, `lookup_correlated`, and `lookup_event` to it. The head's query executor
is unchanged.

**Why:** the alternative was a fan-out inside `QueryService`, reached only when an
installation has more than one tablet. That is the path nobody runs at home and
everybody runs in production, which is the shape of every defect this project has
paid for. Phase 1 put a real `Store` contract in place so that a later phase
could replace the implementation without moving the seam, and Phase 7 already
used it once for writes. This is the same move for reads.

**What this fixed that was worse than a missing feature.** `LocalReplica::partial`
ignored the request's `kind` and answered a `rows` request with a count. A
coordinator asking for rows would have received none and merged them into an
answer of zero, marked complete. Two tablets, both healthy, and a query that
returns nothing and says nothing.

**What is pushed down and what is not.** A count and a trend are computed on the
tablet and merged, which is what `docs/QUERY.md` section 10 asks for. A general
aggregate is not: the head's executor computes its measures over rows, so a scan
that feeds one moves rows. That is correctness-preserving and it is not the
design's intent.

**Cost to change:** moderate. Pushing the general algebra down means mapping
measures and dimensions onto partial states, and not every measure has one.
**Revisit:** **yes.** The push-down of the general aggregate is the performance
half of this and it is not built.

## L102. The locator was built in two places, and the two went out of step

**Phase:** 7 (revisited)
**Decision:** `tallyowl_store::segmented::index_row` is the one place a row's
exact-lookup values enter a locator run. The seal path, the compaction path, and
the segment install path all call it.

**How it was found.** Not by a failing test. Writing the segment install path
meant reading both of the existing ones side by side, and they were different:
the seal path indexed `event_id`, `trace_id`, `session_id`, `request_id`, and
every property; the compaction path indexed `event_id`, `session_id`, and every
property, and dropped `trace_id` and `request_id`.

**Why that was a wrong answer rather than a slow one.** The locator *prunes*.
`lookup_correlated` skips a segment that a non-empty candidate set does not name,
and treats an empty set as "no pruning". So a compacted segment holding a trace
was skipped whenever **any other** segment still named the same trace — which is
the ordinary case for a trace that spans a compaction boundary. The answer came
back smaller, with nothing marked incomplete, which is the failure
`docs/FAILURE_MODES.md` section 2 ranks worst.

A locator has to be built in one place for the same reason a checksum has to be
computed in one place.

**Cost to change:** cheap, and it is done.
**Revisit:** no. Worth reading as an argument for the rule rather than for the
fix: two copies of a derivation will diverge, and this pair diverged without
anybody changing either one on purpose.

## L103. The identity graph is derived from the rows, and nothing else

**Phase:** 8
**Decision:** `crates/tallyowl-head/src/identity.rs` builds the identity graph
by reading `identify`, `alias`, and `group` rows out of the same scan the query
is already doing. There is no durable graph.

**Why:** `AGENTS.md` requires every derived projection to be reproducible from
retained raw data, and a graph that is only ever derived is reproducible by
construction rather than by discipline. Two other properties fall out of it:

- **an erasure removes a person from the graph by the same act** that removes
  their rows, because the graph *is* the rows. A durable graph would need its
  own erasure, and the one that was forgotten would be the one that mattered;
- **there is no second copy to fall out of step with the first.** L102 is what
  happens when there is.

**What it costs, stated rather than hidden.** A query that resolves identity
scans its own range *and* everything before it for identity rows, because an
`identify` from last month is what makes this month's anonymous events belong to
somebody. A materialised graph would make that lookback a lookup.

**Cost to change:** moderate. A materialisation is a cache in front of this, and
this stays as the thing that rebuilds it.
**Revisit:** **yes.** The lookback is the cost, and it grows with the
installation's age rather than with the query's range.

## L104. A funnel correlates by latest-known identity, and a test found the other choice wrong

**Phase:** 8
**Decision:** `QueryForm::Funnel` resolves identity as latest known. A timeline
does too. Nothing in this build uses event-time resolution by default, and both
are implemented.

**Why, and this is worth keeping because it was written the other way first.**
The first version used event time, with a comment saying that a sign-up funnel
"should start where the person started rather than where they signed in". That
sentence is true about *when a step happened* and wrong about *who did it*. Under
event-time resolution, a person who views the pricing page anonymously, signs in,
and buys is **two correlation keys**: an anonymous visitor who vanished at step
one and a customer who appeared at step two. Every sign-up funnel would have
shown nobody converting.

`docs/DATA_MODEL.md` section 3.5 keeps both resolutions because they answer
different questions. What a funnel needs is one key for one person, and that is
latest known. The steps keep their own times either way.

**How it was found.** A fixture written from the outside: three rows, one
person, and an assertion that the funnel counts one conversion. The test failed
on the first run and the code was wrong, not the test.

**Cost to change:** cheap, and it is one field on the question.
**Revisit:** no. But the ability to *ask* for event-time resolution should reach
the contract, because a cohort question genuinely wants it.

## L105. An unordered funnel opens its window on any step, not on the first one

**Phase:** 8
**Decision:** In ordered mode a sequence opens at the first row matching step
one. In unordered mode it opens at the first row matching **any** step.

**Why:** `docs/QUERY.md` section 12.1 says "unordered mode does not" require the
steps in order, and a window anchored on step one contradicts that: a person who
did step two and then step one has done both, and a window that started at step
one would have closed before it opened. The first version anchored both modes on
step one and the unordered case counted nobody, which made unordered mode a
slower way of asking the ordered question.

**Cost to change:** cheap.
**Revisit:** no.

## L106. Collection policy and saved analyses are in memory, and a restart loses them

**Phase:** 8
**Decision:** `PolicyService` and `SavedService` hold their state behind a mutex
in the head's process. Neither is durable.

**Why:** both are small control-plane records and both belong in the control
catalog beside workspaces, projects, and keys. Putting them there is a schema
change to the durable catalog, and this phase's exit criteria are about
behaviour rather than about persistence: a policy that compiles, applies, and
refuses correctly is the part that is hard, and the part a wrong answer comes
from.

**What that means for an operator today, said plainly.** A head that restarts
collects everything again and shows no saved analysis. That is a real gap and it
is not a subtle one.

> **Fixed, the same day.** The owner read this and asked for it, which is what
> marking an entry for revisit is for. **L112** holds what was built: three
> record kinds in the control catalog, read back at start-up, written before
> they are applied. The rest of this entry stands as the reasoning that was
> wrong, and it was wrong in a particular way worth keeping — "this phase's exit
> criteria are about behaviour rather than about persistence" is true and it is
> not a reason to ship a surface that loses its state.

**Cost to change:** moderate. It is two tables in the control catalog and the
same shape the key and session records already use.
**Revisit:** ~~**yes.** This is the first thing to build in Phase 9 or before
it.~~ **Done.** See L112.

## L107. The collection policy is applied at the head and not yet at the collector

**Phase:** 8
**Decision:** `crates/tallyowl-head/src/ingest.rs` applies the compiled policy
before anything is durable. `docs/POLICY.md` section 7 also distributes the
snapshot to collectors, and that distribution is not built.

**Why the head is the right place even though it is not the only one.** The head
is the durability boundary. A rule enforced only at the edge is a rule an edge
that skipped it can break, and a row that reached storage cannot be
un-collected. So the head enforces it whether or not a collector did.

**Why the collector still has to.** A blocked event that reaches the head has
already cost a batch, a queue write, and a delivery. The point of policy at the
edge is that it costs nothing, and a kill switch that only takes effect after
transport is not a kill switch.

**What a refused item does.** It is counted in
`tallyowl_events_rejected_total{reason="collection-policy"}` and named on the
receipt as a rejected item. A silent drop is how somebody discovers a policy by
noticing a gap months later.

**Cost to change:** moderate. The collector already has `fetch-policy`, which
answers "there is nothing to apply" today; the missing half is the head-to-
collector fetch and the cache POLICY.md section 7 describes.
**Revisit:** **yes.**

## L108. `get-policy` changed shape, because the shape it had could not carry a policy

**Phase:** 8
**Decision:** `get-policy` was `ListRequest -> Empty` and is now
`PolicyRequest -> CompiledPolicy`. `put-policy` and the five saved-analysis
operations are new, at wire IDs 17 to 22.

**Why a change rather than a new operation.** The rule is to add a message and a
wire ID rather than change an existing one, and it is the right rule when
somebody is using the old shape. Nothing answered `get-policy` and nothing
called it: it returned `Empty`, which is the shape of a placeholder rather than
of a contract. Adding `get-policy-2` beside a placeholder would have left the
placeholder in the contract for ever.

**Cost to change:** cheap, and it is done. Every generated package regenerates.
**Revisit:** no.

## L109. A query form that authorization forgets is a query form nothing authorizes

**Phase:** 8
**Decision:** `query::projects_named` matches exhaustively on `QueryForm`, and a
request that names no project at all is refused.

**How it was found, and it is the reason this entry exists.** The four new
domain operators were built and wired before this was looked at.
`projects_named` read the trace form and the node tree, and returned an empty
list for anything else — and `authorize_query` loops over that list, so an empty
list authorised nothing and permitted everything. A funnel naming another
tenant's project would have been answered.

It was never reachable, because the forms did not exist until the same change
added them. It would have been reachable for exactly as long as it took somebody
to notice.

**The fix is the exhaustive match rather than four more lines.** Adding a form to
`QueryForm` and not to this now stops compiling, which is the only kind of
reminder that survives a busy afternoon.

**Cost to change:** cheap, and it is done.
**Revisit:** no. Worth reading as an argument for the shape: a `_ => {}` arm in
an authorization function is a decision to permit whatever is added later.

## L110. A retention month is 28 days, and the result says which period it used

**Phase:** 8
**Decision:** `Period::Month` is 28 days. `docs/QUERY.md` section 12.2 asks for
calendar periods "in the supplied timezone" and this build carries no timezone
database.

**Why not 30 days.** Because 30 days is nearly a calendar month and 28 is
obviously not one. A number that is nearly right invites somebody to read it as
the thing it nearly is; one that is plainly a fixed period invites them to read
the label. The result names its period, so a reader sees `day`, `week`, or
`month` beside the counts.

**Cost to change:** moderate. It needs a timezone database and the `timezone`
field the contract already carries on a `TimeRange`.
**Revisit:** **yes.** A monthly retention matrix is a thing people ask for and
this one is not calendar-accurate.

## L111. The reference application's identity journey runs beside the sessions, not inside them

**Phase:** 8
**Decision:** `expandJourney` in the simulator adds one identity journey for
each person — three client surfaces, three anonymous identifiers, one known
identifier, a funnel sequence, and return visits — as a separate pass, with its
own event names.

**Why not extend the existing sessions.** The sessions carry an `end_user`
*property* and no envelope identity, so a funnel by end user cannot see them at
all. Adding envelope identity to them would have changed every existing ledger
number in the same commit that added the new ones, and a fixture that changed
for two reasons at once is one nobody can check.

The journey's names are its own — `journey-home`, `journey-checkout`,
`journey-purchase` — so a funnel over it counts it and nothing else. **A fixture
whose steps could also be matched by other traffic is one nobody can work out by
hand**, and "explainable exact results" is the exit criterion.

**Cost to change:** cheap.
**Revisit:** no.

## L112. Collection policy and saved analyses are durable, in the catalog that already holds the rest of the control plane

**Phase:** 8 (revisited)
**Decision:** `PolicyRecord`, `AnalysisRecord`, and `DashboardRecord` are
control-plane records in `crates/tallyowl-store/src/control.rs`, under
`control/policy/`, `control/analysis/`, and `control/dashboard/`.
`PolicyService::open` and `SavedService::open` read them back at start-up, and
every write goes to the catalog before it reaches memory.

**Why this is where they go.** `docs/FAILURE_MODES.md` section 7 lists the nine
things a catalog rebuild from segment manifests cannot restore, and "saved
dashboards, queries, cohorts, funnels, and alerts" is one of them. That
sentence is the argument: something a **rebuild** cannot recover is something a
**restart** must not lose. They sit beside the workspaces, the projects, and the
keys, in the same transactional catalog, so one snapshot carries the whole
installation rather than most of it.

**They are records the catalog can read, not a blob the head handed over.** An
opaque payload would have been half the code, and it would have made every
recovery verb and every inspection useless on exactly the state
`tallyowl-head rebuild` already warns it cannot bring back.

**The working copy is still in memory, and that is not a hedge.** The ingest
path asks for a compiled policy for every row; a durable read for every row
would put redb on the hot path for no benefit. The two cannot disagree, because
a write reaches the catalog **first** and memory only when that returns: a
policy that could not be made durable was not applied either, and the refusal
says both. There is a per-project cache of the compiled result, emptied on every
write.

**What this does not solve, said plainly.** Two head processes over two data
directories still each hold their own policy. Coherence across a cell is the
controller quorum's problem and it is Phase 7's shape, not this one's; nothing
here pretends otherwise.

**A stored record this build cannot read is skipped and named, not fatal.** A
head that refused to start because one document was written by a newer release
is a head an upgrade could brick. The rest applies and the start-up log names
what did not. The policy version is still read from the catalog rather than
counted from what loaded, so a collector comparing versions gets the same number
across a restart even when a record was skipped.

**How it was checked.** Four tests over a store opened twice on one directory,
and once through the running loop: `put-policy` over the real control socket,
`./tools.sh dev down`, `./tools.sh dev up`, and `get-policy` answering with the
same version and the same blocked event name.

**Cost to change:** cheap now. It is three prefixes in a catalog that already
had eleven.
**Revisit:** no. L106 said this was the first thing to build and it is built;
that entry now points here.

## L113. The collection policy reaches the collector, and both ends compile from one snapshot

**Phase:** 9
**Decision:** The head answers `fetch-policy` on the collector contract. A
collector fetches on a timer, passes the version it holds, applies a snapshot at
a batch boundary, and judges every item of a batch against that one snapshot.
The head keeps applying the same policy at commit.

**Why:** L107 named this as the other half of L112, and the implementation
prompt named it as the first thing to build next. The head enforcing the policy
means nothing wrong is stored either way. What the collector saves is everything
before the commit: a blocked event that only the head refuses has already cost a
batch, a durable queue write, a delivery attempt, and a receipt. A kill switch
that only takes effect after transport is not a kill switch.

**The two ends read one compilation, translated twice.** `PolicyService::compile`
produces one `Compiled` and two functions render it: `to_wire_compiled` for the
control contract and `to_wire_snapshot` for the collector's. A second
compilation would drift, and the drift would be a collector dropping something
the head would have kept — data no query can find again, and nothing to say it
happened.

**A policy refusal is not a rejected item on the receipt.** A rejected item tells
a producer that something went wrong and invites a retry, and a driver that
retried a policy refusal would retry it for ever. The refusal is counted in
`tallyowl_items_dropped_by_policy_total`, which is where an operator looks. A
batch the policy empties is not written to the queue at all.

**A collector names its source and resolves it from its own credential**, on
each fetch, through the resolver that already caches it. It was tempting to
configure a source identifier and skip the resolution; that would have been the
first place in the system where a collector held tenancy of its own, which D32
forbids for the ingest path and there is no reason to relax here. It also means
a collector that could not reach the head at start-up still fetches a policy
when the head returns.

**A snapshot with version 0 is refused.** The head raises the version on every
write and never hands out 0 for a policy somebody set, so a 0 is a snapshot that
was never compiled. Applying it would let a half-built response switch off
collection everywhere at once.

**What is not built.** A collector's readiness does not fail on staleness. The
staleness is computed and reported, and `POLICY.md` section 8's second required
test asserts it; nothing turns it into a readiness failure, because a collector
that stopped accepting telemetry when it lost the head would turn a control-plane
outage into a data-plane outage. That is the wrong trade and it is deliberate.

**Cost to change:** cheap. One module in the collector, one operation on the
head, and one setting.
**Revisit:** yes. The fetch interval is a setting with a first value of 30
seconds and nothing has measured what a real installation wants.

## L114. A channel is classified at read time, and stored as well

**Phase:** 9
**Decision:** `crate::campaign` classifies a touch into a channel from the
medium, the source, the click identifier, and the referring domain. The
projection stores `campaign_channel` and `campaign_classifier_version` on the
row. **Attribution never reads them**: it classifies again from the same raw
fields.

**Why:** the Phase 9 exit criterion is "model changes recompute from immutable
facts", and a stored channel is a fact about the classifier that ran rather than
about the traffic. A corrected classifier would disagree with every row written
before it, and the only remedy would be a rewrite of cold segments. Classifying
at read time makes a correction take effect at the next question and leaves
every stored byte alone.

The stored copy earns its place separately: a general aggregate groups by a
stored column, and a person reading one touch wants to see what it was called.
The row carries the classifier version, so where the two differ the difference
is visible rather than silent.

**A producer cannot send a channel**, and that is the point. A producer that
could name its own channel could put paid traffic in the organic column, and the
number that decides a marketing budget would be one the marketing team wrote.

**The site match is by label, not by substring.** `notgoogleatall.example`
matched a bare `contains("google")` and would have gone in the organic-search
column, which is the row nobody questions. There is a test named after it.

**Cost to change:** cheap for a rule, moderate for the shape. Adding a channel
means raising `CLASSIFIER_VERSION`; every stored row keeps the version that
wrote it and every read reclassifies, so nothing has to be migrated.
**Revisit:** yes. The engine and network lists are short and hand-written. A
real installation will meet a search engine and a social network that are not on
them, and both land in `referral` rather than anywhere wrong.

## L115. A touchpoint is a campaign touch or a tagged page view, and never a bare referrer

**Phase:** 9
**Decision:** attribution counts a `campaign-touch` row always, and a `page-view`
row only when it carries campaign parameters. A referrer alone is not enough.
Two rows that describe one landing — an explicit touch and the page view beside
it — fold into one touch when they agree and are within two seconds.

**Why:** two failures, in opposite directions.

**A browser sends a referrer for a link inside the application** as readily as
for one from outside it. A rule that took any referrer would make every internal
navigation a touchpoint, and a linear model would give each one a share of the
revenue. The first draft did exactly that.

**A client that sends both an explicit touch and the page view beside it** would
be counted twice, and every linear result from that client would be wrong by a
factor a person could not see from the report. The fold is what stops it, and it
is bounded by time and by the campaign parameters agreeing, so two genuine
clicks on one campaign an hour apart stay two touches.

The consequence is that a referral that is genuinely a touch has to arrive as a
`campaign-touch` row, which says so. The reference marketing site sends one.

**Cost to change:** cheap. Two conditions in one function.
**Revisit:** yes. The two-second fold window is a constant. A client that sends
a touch and a page view further apart than that would be counted twice, and
nothing has measured what a real client does.

## L116. A conversion is idempotent by its order, and the earliest row wins

**Phase:** 9
**Decision:** two conversion rows with one goal and one order identifier are one
conversion. The earliest occurred time wins, and a tie goes to the lower event
identifier. A conversion with no order identifier is its own event.

**Why:** the phase asks for "idempotent conversion and order handling". A
checkout that retried, a webhook that arrived twice, and a person who refreshed
the receipt page produce three genuinely distinct events with three event
identifiers. They are not a duplicate delivery — DELIVERY.md section 6 already
folds those by event identifier, and these are not duplicates of each other in
that sense. Only the order identifier says they are one sale.

**The earliest wins, and that is not arbitrary.** The conversion time is what
the lookback window is measured back from. A repeat that arrived a day late
would widen the window if it decided the time, and a campaign would gain or lose
credit because of when a webhook was retried. There is a test named after it.

**A conversion with no order identifier is not folded.** Nothing says two
purchases of the same value a minute apart are one, and a fold on value and time
would silently lose a genuine second purchase.

**Both rows stay stored.** The fold is at read time, like every other derived
thing in this system, so nothing is lost and a later question can count rows if
it wants them.

**Cost to change:** cheap.
**Revisit:** no.

## L117. Credited value is divided by the largest-remainder rule, so the parts add up

**Phase:** 9
**Decision:** `crate::money` holds an exact decimal amount and divides it
between weights. It lifts the value to six digits past its own scale, hands out
whole units by each weight's share, and gives the units that do not divide to
the largest remainders, one each, earliest first.

**Why:** attribution is the one place in this system where money meets a
fraction, and `docs/QUERY.md` section 4.1 says a decimal never becomes a float.
A third of 19.99 is not a number at any scale, so the question is not whether to
round but where the rounding goes.

**The parts add up to the whole, exactly.** A campaign column that totalled
19.98 from a conversion of 19.99 is a report somebody has to explain, and the
explanation is always the same rounding. The largest-remainder rule is what an
election uses to hand out whole seats from fractional shares, and it is here for
the same reason: the total is fixed and the parts have to add up to it.

**A weight is a share and never an amount.** A caller that passes 2 and 2 means
half each. The models never have to make their weights sum to one, which is what
lets the decay model return raw powers of one half.

**Cost to change:** cheap. Six guard digits is a constant.
**Revisit:** no. The rule is standard and the tests state it.

## L118. The shipped attribution defaults, and why each one holds

**Phase:** 9
**Decision:** 0.4 and 0.4 for the position ends, a seven-day decay half-life, a
thirty-day lookback, ninety days of touchpoint retention, and every model
enabled. D40 now holds the table and the reason for each.

**Why:** D40 said "Phase 9 selects the shipped default values. There is no
migration cost in selecting them later," and this is Phase 9 selecting them. The
reason each one holds is written down beside it, because a default nobody can
argue with is a default nobody can change with confidence.

**Ninety days of retention is three times the lookback**, deliberately. The
coupling in POLICY.md section 5 refuses a window longer than the retained range,
so an operator can widen the window twice before they have to think about
retention. A retention equal to the lookback would refuse the first widening
anybody tried.

**The position model has two special cases** and both are choices rather than
arithmetic. One touch takes everything. Two touches divide what the two ends
were given, in proportion, because there is no middle to hold the remaining 0.2
and an operator who set 0.4 and 0.4 meant the two ends equally. The alternative
— giving the two ends 0.4 each and losing the 0.2 — would make a two-touch
journey credit less than the revenue it came from.

**Cost to change:** cheap. Every one is a number in the control catalog and a
change recomputes rather than migrating.
**Revisit:** yes, generously. Not one of these is measured. They are the values
a person would defend in a meeting, not values this project has evidence for.

## L119. Attribution settings are the project's, and a request can never carry a weight

**Phase:** 9
**Decision:** an attribution request names a model and a window. The weights,
the half-life, and the enabled models come from the project's stored settings.
The contract has no field for a weight on a query.

**Why:** D40 makes a model parameter configuration rather than code. A caller
that could send weights could make one campaign outrank another by the way it
asked, and two people reading the same dashboard would be looking at two
different questions with the same title.

The settings are durable, in the control catalog beside the collection policy
and the saved analyses, for the reason L112 gives. The version rises in the
catalog rather than at the caller, so two heads that wrote the same settings
cannot both call themselves version 3, and every result names the version it was
computed under.

**A project that never set any answers under the shipped defaults**, and a
stored record that will not check is skipped and named rather than fatal. The
same rule as L112: a head that would not start because one record was written by
a newer release is a head an upgrade could brick.

**Cost to change:** cheap.
**Revisit:** no.

## L120. Campaign capture strips the person from a touch, and the campaign from everything else

**Phase:** 9
**Decision:** the collection policy has three campaign-capture levels. At
`unlinked` a `campaign-touch` row loses its session, end-user, and anonymous
links and keeps its campaign fields; every other kind of row loses its campaign
fields and keeps its links. At `none` a campaign touch is refused and every
other kind of row loses its campaign fields.

**Why:** D30 puts consent at the point where campaign data joins an identified
end user, and names the session identifier as that point. It also requires that
"campaign capture stays usable without a session link, so an operator who turns
off session-linked campaign data still measures campaign performance."

The asymmetry is the awkward part and it is deliberate. A page view is somebody
moving around the application and its session is what a funnel correlates on;
taking that away to satisfy a campaign setting would break analytics that have
nothing to do with campaigns. So the page view keeps its session and loses its
campaign, and the campaign fact lives on the touch that has no person on it.

**The consequence, said plainly:** an application that sends campaign parameters
only on a page view records **no** campaign data at the `unlinked` level. It has
to send an explicit campaign touch. `docs/POLICY.md` section 7.1 says so rather
than leaving somebody to discover it.

**Cost to change:** cheap at the head, moderate for the rule. Both ends
implement it and both would have to move together.
**Revisit:** yes. The asymmetry is the kind of rule an operator meets once and
is surprised by, and a different split — for example, stripping the session from
a page view that carries a campaign — is defensible.

## L121. The campaign report is a domain operator, and a cost never divides by channel

**Phase:** 9
**Decision:** `campaign-summary` is a new value of `QueryForm` and a new
optional field on `QueryRequest`. It runs the same attribution the `attribution` operator runs, with
the same model, window, and settings, and adds the end users, the sessions, and
the imported cost. **A report grouped by anything other than the campaign shows
no cost and no return.**

**Why:** `docs/DATA_MODEL.md` section 6 asks a campaign dashboard for "sessions,
end users, conversions, value, cost, and return" with a "channel, campaign, and
content breakdown". A general aggregate cannot express it: the three parts come
from three datasets and the return is a ratio across two of them.

Running the same attribution rather than a second implementation is what stops a
summary row and an attribution row disagreeing about one campaign, which is the
kind of defect somebody notices in a meeting and nobody can settle afterwards.

**A cost import names a campaign and a period.** It does not say how that spend
divided between the channels the campaign reached. Dividing it evenly would be
TallyOwl inventing the number a person is reading the report to find out, so a
channel breakdown shows an empty cost column and says why in a warning.

**A campaign with a cost and no credited value is still a row.** Leaving it out
would make the report say that every campaign paid for itself, which is the one
thing a campaign report exists to answer.

**The return is absent rather than zero when nothing was spent.** A zero reads
as "this campaign earned nothing for its spend", and a campaign with no spend
recorded earned everything for nothing.

**Cost to change:** cheap for the columns, moderate for the shape. A `QueryForm`
value is part of the contract, so the name is chosen once.
**Revisit:** yes. `docs/QUERY.md` did not define this operator before this run;
section 12.7 now does, and the owner may want a different set of columns.

## L122. `get-policy` and `fetch-policy` are two operations, and they stay two

**Phase:** 9
**Decision:** the head answers `fetch-policy` on the collector contract for a
collector, and `get-policy` on the control contract for a person. Collector
intake keeps answering `fetch-policy` with "there is nothing here to apply".

**Why:** they authenticate differently and they answer differently. `get-policy`
needs a session and a project role, because a person is asking. `fetch-policy`
takes no session: a collector authenticates as a node and names a source, and
the head resolves the tenancy. Folding them would mean one operation with two
authentication paths, which is how an authorization check gets forgotten — the
defect L109 found.

**Collector intake still answers `fetch-policy`, and it answers honestly.** The
snapshot the head sends is scoped to the collector's own source, and an
application's driver is a different source with a different scope. Handing the
collector's snapshot to a driver would give an application another source's
policy. What a driver gets is the version this collector applies, on the
`submit-batch` receipt, so it can tell that the rules changed.

**Cost to change:** cheap.
**Revisit:** no.

## L123. The marketing journey is built so that every model divides exactly

**Phase:** 9
**Decision:** the reference application's marketing journey sends three touches
exactly one decay half-life apart, an `identify` after all of them, and a
purchase of 70 with an order identifier that is delivered twice. The ledger
states what each of the six models must credit each campaign with, before
anything is sent.

**Why:** the exit criterion is "every attribution model matches the ledger for
traffic that arrives from the reference marketing site landing pages". A ledger
is only useful if a person can work out its numbers by hand, and five of the six
models produce round numbers only if the journey is arranged for it. Seven days
apart makes the decay weights 0.25, 0.5, and 1, which share out as one, two, and
four sevenths; 70 divides by seven.

**Linear is left not dividing, on purpose.** A third of 70 is not a number, so
that fixture proves the property that matters more than any single figure: the
parts still add back up to 70.

**The campaign parameters come out of a real landing-page address.** The
simulator parses `https://seedstore.example/spring?utm_source=google&...` rather
than writing the parameters out beside it. `docs/TESTBED.md` section 7 asks for
that, and the reason is that a scenario that hand-wrote them would prove that
TallyOwl agrees with the scenario rather than that it reads what a browser sends.

**The purchase is delivered twice under one order.** Two different event
identifiers, a minute apart. That is not the duplicate-delivery case the earlier
phases cover, and only the order fold makes it one conversion.

**One earlier test had to change.** `the_reference_application_revenue_is_exact`
counted stored conversion rows deduplicated by event identifier, and an
idempotent order is two rows and one sale. It now folds by goal and order as
well, which is what TallyOwl does, so the assertion states the product's rule
rather than a count that happened to match.

**The query range in the test bed widened from ten days to twenty.** The journey
spans fourteen. The first run of this test compared two empty answers and
passed; the range is the reason, and a wider one is what fixed it. That is worth
recording because an assertion that passes against nothing is the failure mode
this whole test bed exists to avoid.

**Cost to change:** cheap.
**Revisit:** no.

## L124. Three defects the running loop found, and none of them broke a test

**Phase:** 9
**Decision:** each of the three is fixed and each has a test now. They are
together in one entry because they were found the same way and they say the same
thing about how this run was checked.

The implementation prompt says to run the loop before reading any more code. It
was run at the end as well, and it found three things that 1,079 tests did not.

**A clean installation refused a collection policy, for ever.** The head sent
the compiled defaults at version 0, because nothing had been configured and the
generation starts at zero. The collector refused it — correctly, because the
head raises the generation on every write and never hands out 0 for a policy
somebody set — and logged an error every fetch interval. Both ends were right
and the pair was wrong. The head now answers "there is nothing here to apply",
which is what an unconfigured installation means and what collector intake
already told an app driver.

**A log field replaced another one.** The applied line carried `version`, which
is the field every line already carries for the software version, so the policy
version silently overwrote it. It is `policy_version` now. Nothing would have
caught this except reading a real log line.

**A declared metric was refused and dropped in silence.**
`tallyowl_policy_version` does not end with one of the unit suffixes
`tallyowl_obs::metrics::check_name` permits. `declare` returned a refusal, the
call site discarded it with `let _ =`, and the gauge was missing from the
exposition while everything it measures worked. This is the same defect the
alpha report records against the storage instruments, in the same shape, found
the same way. It is `tallyowl_policy_generation_count` now, and there is a test
that declares every name this module uses and fails on a refusal rather than
discarding it.

**What this says about the tests in this run.** They are good at the rules and
blind to the wiring. Every attribution model, every policy rule, and every
consent case is covered by a fixture a person can check by hand; not one of them
would notice a head and a collector that disagree about what version 0 means, a
log field that shadows another, or a metric name the registry refuses. The three
fixes each have a test now, and the test that would have caught the third is the
one worth copying: it asserts the declaration rather than the exposition,
because the refusal is at the declaration.

**Cost to change:** cheap. Two lines and a name.
**Revisit:** yes, for one of them. `let _ = metrics.declare(...)` appears at
every declaration site in this repository and each one can hide the same defect.
A helper that panicked in a debug build, or a test in `tallyowl-obs` that walks
every declared name, would close it everywhere rather than here.

## L125. Consent is optional, and TallyOwl is not the policy authority

**Phase:** 9
**Decision:** the owner settled the shape of every consent and campaign default
in review, and the answer is a principle rather than a value: **do not police.**
Make the strict path easy for whoever needs it, never impose it, and let an
installation that does not need it carry no cost.

**Why it is recorded here.** D30 already says "TallyOwl is not the policy
authority for an application" and "TallyOwl does not guess a jurisdiction". What
was not written down is the consequence for defaults, and defaults are where a
principle either holds or quietly does not. The owner's words: consent is
optional, this could be an internal company application where consent is
irrelevant, and the job is to make changing law easy to follow rather than to
enforce it on every use.

**Checked against what was built, one by one:**

| Default | Value | Polices? |
| --- | --- | --- |
| Campaign linking | `linked` — keep the touch and its person | No |
| `attribution_needs_consent` | off | No |
| An absent consent state | not a refusal | No |
| A consent state that arrives | stored either way | No, and it is what lets a later policy act |
| Every stricter level | opt in | No |

Nothing is refused unless an operator asks for it, so the build already held the
principle. The value of writing it down is the next decision, not this one.

**What it settled about L120.** The asymmetry in campaign linking only affects
somebody who opted in to the stricter level, so it stays as built. The option
that stored the person-to-campaign tie and only declined to use it was rejected
for the same reason: an installation that chose the setting for a legal reason
would have been failed by it.

**Cost to change:** cheap for any one default. They are numbers and flags in the
control catalog.
**Revisit:** no. This is the owner's framing and it now has somewhere to live.

## L126. `capture` meant two things, and one of them had to go

**Phase:** 9
**Decision:** `CampaignCapture` is `CampaignLinking`, `campaign_capture` is
`campaign_linking`, and **touch** is the one approved noun — `touchpoint` is on
the do-not-use list, and `touchpoint_retention_ms` is `touch_retention_ms`.

**Why:** `capture` and `capture-critical` are the ingest operations a browser
calls. They mean "send telemetry". Naming a policy level `capture` gave one word
two meanings, which `docs/DOCUMENTATION.md` forbids in as many words: "Use each
technical term with one meaning."

`linking` is also the better word for what the setting does. It decides whether
a touch is joined to the person who made it, and the values `linked`,
`unlinked`, and `none` read as a sentence with it.

**The second half is a rule this run broke and did not notice.**
`docs/DOCUMENTATION.md` says "Add a term to this list before you use it with a
new project-specific meaning." Phase 9 introduced **campaign**, **touch**,
**channel**, **conversion**, **attribution model**, and **assist**, and added
none of them. The owner found it by asking what two of the words meant, which is
exactly the failure the rule exists to prevent. All six are in the glossary now,
with a one-line meaning each.

**The contract cost was one field**, introduced the same day and used by
nothing outside this repository. Every other occurrence was prose.

**Cost to change:** cheap now and expensive later. A field name is a contract,
and this one had not left the building.
**Revisit:** no.

## L127. An assist is counted from the weight, not from the money

**Phase:** 9
**Decision:** the campaign report and the attribution operator both carry an
`assists` column: touches that were inside the window and took no credit under
the model that was applied. It is counted from the model's weight rather than
from the credited amount.

**Why the column:** `docs/DATA_MODEL.md` section 6 asks a campaign dashboard for
assisted conversions, and without it a single-touch model is silent about
everything it did not pay for. Under `first-touch`, the partner link and the
direct visit were both part of the journey and earned nothing; a report that
omits them says they were not there.

**Why from the weight:** a share of a very small conversion can round away to
nothing at the working scale. Counting from the credited amount would report an
assist for a touch that was given a share, which is a different fact. Having
been given nothing and having been given something that rounded to nothing are
not the same, and the weight is the one that says which happened.

**A model that credits every touch reports no assists**, which is right:
under `linear` nothing assisted, because everything earned.

**Cost to change:** cheap.
**Revisit:** no.

## L128. A starter dashboard is offered once, and the mark says "offered"

**Phase:** 9
**Decision:** the head writes a starter campaign dashboard for each project the
first time it sees one — six panels, as ordinary saved analyses and an ordinary
saved dashboard. The catalog marks that the project **was offered** one, under
`control/seeded/<project>`.

**Why offered rather than exists.** An operator who deletes the starter
dashboard must not find it back after the next restart or the next upgrade. A
check for the dashboard itself would do exactly that, and it is the kind of
behavior that makes people distrust a product's own defaults. "Offered" stays
true after a delete; "exists" does not.

**The mark is written last.** A crash between the dashboard and the mark offers
it again, which writes the same identifiers a second time and is harmless. A
mark written first could lose the dashboard and never offer it again.

**This uncovered a missing operation.** There was no way to delete a dashboard
at all: the contract had `delete-analysis` and nothing for a dashboard, so
"they can always delete it" was not true when it was said. `delete-dashboard` is
wire ID 25. The analyses stay when a dashboard goes, because an analysis is a
question somebody saved and a dashboard is one arrangement of several.

**Two settings rather than none.** `dashboard.starterDashboard` turns it off for
an installation that provisions its own, and
`dashboard.starterConversionGoal` names the goal, because a conversion goal is
the application's own word and `purchase` is only a guess.

**Cost to change:** cheap. One module and a catalog prefix.
**Revisit:** yes. Six panels is a judgment about what a person wants to see
first, and the three model panels side by side are there to make the models
disagree in public rather than in a meeting. Somebody may want fewer.

## L129. The csilgen pin is a version, and the transport pin is a tag

**Phase:** 9
**Decision:** `CSILGEN_VERSION` pins the generator by what `csilgen --version`
reports. `CSILGEN_TRANSPORT_TAG` pins the TypeScript transport checkout to the
release tag `transport-typescript/v0.2.0`. The git-revision pin is gone.

**Why two.** They are two different things and only one of them can be a
version. The generator is a binary on the path, and a version is what a person
can check on any machine — a revision only means something inside a checkout.
The transport is a `git checkout` into `.deps/`, and a checkout needs a ref that
git can resolve. A release tag is the better ref: it names the artifact this
repository consumes rather than a moment in somebody's history.

**A version is coarser than a revision, and that is the cost.** Several csilgen
revisions share `0.1.0`, so this check would not have noticed the drift it
replaces. What actually protects the build is `gen-check`, which generates into
a temporary directory and fails on any difference from what is checked in,
whatever the version says. The comment in `generate.py` says so, so nobody reads
the version check as a guarantee it is not.

**Cost to change:** cheap. Tighten it when csilgen versions finely enough to
bite.
**Revisit:** yes, at csilgen's next release.

## L130. Event-time identity reached the contract, five phases after it was built

**Phase:** 9
**Decision:** every domain operator carries an optional `resolution`. Absent is
latest-known, which is what every operator did before the field existed, so no
caller changes.

**Why:** L104 recorded that event-time resolution was implemented, tested, and
reachable from nothing. Phase 8 recommended putting it on the contract and Phase
9 did not, so it was about to be carried a third time. It is a field and a
`match`.

**The default is the interesting part.** Latest-known has to stay the default,
and L104 says why: a funnel built on event-time identity counted a converting
person twice, once as the anonymous visitor who vanished and once as the
customer who appeared. Every sign-up funnel would have shown nobody converting.
One function decides what absent means, for every operator, so the two cannot
drift apart.

**The fixture is the point.** One person clicks a campaign anonymously, signs
in, and buys. Latest-known credits the campaign; event-time leaves the click
with the anonymous visitor and the conversion unattributed. Both are right about
their own question, and the test says so.

**Cost to change:** cheap.
**Revisit:** no. L104 is closed.

## L131. A hang in the append log: narrowed by a large factor, and not proven closed

**Phase:** 9
**Decision:** three defects in `crates/tallyowl-store/src/wal.rs` are fixed and
the hang they sit near no longer reproduces. **The root cause was not
identified**, and this entry says so rather than claiming a fix it cannot
support.

**How it was found.** The owner asked why four shells were still running. Two of
them were `segmented_store` test binaries hung since 4 August, holding
`catalog.redb`, `erasure.redb`, and a WAL open, every thread in `futex_do_wait`
inside `many_concurrent_commits_all_survive`. The rest were this session's own
`cargo test --workspace` runs stuck behind them, which is why two background
checks never returned while foreground runs passed.

**What was measured, before anything changed.** 120 runs of that test alone on
an idle machine: **6 hangs, about 5 percent**. Every failure was a timeout and
**not one assertion failed**, so no commit was lost. It is a liveness defect and
not a durability one.

**What could not be done.** `kernel.yama.ptrace_scope` is 1, so gdb and eu-stack
are both refused and no stack was obtainable. Every statement below comes from
reading the code and from measurement.

**The three defects, each worth fixing on its own:**

1. **The lock was held across the write and the fsync.** The comment above it
   says "Release the lock across the expensive part. A committer that holds it
   prevents accumulation and defeats the mechanism, which is exactly the mistake
   that made a prototype report no gain." The code then dropped the guard and
   immediately took the lock again to reach `shared.file`. `docs/BENCHMARKS.md`
   records that mistake as 503 frames each second against 16,923. The handle is
   an `Arc<File>` now, cloned under the lock and used outside it.
2. **`pending_frames` was never decremented on the bounded path.** Once a group
   crossed the frame bound the condition stayed true for the life of the log, so
   every later group was byte-bounded however small it was.
3. **`largest_group` recorded the residual rather than the group.** It read
   `pending_frames` after the take, which the unbounded branch had just set to
   zero.

**The wait condition was changed too, and that is the part that did not do what
this entry first claimed.** A caller waited on an absolute byte offset, and
`reclaim_through` and `truncate_to_empty` both rewrite the file and move
`durable_upto` **backwards**. A caller holding an offset from before a rewrite
would wait for one the file can never reach again. That is a real hazard and the
wait is on a logical position now, which a rewrite cannot move. **It was not the
cause of this hang**: the isolation below proves it.

**The isolation, which is the honest part of this entry:**

| Build | Hangs |
| --- | --- |
| Untouched | 6 of 120 |
| Old wait condition, other two fixed | 0 of 120 |
| Old wait condition, fsync back under the lock, `pending_frames` fixed | 1 of 120 |
| Everything fixed | **0 of 400** |

Two independent changes each move the rate, and neither removes it on its own.
**That is the signature of a window that got narrower, not one that closed.** A
single root cause would have shown as one change carrying the whole effect.

**What this means for the next person.** The rate went from about one run in
twenty to none in four hundred, which is a real improvement and is not proof.
Treat it as open. Closing it properly needs a stack — `ptrace_scope` has to be
relaxed, or the log needs instrumentation that records which thread holds what
when a caller stops making progress. The lock ordering between the WAL mutex and
the catalog and segment locks in `SegmentedStore::commit` is the place to look
first, because nothing in `wal.rs` alone explains a hang that survives all three
of these fixes.

**A test that does not test what it was written for.**
`appending_while_a_reclamation_runs_beside_it_makes_progress` was written to
reproduce this and does not: it passes against the code that had the defect,
with twelve writers and two reclaimers. It is kept as a stress test and its
comment says plainly that it is not a regression test. The thing that reproduces
the hang is still the integration test, at one run in twenty.
`reclaiming_moves_the_byte_offset_back_and_never_the_position` does pin the
invariant that the wait condition now depends on.

**Cost to change:** moderate. It is the durability path, and the `Arc<File>` and
the position-based wait both touch how a caller learns its data is safe.
**Revisit:** yes, and this is the highest-value one in this log. An intermittent
hang in the append log that nobody has explained is worth more attention than
anything else outstanding.

> **Read [L132](#l132-the-append-log-cannot-wait-for-ever-and-the-hunt-for-l131-found-three-defects-that-are-not-the-hang) after this.** Two statements above are now
> known to be wrong. A stack **was** obtainable all along — yama permits a tracer
> that is an ancestor of the tracee, so a test started by `gdb --args` traces
> fine, and only the attach to an already-wedged process was refused. And the
> append log is now excluded by argument: it holds no state in which a caller
> waits with nobody making progress. The isolation table above is better read as
> "none of these three was the cause" than as "the window got narrower".

## L132. The append log cannot wait for ever, and the hunt for L131 found three defects that are not the hang

**Phase:** 10
**Decision:** `Wal::append` is proved free of any wait cycle of its own, by an
argument written into the function rather than into this log. Three real defects
the hunt found are fixed. **The hang L131 records is still not reproduced**, and
this entry says what changed about what is known, not that it is closed.

**The first result is a tool, and it is the one the next person needs.**
`kernel.yama.ptrace_scope` is still 1 and it never had to be relaxed. Yama
refuses an *attach* to an unrelated process; it permits a tracer that is an
**ancestor** of the tracee. A test binary started by `gdb --args` has gdb as its
parent, so `thread apply all bt` works. A `SIGINT` to gdb interrupts the inferior
and returns gdb to its command list, so a harness can run the test under gdb,
wait, and take every stack on a timeout. L131 recorded "no stack was obtainable"
and that was wrong: the earlier session tried to attach to a process that was
already wedged, which is the one case yama refuses.

The harness is in the session scratch directory rather than the repository,
because it is five lines of `gdb -batch` around whatever binary is suspect.

**What the search cost, and what it found.** 800 runs of
`many_concurrent_commits_all_survive` under gdb, 210 runs of the whole
`segmented_store` binary at the harness's own parallelism, and a purpose-built
stress driver run under a build that inserted randomised delays at eleven points
across `append`, `reclaim_through`, `seal`, and `commit`. **No hang.** That is on
top of the 400 the earlier session ran.

**The append log is not where a hang can be, and here is why.** A caller waits
only while `committing` is true, and it leaves the wait the moment `committing`
is false — it then becomes the committer itself. A committer never waits: it
writes, it syncs, it takes no other lock while it holds this one, and **every**
path out of its loop clears `committing` and wakes the others, including the
failure path. So an unbounded wait needs a committer that has stopped making
progress, and there is no such state. The proof is in the doc comment on
`append`, where it stays true.

That also explains the isolation table in L131 that nobody could read. Two
changes each moved the rate and neither removed it, and the reason is that
**both changed how long the function holds the lock and neither changed whether
it can wait.** The earlier session read that as "the window got narrower". It is
better read as "none of these three was the cause", which is what it said.

**Three defects, and the first two are worse than the hang.**

1. **A caller could be told a refused write was durable.** When a group commit
   failed, the committer returned an error and left the group's *other* frames
   out of `pending` for ever. Those callers woke, found nothing pending, and took
   the `take.is_empty()` path, which returned `Ok`. A success receipt for bytes
   no device took is the one thing `docs/DELIVERY.md` section 3 says cannot
   happen. The log now stops accepting writes after a refusal, every waiter in
   the refused group gets the error, and readiness fails with the reason. It is
   the rule `docs/FAILURE_MODES.md` section 10 already stated for the append log,
   and the code did the opposite.
2. **A busy log never reclaimed anything.** `reclaim_through` stepped aside while
   a group was in flight *or* anything was pending, and left it to the next seal.
   A committer keeps the role for as long as callers keep arriving, so on a log
   that never goes quiet `committing` never went false and the next seal found
   the same thing, for ever. **Measured: six callers appending without a pause,
   twenty-five seconds, no reclamation at all, and the log holding 100 percent of
   the bytes it had taken.** An append log that only shrinks when the
   installation is idle is the failure L095 already paid for once. Reclamation
   now asks, a committer stands down once its own frame is durable, and a new
   caller waits rather than taking the role. The same run leaves **0.1 percent**.
   `cargo run --release -p tallyowl-store --example wal_reclaim_measure`.
3. **Two seals could publish out of order and take the log with them.** A seal
   drains the open buffer under the append gate, releases it, and spends the
   expensive part outside. A second seal starting in that window can reach the
   catalog first, and `checkpoint` is the highest range any manifest names — so
   `reclaim_through` would remove the frames of the *first* seal's rows while
   those rows were still only in memory. A crash there loses an acknowledged
   batch. Seals take a turn now.

**A fourth, and it is a lesson rather than a defect.**
`appending_while_a_reclamation_runs_beside_it_makes_progress` was kept as a
stress test for this interaction, and the reclamation inside it **never once
rewrote the file**. It was six writers driving a reclaimer that always refused.
The test passed against everything, before and after, because it exercised
nothing. That is L123 again: an assertion that passes against nothing.
`a_log_that_is_taking_writes_still_reclaims` asserts the file got smaller as well
as that something ran, because the count alone still passed against the defect.

**What a future occurrence will look like.** Not a wedge. A refused write says so
and readiness carries it; a reclamation that cannot get in gives up after five
seconds rather than holding its caller; and the liveness argument above is
written where a change to `append` has to read it.

**Cost to change:** moderate. It is the durability path.
**Revisit:** **yes.** The hang is not reproduced and is therefore not proved
gone. What is different is that the append log is now excluded by argument rather
than by a run count, so the next occurrence is a search of the store's other
locks — the gate, the state lock, and the catalog's — and the tool for getting
its stack is written down above.

## L133. The consensus-log bounds are settings, and the measured values stay the defaults

**Phase:** 10
**Decision:** `replication.snapshotEvery` and `replication.keepAfterSnapshot`
replace the two constants L099 named. The defaults are 4,096 and 512, which are
the measured ones. A group reads them when it starts.

**Why they could not stay constants.** The number is a trade rather than a fact.
A tablet snapshot seals, and a seal makes a segment, so a lower threshold means
a shorter log and more small segments — and the catalog, at 3.4 times the size
of the data it indexes, is what pays for a small segment. An installation with
a large device and a small catalog wants a different point on that line from one
with the opposite. L099 said so and picked a number anyway, which was right for
Phase 7 and is not a reason to leave it fixed.

**Why the change reaches a group only at its next start.** openraft takes its
configuration when a `Raft` is built. Restarting a group to change a log bound
is an operator action already, and the alternative — rebuilding a running group
— would trade a real risk for a convenience nobody asked for.

**Zero is floored rather than refused.** openraft rejects a snapshot policy of
zero, and a configuration mistake must not be the reason a node will not start.
A test asserts the floor.

**Cost to change:** cheap.
**Revisit:** no. L099's recommendation is met.

## L134. A month is a month, and the timezone database is compiled in

**Phase:** 10
**Decision:** `crates/tallyowl-head/src/calendar.rs` answers every calendar
question. A calendar interval and a retention period are calendar units in the
zone the range named: an hour is a wall-clock hour, a day starts at local
midnight, a week starts on Monday, and a month is 28, 29, 30, or 31 days.
`chrono-tz` carries the IANA database, compiled into the binary.

**The evaluation the owner asked for: a conversion library is not enough.** The
contract carries a zone *name*, and a name becomes an offset only through a
database — and the offset changes twice a year. A library that converts against
a fixed offset can answer "what is the local time in +01:00" and cannot answer
"when does the day that holds this instant start in Berlin", which is the only
question this needs. So it is a database.

**Compiled in rather than read from the host.** `jiff` and `chrono-tz` both do
the job and the difference is where the database lives. Reading
`/usr/share/zoneinfo` follows the host's updates for free and breaks in a
container built from scratch, which is the container this project would build.
Compiling it in costs about a megabyte and goes stale, and refreshing it is a
dependency bump — a thing this repository already does on a schedule. `chrono`
was already a dependency, so `chrono-tz` is the same date library rather than a
second one, which matters more than it sounds: two ways to do dates in one
codebase is the shape L102 records.

**Everything stays in UTC, and this is the only place that converts.** A fixed
interval never reaches this module. Five minutes is five minutes in every zone,
and a caller who wanted "the day the reader means" asked for a calendar day.

**Two boundary cases that a wall clock has and an instant does not.** A
spring-forward makes a local time *not exist* and an autumn boundary makes one
happen *twice*. A bucket boundary has to be exactly one instant, so the earlier
of a repeated pair is taken and a missing time steps forward to the first that
exists. Santiago moves midnight itself, so a day there can start at 01:00 — a
test uses it, because a rule that only ever met Europe would be a rule about
Europe.

**What moved for a reader.** L110's month was 28 days and said so honestly.
A retention matrix over a Jan-to-Dec range filed an August visit in the ninth
column, because 28 divides into 226 days eight times. It is the seventh month
after January. `a_month_and_a_twenty_eight_day_period_part_company_in_august`
is that row, and it names the arithmetic so the next person does not have to
work out whether the fixture separates the two rules. Three months of that
range do not separate them, and the fixture beside it says so rather than
looking stronger than it is.

**Cost to change:** moderate, and done. The zone travels on the query stage
beside the basis, so an operator that needs it has it and one that does not
cannot accidentally read it.
**Revisit:** no. L110 is closed. The one thing worth watching is the bundled
database going stale, which is a dependency bump and not a design question.

## L135. The identity graph is materialised as a cache, and the answer does not depend on how fresh it is

**Phase:** 10
**Decision:** `IdentityCache` in `crates/tallyowl-head/src/identity.rs` holds one
built graph for each project. A question folds the identity rows the held graph
does not cover yet, rather than the whole history. `Identity::build` is still the
only thing that builds a graph.

**What it cost before.** L103 stated it and did not measure it: a question that
resolves identity scanned its own range **and everything before it**, because an
`identify` from last year is what makes this month's anonymous events belong to
somebody. The cost grew with the installation's age rather than with the
question, which is why L103 called it the one deferred item that gets worse with
no operator action. The window a question folds is now bounded by
`query.identityRefresh`, five minutes by default.

**It is a cache and it is not durable, on purpose.** `AGENTS.md` requires a
derived projection to be reproducible from retained raw data, and one that is
only ever derived is reproducible by construction rather than by discipline. A
restart costs the first identity question one rebuild — the cost every identity
question paid before this existed. L112's rule that control-plane state must
survive a restart is about what a person **authored**, and a cache is not that.

**The hard part was not the cache. It was determinism.** A held graph knows every
identity row the installation has, including rows *after* the range a question
asks about, so a question about last week would answer differently depending on
when the graph was last built. Two runs of one query would disagree and neither
would be reproducible.

`Identity::bounded_to` is the answer: the graph is cut back to what a fold up to
the end of the range would have produced, before the caller sees it. That is a
truncation rather than a horizon threaded through every reader, which matters —
a reader that forgot the horizon would be a silent wrong answer, and there is no
reader to forget it.

**One thing had to change shape for that.** A trait kept only its winning value
and the time it was set, and a truncation cannot recover the value that held
before a removed one. Traits keep their history now. It costs one entry for each
trait write and it is the difference between "the same answer" and "nearly".

**An erasure empties it.** The graph *is* the rows, so removing a row removes
what it said; a cache in front would keep it. The tombstone generation is part of
what a held entry is valid for.

**The test that proves the erasure case had to be built twice.** The first one
erased the person's events, and it passed against a cache that ignored erasures
entirely — the rows were hidden at the scan, so the funnel counted nobody either
way. It erases the `identify` row alone now: both events stay visible, and a held
graph still ties them together where a rebuilt one cannot. That is L123 again,
and the comment in the test says so.

**Cost to change:** moderate. `apply` and `bounded_to` are the seam; a durable
materialisation would sit behind the same one.
**Revisit:** yes, on one point only. Five minutes is the longest an `identify`
that arrived late can go unseen by a question about a range that ended before it,
and nothing has measured how often that happens. `query.identityRefresh` is the
setting, and zero restores the old behaviour exactly.

## L136. The general aggregate is pushed down, and the tablet runs the coordinator's own aggregation

**Phase:** 10
**Decision:** a tablet answers `partial-aggregate` by running the head's own
`Accumulator` over its own rows and sending back the accumulator states. The plan
travels as an encoded `QueryNodeBox` and the states travel as bytes that the
cluster crate never reads.

**Why the plan is bytes rather than a declaration on the cluster contract.** The
obvious shape was to declare measures and dimensions in `tallyowl-cluster.csil`
and have the storage node compute them. That is a **second implementation of a
sum**, and L102 records what a second implementation of a derivation costs: the
locator was built in two places and the two went out of step without anybody
changing either one on purpose. So the contract carries the coordinator's plan
and the coordinator's partial states, and there is one aggregation.

**Where the seam is.** `Store::partial_aggregates` takes bytes and returns bytes,
and its default answer is "there is nothing to ask", which is what a home
installation is. D25 keeps the store contract small and an aggregate is query
algebra rather than storage; a store that understood measures would be a second
place the algebra lived.

**A node that computes a partial aggregate reads its local store and never the
replicated one.** The other way round, a pushed-down aggregate would fan out
again from every tablet it reached and the fan-out would not terminate.

**What is not pushed down, and why each one is a correctness reason.**

- **A filter above the scan.** Pushing it down means pushing the expression
  language into the storage contract. An aggregate directly over a scan is
  pushed down and anything else reads rows, which is what everything did before.
- **`rate`, `increase`, `quantile`, and `histogram_merge`.** Each has a state
  and none of them is a number. The tablet says it has no partial state by
  answering nothing, and the coordinator falls back. A refusal to push down is a
  slower answer; a partial state that merged wrongly is a wrong one.

**The exact sum is the part that needed care.** A decimal sum keeps its units and
its scale, and two tablets can hold different scales for the same money: 1.5 and
1.50 are one number written two ways. Adding the units without aligning the
scales answers 16.5 for a total of 3.00.
`two_exact_sums_at_different_scales_add_at_the_wider_one` is that case, and it is
two rows a person can check by reading them.

**The test fixture refuses to scan, and that is the test.** `TwoTablets::scan`
returns an error, so a coordinator that asked for rows fails rather than
answering right for the wrong reason. Without it the tests would prove the
arithmetic and say nothing about where it happened, which is the whole subject.

**Cost to change:** moderate. It is one new wire field on each side of
`partial-aggregate` and one new default method on the store contract.
**Revisit:** yes, on the two exclusions. A filter push-down is the larger of the
two and it needs a storage-level predicate that is not the head's expression
language. Nothing has measured what either one is worth, because nothing runs
more than one tablet outside the tests.

## L137. The durable task boundary became its own crate, because the head needs the same one

**Phase:** 10
**Decision:** `crates/tallyowl-queue` holds `DurableQueue`, the Corndogs client
behind it, and the test double. The collector re-exports it under the name it
already used, so nothing that reads `tallyowl_collector::durable` moved.

**Why not a second adapter in the head.** A collector puts a batch in a queue;
the head puts an alert evaluation, a notification, and every projector pass in
one. Two adapters would be two sets of rules about what a claim means, when a
retry parks, and what quarantine is, and the two would go out of step without
anybody changing either one. L102 is what that costs.

**Why not a dependency from the head on the collector.** It would compile
intake, the forwarder, and the compatibility receivers into a head that runs
none of them, and it points the wrong way: a head does not use a collector.

**Cost to change:** cheap, and it is done.
**Revisit:** no.

## L138. Alerting is a schedule, a state, and a delivery, and all three are durable

**Phase:** 10
**Decision:** an alert rule and its instance live in the control catalog; every
evaluation and every notification is a Corndogs task; a delivery attempt is
recorded where an operator can read it.

**The instance is the part that had to be durable and is easy to leave in a
process.** `docs/ALERTS.md` section 5: a state change sends a notification and a
repeated evaluation in the same state does not. That rule is worth nothing if a
restart forgets which state a rule was in, because every restart would then be a
notification storm. `a_worker_restart_does_not_resend_a_notification` opens the
data directory twice to prove it.

**The evaluation and the notification are two queues.** A receiver that is not
answering must not stop the next evaluation, and an evaluation that is slow must
not delay a notification that is ready. The state is stored before the
notification is queued at all, so a queue that refuses costs a notification and
never the state.

**A notification for a state that has since changed is dropped.** It waited
while the alert recovered, and sending it would tell somebody a firing alert is
firing when it stopped ten minutes ago. The recovery queued its own.

**One executor, and that is the design.** An alert runs its query through the
same `QueryService` the dashboard uses, so an alert value always matches what a
person sees. A second evaluation engine gives a second set of semantics, the
alert then fires on a number nobody can reproduce, and that destroys trust in
every alert. `an_alert_value_is_the_value_the_same_query_gives_a_person` asserts
it against the dashboard's own answer rather than against a second call to one
function, because the second would prove nothing about the property.

**Two things are refused when a rule is written rather than when it runs.** A
`bounded-stale` alert, because a stale read can answer from a replica that lags
and the alert would fire on replication lag and report it as a change in the
data; and an interval below a floor, because an alert is a whole query each time
it runs. Both refusals reach an operator while they are looking at the rule.

**Cost to change:** moderate. Six new operations and two new record kinds.
**Revisit:** **yes**, on the evaluation budget. `docs/ALERTS.md` section 7 asks
for a separate query budget pool and this gives an evaluation half the runtime a
person's query gets. That is the shape one process can have, it is not a pool,
and nothing has measured whether it is enough to keep a dashboard responsive
under a thousand rules.

## L139. A webhook to an address that asked for TLS is refused rather than sent in the clear

**Phase:** 10
**Decision:** `notify::split_url` refuses an `https://` webhook address by name.
An operator terminates TLS in front of the receiver and gives the `http://`
address, or uses the native callback.

**Why this is a refusal and not a gap.** This build has an outbound HTTP client
and no outbound TLS client, and the tempting shortcut is to strip the scheme and
send the request anyway. That sends what an alert observed across the network
the operator asked to be protected from, and says nothing about having done so.
A refusal that names the alternative costs an operator five minutes; the other
costs them the thing they were protecting.

**The signature covers the timestamp as well as the body**, so a replay of an
old body is visible as an old timestamp rather than accepted as a fresh alert.
It is a keyed BLAKE3 hash, which is a message authentication code, and D44
already keeps the crate in the workspace.

**Cost to change:** cheap. An outbound TLS client is the change, and the refusal
is one function.
**Revisit:** **yes.** An operator who wants a webhook to a service on the public
internet needs TLS, and a terminating proxy is a real answer for an installation
that has one and not for a home installation that does not.

## L140. A rule was always about to be due, and only the running loop found it

**Phase:** 10
**Decision:** the scheduler measures a rule's stagger from the moment it first
saw the rule, not from the current tick.

**What it was.** `docs/ALERTS.md` section 7 asks the scheduler to stagger
evaluations so that a thousand rules on one interval do not all run on the same
second. The offset was applied as `now + offset`, and `now` moves. Every tick
set the deadline a few seconds into the future, so a rule was permanently about
to be evaluated and never was.

**Why the whole test suite passed against it.** Every test drove the *runner*
with a piece of work it built itself, which is the right way to test an
evaluation and says nothing about whether one is ever scheduled. The loop is
what found it: a rule was written, an event was sent, twenty seconds passed, and
`list-alert-instances` said nothing had been evaluated.

This is the third time this project has recorded the same lesson. L124 found
three defects by running the loop that the whole suite missed, and said "the
tests in this run are good at the rules and blind to the wiring". A scheduler is
wiring.

**The test that would have caught it** drives `tick` across a clock rather than
calling the runner: thirteen ticks over one interval must queue the rule at
least once and at most twice. The first half is the defect and the second half
is the opposite defect, which a fix that queued on every tick would have.

**Cost to change:** cheap, and it is done.
**Revisit:** no.

## L141. A metric name the registry refuses is a panic at boot, and it caught two

**Phase:** 10
**Decision:** every new instrument is declared through a helper that expects the
result, as L124 decided at the Phase 9 review.

**It caught two names in this phase**, and one of them was not in the alerting
code at all: `tallyowl_identity_graph_reuses` and
`tallyowl_identity_graph_builds`, added for L135, end in no unit and the
registry refuses them. Under the old `let _ =` they would have vanished, and the
missing gauge would have been how somebody found out — which is exactly how this
defect was found the previous two times.

The names are `..._reuses_count` and `..._builds_count` now, and the workflow
gauges are `tallyowl_workflow_pending_count` and
`tallyowl_workflow_quarantined_count` for the same reason.

**Cost to change:** cheap.
**Revisit:** no. It is worth reading as evidence that the Phase 9 decision paid
for itself inside one phase.

## L142. A webhook gets TLS, and the public roots reach exactly one connection

**Phase:** 10 (revisited)
**Decision:** `notify::connect` opens a TLS connection for an `https` webhook
address. The trust roots come from the platform store, and a bundled set is the
fallback when the platform has none.

**What this replaces.** L139 refused an `https` address, on the reasoning that
sending an alert in the clear to an address that asked for TLS is worse than
refusing. That reasoning was right and the refusal was a stand-in for a client
this build did not have. It has one.

**The boundary is the part worth reading.** Every other TLS hop in TallyOwl
verifies against the **installation's own authority**: a collector reaching a
head, a node reaching a node. A webhook is the one connection to something
TallyOwl did not issue a certificate to, so it is the one place a public root
store belongs. A change that let `public_roots` reach any other hop would accept
any certificate on the internet as a TallyOwl node, which is why it is a private
function in the module that makes that one connection.

**Platform store first, bundled second, and this is the opposite of L134.** The
timezone database is compiled in because a stale zone is a wrong answer nobody
sees and a container from scratch carries no zone data. Trust roots differ on
one point: an operator whose webhook receiver uses a private certificate
authority has to be able to add a root, and they do that by putting it where the
platform store looks. So the platform store wins when there is one, and the
bundled set is what a scratch container falls back to.

**A webhook sends one request and reads one response**, so `rustls::StreamOwned`
is right here. L058 replaced it on the RPC path because a TLS connection there
serves many correlated requests at once; this one does not.

**An address with no scheme is refused rather than guessed at.** Guessing
`http://` for an address a person wrote without one is the same downgrade by
another route.

**Cost to change:** cheap.
**Revisit:** no. L139 is closed.

## L143. The native callback is one operation on a service, and the connection is the authentication

**Phase:** 10 (revisited)
**Decision:** `notify::RpcCallbacks` delivers a notification over CSIL-RPC to
`TallyOwlAlertReceiver.notify`. A receiving service declares that one operation
and takes the same notification body a webhook receives.

**What this replaces.** The channel and the target kind were declared and had no
transport, so a rule naming a callback was refused when it was written. It is
delivered now, and `AlertService.callbacks_available` is true on a running head.

**There is no signature, and that is the difference between the two channels.**
A webhook is signed because it crosses to a system TallyOwl did not issue a
certificate to. A native callback runs over the same mutual TLS every other
node-to-node hop uses, and there the connection **is** the authentication.
Signing it as well would be a second answer to a question already answered, and
the second answer is the one somebody trusts after the first is misconfigured.

**One operation rather than a service shape.** The point of the native channel
is that a service which already speaks to TallyOwl adds a handler, rather than
an HTTP endpoint, a signature check, and a JSON parser. Anything more than one
operation would be TallyOwl asking a consumer to implement a protocol.

**The body is the same body.** A receiver that wants to move from a webhook to a
callback, or run both, reads one shape. It also means the rule that a
notification never carries a telemetry row is one rule and is tested once.

**Cost to change:** cheap.
**Revisit:** no.

## L144. The alert budget pool bounds what alerting takes, and reserves nothing

**Phase:** 10 (revisited)
**Decision:** `alerts::BudgetPool` holds `query.alertConcurrency` permits. An
evaluation takes one before it runs a query, waits a quarter of a second for
one, and puts the work back on the queue if none is free.

**What this replaces.** L138 gave an evaluation half the runtime a person's
query gets and said plainly that a deadline is not a pool. This is the pool
`docs/ALERTS.md` section 7 asks for. The deadline stays, because a pool without
one lets a single heavy evaluation hold a permit indefinitely.

**A full pool is not a failed evaluation.** The tempting shortcut is to reuse
the `error` outcome, which already exists and already means "TallyOwl could not
answer". It would make a rule go `unknown` whenever the installation was busy —
quiet exactly when somebody needs it, and an operator learns to ignore a state
that behaves that way. It is a `Retry` at the workflow layer: the rule keeps the
state it had, and the evaluation delay rises, which is the indicator section 8
says matters.

**One is the default, and one is what the shape already gives.** The head runs a
single worker for the evaluation queue, so alert concurrency is one by
construction today and this pool changes nothing. That is the point: **the
isolation was an accident of how many threads got started, and an accident is
lost by accident.** A change that adds a second worker now has to raise the pool
on purpose.

**It reserves nothing, and the document says so.** A pool cannot promise a
person looking at a screen any share of the path; that needs a scheduler this
system does not have. It bounds what alerting takes, which is the half that can
be bounded, and claiming more would be a promise the code cannot keep.

**Zero is floored to one.** A configuration mistake must not be the reason
nothing is ever evaluated.

**Cost to change:** cheap.
**Revisit:** **yes**, on the number rather than on the shape. Nothing has run a
thousand rules on a one-minute interval, and that is the case that decides
whether one permit and one worker are enough. This project has a load harness.

## L145. The append log says what it is doing, so the next stall is diagnosed rather than noticed

**Phase:** 10 (revisited)
**Decision:** `Wal::state` returns the log's whole internal state, and the head's
readiness watcher reports it when a group commit stays in flight across two
rounds without making anything durable.

**This is not a fix and it is not pretending to be one.** L131 is open and L132
says why. This is the instrumentation that makes the *next* occurrence a
diagnosis instead of a discovery.

**How L131 was found is the argument for it.** Somebody noticed that four shells
were still running. Two of them had been wedged for two days. The state that
would have explained it — whether a committer was in flight, whether anything
was pending, whether `durable_before` had stopped moving — was inside a mutex
that nothing could read from outside, so the only way to see it was a debugger
on a process that had already stopped.

**A watcher that stops reporting has said the most useful thing it can say.**
Reading the state takes the log's lock. If the watcher goes quiet, the log is
held, and the next place to look is whoever holds it — which is exactly the
question L132 leaves the next person with, now narrowed to the store's other
locks.

**The signature to look for is written down** in the doc comment on `WalState`:
`committing` true with `durable_before` unchanged over several seconds means a
committer took a group and has not come back. That is a device or a lock outside
`wal.rs`, and not the protocol inside it — which is the half L132 proved.

**Where it goes next.** Phase 11 builds cross-cluster soak tests, and a soak is
the first thing in this project's history that will run the concurrency that
produced the hang for days rather than for seconds. The soak should watch this
line.

**Cost to change:** cheap.
**Revisit:** no, but read it beside L132 rather than alone.

## L146. The queue is the one that knows what is waiting

**Phase:** 10 (revisited)
**Decision:** `Workflows::status` reads queue depths from Corndogs through
`GetQueueAndStateCounts`. The head keeps only what Corndogs does not hold: how
many attempts failed and what the last one said.

**What this replaces, and the mistake in the first version.** The counts were
kept in this process, incremented on submit and decremented on completion. That
reads as zero after a restart, and zero looks like "there is no work" rather
than like "this number is not the truth". During a rolling upgrade — which Phase
11 drills — an operator would watch every queue empty and refill.

**The correction is more interesting than the fix.** The Phase 10 report said
Corndogs "holds the truth and has no operation that reports it", and proposed
asking for one. It has three: `GetQueueTaskCounts`, `GetTaskStateCounts`, and
`GetQueueAndStateCounts`. The claim was written from memory of the client
wrapper this project uses rather than from the contract, and the contract was
four lines away. **A dependency's capability is a thing to read, not to
remember** — the same rule section 10 of the implementation prompt states about
csilgen, applied to the other direction.

**One thing is still this process's own, and the report says so.** Corndogs
reports how many tasks are in a state, not when each one arrived, so the lag is
measured from the oldest piece of work this head is still waiting on. A restart
loses that and keeps the depth. Reporting a lag of zero beside a depth of forty
is visibly incomplete, which is better than a plausible wrong number.

**Work in a backoff counts as waiting.** It is going to run and it has not.
Counting only the queued state would show an empty queue while everything in it
was backing off, which is exactly the moment an operator is looking.

**Cost to change:** cheap.
**Revisit:** no.

## L147. L097 wanted a marker in the segment format, and the marker is on every row

**Phase:** 10 (revisited)
**Decision:** `SegmentedStore::reconcile_segment` installs a segment onto a
store that already holds some of its rows, keeping the rows this store does not
have. `copy_tablet` reconciles instead of refusing.

**What L097 said, and why it was wrong about the fix.** "Reconciling a copy onto
overlapping data needs a way to tell two replicas' segments apart, and neither
the segment format nor the manifest carries one. This is a format change rather
than something waiting on evidence." The observation is right: two replicas that
applied the same entries build differently shaped segments, so a content address
cannot tell a copied segment from one the target built itself.

**The conclusion does not follow, because a segment was the wrong unit.** A row
carries a producer-assigned `event_id`. `AGENTS.md` requires that value to stay
exactly retrievable at any cardinality, `docs/DELIVERY.md` rests the whole
delivery contract on stable IDs and logically idempotent ingestion, and the query
path already counts one logical event however many physical rows carry it. The
identity that was needed has been on every row since Phase 3.

So the reconcile keeps the rows this store does not hold and writes those. **No
format change, and the case that was refused for three phases is not refused.**

**It costs one exact lookup for each row**, which the locator makes a probe
rather than a scan, and it runs on a recovery path rather than a hot one. A
target that holds none of the tablet still takes the files whole, which is the
ordinary case and the cheap one. `(tablet_id, log_range)` overlap is the trigger
between the two, and it is deliberately generous: a false yes costs a row-by-row
install, and a false no would count the overlap twice.

**The test that asserted the refusal is now the test that asserts the
reconcile**, with a fixture that has a shared row and a row only the source
holds — which is the case the refusal was hiding.

**What this says about the other deferrals.** L097 was carried through Phases 8,
9, and 10 as "not a deferral a decision can lift", and it was a deferral one
reading could lift. It is worth asking the same question of the rest: the
statement was about what the *format* carries, and the answer was in what the
*data* carries.

**Cost to change:** moderate, and done. It is a new path rather than a change to
the existing one, so a copy onto an empty node behaves exactly as it did.
**Revisit:** no. L097 is closed.

## L148. The charts have templates, and a render refuses what a start would refuse

**Phase:** 11
**Decision:** both charts render real manifests: a StatefulSet, services, a
disruption budget, and a configuration document for the head; a Deployment, a
service, and a horizontal autoscaler for the collector. The rendered
configuration is the values tree with the deployment keys removed, so the
chart-parity test keeps protecting it without knowing the templates exist.
`./tools.sh helm-check` renders every profile and proves every refusal.

**The refusals run at render, which is earlier than the pod.** Each rule
TallyOwl refuses at start — `local-one` on a multi-voter tablet, a missing
replication address, a gcGrace inside the query runtime, a deduplication
window the retry window outlives, a merge threshold that would thrash — is
refused by `helm template` with the setting named. The check also asserts each
refusal names its setting, because a refusal that fires for the wrong reason
reads as passing.

**Two new deployment keys are deployment concerns, not settings.**
`autoscaling` scales the collector, which is stateless; the head does not
autoscale, because adding a voter is a placement decision. `corndogsDeployment`
runs one Corndogs beside the head for the home profile and exposes it as a
service the collector release references.

**Cost to change:** cheap.
**Revisit:** no. The install-upgrade-rollback test in a disposable cluster
(DEPLOYMENT.md section 8 item 7) is not built; it needs a cluster CI runner.

## L149. A follower forwards a proposal to the leader, and ingest works on every voter

**Phase:** 11
**Decision:** `ConsensusKind` gains `proposal`. A voter that takes a write
while a peer leads hands the proposal to the leader over the same connection
every other consensus message uses, one hop, and returns the leader's outcome.
The server side proposes locally and never forwards again.

**The soak found this in its first hour, and it is the reason Phase 11 runs
things for days.** Three head processes formed a voter group — the first time
in this project's history consensus ran across real processes — and the
election chose head-2. The collectors deliver to head-1, and head-1 refused
every batch with "has to forward request to" while the queue grew without
bound. Every prior replication test ran one voter, and one voter is always its
own leader, so the whole suite was blind to the case. Worse: without the
forward, a head that restarts after an outage comes back as a follower, so the
soak's own recovery criterion could never pass.

**Why a variant and not a new operation.** Section 10.1 discipline: a tagged
variant instead of a new kind of polymorphism. `deliver-consensus` already
means "hand one consensus message to the named group", the reply shape already
carries an accepted flag, a refusal, and a payload, and a client write is a
consensus concern. An old node that receives the unknown kind refuses it with
the reason, which degrades a mixed-version cell to leader-only ingest rather
than to an error nobody can read.

**The generation travels as zero on purpose.** The placement fence exists for
routing decisions, and a proposal's authority is membership: the same mutual
TLS that authorizes an append. A forwarded proposal that raced a leadership
change gets a retryable refusal, and the forwarder's next attempt asks whoever
leads by then.

**Cost to change:** moderate. One contract variant, one dispatch arm, one
forward function.
**Revisit:** no.

## L150. The soak is a supervised cluster, a driver that reconciles, and a journal

**Phase:** 11
**Decision:** `./tools.sh soak up` runs the cross-cluster soak: one Corndogs,
three head voters under `local-quorum`, two collectors delivering to the first
head, a Go driver offering paced load, and a monitor that watches, restarts,
and injects the outage schedule. Everything has a recorded PID, `soak down`
stops the tree and names any survivor, and every event lands in a journal the
report reads back.

**The outage schedule is the exit criteria made periodic.** Every six hours a
voter dies abruptly for two minutes, which quorum must survive. Every twelve
hours the ingest head dies abruptly for ten minutes, which the collectors must
hold: intake keeps accepting, the queue holds the batches, and the drain after
recovery must reconcile. The kills are `SIGKILL` because an operator's outage
is not a drain.

**Reconciliation is windows, not totals.** The driver checkpoints each
producer's counter every ten seconds and, once a window is older than the
check age and nothing older than its edge still waits in the queue, counts the
window's committed events and compares. A shortfall is a loss, a surplus is a
duplication, and the same count answers both because deduplication makes it a
count of logical events. Random acknowledged request IDs are also asked for
exactly one row each. The gate is the oldest waiting age rather than an empty
queue, because a live queue never reads exactly zero.

**Two numbers this soak measured that nothing had measured.** A paced producer
under the driver's 100 ms linger seals a handful of events into each batch,
and the replicated commit path on this one machine sustains about seven
batches each second end to end — so the soak driver fills real batches with a
two-second linger, and the default 500 events each second rides inside the
ceiling. Both numbers are one machine's, with three voters and a queue sharing
one device; a real cell spreads them.

**The collector holds the agreed outage window only if the key grace covers
it.** `collector.keyCacheGrace` defaults to 60 seconds, and a head outage
longer than that refuses new batches once cached key answers expire. The soak
sets it to one hour, and the operations runbook says to set it to the window
an installation intends to hold. This is the kind of fact a drill exists to
surface.

**Cost to change:** cheap. It is tooling and a driver.
**Revisit:** yes, on the seven-batches-each-second ceiling: it was measured
incidentally, on one device carrying every voter and the queue, and the
capacity envelope for a replicated cell deserves a measurement of its own.

## L151. The stall signature is numbers, and the queue depth is the queue's answer

**Phase:** 11
**Decision:** the store publishes `tallyowl_wal_commit_in_flight_count`,
`tallyowl_wal_durable_position_count`, and
`tallyowl_wal_pending_frames_count`, sampled beside the other store gauges.
The collector publishes `tallyowl_delivery_queue_depth_count` from Corndogs'
own counts at each sweep, and `tallyowl_delivery_oldest_waiting_ms` as its own
lower bound, re-learned from each claimed task's acceptance time. The health
report reads the same numbers instead of the zeros it hard-coded.

**Why.** L145 wrote the stall signature into a log line, and a warning nobody
scraped is a warning nobody saw: the soak's monitor needed the state as
numbers, and an operator's alert needs the same. The collector's hard-coded
zero was L146's defect on the other side of the wire: a depth this process
counted would zero on restart, so the queue's answer is copied at each sweep,
and a depth beside an age of zero says plainly that a restart lost the age.

**Cost to change:** cheap.
**Revisit:** no.

## L152. The drill measures what procedure 3 promises, and not more

**Phase:** 11
**Decision:** `./tools.sh drill dr` runs the disaster: load, snapshot,
more load, an abrupt kill, a destroyed data directory, a timed restore, a
verification, then a deleted catalog and a timed rebuild. The report states
the recovery time, what came back, and what was lost.

**The first version of this drill asserted more than the design promises, and
failed honestly.** It expected the restored count to equal what was
acknowledged before the snapshot, and the count came back higher: the queue
still held some post-snapshot batches, the restore replayed them, and stable
batch IDs made the replay one logical commit each. That is procedure 3 step 5
working — "the queue recovers more than the snapshot alone" — and the drill
now asserts the bracket: nothing acknowledged before the snapshot is missing,
nothing replays twice, and the loss equals what the queue no longer held. It
also expected a rebuilt catalog to answer the old key, and FAILURE_MODES.md
section 7 says plainly that keys do not survive a rebuild; the rebuild leg now
verifies the two promises the design makes — the stored files are back and
the operator is told what is not.

**Cost to change:** cheap. It is tooling.
**Revisit:** no.

## L153. A property named `request_id` is shadowed by the column, and the alpha lookups measured misses

**Phase:** 11
**Decision:** the load and soak drivers set correlation IDs through the
envelope's own column (`WithRequest`), which is what the exact index answers.
Nothing in the product changed.

**What was found.** A filter on the field name `request_id` resolves to the
row's request column. The load harness had been sending `request_id` as a
client property, so every point lookup the alpha report timed was a locator
miss over an empty column: 41 ms was the cost of finding nothing. The soak's
first exact lookups answered zero rows for events that were provably
committed, which is how this surfaced.

**The sharp edge is bigger than the harness.** An application that sends a
client property named `request_id`, `session_id`, `trace_id`, or `event_id`
stores a value no filter can reach, silently: the field reference reads the
column and the property is shadowed. The collector's protected-name rule
exists for exactly this shape — "a protected name refuses a client value and
counts the refusal" — and the four correlation names are not in
`PROTECTED_KEYS`. Adding them refuses data that today is stored, which is a
collection-behavior change, so it is recorded here rather than made
unilaterally at the end of a phase.

**Cost to change:** cheap, in `tallyowl-wire`'s protected list.
**Revisit:** **yes.** Recommendation: add the correlation names to the
protected list, so the refusal is visible at intake instead of silent at
query time. The alpha report's point-lookup latencies also need re-measuring
against the fixed harness before anything cites them again.

## L154. One evaluation permit sustains about thirty-six one-minute rules

**Phase:** 11
**Decision:** none; this is the measurement L144 asked for. `put_alert -- many
1000` wrote a thousand rules on a one-minute interval against the soak's
ingest head while it carried 500 events each second. The evaluation counter
advanced 80 in 132 seconds: about 0.6 evaluations each second, each one a
whole trend query at about 1.6 seconds under that load.

**What the number means.** A thousand one-minute rules ask for 16.7
evaluations each second, so one permit and one worker deliver each rule about
every twenty-eight minutes rather than every minute. The pool did exactly what
L144 designed: evaluation delay rose, the rule states stayed truthful, and no
rule went `unknown` for being queued. The bound it promises also held the
other way — ingest stayed at its full rate with a delivery queue depth of
four while alerting saturated its own pool, which is the workload-isolation
half of the Phase 11 deliverable, observed rather than asserted.

**Cost to change:** raising `query.alertConcurrency` is a setting; making it
matter needs more evaluation workers, which L144 already says.
**Revisit:** yes. An installation that wants a thousand one-minute rules
needs roughly thirty times this throughput: more workers and permits, cheaper
evaluation queries, or both. The number to design against is now measured.

## L155. The audit passes over two advisories by name, and one belongs to linkkeys

**Phase:** 11
**Decision:** `./tools.sh audit` runs `cargo audit` and `cargo deny`. The
ignore file carries two entries with their reasons: a DNS encoding exhaustion
in `hickory-proto` 0.24, reachable only through the pinned linkkeys
revision and only when LinkKeys sign-in is on, whose fix is a linkkeys
upgrade to hickory 0.26; and an `rkyv` advisory for code no TallyOwl build
compiles. License checks clarify the first-party crates whose metadata
carries no license field, because the generated crates cannot be hand-edited
and the statement has to live somewhere the drift check permits.

**The advisory fetch needed a git identity.** The git library `cargo audit`
uses refuses a reflog signature containing angle brackets, and this machine's
git name carries them. The audit verb supplies its own identity for the
fetch, named for the tool, so the audit runs whatever the host's git
configuration says.

**Cost to change:** cheap.
**Revisit:** yes, on the hickory entry: it is the one advisory with a real
path into a running head, and closing it is one dependency bump away once
linkkeys moves.

## L156. The fuzzer is seeded and in-tree, because there is no nightly compiler

**Phase:** 11
**Decision:** the malformed-frame tests and a 1,000-round seeded mutation
fuzz live in `crates/tallyowl-rpc/tests/malformed_frames.rs` and run on every
`cargo test`. A libFuzzer build needs a nightly toolchain this machine does
not have, so the coverage-guided fuzzer is deferred and the seeded loop is
not pretending to be one: its seed is recorded, its corpus is stated, and its
assertion is that the listener answers a well-formed request after every
round.

**Cost to change:** cheap. A `fuzz/` workspace beside `prototypes/` slots in
when a nightly toolchain exists.
**Revisit:** yes, when the toolchain exists.

## L157. The CI jobs are a trusted plugin, and the proposed layout was corrected by reading

**Phase:** 11
**Decision:** `.reactorcide/` holds nine job definitions, three workflows
(pull request, main, tag release), and one lifecycle plugin whose functions
call the same `tallyowl_tools` modules `tools.sh` calls. The publish job is
tag-triggered, refuses local runs, and refuses to run at all until the owner
names the registries — a refusal with the reason, not a pretend publish.

**CI-CD.md proposed a `pipelines/` directory, and the shipped mechanism is a
trusted plugin selected by an environment variable.** The proposal was
written before Reactorcide shipped its trust model; the plugin form is what
keeps pull-request source from rewriting the CI commands. The document now
records the shipped shape and the two facts the proposal had wrong, so nobody
restores the old layout from memory. This is L146's lesson again, in the
other direction.

**Cost to change:** cheap.
**Revisit:** no.

## L158. The rolling upgrade is a drill today and a version window at the first release candidate

**Phase:** 11
**Decision:** `./tools.sh soak roll` restarts every service of the running
soak gracefully, in DEPLOYMENT.md section 7's order: the ingest head, then
the storage voters one at a time, then the collectors. The driver keeps
offering through it, and the reconciliation windows afterwards say whether
anything was lost.

**Why there is no two-version drill.** There is one version, 0.0.0, and D31
opens the client compatibility window at the first release candidate. A
two-version drill built now would drill this build against itself and report
a compatibility it never tested. The procedure, the order, and the
verification exist; the second version plugs in the day it exists.

**Cost to change:** cheap.
**Revisit:** no.

## L159. The overload drill could not reach the refusal boundary, and that is the result

**Phase:** 11
**Decision:** `./tools.sh drill overload` judges four bounds: no silent loss,
intake alive, memory bounded, and a backlog that drains. It does not demand a
refusal, because on this machine no refusal is reachable: four unpaced local
producers offered about 43,000 events each second — five times the measured
sustained rate and eighty-six times the commit ceiling — and intake absorbed
all 2.57 million of them. 284 MB of resident memory across the head and the
collector, 115 MB of disk for the run, and the backlog drained at fifteen
batches each second when the offer stopped.

**The first version of this drill demanded visible refusals and failed
honestly, twice.** At eight times the commit rate nothing was refused, because
the durable queue is the buffer and intake accepts at the network's pace, not
the store's. Unpaced, the producers still could not outrun intake. The
refusal machinery exists and is tested — the driver refuses at its
unacknowledged bound and intake counts what it refuses — but a drill that
demands a refusal this hardware cannot produce would fail for ever for the
wrong reason. What overload must never produce is silent loss, and that is
what the verdict now checks: everything captured is acknowledged or counted.

**Cost to change:** cheap. It is tooling.
**Revisit:** yes, in one narrow sense: a refusal under real overload has never
been observed end to end, only unit-tested. A machine with more producer cores
than this one, or producers on a second machine, would reach the boundary.

## L160. The reconciliation trusted checkpoints, and the store was more exact than the harness

**Phase:** 11
**Decision:** the soak driver snapshots every producer's counter at the moment
it creates a window edge, so a window's expected count is the difference of
two exact readings. The comparison allows one event for each producer, which
is the width of the stamp race at an edge instant, and nothing else.

**What the first four windows taught.** All four were reported as mismatches,
and the committed side was exactly 148,800 three times running — eight
producers at their integer-division rate of 62 events each second across a
300-second window, to the event. The expected side came from checkpoints
taken every ten seconds, so each edge carried up to ten seconds of staleness:
the first window was short by 4,798, which is one checkpoint interval of
events, and the rest wobbled by the residual between edges. The store counted
perfectly; the harness interpolated. Every exact lookup passed alongside —
eight of eight.

**The rule this repeats** is section 9's: never add component measurements
and call the sum an answer, and distrust a negative result about your own
design. A reconciliation that accuses the store must first be exact about
what it offered, and a checkpoint cadence is a measurement of the harness.

**The seed had the same lesson in it.** A restarted driver incarnation under
one fixed seed re-issued request IDs an earlier incarnation had already
committed, and every exact lookup then answered two rows — which reads as
duplication and is the harness colliding with itself. The seed now defaults
to the incarnation's start time and the status file records whichever ran,
so the run stays reproducible and two incarnations can never share an ID.

**Cost to change:** cheap, and done.
**Revisit:** no.

## L161. A supervisor that is a parent must not ask a zombie whether it is alive

**Phase:** 11
**Decision:** the soak tooling's liveness check reads the process table's own
state rather than sending signal zero, and reaps any zombie it finds.

**What happened.** The monitor starts every soak process, which makes it
their parent, and a child it has not reaped answers signal zero as if it were
alive. A killed driver therefore sat as a zombie the monitor reported as
running, and the monitor — whose whole job is restarting what dies — never
restarted it. The soak ran driverless until the monitor was restarted with
the corrected check. The same class of leak was already fixed in the drills,
where unreaped children accumulated as defunct entries; the monitor's version
was worse because a supervisor's blindness is silent.

**Cost to change:** cheap, and done.
**Revisit:** no. It is the sort of defect the soak exists to shake out of the
harness before it is trusted to accuse the store.

## L162. The owner said push the filter down, and the contract did not have to move

**Phase:** 11 (review)
**Decision:** an aggregate over filters over a scan is pushed down. The
coordinator walks through the filter chain to find the scan that routes the
plan, and each tablet applies the same chain with the coordinator's own
expression code before folding its rows. The owner approved lifting L136's
filter exclusion when the Phase 11 report put the question to them.

**The contract question dissolved on contact with the shape L136 chose.**
The exclusion was recorded as "pushing the expression language into the
storage contract", and when the plan was made to travel as encoded bytes the
cluster never reads, the predicate became one more thing inside those bytes.
`Store::partial_aggregates` keeps its bytes-in, bytes-out signature, the
CSIL contract is untouched, and D25's boundary — a store that never learns
the query algebra — holds exactly as before. What the owner's decision
actually bought is that a filtered aggregate no longer moves rows over the
network.

**One implementation, still.** The tablet-side filter runs `expr::prepare`
— the executor's own code — in the executor's own order, innermost first.
The test fixture whose `scan` refuses is what proves the filter went down
rather than being applied to rows that quietly crossed anyway, and a
stacked-filter case proves the chain.

**What is still not pushed down** is unchanged and unchanged for the same
reason: `rate`, `increase`, `quantile`, and `histogram_merge` have partial
states that do not merge exactly, and a refusal to push down is a slower
answer where a wrong merge is a wrong one.

**Cost to change:** cheap, and done. Two functions and two tests.
**Revisit:** no. What remains unmeasured is what it is worth at real
fan-out, which needs a multi-tablet installation; the shape no longer blocks
on a decision.

## L163. The stall signature appeared, on a follower, and it clears itself

**Phase:** 11 (the soak's first night)
**Decision:** none yet; this is the observation L131 has waited two phases
for, recorded while it is fresh. The soak's journal holds the raw events.

**What was seen.** Three episodes in seven hours, all on head-2, a follower
voter that serves no ingest: a group commit in flight while the durable
position stood still — at 42,514 for at least 60 seconds, at 64,549 for at
least 150, and at 73,223 for at least 210 — and each episode then cleared on
its own. Reconciliation stayed perfect through all three: 27 of 27 windows
clean and 184 of 184 exact lookups right across 12.2 million events, because
the other two voters kept the quorum and nothing needed head-2 to answer.

**What it changes about L131.** The hang now has a milder, recurring,
self-clearing form that reproduces within hours under replicated load —
not the permanent wedge that sat for two days, but the same signature:
`committing` held while nothing becomes durable, which L132 proved cannot be
a wait inside `wal.rs` alone. That it happens on a follower narrows the
suspects again: a follower's commits come from consensus apply, so the
interaction is between the apply path, the store's own locks, and the
device — with three voters and a queue sharing one disk. That it clears
after minutes says the committer eventually returns, which no theory about
an unbounded protocol wait predicts; a starved or serialized fsync fits
better.

**What was lost, and the rule it bought.** The scheduled voter outage killed
head-2 hours after the last episode, so no stacks were taken. The monitor
now spares a head that showed the signature in the last six hours, and its
journal entry says why: that process may be the only evidence L131 has ever
left alive.

**Cost to change:** nothing changed in the product.
**Revisit:** **yes, and this is the highest-value open thread.** The
episodes recur every one to two hours on this soak. The next one is
attachable: the head stays alive, the journal names it, and L132's gdb
method needs an ancestor or root. Take the stacks during an episode and
L131 stops being a mystery.

## L164. The stall detector lives inside the thread that stalls, and the stall is longer and more frequent than the journal says

**Phase:** 11 (the soak's second day)
**Decision:** none yet. This records what direct observation of a live
episode adds to L163, before the stacks are taken. The capture tooling is
at `run/soak/stall-capture/`; the stacks need root, which needs the owner.

**The detector is its own blind spot.** The head's stall watcher
(`crates/tallyowl-head/src/main.rs`, `watch_disk_space`) is one thread
that samples the store gauges and then reads the append-log state, every
ten seconds. When that thread stops, three things stop at once: the WAL
gauges freeze at their last values, the warning line cannot be written,
and the monitor — which reads those gauges — sees nothing. The code
comment beside it says a watcher that stops reporting is itself the
signal; that is true, and nothing watches for that signal. The journal
records an episode only when the frozen gauges happen to hold
`committing=1`; an episode frozen at `committing=0` is invisible.

**What was observed on 2026-08-10, 14:10 to 14:35 UTC.** The durable
position gauges on all three heads stood frozen at three different
values — 155,934, 152,089, 152,766 — for between 12 and more than 25
minutes, while the commit watermark climbed at about four batches each
second, intake accepted about 500 events each second, reconciliation
counters rose, and the data directory's WAL and catalog files carried
current mtimes. Nothing in the pipeline was stalled; only the observers
were. Head-2's gauge then jumped to 157,405, so its sampler ran again:
episodes clear, but they run minutes to hours, not only the 60 to 210
seconds the journal has recorded.

**The driver froze the same way.** `status.json` stopped at 11:34:33Z
while the producers kept producing at full rate — the frozen `captured`
count is 3.1 million behind what intake accepted since. Its eight
producer connections each hold exactly 604 unread response bytes, and the
frozen exact-lookup count fits a status loop blocked on a query into the
same wedge. So `soak status` has been reporting a five-hour-old snapshot
as current.

**What the thread evidence says before the stacks do.** During a live
freeze on head-1: both active consensus threads cycle through
uninterruptible disk sleep in `jbd2_log_wait_commit` (the ext4 journal,
shared by three voters, the queue, and a 2.5 GB catalog on one device),
which is the serialized-fsync shape L163 predicted; one unnamed head
thread, one worker thread, and one rpc-serve thread each burn close to a
full core; and about 75 parked consensus threads wait on futexes. Which
of these holds what the sampler wants is exactly what the stacks answer.

**Cost to change:** nothing changed in the product yet. The likely
product lessons — the stall watcher needs a watchdog outside itself, and
`soak status` should say how old `status.json` is — are cheap.
**Revisit:** **yes.** The wedge was live and attachable for tens of
minutes at a time. `sudo bash run/soak/stall-capture/capture-all.sh`
takes every thread's stack from all three heads plus the driver the
moment the owner runs it.

## L165. The stacks were taken, and L131 has an answer: the locator is merged from scratch, quadratically, on every read

**Phase:** 11 (the soak's second day)
**Decision:** none needed to diagnose; the owner decides the fix. The
stacks L163 asked for were taken with gdb as root during a live episode,
on all three heads at once, and all three show the same thing. The
captures are under `run/soak/stall-capture/`.

**What the stacks show.** On every head, the stall-watcher thread stands
inside `Catalog::locator` → `Locator::merge` → `LocatorRun::seal` →
`sort_unstable` over `locator::Entry`. On head-1 a second thread — an
rpc-serve thread answering the soak's exact-lookup spot check through
`lookup_correlated` — stands in the identical stack, and had been there
since 11:34Z, more than eight hours. The commit path is healthy in the
same capture: the consensus apply thread is in `fdatasync` inside
`SegmentedStore::commit`, and the watermark climbed through the whole
episode. Nothing is deadlocked. Two threads are doing months of work.

**The defect.** `Catalog::locator()` (catalog.rs) builds the combined
locator by calling `merge` once per stored run, and `LocatorRun::merge`
(locator.rs) is `extend_from_slice` followed by `seal()`, which sorts
and dedups the **whole accumulated vector**. Combining R runs therefore
re-sorts the entire entry set R times: quadratic in runs, on every call,
with no cache. At capture the locator was 1,812,897,312 bytes — about
56 million 32-byte entries over 999 segments and 29.2 million rows. At
alpha scale a call finished in milliseconds; that is why the journal's
first-night episodes ran 60 to 210 seconds and the second day's ran for
hours. The growth of the locator is the growth of the episode.

**Why it wore L131's costume.** The sampler calls `locator()` every ten
seconds to report `tallyowl_locator_bytes`, so once a pass costs more
than minutes the watcher is effectively always inside it: gauges freeze,
the stall warning cannot be written, the monitor reads frozen numbers,
and whichever values were last written — sometimes `committing=1` —
become the "stall signature". The driver's status loop shares a
goroutine with its exact lookup, so `soak status` froze at 11:34Z for
the same reason. Every observer was starved by the same read.

**What this does and does not close.** It explains every observed soak
episode, the frozen gauges, the wedged driver status, and the burning
cores. It does **not** prove the original two-day test wedge in
`many_concurrent_commits_all_survive` had the same cause; a test-scale
locator is small. That connection is plausible — a locator merge held
under the wrong lock would stall commits — and stays open honestly.

**The fix directions, for the owner to rank.** First, seal once: build
the combined entry vector across all runs and sort a single time —
O(N log N) total instead of per-run, a small change in
`Catalog::locator()`. The runs are stored sealed, so a k-way merge of
sorted runs is the further step. Second, cache the merged locator per
manifest generation instead of rebuilding per call. Third, the sampler
should not pay a full merge every ten seconds to report a byte count.
Fourth, the user-grouped cold compaction `prototypes/locator-bench`
already measured shrinks the entry count an order of magnitude; it was
already the ranked largest unbuilt win. Benchmark against a soak-aged
catalog, not an alpha-scale one — this defect was invisible at alpha
scale, which is the lesson: the cost curve, not the cost, was the bug.

**Cost to change:** cheap for the seal-once and the sampler; moderate
for the cache. Nothing changed yet; the soak still runs the old binary.
**Revisit:** **yes — this is the Phase 11 review's first item.** L131
stops being a mystery today; what remains is choosing the fix and
re-running the soak against it.

## L166. The four correlation names are protected, by the owner's decision

**Phase:** 11 review
**Decision:** the owner decided on 2026-08-10 to add `request_id`,
`session_id`, `trace_id`, and `event_id` to `PROTECTED_KEYS`, closing
L153's recommendation. Intake now refuses a client property carrying one
of these names and counts the refusal, exactly as it always did for the
stamped operator keys. The head distributes the same list in the policy
snapshot, so a collector and the head cannot disagree.

**Why:** the shadowed value was silently unreachable by filter — the field
reference reads the column — so nothing usable is lost by refusing, and the
refusal is visible where an operator can act. This is a collection-behavior
change: a client that sends these names today starts seeing them refused,
and the release notes for the first release candidate must say so.

**Cost to change:** cheap; one list in `tallyowl-wire`, and POLICY.md and
DATA_MODEL.md now name both kinds of protected key.
**Revisit:** no. The owner decided it.

## L167. The publish registries are named, and the Rust driver waits on one more decision

**Phase:** 11 review
**Decision:** the owner decided on 2026-08-10: the charts publish as OCI to
`oci://ghcr.io/catalystcommunity/charts`, the five client tarballs publish
to npmjs with public access, and a Go consumer needs only the git tag. The
publish job now carries the push commands and the secret references
(`ghcrpush`, `npmpublish` under `catalystcommunity/ci`), and it refuses
with the missing name until the owner creates each grant. The package job
separates charts from npm tarballs into subdirectories, because both are a
`.tgz` and the publish job must never guess which registry one takes.

**What stays open:** crates.io for the Rust driver. `cargo publish`
requires every path dependency to be published first, so publishing the
driver publishes the crates it depends on — public names on a public
registry, which is a release decision rather than a push command. The
publish job says so instead of half-publishing. D31 already means nothing
publishes before a release candidate exists.

**Cost to change:** cheap; the registry names and grants live in one job
file and one plugin function.
**Revisit:** **yes, once:** the crates.io dependency clearance, at the
release candidate.

## L168. The locator program, built to the owner's full scope

**Phase:** 11 review
**Decision:** the owner decided on 2026-08-10 to take the L165 fix all the
way: seal-once, sampler relief, the per-generation cache, and user-grouped
cold consolidation, benchmarked against a copy of the soak-aged catalog and
then rolled into the running soak. Built:

- **`Locator::from_all_runs`** concatenates every stored run per bucket and
  seals once — one sort per bucket instead of one per run.
  `Catalog::locator()` uses it. The equivalence with the old one-at-a-time
  merge is a test.
- **The sampler reads `Catalog::locator_bytes()`**, a value-length sum that
  decodes nothing, instead of building the whole combination to report its
  size. The gauge now reports stored-run bytes, before combining removes
  repeats, and its help text says so.
- **A second quadratic on the same path is gone**: compaction's
  `retain_segments` predicate scanned a manifest `Vec` once per locator
  entry; it is a `HashSet` now.
- **`SegmentedStore` caches the combined locator keyed by manifest
  generation.** A seal publishes runs and generation in one transaction, so
  the key is exact there; compaction replaces runs after its generation
  moved, so it invalidates explicitly. The memory held is the combined
  locator itself. Lookups no longer rebuild the world per call.
- **Cold consolidation** (`compaction.coldGroupAfter`, default 48 h, zero
  disables): a cold bucket's segments for one project are read together,
  ordered by the grouping value, and rewritten as few segments. Due only
  while a bucket holds more segments than its bytes need, which makes the
  pass idempotent. No restart dance: this pass keeps every visible row, and
  a tombstone that lands mid-pass still hides on read. Tests cover the
  grouping, the idempotence, the hot-segment exclusion, and that an erased
  row does not survive the rewrite.

**Cost to change:** moderate; the store, the head configuration schema, and
STORAGE.md section 11 moved together.
**Revisit:** **yes, for two follow-ups.** The aged-catalog measurement is
BENCHMARKS.md section 23: the old combine extrapolates to 1.7 to 2.8 hours
on 86 million entries and the seal-once combine measures 14.2 seconds, a
roughly 450-fold reduction before the cache amortizes it further. Still
open: the cache-miss price on a catalog this size is 14 seconds for the
first lookup after a generation change, and the sampler's 1.25-second
`locator_bytes` read repeats every ten seconds — both are worth revisiting
once cold consolidation has shrunk the entry count and its effect can be
measured.

## L169. The point lookups re-measured as hits, and the harness can no longer be fooled by a miss

**Phase:** 11 (after the review)
**Decision:** none needed; this is the re-measurement L153 required before
anything cites the alpha point-lookup latencies again. The full table is
BENCHMARKS.md section 24.

**The harness counts what a lookup found.** `testbed/cmd/load` decoded the
query response and threw the rows away, which is the blindness that let the
alpha report time fifty misses without anyone knowing. `PointLookup` returns
the row count now, and the report carries
`point_lookups_that_found_nothing`. Two probe controls,
`LOAD_LOOKUP_OFFSET` and `LOAD_LOOKUP_WORKER`, aim the probe at a value
stored once, a value the ramp's counter reuse stored about ten times, or a
value stored nowhere, and the empty count says which one a run measured.

**What the numbers say.** On a settled 286,658-event home-profile store,
release binaries with the L168 locator program, beside the running soak: a
miss costs about 55 ms — the alpha's 41 ms, plus the soak's noise — a value
stored once costs about 93 ms at p50, and a value stored in ten segments
costs 503 to 719 ms. A hit's cost is roughly linear in the segments that
hold the value, which is the per-lookup form of the L168 argument for
user-grouped cold consolidation.

**The first cold consolidation was about to run silently.** The maintenance
log line neither triggered on nor printed `consolidated_sources` and
`consolidated_outputs`, so the pass everyone is waiting to observe on the
soak — due when its data ages past `compaction.coldGroupAfter`, 48 hours —
would have left no trace in the head log. The counts are in the trigger and
the fields now. The running soak binaries predate this line, so its first
consolidation must be confirmed from the segment count and the catalog
generation instead, or the fix rolled in first.

**Cost to change:** cheap; the harness, one log line in the head, and
BENCHMARKS.md section 24.
**Revisit:** **yes, once:** re-run the settled-lookup table against a
post-consolidation store, so the segment-count dependence above gets its
"after" column.

## L170. The first observed consolidation printed its line, and it raised the price of a hit

**Phase:** 11 (after the review)
**Decision:** roll the L169 maintenance-log fix into the running soak
before its first cold consolidation, rather than confirm the pass from
segment counts alone. The log line is the direct evidence, a third roll
under live load strengthens the rolling-upgrade record, and only
`crates/tallyowl-head/src/main.rs` had changed since the running binaries
were built, so the roll carried nothing else. It ran clean on 2026-08-11
at 19:05 UTC: all five services back and ready in order, reconciliation
windows clean after it.

**The third roll put a number on what a roll costs a client.** The driver
counted 52 refused submissions during the collector restarts — a producer
that cannot reach intake knows it, so reconciliation never expects those
events and nothing was lost. The earlier two rolls showed zero, and
neither zero was a fair measurement: the first roll predates the running
driver, and the second ran while the driver's producers stood wedged
inside the L165 read, offering no load at the restart moment. Fifty-two
events at 500 each second across two collector restarts is the intake gap
a graceful roll actually opens.

**The first cold consolidation was observed — on the section 24 store,
before the soak's.** The settled home-profile store's segments were about
fifteen hours old, so `compaction.coldGroupAfter` was lowered to 12 h in
the local configuration for the measurement; the threshold decides when
the pass is due, never what it does. One pass ran at start-up and logged
`consolidated_sources: 4, consolidated_outputs: 2` — the L169 line fires.
Six segments became two, 9.3 MB and 0.9 MB, against the 8 MiB target.

**The consolidated store answers a hit ten times slower.** The L169 table,
re-run: a miss still costs about 66 ms, but a value stored once went from
93 ms to about 980 ms, and the ten-segment value from 503 to 719 ms to
about 1,020 ms. Both hit shapes converged on the price of decompressing
the one 9.3 MB segment nearly every row now lives in. A hit's cost is
linear in the bytes of the segments it reads, not only in their count —
section 24 already said 90 ms per roughly-1 MB segment, and consolidation
multiplied the bytes per segment by nine. BENCHMARKS.md section 24.1 has
the table. The locator shrink L168 measured is real; per lookup it helps
only once the read path can read less than a whole segment, or the target
is smaller than 8 MiB. Neither change is made here; the owner ranks them.

**The maintenance line has censored `files_reclaimed` since the line
existed.** The logger's privacy filter matches banned names as substrings,
and "reclaimed" contains "claim", so every "Maintenance reclaimed space."
line ever printed dropped that count and stamped `refused_fields: 1` — the
dev head's consolidation line demonstrated it. The field logs as
`files_freed` now, with a comment naming the constraint. The soak binaries
rolled tonight predate this rename; it rides the next natural roll. Worth
a thought later: the filter's `ip` entry already matches whole segments
only, and "claim" may deserve the same, but loosening a privacy matcher is
the owner's call, not a benchmark session's.

**Cost to change:** cheap; one field name in the head, BENCHMARKS.md
section 24.1, and this entry. Nothing in the store moved.
**Revisit:** **yes, twice.** First, the soak's own first consolidation and
the section 23 "after" numbers from a post-consolidation catalog copy,
due once its data ages past 48 h. Second, the hit-latency trade above:
sub-segment read granularity, a smaller consolidation target, or a
deliberate decision that cold hits may cost a second.

## L171. The consolidation pass held a whole day in memory, the lab found it before the soak fired it, and finding it cost the soak

**Phase:** 11 (after the review)
**Decision:** the owner's standing directive for the session — do not wait
for wall-clock time to test what is arithmetic. Cold consolidation's
trigger became a unit test at the shipped 48 h default
(`cold_consolidation_fires_on_its_own_once_rows_age_past_the_shipped_default`),
and the pass at soak scale became a lab: a copy of the section 23
aged catalog plus hard links of its sealed segments, consolidated in place
by the `consolidation_probe` in `crates/tallyowl-store`, hours before the
running soak's own data would age past the default.

**The lab's first run was the finding.** `consolidate_cold` bounded its
pass with `cold_group_batch_bytes` only *between* bucket groups — the
first group always loaded whole, and its rows were held decompressed,
grouped into a `BTreeMap` copy, and chunked into output copies, all at
once. A soak-aged day is one group. The probe reached 69.6 GB of resident
memory and the kernel OOM killer ended it, at 2026-08-11 19:40 UTC. The
comment on the setting already said why it exists: "instead of holding a
day of rows in memory at once." The implementation did not honor it, no
test held it, and nothing smaller than a soak-aged day could have shown it
— the dev store's whole bucket was nine megabytes.

**What the finding cost.** The kernel kill's blast radius included the
terminal session unit that parented the soak, and systemd took every soak
process with it: the 38.9-hour run ended at 19:40 UTC with 69.5 million
events captured, 37 of 37 reconciliation windows clean, 210 exact lookups
none wrong, and three clean rolls — the third 35 minutes earlier, its
windows still pending, so run 1 never confirmed them. The run's final
status and journal are preserved beside the live ones as
`run/soak/status-run1-final.json` and `run/soak/journal-run1.jsonl`. The
honest ledger: had the lab not run, all three live heads would have run
the same unbounded load themselves at hour 48, on their own maintenance
threads, nine hours later — the lab moved the detonation into a
sacrificial process and off the product's record, and an unlucky process
tree took the soak anyway.

**The fix, and its knobs.** One pass now takes at most the budget from
*inside* a group: the smallest segments first, so many small segments
shrink toward the target and the passes converge on the maintenance
interval; a taken set that would not shrink is left alone, so a bucket may
settle one segment above the ideal count rather than churn. A regression
test holds an oversized bucket to partial passes. `coldGroupTarget` and
`coldGroupBatch` are configuration now (schema, example, both charts —
the parity test forced every document), because the right budget depends
on the machine: the soak's heads share one box and run `64MiB`.

**Run 2 observed everything run 1 was waiting for, in its first minutes.**
The soak restarted on the fixed binaries at 2026-08-12 03:37 UTC against
the intact data directories — the driver seeds by start time, so a
restarted incarnation cannot re-issue run 1's request IDs (its own
comment records that lesson). The data stood 57 hours old, past the
untouched 48 h default, and the startup maintenance pass consolidated
naturally on every head: `consolidated_sources: 26` to `27` — a 64 MiB
bite, exactly the budget — `consolidated_outputs: 3`, and `files_freed`
printing where the censored `files_reclaimed` (L170) never could. The
lab's converged copy gave BENCHMARKS.md section 23.1 its after column:
1,455 stored runs became 2, the combine fell from 14.2 s to 1.55 s, and
999 segments became 168 that compress better than their sources did.

**Cost to change:** moderate; the store's pass, two new settings through
schema and charts, the soak configuration, STORAGE.md section 11, and
BENCHMARKS.md section 23.1.
**Revisit:** **yes, twice.** The soak's nodes converge over the next
hours; check the segment counts fall toward a few hundred and the memory
of each head stays flat across bites. And the pass's expansion factor —
64 MiB stored became several gigabytes resident — deserves its own
measurement before anyone raises a budget: the row form is many times the
segment form, and the budget is stored bytes, not resident bytes.

## L172. The owner priced the trade, the gauge became a counter, the filter learned word boundaries, and the soak retired

**Phase:** 11 (after the review)
**Decision:** five, all the owner's, decided 2026-08-12 over the open
items, with a standing directive alongside them: **no long wall-clock
validation in dev, ever again** — anything arithmetic gets a unit test or
an offline lab against copied data, and validation that genuinely needs
wall-clock time runs against a live environment. Run 1 is called what it
was: a plan this project should not have made.

**One: `compaction.coldGroupTarget` defaults to 2 MiB.** A cold hit
decompresses every candidate segment whole, so the target is the price of
a cold exact lookup. The re-measure (BENCHMARKS.md section 24.2) puts a
once-stored hit at 215 ms against a ~2.5 MB segment, on the 80 to 105 ms
per megabyte line every measurement since section 24 has followed.
Sub-segment read granularity stays the future lever if that must fall
further.

**Two: `compaction.coldGroupBatch` defaults to 32 MiB.** A 64 MiB bite
measured 12 to 22 GB resident on the run-2 heads; the budget is stored
bytes and the rows cost many times that. Both settings stay loudly
configurable — schema help text carries the expansion warning, and the
soak tooling's own 64 MiB override is gone because the default no longer
needs overriding.

**Three: the sampler's gauge is a maintained counter, and it moves
atomically.** `locator_bytes()` read 2.7 GB of stored run values every ten
seconds on a consolidated soak-aged catalog. The total now lives in one
catalog key and moves inside the same transaction as every locator run
write and removal (`write_durable`, `remove_durable`), so the gauge can
never disagree with the stored runs — the owner's condition. A catalog
from before the counter pays one full sum, inside a write transaction so
the stored total cannot race a run write, and never pays again. A test
holds the counter equal to a fresh scan through publish, replace, and
migration.

**Four: the privacy filter matches whole segments.** The substring match
that censored `files_reclaimed` (L170) now matches banned terms as whole
`_`/`-`/`.` separated segments, plurals included: `claims`, `claim_type`,
`x-api-key`, and `client_ip` are still refused; `files_reclaimed`,
`description`, and `recipient` pass. The `ip` special case became the
general rule. Tests pin both directions.

**Five: soak run 2 ended by decision, hours in, not days.** Before it
retired it had already shown everything it was going to: natural cold
consolidation at the untouched 48 h default on all three heads within
minutes of restart, bounded bites, the L169 log line, and `files_freed`
printing. The re-measure after it (section 24.2) also recorded two
smaller truths: a store whose segments already exceed the target is never
due — the pass merges scatter and refuses to split health — and section
24's once-stored probe offset had silently stopped being once-stored on a
faster ramp, which `point_lookups_that_found_nothing` cannot catch,
because it counts a probe that finds nothing and not one that finds too
much. A "stored once" aim is a fact of a store's write history, verified
here by the misses beginning where the prelude's counters end.

**Cost to change:** cheap to moderate; the counter in the catalog, the
matcher in the logger, defaults through schema, example, and both charts,
and BENCHMARKS.md sections 23.1 and 24.2.
**Revisit:** **once.** Sub-segment reads, only if a ~200 ms cold hit ever
matters; everything else here is decided and measured.

## L173. The first release candidate is 0.1.0-rc.1, and one verb writes it in nineteen places

**Phase:** release candidate
**Decision:** the version is **0.1.0-rc.1**, not 1.0.0-rc.1. The alpha gate is
met and Phases 7 to 11 are built, and none of it has run anywhere but here.
0.x also keeps D31's protocol window honest: a 0.x release may change the
protocol with one minor release of overlap, and a 1.0 promises more than this
project can support today.

**The shape** is three numbers with an optional `-rc.N`. Cargo, npm, Helm, and
a Go module tag all read that shape the same way, and a looser one does not
guarantee it. A release tag is the version with a `v` in front.

**One version, nineteen sites, one verb.** `./tools.sh version set` writes the
Rust workspace version and every workspace path-dependency version, the four
CSIL `package_version` values, both `Chart.yaml` versions and both
`appVersion` values, three package manifests, the browser and Go driver
envelope stamps, the tooling's own manifest, and the Go module requirements —
then regenerates the clients, which is where the generated versions come from.
`version check` fails the build when two sites disagree, and CI runs it before
anything is built. The site list is code, not prose: a file that starts
carrying a version gets a line in `SITES`, and a site that stops matching its
pattern is a refusal that names the file.

**Why a version site list rather than one file everything reads.** Four
languages and two package managers each want the version in their own manifest,
and none of them will read somebody else's. The choice is a list that is
checked or a drift nobody notices until a chart asks for an image tag that does
not exist.

**The path dependencies now carry versions.** `cargo publish` refuses a
dependency that has only a path. Adding `version` beside each `path` costs
nothing in a workspace build and is a prerequisite for the crates.io push the
owner clears.

**Cost to change:** cheap. One list in `tools/tallyowl_tools/release.py`, and
`./tools.sh version set <other>` rewrites the tree.
**Revisit:** **yes.** The number itself. If the owner wants the first candidate
to be 1.0.0-rc.1, it is one command and a re-read of the release notes.

## L174. One image, one registry, and therefore two grants rather than three

**Phase:** release candidate
**Decision:** the head and the collector ship as **one container image**,
`ghcr.io/catalystcommunity/tallyowl`, built from a `Containerfile` at the
repository root. Both charts already ran one image repository and selected the
binary with `command`, so this follows what the charts say rather than changing
it.

**Why ghcr.io.** The charts named
`containers.catalystsquad.com/public/catalystcommunity/tallyowl` and nothing
built or pushed that image. Pushing there needs a credential the owner would
have to create, which would be a **third** grant beside `ghcrpush` and
`npmpublish`. ghcr.io takes the image on the grant the charts already use, so
the release candidate needs the two grants the owner already knows about. D31
reserved the container registry for the release candidate and this closes it.

**The image compiles from source** rather than copying a binary from a build
machine, because a binary built against another machine's libraries is not the
binary the chart runs.

**It runs as user and group 65532**, and both charts now carry a
`securityContext` with the same numbers. The head chart sets `fsGroup`, because
a mounted volume arrives owned by root and a storage node that cannot write its
data directory does not start. `securityContext` joins the chart-parity test's
list of deployment keys, beside `image` and `resources`.

**Cost to change:** cheap. The registry is one value in each chart, and
`release.default_image_registry` reads the chart rather than holding a second
copy.
**Revisit:** **yes.** If the owner would rather publish to
`containers.catalystsquad.com`, that is a third grant and two chart values.

## L175. The Go module paths named directories that do not exist, and no consumer could have fetched them

**Phase:** release candidate
**Decision:** the four generated Go clients declared
`github.com/CatalystCommunity/tallyowl/clients/tallyowl-<name>` while their
`go.mod` files live in `generated/go/tallyowl-<name>-api`. The module proxy
resolves a module by its path inside the repository, so **no consumer could
ever have fetched them**. Every build here passed because `replace` directives
point at the local directories, and a `replace` in a dependency is ignored by
the module that depends on it.

L167 recorded "the git tag itself; the module proxy needs no push" for the Go
driver. That was true about pushing and false about fetching.

**Built:** the four `go_module` options in the specifications now name the
directory the generated module is in, the imports across the app driver and the
test bed follow, and the requirement versions are a version site so that a
release stamps them. The `replace` directives stay, because a build inside this
checkout must not need a published tag.

**What this costs the owner:** a release needs one tag for each published Go
module, beside the repository tag. `docs/CI-CD.md` section 6 lists the six
tags, and the release notes list them again for a consumer.

**Why not move the generated code to `clients/` instead**, which would have
kept the prettier path: `generated/` is where generated code lives and that
rule has one exception today, which is none. A longer import path is cheaper
than a second place to look for generated code.

**How it was found:** by reading `go.mod` while writing the release notes,
which is the same lesson L146 recorded — a statement about what something has
is a thing to read rather than to remember.

**Cost to change:** moderate now, expensive after the first tag. A published
module path cannot be renamed without a new major version.
**Revisit:** no.

## L176. The release job needs Helm and Node, and the runner image has neither

**Phase:** release candidate
**Decision:** the package and publish jobs fetch Helm and Node the way
`./tools.sh deps` already fetches DuckDB, at versions pinned in
`tools/tallyowl_tools/deps.py`.

**Why:** the package job runs `helm package` and `npm pack`, and the publish
job runs `helm push` and `npm publish`. The Reactorcide runner image carries
podman, Git, uv, Go, and Rust, and carries **no Helm and no Node**. That was
read out of the image with `docker run ... command -v`, not assumed: the same
run is why the image build uses podman first and the image push does not need a
BuildKit sidecar.

**The package and publish split.** Everything that touches no secret is now a
`tools.sh` verb — `release package` builds both charts, the five client
packages, and the service image, and saves the image to the artifact directory.
Everything that touches a secret stays in the trusted CI plugin: the registry
logins, the pushes, and the grant refusal. A test reads both files and fails
when the two grant lists drift apart.

**The crates.io clearance is a printed list now.** `./tools.sh release
crates-plan` walks the Rust app driver's dependencies and prints the crates a
publication covers, dependencies first, marking the ones whose manifests say
`publish = false`. The clearance L167 reserved for the owner is a decision
about that list, and the list is no longer something somebody has to work out
by hand at the moment of the release.

**Cost to change:** cheap.
**Revisit:** no.

## L177. Every npm tarball this project would have published was empty or broken, and packing one is now a check

**Phase:** release candidate
**Decision:** the release publishes **one** npm package, `@tallyowl/browser`,
and it ships compiled output. The four generated TypeScript clients are not
published at this candidate.

**What was found**, by packing a tarball and reading what was inside it rather
than trusting `npm pack`:

- a generated client's `package.json` says `files: ["dist"]` and nobody built
  `dist`. `npm pack` produced a tarball holding **one file**, the manifest.
  Publishing it would have put four packages on npmjs that install and import
  nothing, and a published version cannot be replaced;
- `@tallyowl/browser` exported `./src/index.ts`, and that source imports
  `../../../generated/typescript/...`, which is outside the package. An
  installed copy could never have resolved it.

**Built:**

- the browser package publishes `dist`. Its compiler puts the generated client
  and the CSIL transport under the same `dist`, because `rootDir` is the
  repository root, so the tarball is self-contained: 22 files, including the
  entry point, the codecs, and the transport;
- `release package` builds each package before it packs it, and then reads
  every tarball and refuses one that does not carry the entry point. A refusal
  is better than a broken publication that cannot be taken back.

**Why the generated clients wait.** `csil/tallyowl-ingest.csil` says at the top
that an application includes the specification in its own CSIL build, so a
published client is a convenience rather than the path. It also does not
compile on its own: the emitted server dispatch throws
`{ code: 404, message } satisfies ServiceError`, and `ServiceError.code` in
`csil/types/common.csil` is a text enum. `tsc` reports four errors and still
emits, which is how this went unnoticed — nothing in this repository compiles
that file, because the browser package includes only `types.gen.ts` and
`codec.gen.ts`.

**This is a csilgen defect rather than a missing capability**, so section 10.1's
three-shapes rule does not apply: the generator writes a value that does not
satisfy the type it names. It needs the owner to take it to the csilgen
maintainer. Changing `ErrorCode` to a number to satisfy a template would move
the wire contract to fit a generator, which is the wrong direction.

**Cost to change:** cheap. One tuple in `tools/tallyowl_tools/release.py` adds
the generated clients back the day they build.
**Revisit:** **yes.** Publish the generated clients once csilgen emits a server
dispatch that matches the contract's own error type.

## L178. The version is computed from the commits, and the release job is the one place CI writes to the source

**Phase:** first release
**Decision:** the owner directed on 2026-08-12 that TallyOwl release the way
every other repository here releases: `semver-tags` reads the conventional
commits since the last tag and says what the next version is, and a merge to
main runs every gate and then cuts, tags, and publishes it.

**The exception this needs, and the owner's reason for it.** CI-CD.md
constraint 7 said "CI does not stage, commit, or push source changes". The
release job now does exactly that, for one file set: the nineteen version
sites, in one commit, with one tag for the repository and one for each Go
module, pushed atomically. The owner's reason, in their words: *if CI doesn't,
a human does and that can introduce error. This particular piece is the only
real exception.* The constraint is rewritten rather than quietly broken.

**What the numbers are.** semver-tags starts an untagged repository at 0.1.0,
and the four conventional commits in this one include `feat:`, so the first
published release is **0.2.0**. The candidate this session cut first,
0.1.0-rc.1, is gone: `--pre_release_string rc` does increment `rc.1` to `rc.2`,
but on this repository it produces a pre-release of a 0.1.0 that was never
published, and finalizing later jumps to 0.2.0 and skips it. The owner's
instruction covered that case — straight semver if the counter does not carry
cleanly — so 0.2.0 it is.

**`release cut` refuses to run outside the release job.** It computes the
version, writes `dist/release.json`, and stops unless `TALLYOWL_RELEASE=1`.
Nothing in this repository commits or pushes on a workstation, and that
includes the tool that exists to commit and push.

**Cargo.lock became a version site**, because the service image builds with
`--locked` and the first 0.2.0 image build failed on a lock file still holding
0.1.0-rc.1. `version set` now writes the lock as well, and `version check`
reads it. A version site list earns its place the day it catches something.

**Cost to change:** cheap. The trigger is one workflow file and the
computation is one flag.
**Revisit:** no. The owner directed it.

## L179. The registry is the one the organization already runs, and two of the three grants already existed

**Phase:** first release
**Decision:** the image goes to
`containers.catalystsquad.com/public/catalystcommunity/tallyowl`, the charts go
into the `catalystcommunity/charts` repository and onto a GitHub release, and
ghcr.io is gone from this repository.

**Two hostname corrections, from measurement rather than memory.** The owner
asked for `containers.catalystcommunity.com`; that name has no DNS record from
this machine, while `containers.catalystsquad.com` resolves and answers `/v2/`
with 200 and is what corndogs, the runner images, and TallyOwl's own chart
already name. The coordinator API was named as `reactorcide.containers.com`,
which resolves to a hosting provider's parked address; the organization's
coordinator is `https://reactorcide.catalystsquad.com`, from
`~/.config/reactorcide/k8s-deploy.yaml`. Both were raised rather than guessed
at, and the owner chose the host that resolves.

**What this cost in grants: less than nothing.** `catalystcommunity/registry`
already holds `user` and `password`, and `catalystcommunity/ci` already holds
`githubpat`. The ghcr plan needed a grant that did not exist (`ghcrpush`) and
this plan needs none for the image or the charts. **`npmpublish` is the only
publish secret a release still waits on**, beside the crates.io clearance.

**How the pieces move**, following corndogs rather than inventing a second
shape: the package step builds the image and saves it, and the publish step
sends the archive with `crane`, so the job that holds the push grant needs no
container daemon at all. The charts are copied into the charts repository,
which indexes them on merge.

**Cost to change:** cheap. The registry is one value in each chart, and the
release job reads the chart rather than holding a second copy of it.
**Revisit:** no.

## L180. npm is removing the token that publishes, so the release stages and a person approves

**Phase:** first release
**Decision:** the release job runs `npm stage publish`, not `npm publish`. The
staged version waits until a maintainer approves it with two-factor
authentication, and only then is it installable.

**Why:** npm deprecated the 2FA-bypass granular access token in August 2026 and
removes its publish capability in January 2027. The two paths after that are
trusted publishing with OIDC, which federates with GitHub Actions and GitLab
rather than with Reactorcide, and staged publishing with a human approval.
Staging is the one that fits, and it is also the better posture: the token this
repository holds cannot publish anything on its own.

**What it changed here:** the pinned Node moved to 26.1.0, because `npm stage`
arrives in npm 11 and the previous pin carried npm 10. The release notes tell
an installer that the package appears minutes after the release rather than
with it.

> **Corrected by L190.** "`npm stage` arrives in npm 11" is wrong to the minor:
> it arrives in **11.16.0**, and Node 26.1.0 carries npm 11.13.0. This pin
> could not stage anything, and the first release found that out after it had
> published four things it could not take back. The pin is now Node 26.9.0.

**Cost to change:** cheap; one command in the release job.
**Revisit:** **yes**, if npm ever federates with Reactorcide. Trusted
publishing with a stage-only grant would then be better than a stored token.

## L181. The npm scope is the organization's, not the product's

**Phase:** first release
**Decision:** the browser package is `@catalystcommunity/tallyowl-browser`. The
name was `@tallyowl/browser`, and the owner said on 2026-08-12 that the npm
organization is `catalystcommunity`. A scope is an organization, not a product,
and this organization has one: `@catalystcommunity/proto2graphql` and
`@catalystcommunity/ui-typescript-hello-app` are already published that way, so
the product goes in the name rather than in the scope.

The two packages that are not published moved with it —
`@catalystcommunity/tallyowl-dashboard` and
`@catalystcommunity/tallyowl-testbed-webapp` — so the repository has one
convention rather than a published one and a private one.

**A granular token cannot name a package that has never been published**, which
is why the scope matters before the first release rather than after it: the
grant is written against `@catalystcommunity`, which exists.

**Cost to change:** cheap now, expensive after the first publish. A published
name is permanent, and a rename is a new package plus a deprecation on the old.
**Revisit:** no.

## L182. crates.io is off, and the release page carries the binaries instead

**Phase:** first release
**Decision:** the owner turned crates.io publication off on 2026-08-12, until
the GitHub release pages have proved themselves. A release now attaches one
archive of both service binaries, with the license and a `SHA256SUMS`, beside
the two charts.

**Why this is the right order.** A crate name on a public registry cannot be
taken back, and publishing the Rust app driver publishes the seven crates it
depends on. Waiting until the automated release has run a few times costs
nothing: a Rust application depends on the driver by Git revision meanwhile,
which is how this repository already depends on csilgen, Corndogs, and
LinkKeys.

**The binaries come out of the image**, with `create` and `cp`, rather than off
the build machine. A downloaded TallyOwl and a deployed TallyOwl are then one
build against one set of libraries, and the release notes say what glibc that
needs. A statically linked build for a machine older than the image is a
different job, and this release does not pretend to be it.

**`./tools.sh release crates-plan` still exists** and still prints the seven
crates in push order. It now says the publication is off rather than pending,
so the list stays ready for the day the owner turns it on.

**Cost to change:** cheap in both directions. Turning crates.io on is the
clearance, one grant, and the push loop the plan already describes.
**Revisit:** **yes**, at the owner's word.

## L183. The version window is enforced, so Phase 11's third exit criterion passes on a mechanism rather than on a promise

**Phase:** first release
**Decision:** the owner asked on 2026-08-12 for Phase 11's third exit criterion
— "rolling upgrades maintain adjacent-version clients" — to be fixed rather
than carried. It is fixed by making the window real code:

- `SubmitBatchRequest` and `CommitBatchRequest` each carry an optional
  `protocol_version`. An app driver declares what it speaks; a collector
  declares its own when it forwards, because that is the hop the head is
  judging;
- `tallyowl_wire::protocol` holds `PROTOCOL_VERSION`,
  `ACCEPTED_PROTOCOL_VERSIONS`, and the refusal text. **One list**, for the
  same reason `PROTECTED_KEYS` is one list: collector intake enforces it and
  the head enforces it again, and two copies of a window drift;
- a version outside the window is refused with `schema-unsupported`, a message
  that names both ends, and `tallyowl_protocol_version_refused_total` labelled
  by version. Intake refuses **before** the durable write, so an application
  learns at the call rather than after a quarantine;
- an absent declaration is the current version. Every driver written before
  this field predates the window, and refusing them would refuse everything.

**Why the head checks a value the collector already checked.** The upgrade
order in DEPLOYMENT.md section 7 puts the head first, so for the length of a
roll every collector still running is one version behind the head it forwards
to. The head is the authority on what it can store, and a collector further
behind than the window must not write rows this build would read wrongly.

**What the test proves, and what it does not.**
`crates/tallyowl-head/tests/protocol_window.rs` holds the window across a head
restart — the roll itself — and also proves that a retry after the restart
deduplicates rather than commits twice. What it cannot prove is behavior
difference: there is one protocol version, so no member of the window differs
from another. L158 refused to drill a build against itself and call it
compatibility, and that refusal still stands; what changed is that the
mechanism the second version will need is built and tested **before** it
arrives rather than after.

**Cost to change:** cheap. The window is one constant and one function.
**Revisit:** no, but **act on it**: the day a second protocol version exists,
add it to `ACCEPTED_PROTOCOL_VERSIONS` and drill the two binaries against each
other. The test that holds the window's size to two members will fail if
somebody widens it without deciding to.

## L184. The review found the release job could not run, and the order it ran in was the dangerous one

**Phase:** first release
**Decision:** a multi-agent review of the release work confirmed ten defects,
four of them in the release job itself, and all ten are fixed. Two are worth
carrying as lessons rather than as changelog lines.

**The order was wrong, and that was the serious one.** The job pushed the
version commit and six immutable tags **before** it built a single artifact. A
failure in the npm build, the tarball check, or the image build would have left
`v0.2.0` and five Go module tags public with nothing behind them — and the Go
module proxy caches a tag for ever, so the next run would compute 0.2.1 and
0.2.0 would be a dead version permanently. The job now stamps, builds and
verifies every artifact, pushes the image, and **tags last**. If main moved
while the artifacts were building, the atomic push is refused and so is the
release, because the version was computed from a commit that is no longer the
head. Everything after the tag is repeatable, so a re-run finishes a release
rather than starting a broken second one.

**Three ways it could not have run at all**, each from a boundary that was
assumed rather than read: `deps.semver_tags_program()` refused instead of
fetching, so the job would have failed on its first step; the git credential
helper was configured per-repository, so the push into the charts repository
had no credential; and the fetched `npm` is a shim with a `#!/usr/bin/env node`
line, so it needed a PATH the plugin never gave it. The tool lookups fetch on
demand now, the helper is global, and the npm call gets the same environment
`tallyowl_tools.packages` builds for every other npm call.

**Two more that made the version untrustworthy.** The version was computed
before the rebase it tagged, so a `feat:` merging mid-job would have shipped
inside a patch release and its minor bump would never have been applied — the
rebase happens first now. And `set_version` probed for `csilgen` with `which`,
which cannot see `.deps/bin`, so **every** version cut since the generator
became a pinned release had silently skipped regeneration and stamped the
generated files instead. The fallback is gone: there is one way to produce
generated code, and it is the generator.

**The lesson the last one shares with L177 and L175:** a check that silently
takes the other branch is worse than no check. Each of the three was a fallback
written for a case that had stopped existing, and each hid the thing it was
meant to protect.

**Two the review found outside the pipeline.** The protocol-refusal counter
labelled the metric with the **caller's** version number, so a broken client
could grow one series per number it invented, from the path whose whole purpose
is a cheap rejection; the label is now `too-old` or `too-new`, and the version
still reaches the operator in the refusal message. And `securityContext`
reached the settings ConfigMap, which would have made `config check` fail on
every stock installation: the chart-parity test filters the deployment keys out
**before** it loads, so it could not see it. `helm-check` now reads the
rendered ConfigMap, which is where the fault appeared, and refuses any
deployment key in it. That check was verified by reintroducing the fault.

**Cost to change:** cheap.
**Revisit:** no.

## L185. Every job was run in the runner image, and four of them could not have passed

**Phase:** first release
**Decision:** the owner asked on 2026-08-13 for the test suite to be timed in a
`run-local` job. Running one job that way found enough that all seven were run,
and four failed in ways no amount of reading would have shown.

**What the timing says.** `test-rust` in the runner image, from an empty target
directory: **121 seconds**, of which 51 are the compile, for 1,208 tests. The
job's timeout is 3600. Nothing here is near its limit, and the earlier worry
about a cold compile was wrong by an order of magnitude. `helm-check` is 4
seconds, `audit` 26, and the rest are seconds.

**Four failures, each one a boundary that was assumed:**

- **the Go module cache belongs to root.** The image sets `GOPATH=/go` and that
  directory is root-owned, while a job runs unprivileged. Every Go command
  stopped with `could not create module cache`, which took the **Rust** suite
  down with it, because `go_driver.rs` builds and runs the real Go driver
  against a real collector. Go now gets a cache under `.deps` when the
  configured one is not writable, and the Rust suite runs with that environment
  too;
- **`cargo fmt` is not installed.** The image carries a minimal toolchain:
  `cargo-clippy` is there, `cargo-fmt` is not. `lint` failed on its first line.
  The component is added on demand now, the way every other tool here is
  fetched on demand;
- **csilgen without its generators refuses every target.** The core release
  archive carries the binary and nothing else; csilgen loads a WASM generator
  for each target from `~/.csilgen/generators`, and with none it says "Unknown
  target 'rust'". Provisioning now **fetches the published generator archive
  and builds from the pinned checkout only when there is none**.

  Today it builds, and the reason is worth stating exactly, because the first
  version of this entry got it wrong. csilgen's release job **does** archive
  each generator as `csilgen-generator-<language>-<version>.tar.gz`, and the
  `generator-*/v0.2.0` releases **do** exist — as drafts with no assets,
  created 2026-08-03 and never filled, while `csilgen-core/v0.2.1` published
  normally four days later. So the machinery is right and the artifacts are
  absent. The day a generator release carries its archive, this fetches it and
  the build disappears with no change here;
- **the conventional-commit gate did not exist**, so a merge whose commits said
  nothing would have produced no release and no explanation. It exists now, and
  its list of types is read out of `semver-tags --help` by a test rather than
  copied, so the gate and the calculator cannot drift.

**The lesson, again and more expensively:** every one of these was invisible to
reading and obvious to running. The tool inventory I took from the image was
right and still insufficient — `cargo-clippy` being present said nothing about
`cargo-fmt`, and a binary being present said nothing about the plugins it
loads. Run the job.

**Cost to change:** cheap.
**Revisit:** **yes, once:** the csilgen generator releases are empty drafts, so
the published CLI cannot generate anything on a machine that has only the
release. Either those drafts get their assets, or the core release carries the
generators with the binary. Both are the maintainer's call, and TallyOwl needs
no change when either happens.

## L186. The csilgen release is read, not guessed, so the combined one needs a version bump and nothing else

**Phase:** first release
**Decision:** the owner said on 2026-08-15 that csilgen is moving to one
release for each version — the command line for each platform, the transport
library for each language, and a single tarball of every generator, all under
one `csilgen/<version>` tag. Provisioning is prepared for it now.

**Assets are chosen by shape rather than by a name somebody typed.**
`tools/tallyowl_tools/csilgen_release.py` reads the release from the GitHub API
and matches each artifact with a pattern: the command line for this platform,
anything that looks like the generators tarball, and the TypeScript transport
and not another language's. Two plausible matches is a refusal rather than a
guess, because taking the first of two archives is how a build ends up running
something nobody chose. `./tools.sh deps show` prints the tag it resolved, the
asset it would take for each artifact, and every asset the release holds — one
command to run on release day.

**Three fallbacks stay until they are not needed.** No combined release exists
yet, so today the command line still comes from its per-platform asset, the
generators are still built from the pinned checkout, and the transport still
comes from a git clone. Each falls back on its own, so the release can land
one artifact at a time.

**The transport keeps its path**, whatever the archive calls itself inside.
Eight source files reach it at `.deps/csilgen/transports/typescript/src` by
relative path, and `extract_tree` drops the archive's wrapper directory to land
exactly there. Moving that path is a separate decision from changing where the
bytes come from, and this is only the second one. When the transport asset
exists, the 200 MB clone of a repository this project reads one directory of
disappears.

**A tag holds a slash, and an unescaped one is a different endpoint.** That is
what made an existing csilgen release look absent earlier in this session: the
API answered 404 for `releases/tags/generator-rust/v0.2.0`, which reads as a
path nobody serves, and the conclusion drawn from it — "csilgen publishes no
generator artifact" — was wrong. The generator releases existed as empty
drafts. The escaping is a test now, with that mistake written into it.

**Cost to change:** cheap. On release day: raise `CSILGEN_VERSION`, run
`./tools.sh deps show`, and widen a pattern if an asset is named differently
than these tests expect.
**Revisit:** no.

## L187. The image carries the dashboard, a pod binds the network, and a Gateway route is how anybody sees it

**Phase:** first release
**Decision:** the owner asked on 2026-08-18 for the three things that stood
between the two existing surfaces — the dashboard and the reference
application — and a real environment.

**The image had no dashboard.** The chart pointed `dashboard.assets` at
`packages/dashboard/dist` and the image held two binaries, so a deployed head
served an empty page. The image now builds the bundle in a Node stage and keeps
it at `/usr/local/share/tallyowl/dashboard`. That stage compiles TypeScript
rather than linking machine code, so none of the library concerns that make the
Rust stage build from source apply; it is there so the image is self-contained.

**Every listening address in a rendered chart was loopback**, which in a pod
reaches nothing while the Service in front of it forwards to a port no client
can use. Collectors could not have reached the head, and nobody could have
reached the dashboard.

**The fix keeps the loader's defaults and changes the deployment.** The
alternative was to change the built-in defaults to `0.0.0.0`, which would open
a port on a workstation because somebody ran `dev up`. Instead the charts carry
two deployment values — `bindAddress` and `dashboardAssets` — and the
`settings` helper rewrites the rendered configuration from them. The port
always comes from the setting, and an empty `replication.listen` stays empty,
because an empty value means a node with no replication port. The chart-parity
test still compares values against the loader, unchanged, because the values
are unchanged.

**Gateway API rather than Ingress**, by the owner's decision. An HTTPRoute
attaches to a Gateway the platform already runs; the charts create no Gateway,
because a Gateway carries a listener and an address that belong to the
platform. **Ingest gets no route at all**: it is CSIL over TLS over TCP, and a
route in front of it would be the generic HTTP ingest API AGENTS.md forbids.
Three refusals are rendered and proved: a route with no Gateway named, a
dashboard route with the dashboard off, and a dashboard route on the collector
chart, which serves none.

**`helm-check` grew two more checks and then hid them.** It now refuses a
loopback bind and a relative asset path in a rendered chart — and while adding
them, an edit nested the refusal loop inside a new function, so the verb
returned `None`, the dispatcher read that as success, and `helm-check` passed
while checking nothing. The same silent-green failure L124 recorded, in the
tool that exists to prevent it. There are two tests on the shape of
`helm.check` now: it must end by returning 0, and each named step must appear
in its body.

**Cost to change:** cheap. `bindAddress`, `dashboardAssets`, and the whole
gateway block are deployment values with defaults.
**Revisit:** no. Ingress if somebody asks, and nobody has.

## L188. The project, the webhook, and two grants exist, and csilgen is at 0.2.7

**Phase:** first release
**Decision:** the owner asked on 2026-09-17 for the Reactorcide side to be set
up. It is:

| What | Value |
| --- | --- |
| Project | `01a0b27f-fe34-d888-54fa-7cc571662037`, enabled, queue `reactorcide-jobs` |
| Events | `pull_request_opened`, `pull_request_updated`, `pull_request_merged` |
| Webhook | GitHub hook `681166153`, `pull_request` and `push`, the shared secret |
| Grants | `tallyowl-release-registry` and `tallyowl-release-ci`, both matching the node `tallyowl-release` exactly |

**No `tag_created` event.** TallyOwl releases on a merge, and the release job
makes its own tags. An event this project cannot act on is an event that should
not start a workflow.

**One node holds every grant.** Every gate — the contract, three language
suites, the charts, the audit, the commit gate — runs with none, which is what
makes a pull request from a fork safe to run.

**Every workflow node is renamed for its job**, following Ichoi: the node
`release` became `tallyowl-release` and so on. A grant subject matches a job
name, and a node named differently from its job is a grant that reads correctly
and matches nothing.

**Two facts that were read rather than assumed.** The coordinator finds a
project by an exact string compare on `repo_url`, so it is
`github.com/catalystcommunity/tallyowl` with no scheme and no `.git`; a project
created any other way answers 500 to every delivery and looks like a wrong
secret. And the per-provider maps (`vcs_token_secrets`, `webhook_secrets`) are
set beside the single-value fields, as the catalystlinkkeys record has them.

**csilgen is at 0.2.7, and the upgrade changed the codecs.** The pin moved from
0.2.1, and every `codec.gen.*` changed — which is the diff worth reading rather
than accepting. It is **decoder hardening, not a wire change**: a 64-level
nesting limit, and bounds checks rewritten so a length from the wire cannot
overflow the addition that validates it. Encoding is untouched, and
`every_vector_matches_the_committed_bytes` proves it across Rust, Go, and
TypeScript.

**Generation also emits a schema descriptor now**, one `*.csil-schema.cbor` for
each entry specification, about 20 KB each. They are generated output like
everything else in `generated/`, so they are checked in and `gen-check`
compares them.

**The transport archive is one directory deeper than expected**, and the
refusal written a month ago said so precisely: it holds
`transports/typescript/src/index.ts`. The unpacker now finds the directory that
holds a marker file rather than dropping a fixed number of leading parts, which
is the difference between a release-day refusal and a release-day fix.

**The audit gate caught a real advisory the same week.**
`RUSTSEC-2026-0285`, published 2026-09-14: rustls 0.23.43 accepts TLS 1.3
handshake messages across encryption level boundaries, medium severity. That is
TallyOwl's mutual-TLS path between a collector and a head. The lock file moved
to rustls 0.23.45, the suite passes, and the whole pull-request workflow is
green again. A dependency audit that only ever passes is an audit nobody has
tested; this one earned its place three days after the advisory existed.

**Two clippy lints arrived with rustc 1.98** and failed `lint` on code nobody
had touched: `drain_collect` in the fake queue and `chunks_exact_to_as_chunks`
in the protobuf reader. Both are now written the way the newer compiler asks.

**Cost to change:** cheap.
**Revisit:** no.

## L189. The first real release stopped at the one input the test job supplies and the package job did not

**Phase:** first release
**Decision:** the merge of pull request 1 started `TallyOwl Release` on
2026-09-20. All eight gates passed. `release stamp` computed **0.2.0** — one
`feat:` commit, one bump — rebased onto main, wrote all nineteen version sites,
regenerated the clients, and repacked `Cargo.lock`. Both charts packed. Then
`npm run build` in `packages/browser` failed:

```
test/transport.ts(7,15): error TS2307: Cannot find module
'../../../.deps/csilgen/transports/typescript/src/index.ts'
```

**The transport is an input to packaging, not only to testing.** Four
TypeScript packages reach `.deps/csilgen/transports/typescript/src` by relative
path, `tsc` copies it under each package's `dist` because `rootDir` is the
repository root, and the image build copies the same directory out of the build
context — `.dockerignore` excludes `.deps` and names that one path back in.
`packages.typescript_test` and `packages.typescript_install` both call
`deps.fetch_csilgen()` first. `release.package` did not. So the directory
existed on every machine that had run the tests, and on no machine that had
not.

**A green TypeScript suite is the one signal that cannot catch this**, because
the suite is what leaves the transport on disk. Running every job in the runner
image (L185) did not catch it either: the jobs ran in one workspace, in order,
and `test-ts` went first.

**The fix is one call and one test.** `release.package` now fetches the
transport before anything TypeScript runs. `tools/tests/test_release_package.py`
records the order of the calls and refuses a build that starts before the
transport is asked for. The test fails against the code that shipped, which is
the only proof that it tests anything.

**Nothing was published.** The order this job runs in is the one L184 put it
in: artifacts first, tags last. The failure happened five steps before the
first tag, so there is no `0.2.0` tag, no image, no chart, no release, and no
staged package to take back. A release that fails early is a release that costs
nothing.

**Cost to change:** cheap.
**Revisit:** no.


## L190. 0.2.0 is published, and four things that cannot be taken back went out before a three-minor version gap stopped the fifth

**Phase:** first release
**Decision:** the release job ran on 2026-09-20 and did almost all of it. The
image is at `containers.catalystsquad.com/public/catalystcommunity/tallyowl:0.2.0`.
Six tags are pushed. The release page carries the binary, the checksums, and
both charts. `catalystcommunity/charts` holds `tallyowl-0.2.0.tgz` and
`tallyowl-collector-0.2.0.tgz`. Then:

```
Running: npm stage publish .../catalystcommunity-tallyowl-browser-0.2.0.tgz --access public
Unknown command: "stage"
```

**The command was right and the npm was three minors too old.** `npm stage`
exists, with exactly the semantics L180 wanted: `publish`, `list`, `view`,
`approve`, `reject`, `download`, described by npm as "deferring
proof-of-presence (2FA) to a later point in time". It arrived in **npm
11.16.0**. The pin here said Node 26.1.0, which carries npm **11.13.0**, and
the comment beside it said "npm 11 is the first with `npm stage publish`" —
right about the major, wrong about the minor, and nothing ever compared the
two figures.

| | |
| --- | --- |
| First npm with `npm stage` | 11.16.0 |
| npm that Node 26.1.0 carries | 11.13.0 |
| npm that Node 26.9.0 carries | 11.19.1 |

The pin is now Node 26.9.0, and `deps.NODE_NPM_VERSION` records the npm that
Node carries beside `deps.NPM_STAGE_MINIMUM`, which is the figure that matters.
`tools/tests/test_deps.py` compares them, so a Node bump that carries npm
backwards fails a gate rather than a release. That is the arithmetic test L172
asks for: the version comparison is decidable offline and needed no release to
find out.

**The order was right and it was not enough.** L184 put the tag last because a
tag cannot be taken back. That protected the tag from a failed *build*. It did
not protect anything from a failed *publisher*, because the publishers run
after the tag by design: a release page needs a tag to hang on. So a publisher
that could never work took the whole release with it, after four irreversible
steps.

**Every publisher is now probed before the first one runs.** Between packaging
and the image push: `crane version`, `gh auth status`, `npm stage list`, and
`npm stage publish --dry-run` on each tarball. `npm stage list` reads the
`/-/stage` endpoint, so one command proves the subcommand exists and the token
authenticates. The dry run then does everything a staged publish does except
upload — it reports "Staging to … (dry-run)" and "(staged)" — so a refused
tarball is found while nothing is public. Both were run by hand against the
real 0.2.0 tarball before this was written.

**What the first claim here got wrong.** The first reading of this failure
recorded that `npm stage publish` was not a command at all, on the evidence of
`npm help` in 11.13.0 and a changelog that names staged publishing without
naming a CLI. That was one version of npm away from the answer. The lesson is
narrower and more useful than "the feature does not exist": **a missing
subcommand is a version question, and the version to check is the tool's, not
the language runtime's.** Node 26 was new. Its npm was not new enough.

**0.2.0 stays 0.2.0**, and it is now complete: the image, six tags, the release
page, both charts, and `@catalystcommunity/tallyowl-browser@0.2.0` on npmjs,
byte-identical to the tarball this repository packs (`220d1c4d…`).

**Staging cannot create a package.** Staging 0.2.0 by hand was refused:

```
POST /-/stage/package/@catalystcommunity%2ftallyowl-browser
404 — Package "@catalystcommunity/tallyowl-browser" not found
```

`npm stage publish` defers the 2FA on a new *version*. The package itself must
already exist, so the **first** publish of each npm package is a person running
`npm publish` once, with 2FA, and every release after that stages. The owner
published 0.2.0 that way.

The release job now refuses this rather than discovering it at the end: the
preflight reads each packed tarball's own `package/package.json` for the real
scoped name — `npm pack` flattens the scope into the file name and it cannot be
read back — asks npm for each, and names the `npm publish` command a maintainer
must run. Nothing is published and nothing is tagged when it refuses.

**A negative answer from the public registry is not proof.** Checking
`https://registry.npmjs.org/@scope%2fname` before the publish cached a 404 on
npm's public CDN, and afterwards the anonymous check kept serving that cached
404 while the package was there and public. An authenticated read bypasses the
cache and answered correctly. Verify a publish with the token, not anonymously,
and do not probe the public URL beforehand.

**Resource requests, while the job was open.** Nothing here asked for CPU or
memory, so every job took the cluster default. The three that compile —
`test-rust`, `package`, `release` — now request 4 cores and limit at 8, with
12 GiB; `validate` takes 2 and 4; the quick gates take 1 and 4. The requests
stay small on purpose: eight gates run at the same time, and a request the
cluster cannot satisfy is a job that does not start at all.

**Cost to change:** cheap.
**Revisit:** no.
