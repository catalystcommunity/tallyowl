# CI/CD design

## 1. Constraints

- Reactorcide is the only CI/CD orchestrator for this repository.
- Workflow behavior uses runnerlib.
- Bash scripts are not part of the build, test, generation, packaging, or
  deployment interface.
- Local and remote CI call the same Python entry points.
- Untrusted source never controls trusted secret-bearing pipeline code.
- Use Reactorcide secret references and masking. Do not write a secret value to
  a repository file, artifact, command line, or log.
- CI does not stage, commit, or push source changes, **except the release job**.

The exception is the owner's decision of 2026-08-12, and it is narrow. The
release job writes one version into the tree, commits that, tags it, and
pushes. It changes nothing else, and no other job may write to the source at
all. The reason is the one the owner gave: if CI does not do it, a person does,
and a person writing one version into nineteen files gets one of them wrong
eventually. See L178.

## 2. One entry point

A person types `tools.sh`. Reactorcide calls the same Python modules. Local and
CI therefore run identical code.

```text
tools.sh <verb>            thin dispatcher, no logic
   └─ uv run python -m tallyowl_tools.<verb>
         └─ runnerlib event lifecycle
               └─ cargo | go | npm | csilgen  (argument arrays, shell=False)
```

`tools.sh` holds no logic. It exists because a person should not have to
remember a Python module path, and because every other repository here has one.

Verbs cover build, test, generate, lint, and the reference application.

Two rules that this entry point exists to enforce:

- csilgen runs from the `csil/` directory, because an `include` resolves
  against the working directory. That rule lives in code that both paths call,
  not in prose that a person has to remember. See [../csil/README.md](../csil/README.md).
- the tooling fetches a pinned csilgen **release** for this machine's
  architecture, and pins the TypeScript transport by its release tag. The two
  are different pins because they name different things: the generator is a
  binary, and the transport is a checkout that `git` has to resolve a ref for.

  The pinned binary comes before anything on a developer's path, because a
  generator somebody built by hand is a generator nobody else has, and the
  generated code is checked in. csilgen's own `--version` reports 0.1.0 for
  every 0.2.x release, so the version it prints cannot be the check.
  `gen-check` is the check: it generates into a temporary directory and fails
  on any difference from what is checked in.

`uv` provisions the Python version. A developer needs `uv` and nothing else to
run the verbs that need no cluster.

## 3. Layout

This section first proposed a `pipelines/` directory of standalone Python
modules. Reactorcide's shipped, trust-safe mechanism is different, and this
repository uses the shipped one: a **lifecycle plugin** in
`.reactorcide/plugins/`, loaded from the trusted CI source, selected by an
environment variable. A plain module command in a job file would be loaded
from the application source and lose the trusted-plugin precedence rule. The
correction is one more instance of the L146 lesson: a dependency's capability
is a thing to read, not to remember.

```text
.reactorcide/
  jobs/            one YAML for each job: image, command, timeout, environment
    validate.yaml
    gen-check.yaml
    test-rust.yaml
    test-go.yaml
    test-ts.yaml
    helm-check.yaml
    audit.yaml
    package.yaml
    release.yaml
  workflows/       the graphs: which jobs, on which events, in which order
    pr.yaml
    main.yaml
    release.yaml
  plugins/
    plugin_tallyowl_jobs.py    the trusted dispatch: one function for each job
```

Every job's command is `runnerlib run --job-command true`, and
`REACTORCIDE_TALLYOWL_JOB` selects the work. Each job function calls the same
`tallyowl_tools` module that `tools.sh` calls, so local and CI cannot drift.

Two facts the proposal had wrong, recorded so nobody restores it from memory:

- job YAML cannot set resource limits. CPU and memory ceilings come from the
  organization's execution profile, not from the repository;
- workflow variables, outputs, change detection, and `for_each` come from
  runnerlib's `src.workflow` module inside a plugin, not from standalone
  pipeline scripts.

## 4. Workflow graph

