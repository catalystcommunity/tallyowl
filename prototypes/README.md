# Prototypes

This directory holds decision-support code. It is not product code, and no
service depends on it.

**The prototypes hold their own Cargo workspace.** Keep them out of the product
workspace. A product build must never compile a benchmark, because the
development loop in PLAN.md Phase 1 depends on a fast build. Add a prototype to
`prototypes/Cargo.toml`, never to the root workspace.

**Keep them.** They are the evidence behind every number in BENCHMARKS.md, and
that document refuses a claim without a measurement. They also earn their place
again whenever a dependency changes: a Corndogs release once reversed the D4
spool decision, and only a re-run found it.

Each prototype answers a question that a decision in
[../docs/DECISIONS.md](../docs/DECISIONS.md) leaves open. Results go in
[../docs/BENCHMARKS.md](../docs/BENCHMARKS.md).

| Prototype | Answers | State |
| --- | --- | --- |
| `page-bench` | D17 page encodings, compression, and sizing. D44 checksum cost. | Measured |
| `catalog-bench` | D3 embedded catalog engine | Measured |
| `index-bench` | D20 and D25 exact high-cardinality index, and the Tantivy comparison | Measured |
| `tier-bench` | D24 cold tiering, range reads, and the page cache | Measured |
| `delivery-bench` | D33 Corndogs throughput, sweep cost, and the D4 accept path | Measured |
| `consensus-bench` | D15 replication, on a real openraft cluster | Measured |
| `wal-bench` | Group commit, from STORAGE.md section 15 item 2 | Measured |
| `segment-bench` | D10 bytes for each event, from a whole segment | Measured |
| `locator-bench` | D20 tablet locator at 100 million users | Measured |

`consensus-bench` runs a real openraft cluster over an in-memory log and
transport. Election, partition, membership change, and snapshot transfer all
work. Election under disk latency, a non-isolation partition, and recovery from
a corrupt log still need a real storage and network implementation.

`segment-bench` writes a whole segment to real storage, reads it back, verifies
its checksums, and probes its index. It uses generated values, so it gives a
floor for its column set rather than a promise.

`locator-bench` builds real locator runs at full user cardinality. It measures
the structure and the probe. It does not measure the segment opens that follow
a probe, which is where the remaining cost sits.

## Rules

**Never benchmark storage on tmpfs.** On a normal Linux desktop `/tmp` is
memory. A storage benchmark there reports a number that the hardware cannot
reach. The first catalog run made this mistake and overstated durable commits
by 79 times. See BENCHMARKS.md section 2.

**Record the seed.** Every generator takes a seed and defaults to one. A result
that nobody can reproduce is not a result.

**State the filesystem.** A result without its storage medium is not
interpretable.

**Measure the thing that the design claims.** The design claims durable
acknowledgement, so the benchmark must call `fsync` and must say so.

**Distrust a negative result about your own design.** The first group-commit
run showed no gain. That was a bug in the prototype, not a fact about group
commit. A result that says a design idea fails needs the same scrutiny as one
that says it works.

**Never add component measurements together and call the sum an answer.** The
first capacity envelope added 21.0 bytes of column data to 20.3 of index and
1.3 of catalog receipt, and got 42.6. A whole segment costs 48.9. Each part was
measured correctly on its own column with its own distribution. Together, the
columns carry correlated higher-cardinality values and compress worse. Build the
combination and measure it.

**Re-measure after a dependency changes.** The `accept-path` mode compared a
collector payload spool against a payload in the Corndogs task. The spool won
at high load. Corndogs then moved payloads out of its B+tree, and the same
comparison reversed. TallyOwl dropped the spool. A measurement is true of the
system it ran against, not of the idea.

## Running

```
cd prototypes
cargo build --release
./target/release/page-bench [rows] [seed]
./target/release/catalog-bench [receipts] [directory]
./target/release/index-bench [rows] [seed]
./target/release/tier-bench [seconds]
./target/release/wal-bench [frame-bytes]
./target/release/consensus-bench
./target/release/segment-bench [rows] [seed]
./target/release/locator-bench [seed]
```

`catalog-bench`, `tier-bench`, and `segment-bench` write to
`~/.cache/tallyowl-bench` unless given a directory. `segment-bench` reads
`TALLYOWL_BENCH_DIR`. That directory must be on real storage, and
`segment-bench` refuses a memory-backed path.

`delivery-bench` is a Go module and needs a running Corndogs. It has three
modes: the default submit and sweep run, `MODE=sweep-curve` for sweep cost
against live task count, and `MODE=accept-path` for the D4 spool comparison.

```
STORAGE_BACKEND=file CORNDOGS_FILESTORE_DIR=<real-disk-dir> \
  CORNDOGS_FILESTORE_SYNC=group corndogs run &
cd prototypes/delivery-bench && go build -o /tmp/delivery-bench . && /tmp/delivery-bench
```
