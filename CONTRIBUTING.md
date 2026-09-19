# Contributing to TallyOwl

## What you need

`uv` and a Rust toolchain. Nothing else for any verb that does not need a
cluster.

Corndogs must be reachable for `dev up`. A `corndogs` binary on the path is
used when one exists; otherwise a checkout beside this repository is run from
source, which is what most people here have.

## Start

```sh
./tools.sh setup      # generate from csil/, build, and write a local configuration
./tools.sh dev up     # start Corndogs, the head, and the collector
```

Then send an event and read it back:

```sh
cargo run -p tallyowl-driver-rust --example send_one_event
```

The dashboard is at <http://127.0.0.1:5120>. It serves its own document, the
sign-in callback route, and the browser carrier the dashboard talks to the head
over. `./tools.sh build` builds its bundle; without that the page loads and says
so rather than failing silently.

Stop with `./tools.sh dev down`, which leaves the data directory, or
`./tools.sh dev reset`, which removes it.

### The two credentials `dev up` makes

TallyOwl issues credentials, so the loop asks for them rather than inventing
one. Before it starts the head, `dev up` runs the same two commands an operator
runs, and writes what they print into the data directory:

| File | What it is | Made by |
| --- | --- | --- |
| `data/collector.key` | A source key. It authenticates an **application**, and `collector.apiKey` points at it | `tallyowl-head provision local` |
| `data/operator.session` | A session. It authenticates a **person**, and a query needs one | `tallyowl-head session create operator` |

**Neither is a substitute for the other.** A source key presented to a query is
refused, and a session presented to intake reaches no project. Each is printed
once and stored as a digest, so a lost one is replaced rather than recovered:
delete the file and run `dev up` again, or run the command yourself.

Both commands need the data directory to themselves, so stop the head first if
it is running. `./tools.sh dev reset` removes the data directory and the next
`dev up` makes a fresh pair.

LinkKeys owns human authentication and is off until an operator names a trusted
domain in `linkkeys.trustedDomains`. `session create` is what an installation
with no LinkKeys domain uses, and it is also how the first person gets a
membership at one that has a domain.

## The verbs

`tools.sh` is the front door and holds no logic. Every verb calls a Python
module under `tools/`, and the Reactorcide jobs call the same modules, so a
local run and a CI run cannot drift.

| Command | Does |
| --- | --- |
| `./tools.sh setup` | Generate from `csil/`, build, and write a local configuration file |
| `./tools.sh gen` | Generate the clients from `csil/` |
| `./tools.sh gen-check` | Generate into a temporary directory and fail on drift |
| `./tools.sh csil-validate` | Check every specification |
| `./tools.sh deps` | Fetch the pinned dependencies a package manager cannot |
| `./tools.sh golden` | Rewrite `golden/vectors.json` from the Rust side |
| `./tools.sh build` | Build every service |
| `./tools.sh test` | Run every test, in every maintained language |
| `./tools.sh test-rust`, `test-go`, `test-ts` | Run one language's tests |
| `./tools.sh test-tools` | Run the tooling's own tests |
| `./tools.sh fmt` | Format the Rust code |
| `./tools.sh lint` | Check formatting and run the linter |
| `./tools.sh check` | Validate, format, lint, and test. Run this before you push |
| `./tools.sh helm-check` | Lint the charts, render every profile, and prove every refusal refuses |
| `./tools.sh audit` | Check dependencies: advisories, licenses, and sources |
| `./tools.sh dev up` | Start the home profile |
| `./tools.sh dev up --without head` | Start the rest, so a debugger owns the head |
| `./tools.sh dev logs` | Follow every log |
| `./tools.sh config check` | Resolve the configuration and say where each value came from |
| `./tools.sh drill dr` | The disaster-recovery drill: back up, destroy, restore, rebuild, and measure |
| `./tools.sh drill overload` | Offer more than the path commits, and prove the bounds hold |
| `./tools.sh soak up` | Start the cross-cluster soak: three head voters, two collectors, load, and a monitor |
| `./tools.sh soak status`, `soak report`, `soak roll`, `soak down` | Watch, summarize, roll, or stop the soak |
| `./tools.sh commits check` | Fail when a commit does not say what kind of change it is, and say what each one does to the version |
| `./tools.sh version` | Say what version this working tree is |
| `./tools.sh version set <version>` | Write one version over every version site and regenerate the clients |
| `./tools.sh version check` | Fail when two version sites disagree |
| `./tools.sh release plan` | Say what the next release would be, from the conventional commits since the last tag |
| `./tools.sh release stamp` | Bring the tree up to date and write the next version into it. Commits nothing; on a workstation it prints the plan and stops |
| `./tools.sh release tag` | Commit the stamped version, tag it, and push. The last step of a release, after every artifact exists |
| `./tools.sh release package` | Build both charts, the browser package, and the service image |
| `./tools.sh release image` | Build the service image only |
| `./tools.sh release crates-plan` | Say which crate names a crates.io publication takes, in push order |
| `./tools.sh release check-tag <tag>` | Fail when a release tag is not this version |

