"""Build, test, format, and lint.

Every verb here is the one CI runs, because CI calls this module rather than a
copy of these commands. See docs/CI-CD.md section 2.
"""

from __future__ import annotations

from .commands import REPOSITORY_ROOT, ToolFailed, require, run, say
from . import deps, dev, generate, packages

#: The generated crates are checked in and never hand-edited, so a lint that
#: fails the build on one of their warnings would fail on something nobody may
#: fix. They still compile as part of every build.
GENERATED_CRATES = (
    "tallyowl-ingest-api",
    "tallyowl-collector-api",
    "tallyowl-control-api",
)


def _cargo() -> str:
    return require(
        "cargo",
        "Install the Rust toolchain from https://rustup.rs and run this again.",
    )


def _exclude_generated() -> list[str]:
    arguments: list[str] = []
    for crate in GENERATED_CRATES:
        arguments.extend(["--exclude", crate])
    return arguments


#: Denying warnings on the command line would reach the generated crates too,
#: because they are path dependencies of ours. The `[lints]` table in each of our
#: manifests scopes the denial to the crates this project writes. See the
#: workspace Cargo.toml.


def setup() -> int:
    """Everything a clean clone needs before `dev up`."""
    say("Setting up")
    _cargo()
    # The generator is a pinned release, fetched for this machine.
    deps.fetch_csilgen_binary()
    # npm cannot depend on a subdirectory of a Git repository, so the pinned
    # CSIL transport arrives here rather than through a package manager.
    deps.fetch_csilgen()
    # The Parquet export is verified in DuckDB, which is a tool TallyOwl does
    # not control. That is the point: an export has to be readable by
    # something else. See L025.
    deps.fetch_duckdb()
    generate.validate()
    generate.generate()
    build()
    packages.typescript_install()
    dev.setup_configuration()
    say("Ready. Start the system with `./tools.sh dev up`.")
    return 0


def build() -> int:
    say("Building")
    run([_cargo(), "build", "--workspace"], cwd=REPOSITORY_ROOT)
    packages.go_build()
    return 0


def test() -> int:
    """Every suite in every maintained language.

    The golden vectors only prove agreement when all three languages check
    them, so a Rust-only `test` would report green on a contract that two
    languages cannot encode.
    """
    say("Testing Rust")
    run([_cargo(), "test", "--workspace"], cwd=REPOSITORY_ROOT, env=packages.go_environment())
    packages.go_test()
    packages.typescript_test()
    test_tools()
    return 0


def test_tools() -> int:
    """The tooling's own tests.

    The tooling decides which version every artifact carries and which crate
    names a publication takes. That is arithmetic, so it is tested here rather
    than discovered during a release.
    """
    say("Testing the tooling")
    run(
        [
            require("uv", "Install uv from https://docs.astral.sh/uv/ and run this again."),
            "run",
            "--project",
            ".",
            "python",
            "-m",
            "unittest",
            "discover",
            "--start-directory",
            "tests",
            "--top-level-directory",
            ".",
            "--verbose",
        ],
        cwd=REPOSITORY_ROOT / "tools",
    )
    return 0


def test_rust() -> int:
    say("Testing Rust")
    # DuckDB before the suite, not an instruction after it. The Parquet export
    # test reads `.deps/bin/duckdb` and refuses to skip when it is absent —
    # verifying an export with the library that wrote it proves nothing (L025)
    # — so a CI job that never fetched it fails on a missing tool rather than
    # on TallyOwl. That is what happened on the first production run.
    deps.fetch_duckdb()
    # The Go environment as well: `crates/tallyowl-collector/tests/go_driver.rs`
    # builds and runs the Go app driver against a real collector, so a Go cache
    # it cannot write takes the Rust suite down with it.
    run([_cargo(), "test", "--workspace"], cwd=REPOSITORY_ROOT, env=packages.go_environment())
    return 0


def golden() -> int:
    """Rewrite `golden/vectors.json` from the Rust side.

    Regenerating is a deliberate act. A change to that file in a review means
    the wire changed, and a reviewer should see that from the file alone.
    """
    say("Writing the golden vectors")
    run(
        [_cargo(), "test", "-p", "tallyowl-golden"],
        cwd=REPOSITORY_ROOT,
        env={"TALLYOWL_UPDATE_GOLDEN": "1"},
    )
    say("Written. Check `golden/vectors.json` into the change that caused it.")
    return 0


def format_code() -> int:
    _rust_component("rustfmt", ["fmt", "--version"])
    say("Formatting")
    run([_cargo(), "fmt", "--all"], cwd=REPOSITORY_ROOT)
    return 0


def _rust_component(name: str, probe: list[str]) -> None:
    """Make sure a toolchain component is there, and add it when it is not.

    The Reactorcide runner image installs a minimal profile: `cargo clippy` is
    there and `cargo fmt` is not, which a run-local job found by failing on the
    first line of `lint`. `rustup` is in the image, so the component is one
    command away — and asking for it is better than telling somebody to.
    """
    from .commands import which

    if run([_cargo(), *probe], check=False, capture=True, quiet=True).ok:
        return
    rustup = which("rustup")
    if not rustup:
        raise ToolFailed(
            f"`cargo {probe[0]}` is not available and there is no `rustup` to add "
            f"it. Install the {name} component for this toolchain."
        )
    say(f"Adding the {name} component to this toolchain")
    run([rustup, "component", "add", name])


def lint() -> int:
    _rust_component("rustfmt", ["fmt", "--version"])
    _rust_component("clippy", ["clippy", "--version"])
    say("Checking formatting")
    run([_cargo(), "fmt", "--all", "--", "--check"], cwd=REPOSITORY_ROOT)
    say("Linting")
    run(
        [
            _cargo(),
            "clippy",
            "--workspace",
            *_exclude_generated(),
            "--all-targets",
        ],
        cwd=REPOSITORY_ROOT,
    )
    packages.go_vet()
    return 0


def check() -> int:
    """What a person runs before they push, and what a pull request runs."""
    from . import release

    release.check()
    generate.validate()
    format_code()
    lint()
    test()
    say("Everything passes.")
    return 0


def _unused() -> None:  # pragma: no cover
    raise ToolFailed("unreachable")