```text
ingest
├── format-lint ───────────────┐
├── unit-rust ─────────────────┤
├── unit-go ───────────────────┤
├── unit-typescript ───────────┤
├── csil-generate-and-diff ────┤
├── csil-cross-language ───────┤
├── helm-render ───────────────┤
└── docs-check ────────────────┤
                               ▼
                         integration
                    ┌──────────┼──────────┐
                    ▼          ▼          ▼
                embedded   replicated  corndogs
                 store       store
                    └──────────┼──────────┘
                               ▼
                           testbed
                    ┌──────────┴──────────┐
                    ▼                     ▼
              fast scenario         full scenario
              (pull request)        (main, scheduled)
                    └──────────┬──────────┘
                               ▼
                           package
                               ▼
                    release (merge to main)
```

The repository trusts the ingest job pipeline code. The job examines trigger type and changed
paths, then emits the relevant nodes. Pull requests run validation and bounded
integration work without publish and deploy secrets. A merge to main runs the
same gates and then the release job, which is the only job that holds a publish
grant and the only job that writes to the source.

## 5. Validation jobs

### Conventional commits

Every commit a pull request adds must say what kind of change it is, because
`semver-tags` computes the version from exactly that. A subject that matches
nothing is not a style complaint: it is a release that does not happen, for a
reason nobody sees.

`./tools.sh commits check` reads the commits the branch adds and prints what
each one would do to the version — major, minor, patch, or nothing. The types
it accepts are semver-tags' own defaults, and a test in
`tools/tests/test_commits.py` reads them out of the tool rather than copying
them, so the gate and the calculator cannot drift. `norelease:` is the one
addition: a change the author decided should move no version.

### Format and lint

- Rust formatting and linting;
- Go formatting, vetting, and tests for the maintained app driver;
- TypeScript formatting, typecheck, and lint;
- Python pipeline formatting, lint, and typecheck;
- Markdown links and repository language rules;
- forbidden generated-file modifications outside regeneration output.

There is no automated ASD-STE100 checker. Writers apply the rules as they
write, and the project owner reviews the documents periodically. The project can add a
checker later to make the work easier, but no job depends on one. See
[DOCUMENTATION.md](DOCUMENTATION.md).

### CSIL generation

1. Generate into a temporary artifact workspace.
2. Compare with checked-in generated output.
3. Fail with a concise regeneration instruction on drift.
4. Run CSIL validation and fixed wire-ID checks.
5. Run golden CBOR vectors across maintained targets.

The job never modifies the source checkout and never stages regenerated files.

### Unit and integration tests

- Unit jobs split by language and package.
- Embedded-storage tests use real temporary data directories and abrupt process
  termination rather than mocked durability behavior.
- Replication tests run real multi-node TallyOwl storage processes with network
  and process failure injection.
- Delivery tests run a real Corndogs service with the selected durable backend.
- Protocol tests run collector and head processes over TLS over TCP.
- Failure injection gets a separate resource-heavier job.
- Helm rendering and policy checks need no cluster.
- Chart installation and upgrade tests run only in an isolated disposable cluster.

### Reference application test bed

The `testbed` job runs the reference application against real TallyOwl
processes. See [TESTBED.md](TESTBED.md).

- A pull request runs the fast scenario with the synthetic browser path.
- A main-branch build runs the full scenario with a real headless browser.
- A scheduled build runs the long virtual-clock scenario and the failure cases.
- Every run records its seed. A failure report gives the reproduction command.
- The job compares query results with the simulator ledger. A mismatch fails
  the build.
- The job needs no publish or deploy secret.
- The job runs against real processes, never mocks.

The same job must run on one developer machine with one command.

## 6. Packaging and release

Package jobs produce immutable artifacts identified by the source commit:

- one container image that holds the head and the collector. Each chart selects
  its binary with `command`;
- TallyOwl and collector Helm charts;
- the TypeScript browser package, built before it is packed. The package job
  refuses a tarball that holds a manifest and no entry point, because `npm
  pack` produces exactly that when nobody built first;
