# CSIL specifications

These files are the source of truth for every TallyOwl public type and service
interface. Never edit generated output. Change a `.csil` file and generate
again.

## Files

| File | Contract |
| --- | --- |
| `types/common.csil` | Shared types. No service. Every entry specification includes it. |
| `tallyowl-ingest.csil` | Telemetry envelopes and capture operations. An application includes this file. |
| `tallyowl-collector.csil` | Durable intake, batch transfer, receipts, policy, and health. |
| `tallyowl-control.csil` | The query algebra, alerts, and administration. |
| `tallyowl-cluster.csil` | Replicated storage: consensus transport, snapshot and segment transfer, distributed query, node health, and topology. Node to node and operator only; no application reaches it. |

## Wire IDs

Each service holds one wire ID. Each operation holds one wire ID inside its
service. Assign a wire ID one time and never use it again for something else.

| Service | Wire ID |
| --- | --- |
| `TallyOwlIngest` | 1 |
| `TallyOwlCollector` | 2 |
| `TallyOwlControl` | 3 |
| `TallyOwlReplication` | 4 |
| `TallyOwlCluster` | 5 |

csilgen needs a wire ID on every operation of a service or on none of them. A
partial set is a hard error.

## Generation

Run csilgen from this directory. An `include` path resolves against the working
directory, not against the file that holds the statement.

```
cd csil
csilgen generate --input tallyowl-ingest.csil --target rust --output ../generated/rust/tallyowl-ingest
```

Generation from the parent directory fails while an `include` is present. The
repository task runner therefore changes to this directory first.

## Language constraints found by validation

The parser reserves several words. A field cannot use one as its name.

| Reserved | Use instead |
| --- | --- |
| `service` | `service_name` |
| `from` | `range_start` |
| `to` | `range_end` |

An `include` statement must come before the `options` block. The parser refuses
an `include` that follows `options`.

Validate every file after a change:

```
cd csil
csilgen validate --input types/common.csil
csilgen validate --input tallyowl-ingest.csil
csilgen validate --input tallyowl-collector.csil
csilgen validate --input tallyowl-control.csil
csilgen validate --input tallyowl-cluster.csil
```

## Package emission

Each entry specification emits Rust, Go, and TypeScript. csilgen can emit more
languages. A generated client is free. A maintained app driver is not, and the
project maintains three. See the client library rollout in
[../docs/PLAN.md](../docs/PLAN.md).
