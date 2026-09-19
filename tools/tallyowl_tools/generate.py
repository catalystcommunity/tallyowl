"""Generate the clients from `csil/`.

Two rules live here in code rather than in prose that a person has to remember,
which is the reason D55 gives for this entry point existing at all:

- **csilgen runs from the `csil/` directory**, because an `include` path
  resolves against the working directory rather than against the file that holds
  the statement;
- **csilgen is a pinned release rather than whatever is on the path**, once
  csilgen publishes releases.

Generated output is checked in. CI generates into a temporary directory and
compares, so drift fails the build instead of surprising somebody later. See
docs/CI-CD.md section 5.
"""

from __future__ import annotations

import filecmp
import shutil
import tempfile
from pathlib import Path

from .commands import REPOSITORY_ROOT, ToolFailed, run, say, warn, which

CSIL_DIR = REPOSITORY_ROOT / "csil"
GENERATED_DIR = REPOSITORY_ROOT / "generated"

#: The entry specifications. `types/common.csil` is included by each and is not
#: an entry of its own. See csil/README.md.
ENTRY_SPECIFICATIONS = (
    "tallyowl-ingest",
    "tallyowl-collector",
    "tallyowl-control",
    "tallyowl-cluster",
)

#: Every language the project generates today. A generated client is free; a
#: maintained app driver is not, and the project maintains three.
TARGETS = ("rust", "go", "typescript")

#: The generator is pinned and fetched, and the pin lives in `deps.py` beside
#: every other fetched tool. `deps.CSILGEN_VERSION` is that pin.
#:
#: A released binary rather than one each person builds: the generated code is
#: checked in and `gen-check` compares against it, and that comparison only
#: means something when every machine runs the same generator. csilgen's own
#: `--version` still reports 0.1.0 for every 0.2.x release, so the version it
#: prints cannot be the check. `gen-check` is the check.

#: The csilgen ref the TypeScript transport is taken from.
#:
#: This one has to be a git ref rather than a version, because it drives a
#: checkout: `.deps/csilgen` supplies the transport the TypeScript packages
#: import, and `git checkout` needs something git can resolve. It is a release
#: tag rather than a bare revision, so it names the artifact being consumed
#: rather than a moment in somebody's history.
CSILGEN_TRANSPORT_TAG = "transport-typescript/v0.2.0"


def csilgen_program() -> str:
    """The pinned generator, fetched if this is the first run."""
    from . import deps

    return deps.csilgen_binary()


def validate() -> None:
    """Check every specification, including the shared types file."""
    program = csilgen_program()
    say("Checking the contract")
    for name in ("types/common.csil", *[f"{n}.csil" for n in ENTRY_SPECIFICATIONS]):
        run([program, "validate", "--input", name], cwd=CSIL_DIR, quiet=True)
    say("The contract is valid.")


def generate(into: Path | None = None) -> Path:
    """Generate every entry specification for every target."""
    program = csilgen_program()
    destination = into or GENERATED_DIR
    say(f"Generating into {destination.relative_to(REPOSITORY_ROOT) if destination.is_relative_to(REPOSITORY_ROOT) else destination}")

    for name in ENTRY_SPECIFICATIONS:
        for target in TARGETS:
            output = destination / target / f"{name}-api"
            # The generator writes over what is there, and a removed type would
            # otherwise leave a stale file behind that still compiles.
            if output.exists():
                shutil.rmtree(output)
            run(
                [
                    program,
                    "generate",
                    "--quiet",
                    "--input",
                    f"{name}.csil",
                    "--target",
                    target,
                    "--output",
                    str(output),
                ],
                cwd=CSIL_DIR,
                quiet=True,
            )
    say("Generated.")
    return destination


def check_drift() -> None:
    """Generate into a temporary directory and compare with what is checked in.

    The job never modifies the source checkout and never stages regenerated
    files. It says how to fix the drift and stops.
    """
    validate()
    with tempfile.TemporaryDirectory(prefix="tallyowl-gen-") as temporary:
        fresh = Path(temporary)
        generate(into=fresh)
        differences = _compare(fresh, GENERATED_DIR)
        if differences:
            warn("The generated code does not match the contract.")
            for path in differences[:20]:
                warn(f"  {path}")
            if len(differences) > 20:
                warn(f"  ... and {len(differences) - 20} more")
            raise ToolFailed(
                "Run `./tools.sh gen` and commit the result. Generated code is "
                "never hand-edited; change the .csil source instead."
            )
    say("The generated code matches the contract.")


def _compare(fresh: Path, committed: Path) -> list[str]:
    """Every path that differs, relative to the generated root."""
    differences: list[str] = []
    if not committed.exists():
        return ["generated/ does not exist"]

    fresh_files = {p.relative_to(fresh) for p in fresh.rglob("*") if p.is_file()}
    committed_files = {
        p.relative_to(committed) for p in committed.rglob("*") if p.is_file()
    }

    for missing in sorted(fresh_files - committed_files):
        differences.append(f"missing from the repository: {missing}")
    for extra in sorted(committed_files - fresh_files):
        differences.append(f"in the repository and not generated: {extra}")
    for shared in sorted(fresh_files & committed_files):
        if not filecmp.cmp(fresh / shared, committed / shared, shallow=False):
            differences.append(f"differs: {shared}")
    return differences
