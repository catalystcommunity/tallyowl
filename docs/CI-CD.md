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
- CI does not stage, commit, or push source changes.

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
- the tooling pins csilgen to a released version rather than a local build,
  once csilgen publishes releases through its own Reactorcide pipeline.

`uv` provisions the Python version. A developer needs `uv` and nothing else to
run the verbs that need no cluster.

## 3. Proposed layout

```text
.reactorcide/
  jobs/
    ingest.yaml
    validate.yaml
    integration.yaml
    testbed.yaml
    package.yaml
    release.yaml
  pipelines/
    ingest.py
    validate.py
    integration.py
    testbed.py
    package.py
    release.py
    commands.py
```

Job YAML selects triggers, runner image, resource limits, identity, timeout,
capabilities, and a single Python module command. It does not embed chained shell
commands.

Pipeline modules:

- use the runnerlib `WorkflowContext` and `workflow_context` helpers;
- use runnerlib change detection to avoid irrelevant expensive jobs;
- schedule independent jobs in parallel with explicit dependencies;
- publish workflow variables and outputs rather than parsing another job's logs;
- use `for_each` for supported language and package matrices;
- invoke repository tools through a small typed Python helper using
  `subprocess.run([program, arg, ...], check=True, shell=False)`;
- write artifacts only under the Reactorcide artifact directory.

`commands.py` is a process-execution helper, not a new workflow framework. It
standardizes working directory, timeouts, safe environment allowlists, captured
diagnostics, and secret-safe command display while leaving orchestration to
runnerlib.

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
                     release (tag only)
```

The repository trusts the ingest job pipeline code. The job examines trigger type and changed
paths, then emits the relevant nodes. Pull requests run validation and bounded
integration work without publish and deploy secrets. Main-branch and tag workflows
may add package or release nodes under project policy.

## 5. Validation jobs

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

- collector and head container images;
- TallyOwl and collector Helm charts;
- generated CSIL client packages;
- TypeScript browser package;
- software bill of materials, checksums, and provenance metadata.

A separate release job publishes the artifacts. This job uses verified package
artifacts. It does not build them again.

Release nodes run only for approved tag events. They use narrow secret grants
for each registry.

If the project needs deployment, add an explicit protected node. Packaging must
not cause deployment.

## 7. Local execution

Every non-publishing job must work through Reactorcide's canonical local runner.
The repository documentation will give `run-local` examples after job files
exist. Local execution should:

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
