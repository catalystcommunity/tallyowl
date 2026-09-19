"""Build and test the Go and TypeScript packages.

Phase 2 of `docs/PLAN.md` makes the contract trustworthy across languages, and
a contract that only one language checks is not checked. Every verb here runs
in CI through the same module, so a local run and a CI run cannot drift.

**A missing toolchain is a refusal, not a skip.** A suite that quietly does not
run reports the same green as a suite that passed, and the golden vectors exist
precisely to catch a language disagreeing with the other two.
"""

from __future__ import annotations

import os
from pathlib import Path

from . import deps
from .commands import REPOSITORY_ROOT, ToolFailed, run, say, which

GO_PACKAGES = (REPOSITORY_ROOT / "packages" / "driver-go",)
TYPESCRIPT_PACKAGES = (
    REPOSITORY_ROOT / "packages" / "browser",
    REPOSITORY_ROOT / "packages" / "dashboard",
    REPOSITORY_ROOT / "testbed" / "webapp",
)

#: The shared toolchain bundle every repository here uses. See the project
#: CLAUDE.md; it provides Go and Node without root.
CATALYST_TOOLS = Path(os.environ.get("CATALYST_TOOLS", Path.home() / ".local/catalyst-tools"))


#: Where Go caches modules and builds when the configured place is not
#: writable. `.deps` is git-ignored and already holds every fetched tool.
GO_HOME = REPOSITORY_ROOT / ".deps" / "go"


def go_environment() -> dict[str, str]:
    """Give Go a cache it can write to.

    The Reactorcide runner image sets `GOPATH=/go` and that directory belongs
    to root, while a job runs as an unprivileged user. Every Go command then
    stops with `could not create module cache: mkdir /go/pkg: permission
    denied` — including the Go commands that Rust integration tests spawn, so
    it takes the Rust suite down with it. A run-local job found exactly that.

    A workstation whose `GOPATH` is writable keeps it. Nothing here overrides a
    developer's own cache.
    """
    gopath = Path(os.environ.get("GOPATH", "")) if os.environ.get("GOPATH") else None
    if gopath and os.access(gopath, os.W_OK):
        return {}
    if gopath is None:
        default = Path.home() / "go"
        if os.access(default.parent, os.W_OK):
            return {}
    for directory in (GO_HOME / "pkg", GO_HOME / "cache"):
        directory.mkdir(parents=True, exist_ok=True)
    return {
        "GOPATH": str(GO_HOME),
        "GOMODCACHE": str(GO_HOME / "pkg" / "mod"),
        "GOCACHE": str(GO_HOME / "cache"),
    }


def _toolchain_environment() -> dict[str, str]:
    """Put the catalyst-tools Go and Node on the path for a child process.

    Sourcing a shell script would need a shell, and orchestration does not live
    in shell. The bundle's layout is fixed, so the paths are added directly.

    A release job has no catalyst-tools bundle, so the Node that `./tools.sh
    deps` fetches into `.deps/node` is on the path as well. A workstation that
    has its own Node keeps using it: `which` runs before either of these.
    """
    additions = [
        CATALYST_TOOLS / "go" / "bin",
        CATALYST_TOOLS / "node" / "bin",
        CATALYST_TOOLS / "bin",
    ]
    fetched = deps.node_bin()
    if fetched:
        additions.append(fetched)
    present = [str(p) for p in additions if p.is_dir()]
    environment = dict(go_environment())
    if present:
        environment["PATH"] = os.pathsep.join([*present, os.environ.get("PATH", "")])
        environment["GOTOOLCHAIN"] = os.environ.get("GOTOOLCHAIN", "local")
    return environment


def _program(name: str, how_to_install: str) -> tuple[str, dict[str, str]]:
    # Node before the lookup, not after the failure. A release job and a CI job
    # each start from an image with no Node, and the fetch is what makes the
    # verb work rather than explain itself.
    if name in ("npm", "node") and not deps.node_bin() and not which(name):
        deps.fetch_node()
    environment = _toolchain_environment()
    original = os.environ.get("PATH", "")
    # **`PATH` is optional in that dictionary.** It carries the Go cache
    # settings whenever the configured cache is not writable, which is every
    # CI job, and it carries `PATH` only when there is a toolchain directory to
    # add. Reading it unconditionally is how `test-go` failed in production
    # with a `KeyError` while passing on a workstation, where `.deps/node/bin`
    # happened to exist.
    if environment.get("PATH"):
        os.environ["PATH"] = environment["PATH"]
    try:
        found = which(name)
    finally:
        os.environ["PATH"] = original
    if not found:
        raise ToolFailed(f"`{name}` is not installed. {how_to_install}")
    return found, environment


TOOLCHAIN_HELP = (
    "Install the shared toolchains with "
    "`bash tools/install-transport-toolchains.sh`, or put your own on the path."
)


def npm_program() -> tuple[str, dict[str, str]]:
    """npm, and the environment a child process needs to find it.

    The package and publish jobs need this as well as the test verbs, so the
    lookup is public rather than repeated.
    """
    return _program("npm", TOOLCHAIN_HELP)


def go_test() -> int:
    """Run the Go suites, including the golden vectors."""
    program, environment = _program("go", TOOLCHAIN_HELP)
    for package in GO_PACKAGES:
        say(f"Testing {package.relative_to(REPOSITORY_ROOT)}")
        run([program, "test", "./..."], cwd=package, env=environment)
    return 0


def go_build() -> int:
    program, environment = _program("go", TOOLCHAIN_HELP)
    for package in GO_PACKAGES:
        run([program, "build", "./..."], cwd=package, env=environment)
    return 0


def go_vet() -> int:
    program, environment = _program("go", TOOLCHAIN_HELP)
    for package in GO_PACKAGES:
        run([program, "vet", "./..."], cwd=package, env=environment)
    return 0


def typescript_install() -> int:
    """Fetch the pinned transport and install each package's dependencies."""
    deps.fetch_csilgen()
    program, environment = _program("npm", TOOLCHAIN_HELP)
    for package in TYPESCRIPT_PACKAGES:
        say(f"Installing {package.relative_to(REPOSITORY_ROOT)}")
        run(
            [program, "install", "--no-audit", "--no-fund", "--silent"],
            cwd=package,
            env=environment,
        )
    return 0


def typescript_test() -> int:
    """Type-check and run the TypeScript suites, including the golden vectors."""
    deps.fetch_csilgen()
    npm, environment = _program("npm", TOOLCHAIN_HELP)
    node, _ = _program("node", TOOLCHAIN_HELP)
    for package in TYPESCRIPT_PACKAGES:
        if not (package / "node_modules").is_dir():
            run(
                [npm, "install", "--no-audit", "--no-fund", "--silent"],
                cwd=package,
                env=environment,
            )
        say(f"Testing {package.relative_to(REPOSITORY_ROOT)}")
        run(
            [str(package / "node_modules" / ".bin" / "tsc"), "-p", "tsconfig.json"],
            cwd=package,
            env=environment,
        )
        # `rootDir` is the repository root, so the compiler mirrors the
        # package's own path under `dist`. Deriving it keeps a package that
        # lives outside `packages/` working, which `testbed/webapp` does.
        compiled = package / "dist" / package.relative_to(REPOSITORY_ROOT) / "test"
        tests = sorted(str(p) for p in compiled.glob("*.test.js"))
        if not tests:
            raise ToolFailed(
                f"{package.relative_to(REPOSITORY_ROOT)} compiled and produced no tests. "
                "A suite that does not run reports the same green as one that passed."
            )
        run([node, "--test", *tests], cwd=package, env=environment)
    return 0
