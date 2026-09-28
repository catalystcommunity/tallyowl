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
connection proves for every other consensus message. (Until D62 no service
used TLS, so this sentence was a design statement and not a fact. Section 7
gives what D62 changed.) A node that can
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

## 7. Transport security (D62)

D62 moved each trust boundary that crosses a network. THREAT_MODEL.md section 3
gives the new table. These are the changes that a security review must know.

**What is now true.**

- Every CSIL listener on a network address uses TLS. Collector intake and the
  OpenTelemetry receiver show a certificate from operator files. `head.listen`
  and `replication.listen` use mutual TLS with node certificates.
- `renew-node-certificate` accepts only a verified peer whose node ID is the
  node that it renews, and whose certificate serial is the current one.
  Before, any client that knew a node ID got a certificate for it. The setting
  `enrollment.allowUnverifiedRenewal` is removed.
- `commit-batch` accepts a verified collector, and only for the projects in
  the scope of the role token that enrolled it (D32). A peer with no
  certificate is refused.
- A consensus message is refused when its sender is not the node that the
  peer certificate names.
- `head.listen` accepts a client with no certificate, so that a collector can
  enroll and an operator's client can reach the control operations. From that
  client the head refuses `commit-batch`, `resolve-key`, `fetch-policy`, and
  `renew-node-certificate`. `resolve-key` is closed to it because it answers
  which project a key belongs to, which lets a caller test keys.
- The catalog holds no authority key. The operator supplies an intermediate to
  each head, and a head refuses one that does not chain to a trusted root.
- The Helm charts refuse a render with no transport security, and
  `helm-check` runs the service's own `config check` on each rendered profile.

**What is still open.**

1. The Corndogs connection has TLS from Corndogs release 0.7.6, and the
   durable store does not identify its callers: it has no client certificate
   check. Any process that reaches the queue port can submit or claim tasks.
   A NetworkPolicy is the control for that.
2. The OpenTelemetry receiver has no authentication. TLS protects the data in
   transit only.
3. A revoked node works until its certificate expires, 24 hours by default.
   There is no revocation list.
4. Each head holds the intermediate key, so one compromised head can sign a
   node certificate. D62 accepts this cost.
5. A server does not log a refused TLS handshake, because a stranger could
   write a line for each connection. It counts each one in
   `tallyowl_tls_handshakes_refused_total`, with the listener as a label.

**Closed on 2026-09-26.** The native alert callback had no authentication. The
head now signs each callback the way it signs a webhook: a keyed BLAKE3 hash,
with the secret of the target, over the time and the body. The Go and the
Rust app drivers verify it (`VerifyAlertCallback`, `verify_alert_callback`),
and refuse a time that is more than five minutes from the receiver's clock. A
rule with a callback target and no secret is refused when it is written. TLS
proves the receiver to the head, and the signature proves the head to the
receiver.

