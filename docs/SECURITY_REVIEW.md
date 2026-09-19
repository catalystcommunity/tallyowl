# Security review, Phase 11

This document records the Phase 11 security review. It reviews the system
against [THREAT_MODEL.md](THREAT_MODEL.md), reviews the surfaces Phase 11
added, and records the dependency audit and its open findings.

Date: 2026-08-09. Reviewer: the Phase 11 implementation session.

## 1. What this review covered

- the trust boundaries in THREAT_MODEL.md section 3, against the code that
  holds each one;
- the surfaces Phase 11 added: the consensus proposal forward, the soak and
  drill tooling, the Helm templates, and the Reactorcide workflows;
- the dependency audit: advisories, licenses, and sources;
- the malformed-frame tests and the seeded fuzz at the framing boundary.

## 2. Boundary review

**Boundary: the network edge of every listener.** A frame is read behind a
length guard, and the guard refuses an oversized length before it allocates.
`crates/tallyowl-rpc/tests/malformed_frames.rs` proves four properties: an
oversized length prefix does not allocate, a truncated frame does not hang the
reader, garbage inside a well-formed frame does not stop the listener, and
1,000 seeded mutations of a plausible frame leave the server answering. The
seed is recorded in the test.

**Boundary: tenancy.** Tenancy still comes from the connection credential and
never from a payload. The soak and the drill provision their keys through the
same `provision` verb an operator uses. No new code path accepts tenancy from
data.

**Boundary: node to node.** Phase 11 added one operation to this boundary: a
`proposal` kind on `deliver-consensus`, which lets a voter hand a client write
to the leader. The authorization is membership, which the same mutual TLS
connection already proves for every other consensus message. A node that can
send `append-entries` can already put entries into the log, so the proposal
kind grants no capability that the boundary did not already grant. A proposal
travels at most one hop, so two nodes that disagree about the leader produce a
refusal rather than a loop.

**Boundary: the operator surface.** The recovery verbs still require the data
directory, so they run only where an operator can already read every byte. The
drill and soak tooling write keys to files with mode 600, and the tooling
prints a secret only where the operator asked for it (`provision` prints the
key once, which is the existing design).

**Secrets in deployment.** The Helm charts carry `collector.apiKey` as a
`file:` or `env:` reference and never a value, so a rendered manifest holds no
secret. The chart validations refuse configurations TallyOwl would refuse. The
Reactorcide publish job is tag-triggered, carries `disable_run_local: true`,
and holds its secret as a `${secret:...}` reference. Pipeline code prints
commands through a helper that masks values marked sensitive.

## 3. Dependency audit

`./tools.sh audit` runs `cargo audit` against the RustSec database and
`cargo deny` for licenses, duplicate versions, and sources. Both pass. The
ignore list is `.cargo/audit.toml`, and each entry is a finding here:

1. **RUSTSEC-2026-0119, `hickory-proto` 0.24: CPU exhaustion during DNS
   message encoding.** It reaches this build through the pinned
   `linkkeys-local-rp` revision. The newest linkkeys revision still requires
   hickory-resolver 0.24, so no pin bump in this repository can fix it. The
   fix is a linkkeys upgrade to hickory 0.26. **Owner action: upgrade hickory
   in linkkeys and bump the pin here.** Exposure is bounded: the resolver runs
   only when LinkKeys sign-in is enabled, and it is off by default.
2. **RUSTSEC-2026-0235, `rkyv` 0.7: out-of-bounds reads validating hostile
   archives.** It is in the lockfile through an optional `rust_decimal`
   feature that no TallyOwl build enables. `cargo tree -i rkyv` prints
   nothing, so no TallyOwl binary contains the code.

Two unmaintained-crate warnings (`paste`, `rustls-pemfile`) are transitive
and carry no advisory. Watch them at the next dependency bump.

Licenses: every dependency is compatible with Apache-2.0. First-party crates
whose metadata carries no license field are clarified in `deny.toml`, with the
reason beside each. Sources: only crates.io and the three pinned
catalystcommunity repositories are permitted.

## 4. Fuzzing

The fuzzer is a seeded mutation loop in
`crates/tallyowl-rpc/tests/malformed_frames.rs`, run as an ordinary test. It
is not coverage-guided: this machine has no nightly toolchain, which a
libFuzzer build requires. A coverage-guided fuzzer over the CBOR decode paths
is the natural next step when a nightly toolchain is available, and the seeded
loop stays in the suite either way, because it runs on every `cargo test` and
a coverage-guided fuzzer does not.

## 5. Findings, ranked

1. The hickory advisory above. Owner action, in linkkeys.
2. No rate limit protects the enrollment surface beyond the role token's own
   rate control. Accepted in THREAT_MODEL.md section 8 and unchanged.
3. The soak and drill tooling stores operator session tokens in
   `data/*/operator.session` with mode 600. They authorize queries against
   the local installation only. Acceptable for a testbed; a production
   operator uses LinkKeys sign-in.

## 6. Review triggers

THREAT_MODEL.md section 9 lists what must reopen that document. Phase 11
moved no trust boundary: the proposal kind lives inside the node-to-node
boundary that already carried consensus. No credential gained a scope. No
component newly reaches storage or the query path. No compatibility edge
listens by default.