`tools.sh` is the development loop and an operator does not have it. The
recovery and control commands are therefore in the head itself, and each one
needs the data directory to itself, so stop the head first:

| Command | Does |
| --- | --- |
| `tallyowl-head snapshot <directory>` | Copy this installation into `<directory>`, adding only what is new |
| `tallyowl-head restore <directory>` | Restore a snapshot into an empty data directory |
| `tallyowl-head rebuild` | Rebuild the list of stored files by reading them, and say what did not come back |
| `tallyowl-head provision <project>` | Create the project if it is missing, and print one new source key |
| `tallyowl-head project list` | List the workspaces and projects |
| `tallyowl-head key list` | List the source keys, their project, and their state |
| `tallyowl-head key revoke <key-id>` | Stop one key working. Every other key for that source keeps working |
| `tallyowl-head session create <name>` | Sign somebody in and print one session token |
| `tallyowl-head session list` | List the sessions and their state |
| `tallyowl-head session revoke <id>` | End one session |

A key and a session are each printed once. TallyOwl stores a digest and cannot
print either again, so a lost one is replaced rather than recovered.

## The development loop is the home profile

There is no development mode, and there is not going to be one. You run one
head, one collector, and one Corndogs, which is the smallest supported
production deployment. No `if development` branch, no mock, and no in-process
shortcut between the collector and the head.

A setting you change locally is the same setting an operator changes in a chart,
under the same name. A bug that appears in a home installation appears on your
workstation.

Supervision lives in `tools.sh`, never in a service. A service that knew how to
start its siblings would have a development code path.

### Debugging one service

```sh
./tools.sh dev up --without head
```

Then run `tallyowl-head` in your debugger. No container indirection, and no
attach dance.

## Configuration

One name for one setting, in every place it appears:

| Place | Form |
| --- | --- |
| Helm value | `storage.receiptPolicy` |
| Configuration file | `storage: { receiptPolicy: ... }` |
| Environment variable | `TALLYOWL_STORAGE__RECEIPT_POLICY` |
| Command-line flag | `--storage.receipt-policy` |

A command-line argument wins, then an environment variable, then the file, then
the built-in default. `./tools.sh config check` shows which source won for every
value.

**Adding a setting touches four places, and a test enforces it:**

1. `crates/tallyowl-config/src/schema.rs`;
2. `tallyowl.example.yaml`;
3. `charts/tallyowl/values.yaml`;
4. `charts/tallyowl-collector/values.yaml`.

The chart parity test fails when a setting reaches only some of them. That test
is what keeps the development loop matching a deployment.

**A secret is a reference, never a value.** Write `file:/path/to/the/secret` or
`env:NAME`. The loader refuses a literal, because a configuration file gets
copied, pasted, and committed.

## The contract

`csil/` is the source of truth for every public type and service interface.

**Never edit generated code.** Change the `.csil` source and run `./tools.sh
gen`. CI generates into a temporary directory and compares, so drift fails the
build.

csilgen runs from inside `csil/`, because an `include` path resolves against the
working directory. The tooling does that for you; do not run csilgen by hand
from the repository root.

If csilgen appears to lack something you need, read section 10.1 of
`IMPLEMENTATION_PROMPT.md` first. A dozen projects use csilgen without a change
to it, so start from the position that the specification has the wrong shape.

## Tests

- **Cover branches, not the happy path.** A test that only proves the good case
  proves very little here.
- **Do not mock the storage interface.** TallyOwl owns it, so a mock would only
  assert that the mock behaves like the mock. Use a real temporary directory.
- A test double for *another product's* boundary is fine. `FakeQueue` stands in
  for Corndogs so a failure test can refuse, stall, or lose an acknowledgement.
- **Turn every regression into a permanent test.** A defect then cannot come
  back quietly.
- Add failure-path and compatibility tests with every protocol or storage
  change.

## Language

Every message that reaches a person must be understandable by somebody who does
not operate TallyOwl. That covers error messages and codes, health states,
dashboard labels, and notification text.

Read a new message aloud before you merge it. If a listener asks what a word
means, the message fails.

Internal storage terms such as tablet, virtual shard, and locator run stay
technical, because no such reader sees them.

Documentation uses ASD-STE100 Simplified Technical English. See
`docs/DOCUMENTATION.md`.

## What not to do

- Do not name a comparison product anywhere, including in a comment.
- Do not add a generic HTTP ingest API.
- Do not open a compatibility receiver port by default.
- Do not claim exactly-once delivery. The contract is durable at-least-once
  delivery with stable IDs and logically idempotent ingestion.
- Do not build product code inside `prototypes/`. That workspace holds
  decision-support benchmarks, and a product build never compiles it.
- Do not put build, test, or deploy logic in shell. It goes in Python under
  `tools/`.

## Before you push

```sh
./tools.sh check
```

Staging, committing, and pushing are yours. Nothing in this repository does them
for you.