- one archive of the two service binaries, taken **out of the image that was
  just built**, with the license and a `SHA256SUMS` beside it. A downloaded
  TallyOwl is therefore the same build as a deployed one;
- software bill of materials, checksums, and provenance metadata.

The generated CSIL client packages are **not** published. An application
includes the contract in its own CSIL build and generates its own client. See
`docs/RELEASE_NOTES.md` and L177.

`./tools.sh release package` builds all of them into `RC_ARTIFACT_DIR`, and the
release job calls that verb. A person can therefore build the same artifacts
and look inside them before a release runs. The publishing steps push what that
step built and build nothing again: crane sends the saved image archive, and
the chart and package tarballs are the files themselves.

**One version everywhere.** `./tools.sh version set <version>` writes the
version into the Rust workspace and its path dependencies, the CSIL
specifications, both charts, the package manifests, the two app-driver
constants, and the Go module requirements, then regenerates the clients and
writes the new versions into `Cargo.lock`. `./tools.sh version check` fails the
build on any disagreement, and the `validate` job runs it first: a chart that
asks for an image tag nobody built is an installation that never starts, and a
stale lock file is a `--locked` image build that cannot run at all.

**The version is computed, not typed.** `semver-tags` reads the conventional
commits since the last tag and says what the next version is, exactly as every
other repository here releases. `./tools.sh release plan` prints what the next
release would be and changes nothing. `./tools.sh release stamp` brings the
tree up to date with main and writes that version into it, and `./tools.sh
release tag` commits, tags, and pushes. Both refuse unless `TALLYOWL_RELEASE=1`
says it is the release job, so running either on a workstation prints the plan
and stops.

**The order is the whole design.** A git tag cannot be taken back — the Go
module proxy caches it — so the release job stamps, builds and verifies every
artifact, pushes the image, and only then commits and tags. A build that fails
leaves the repository exactly as it was. If main moved while the artifacts were
building, the push is refused and so is the release: the version was computed
from a commit that is no longer the head, and running again computes it from
what main holds now. The steps after the tag — the release page, the charts
repository, the staged package — are each repeatable, so a re-run finishes a
release rather than starting a broken second one.

**Every publisher is probed before the first one runs.** After the artifacts
exist and before the image is pushed, the job asks each publisher to prove
itself: `crane version`, `gh auth status`, `npm stage list`, and `npm stage
publish --dry-run` on each tarball. `npm stage list` reads the staging
endpoint, so it proves the subcommand and the token together; the dry run does
everything a staged publish does except upload. A subcommand this npm does not
have, a tarball that will be refused, or a credential that has expired is
found while nothing is public.

This step is not decoration. Release 0.2.0 pushed the image, cut six tags, made
the release page and committed the charts, and then failed on the fifth
publisher. None of those four steps could be taken back. See L190.

### Where each artifact goes

D31, decided 2026-08-12:

| Artifact | Destination | Grant |
| --- | --- | --- |
| Container image | `containers.catalystsquad.com/public/catalystcommunity/tallyowl` | `${secret:catalystcommunity/registry:user}` and `:password` |
| Helm charts | The `catalystcommunity/charts` repository, and the GitHub release | `${secret:catalystcommunity/ci:githubpat}` |
| Browser package | npmjs, **staged**. A maintainer approves it with 2FA | `${secret:catalystcommunity/ci:npmpublish}` |
| Go modules | The git tags themselves. The module proxy needs no push | none |
| Binaries | The GitHub release page, beside the charts | `${secret:catalystcommunity/ci:githubpat}` |
| Rust driver | **Nowhere yet.** crates.io is off by the owner's decision of 2026-08-12 until the release pages have proved themselves. A Rust application depends by Git revision | none needed |

Every grant a release needs now exists: the registry pair, the git token, and
`npmpublish`. Turning crates.io on later needs the dependency clearance and one
more grant; nothing else is outstanding.

**Why the npm publish is staged.** npm deprecated the granular token that
bypasses 2FA in August 2026 and removes its publish capability in January 2027.
A token that can publish outright is a token this project would rather not
hold. The release job runs `npm stage publish`; the package waits in staging
until a maintainer approves it with 2FA on npmjs.com or with `npm stage approve
<stage-id>`. `npm stage list` says what is waiting. Trusted publishing with
OIDC is npm's other path, and `npm trust` federates with GitHub Actions, GitLab
CI, and CircleCI, not with Reactorcide, so staging is the path that fits.
See L180.

**`npm stage` needs npm 11.16 or newer.** It is not in npm 11.0, and the major
on its own does not settle it. `deps.NODE_VERSION` therefore pins a Node that
carries a new enough npm, and `deps.NODE_NPM_VERSION` records which npm that
is, so the two can be compared. `tools/tests/test_deps.py` compares them and
fails the build when a Node bump carries npm backwards.

Release 0.2.0 is why. It ran `npm stage publish` against the npm 11.13.0 that
Node 26.1.0 carries, and npm answered `Unknown command: "stage"` after four
publishers had already made the release public. See L190.

**The Go tags.** A Go module in a subdirectory resolves through a tag that is
the module directory and then the version. A release therefore creates one
repository tag and one tag for each published Go module, all in one atomic
push:

```text
v0.2.0
packages/driver-go/v0.2.0
generated/go/tallyowl-ingest-api/v0.2.0
generated/go/tallyowl-collector-api/v0.2.0
generated/go/tallyowl-control-api/v0.2.0
generated/go/tallyowl-cluster-api/v0.2.0
```

**What the job image must have.** The Reactorcide runner image carries podman,
Git, uv, Go, and Rust. It carries no Helm, no Node, no crane, no `gh`, and no
Docker client, which was read out of the image rather than assumed.
`./tools.sh deps` fetches each of them into `.deps/` at the versions
`tools/tallyowl_tools/deps.py` pins, so a release job and a workstation use the
same ones. The release job declares Reactorcide's `docker` capability, which
gives it the daemon the image build needs; the push needs no daemon at all.

The release job refuses, and names each missing grant, before it changes
anything.

**The first merge to main publishes**, by the owner's decision of 2026-08-13.
There is no rehearsal release. Every job was run in the runner image with
`reactorcide run-local` instead, which is what found the four defects L185
records; the timings are there too, and `test-rust` from an empty target
directory is 121 seconds against a 3600-second limit.

If the project needs deployment, add an explicit protected node. Packaging must
not cause deployment.

## 7. Local execution

Every non-publishing job works through Reactorcide's canonical local runner:

```sh
reactorcide run-local --job-dir . .reactorcide/jobs/validate.yaml
```

The `publish` job carries `disable_run_local: true` and refuses this path,
because a secret-bearing job must not resolve its references on a developer
machine. Local execution should:

- bind-mount or copy the working tree without changing ownership unexpectedly;
- run with the same container image and user as deployed workers where useful;
- require no secrets for format, lint, generation, unit, or normal integration
  tests;
- place generated comparisons and test artifacts outside tracked source paths.

There is no parallel collection of ad hoc shell wrappers. Developers invoke the
same Python pipeline modules directly for a narrow task or run their Reactorcide
job locally.

## 8. Trust and secrets

- For outside contributions, trusted CI source and untrusted application source
  remain separate using Reactorcide's dual-source model.
- Untrusted jobs receive no release, registry, cluster, or production
  credentials.
- Secret references remain `${secret:path:key}` in job definitions.
- Pipeline code never prints the environment or resolved secret values.
- Scope each publish or deploy grant by project, job, secret path, event, and
  protected ref.
- Inspect artifact contents before promotion. This prevents test output from
  putting secrets in a release.

## 9. Initial Reactorcide milestones

1. Add `ingest`, `validate`, and CSIL drift jobs in Phase 2.
2. Add real service integration jobs with the durable event slice.
3. Add the `testbed` job with the first reference-application scenario.
4. Add package jobs when the first container, chart, and client artifact is usable.
5. Add tag publishing only after the project approves all release decisions.
6. Add deployment workflows only for a concrete environment with an explicit
   promotion and rollback policy.
