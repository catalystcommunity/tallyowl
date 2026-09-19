"""Fetch the pinned dependencies that a package manager cannot pin on its own.

Rust and Go each pin the csilgen transport by revision through their own
manifests. TypeScript cannot: npm has no way to depend on a subdirectory of a
Git repository, and csilgen does not publish the transport to a registry yet.

This module therefore fetches the csilgen repository at the same revision the
Rust and Go manifests name, into a Git-ignored directory, and the TypeScript
packages depend on that path. A clean clone reaches a working build with
`./tools.sh setup` and nothing else, which is the Phase 1 exit criterion the
loop is built around.

One ref, named in one place. `generate.CSILGEN_TRANSPORT_TAG` is that place. It
is a release tag rather than a revision, because it names the artifact this
repository consumes rather than a moment in somebody's history, and because a
person can tell what it is without a checkout to resolve it against.
"""

from __future__ import annotations

from pathlib import Path

from .commands import REPOSITORY_ROOT, ToolFailed, require, run, say, warn
from .generate import CSILGEN_TRANSPORT_TAG

#: Git ignores this directory. It holds a checkout, never an edit.
DEPENDENCY_DIR = REPOSITORY_ROOT / ".deps"
CSILGEN_CHECKOUT = DEPENDENCY_DIR / "csilgen"

#: Where the TypeScript packages find the transport.
TYPESCRIPT_TRANSPORT = CSILGEN_CHECKOUT / "transports" / "typescript"

CSILGEN_REMOTE = "https://github.com/catalystcommunity/csilgen.git"


def _git() -> str:
    return require("git", "Install Git and run this again.")


def _resolved_transport_revision() -> str | None:
    """What the pinned tag points at, when git can say.

    The checkout records a revision rather than a tag, so telling "already at
    the pin" from "somewhere else" needs the tag resolved first. When it cannot
    be resolved, the caller does the checkout, which is the safe direction.
    """
    for repository in (CSILGEN_CHECKOUT, REPOSITORY_ROOT.parent / "csilgen"):
        if not (repository / ".git").exists():
            continue
        result = run(
            ["git", "-C", str(repository), "rev-parse", f"{CSILGEN_TRANSPORT_TAG}^{{commit}}"],
            capture=True,
            check=False,
            quiet=True,
        )
        if result.ok and result.stdout.strip():
            return result.stdout.strip()
    return None


def current_revision() -> str | None:
    """The ref the checkout is at, or `None` when there is no checkout."""
    if not (CSILGEN_CHECKOUT / ".git").exists():
        return None
    result = run(
        [_git(), "rev-parse", "HEAD"],
        cwd=CSILGEN_CHECKOUT,
        capture=True,
        check=False,
        quiet=True,
    )
    return result.stdout.strip() if result.ok else None


def fetch_transport(force: bool = False) -> bool:
    """Take the TypeScript transport out of the combined csilgen release.

    **It lands where it has always landed.** Eight source files reach it at
    `.deps/csilgen/transports/typescript/src` by relative path, so the archive
    is unpacked to exactly that directory whatever it calls itself inside.
    Moving that path is a separate decision from changing where the bytes come
    from, and this change is only the second one.

    Returns False when the release carries no transport asset, and the caller
    clones the repository instead — which is what happens today.
    """
    from . import csilgen_release

    if not force and (TYPESCRIPT_TRANSPORT / "package.json").is_file():
        return True

    release = csilgen_release.find_release(CSILGEN_VERSION)
    if release is None:
        return False
    chosen = csilgen_release.pick(release, "transport-typescript")
    if chosen is None:
        return False

    archive = DEPENDENCY_DIR / "csilgen-transport-typescript.tar.gz"
    try:
        _download(chosen.url, archive, f"the TypeScript transport from {release.tag}")
    except ToolFailed:
        archive.unlink(missing_ok=True)
        return False
    csilgen_release.extract_tree(archive, TYPESCRIPT_TRANSPORT, marker="src/index.ts")
    archive.unlink(missing_ok=True)

    if not (TYPESCRIPT_TRANSPORT / "src" / "index.ts").is_file():
        raise ToolFailed(
            f"{chosen.name} unpacked into {TYPESCRIPT_TRANSPORT} and there is no "
            "`src/index.ts` in it. Eight files import that path; check what the "
            "archive holds and how many leading directories to drop."
        )
    return True


def fetch_csilgen(force: bool = False) -> Path:
    """Put the pinned csilgen revision in `.deps/csilgen`.

    The transport is the only thing this checkout is for. When the csilgen
    release publishes the transport as an asset, that is taken instead and this
    clones nothing: a 200 MB checkout to read one directory is a poor trade
    when the directory is downloadable.

    This is idempotent. A checkout already at the pinned revision costs one
    `rev-parse` and nothing else.
    """
    if not force and (TYPESCRIPT_TRANSPORT / "package.json").is_file():
        return CSILGEN_CHECKOUT

    if fetch_transport(force):
        return CSILGEN_CHECKOUT

    if not force and current_revision() == _resolved_transport_revision():
        return CSILGEN_CHECKOUT

    git = _git()
    DEPENDENCY_DIR.mkdir(parents=True, exist_ok=True)

    if not (CSILGEN_CHECKOUT / ".git").exists():
        say(f"Fetching csilgen at {CSILGEN_TRANSPORT_TAG}")
        # A local clone is much faster than the network and is the ordinary case
        # on a workstation that already has the sibling repository.
        sibling = REPOSITORY_ROOT.parent / "csilgen"
        source = str(sibling) if (sibling / ".git").exists() else CSILGEN_REMOTE
        run([git, "clone", "--quiet", source, str(CSILGEN_CHECKOUT)])
    else:
        say(f"Moving csilgen to {CSILGEN_TRANSPORT_TAG}")
        # `--tags`, because the pin is a tag and a plain fetch does not bring
        # a new one that the checkout has never seen.
        run(
            [git, "fetch", "--quiet", "--tags", "origin"],
            cwd=CSILGEN_CHECKOUT,
            check=False,
        )

    result = run(
        [git, "checkout", "--quiet", CSILGEN_TRANSPORT_TAG],
        cwd=CSILGEN_CHECKOUT,
        check=False,
    )
    if not result.ok:
        raise ToolFailed(
            f"csilgen {CSILGEN_TRANSPORT_TAG} could not be checked out. "
            "The pinned tag is in tools/tallyowl_tools/generate.py."
        )

    if not (TYPESCRIPT_TRANSPORT / "package.json").exists():
        raise ToolFailed(
            f"The csilgen checkout at {CSILGEN_CHECKOUT} holds no TypeScript "
            "transport. The pinned tag may predate it."
        )
    return CSILGEN_CHECKOUT


def transport_path_for(package_directory: Path) -> str:
    """The `file:` specifier one package uses to reach the transport."""
    import os

    return "file:" + os.path.relpath(TYPESCRIPT_TRANSPORT, package_directory)


# ---------------------------------------------------------------------------
# DuckDB
# ---------------------------------------------------------------------------

#: The version the Parquet export is verified against.
DUCKDB_VERSION = "1.4.1"

#: Where it lands. The same per-user location the shared toolchains use, so
#: nothing needs root and nothing reaches a system directory.
DUCKDB_PATH = DEPENDENCY_DIR / "bin" / "duckdb"


def duckdb_program() -> str:
    """Find DuckDB, preferring one the operator already has."""
    from .commands import which as _which

    found = _which("duckdb")
    if found:
        return found
    if DUCKDB_PATH.is_file():
        return str(DUCKDB_PATH)
    raise ToolFailed(
        "`duckdb` is not installed, and the Parquet export is verified with it. "
        "Run `./tools.sh deps` to fetch it, or install your own and put it on "
        "the path."
    )


def fetch_duckdb(force: bool = False) -> Path:
    """Fetch the DuckDB command line into `.deps/bin`.

    The Phase 3 exit criterion is that a clean Parquet export is queryable **in
    DuckDB**. Verifying an export with the same library that wrote it would
    prove the library round-trips and not that the file is readable by a tool
    TallyOwl does not control, which is the whole point of the export contract.

    See docs/IMPLEMENTATION_LOG.md L025.
    """
    from .commands import which as _which

    if not force and (_which("duckdb") or DUCKDB_PATH.is_file()):
        return DUCKDB_PATH

    import platform
    import urllib.request
    import zipfile

    machine = platform.machine()
    system = platform.system()
    target = {
        ("Linux", "x86_64"): "linux-amd64",
        ("Linux", "aarch64"): "linux-arm64",
        ("Darwin", "x86_64"): "osx-universal",
        ("Darwin", "arm64"): "osx-universal",
    }.get((system, machine))
    if target is None:
        raise ToolFailed(
            f"There is no DuckDB build for {system} on {machine} that this script "
            "knows about. Install DuckDB yourself and put it on the path."
        )

    url = (
        f"https://github.com/duckdb/duckdb/releases/download/v{DUCKDB_VERSION}"
        f"/duckdb_cli-{target}.zip"
    )
    DUCKDB_PATH.parent.mkdir(parents=True, exist_ok=True)
    archive = DUCKDB_PATH.parent / "duckdb.zip"

    say(f"Fetching DuckDB {DUCKDB_VERSION} for {target}")
    try:
        urllib.request.urlretrieve(url, archive)  # noqa: S310 - a pinned release URL
    except Exception as error:  # pragma: no cover - a network failure
        raise ToolFailed(
            f"DuckDB {DUCKDB_VERSION} could not be fetched from {url}: {error}. "
            "Install DuckDB yourself and put it on the path."
        ) from error

    with zipfile.ZipFile(archive) as bundle:
        bundle.extractall(DUCKDB_PATH.parent)
    archive.unlink(missing_ok=True)
    DUCKDB_PATH.chmod(0o755)
    return DUCKDB_PATH


# ---------------------------------------------------------------------------
# Helm and Node
#
# The package and publish jobs need both, and the Reactorcide runner image
# carries neither: it has podman, git, uv, Go, and Rust. That was read from the
# image rather than assumed, which is the L146 rule. Both arrive here the same
# way DuckDB does, so a release job and a workstation run the same versions.
# ---------------------------------------------------------------------------

#: The Helm version the charts are packaged and pushed with.
HELM_VERSION = "3.16.4"
HELM_PATH = DEPENDENCY_DIR / "bin" / "helm"

#: The Node version the client packages are packed and published with.
#:
#: Node 26 carries npm 11, and npm 11 is the first with `npm stage publish`.
#: npm deprecated the 2FA-bypass granular token in August 2026 and removes its
#: publish capability in January 2027, so a release token can stage a publish
#: and a person approves it with 2FA. A token that can publish outright is a
#: token this project would rather not hold. See L180.
NODE_VERSION = "26.1.0"
NODE_DIR = DEPENDENCY_DIR / "node"

#: The version calculator every repository here releases with. It reads the
#: conventional commits since the last tag and says what the next tag is.
SEMVER_TAGS_VERSION = "0.6.1"
SEMVER_TAGS_PATH = DEPENDENCY_DIR / "bin" / "semver-tags"

#: Pushes an image from a saved archive, with no container daemon. The publish
#: job builds nothing, so it needs no daemon; this is how corndogs pushes too.
CRANE_VERSION = "0.20.3"
CRANE_PATH = DEPENDENCY_DIR / "bin" / "crane"

#: The GitHub command line, which the release job publishes a release with.
GH_VERSION = "2.63.2"
GH_PATH = DEPENDENCY_DIR / "bin" / "gh"


def _platform_target() -> tuple[str, str]:
    """The operating system and architecture, in the names releases use."""
    import platform

    system = {"Linux": "linux", "Darwin": "darwin"}.get(platform.system())
    machine = {"x86_64": "amd64", "aarch64": "arm64", "arm64": "arm64"}.get(
        platform.machine()
    )
    if system is None or machine is None:
        raise ToolFailed(
            f"There is no release for {platform.system()} on {platform.machine()} "
            "that this script knows about. Install the tool yourself and put it "
            "on the path."
        )
    return system, machine


def _download(url: str, destination: Path, what: str, quiet: bool = False) -> Path:
    """Fetch one pinned release archive.

    `curl` first, because a release host answers a Python library's request
    with a 503 and answers curl with the file. Python is the fallback for a
    machine that has no curl, which the release runner is not.
    """
    from .commands import which as _which

    destination.parent.mkdir(parents=True, exist_ok=True)
    if not quiet:
        say(f"Fetching {what}")

    curl = _which("curl")
    if curl:
        result = run(
            [curl, "--fail", "--silent", "--show-error", "--location",
             "--max-time", "300", "--output", str(destination), url],
            check=False,
            quiet=quiet,
        )
        if result.ok and destination.is_file() and destination.stat().st_size > 0:
            return destination

    import urllib.request

    request = urllib.request.Request(url, headers={"User-Agent": "tallyowl-tools"})
    try:
        with urllib.request.urlopen(request, timeout=300) as response:  # noqa: S310
            destination.write_bytes(response.read())
    except Exception as error:  # pragma: no cover - a network failure
        raise ToolFailed(
            f"{what} could not be fetched from {url}: {error}. Install it "
            "yourself and put it on the path."
        ) from error
    return destination


def helm_program() -> str:
    """Find Helm, preferring one the operator already has."""
    from .commands import which as _which

    found = _which("helm")
    if found:
        return found
    if not HELM_PATH.is_file():
        # Fetch it rather than refuse. A job that needs a tool and is told to
        # run a different verb first is a job that fails on its first step.
        fetch_helm()
    return str(HELM_PATH)



def fetch_helm(force: bool = False) -> Path:
    """Fetch the pinned Helm release into `.deps/bin`."""
    from .commands import which as _which

    if not force and (_which("helm") or HELM_PATH.is_file()):
        return HELM_PATH

    import tarfile

    system, machine = _platform_target()
    archive = _download(
        f"https://get.helm.sh/helm-v{HELM_VERSION}-{system}-{machine}.tar.gz",
        HELM_PATH.parent / "helm.tar.gz",
        f"Helm {HELM_VERSION} for {system}-{machine}",
    )
    with tarfile.open(archive) as bundle:
        member = bundle.getmember(f"{system}-{machine}/helm")
        member.name = "helm"
        bundle.extract(member, HELM_PATH.parent, filter="data")
    archive.unlink(missing_ok=True)
    HELM_PATH.chmod(0o755)
    return HELM_PATH


def node_bin() -> Path | None:
    """The fetched Node's `bin` directory, when there is one."""
    directory = NODE_DIR / "bin"
    return directory if (directory / "npm").exists() else None


def fetch_node(force: bool = False) -> Path:
    """Fetch the pinned Node runtime into `.deps/node`.

    npm and the TypeScript compiler both come with it, so one fetch covers
    packing the client packages and publishing them.
    """
    from .commands import which as _which

    if not force and (_which("npm") or node_bin()):
        return NODE_DIR

    import tarfile

    system, machine = _platform_target()
    # Node names the architecture differently from Helm, and only there.
    node_machine = {"amd64": "x64", "arm64": "arm64"}[machine]
    release = f"node-v{NODE_VERSION}-{system}-{node_machine}"
    archive = _download(
        f"https://nodejs.org/dist/v{NODE_VERSION}/{release}.tar.xz",
        DEPENDENCY_DIR / f"{release}.tar.xz",
        f"Node {NODE_VERSION} for {system}-{node_machine}",
    )
    with tarfile.open(archive) as bundle:
        bundle.extractall(DEPENDENCY_DIR, filter="data")
    archive.unlink(missing_ok=True)
    if NODE_DIR.exists():
        import shutil

        shutil.rmtree(NODE_DIR)
    (DEPENDENCY_DIR / release).rename(NODE_DIR)
    return NODE_DIR


def semver_tags_program() -> str:
    """Find the version calculator, preferring one already on the path."""
    from .commands import which as _which

    found = _which("semver-tags")
    if found:
        return found
    if not SEMVER_TAGS_PATH.is_file():
        # Fetch it rather than refuse. A job that needs a tool and is told to
        # run a different verb first is a job that fails on its first step.
        fetch_semver_tags()
    return str(SEMVER_TAGS_PATH)



def fetch_semver_tags(force: bool = False) -> Path:
    """Fetch the pinned semver-tags release into `.deps/bin`."""
    from .commands import which as _which

    if not force and (_which("semver-tags") or SEMVER_TAGS_PATH.is_file()):
        return SEMVER_TAGS_PATH

    import tarfile

    system, machine = _platform_target()
    # The release carries one archive for each platform, and a legacy
    # `semver-tags.tar.gz` beside them. Name the platform: the legacy archive
    # says nothing about what is inside it.
    archive = _download(
        "https://github.com/catalystcommunity/semver-tags/releases/download/"
        f"v{SEMVER_TAGS_VERSION}/semver-tags-{SEMVER_TAGS_VERSION}-{system}-{machine}.tar.gz",
        SEMVER_TAGS_PATH.parent / "semver-tags.tar.gz",
        f"semver-tags {SEMVER_TAGS_VERSION} for {system}-{machine}",
    )
    with tarfile.open(archive) as bundle:
        bundle.extract("semver-tags", SEMVER_TAGS_PATH.parent, filter="data")
    archive.unlink(missing_ok=True)
    SEMVER_TAGS_PATH.chmod(0o755)
    return SEMVER_TAGS_PATH


def crane_program() -> str:
    """Find crane, preferring one already on the path."""
    from .commands import which as _which

    found = _which("crane")
    if found:
        return found
    if not CRANE_PATH.is_file():
        # Fetch it rather than refuse. A job that needs a tool and is told to
        # run a different verb first is a job that fails on its first step.
        fetch_crane()
    return str(CRANE_PATH)



def fetch_crane(force: bool = False) -> Path:
    """Fetch the pinned crane release into `.deps/bin`."""
    from .commands import which as _which

    if not force and (_which("crane") or CRANE_PATH.is_file()):
        return CRANE_PATH

    import platform
    import tarfile

    system, machine = _platform_target()
    # crane names the platform its own way, and only here.
    crane_system = {"linux": "Linux", "darwin": "Darwin"}[system]
    crane_machine = {"amd64": "x86_64", "arm64": "arm64"}[machine]
    if platform.system() == "Darwin" and machine == "amd64":
        crane_machine = "x86_64"
    archive = _download(
        "https://github.com/google/go-containerregistry/releases/download/"
        f"v{CRANE_VERSION}/go-containerregistry_{crane_system}_{crane_machine}.tar.gz",
        CRANE_PATH.parent / "crane.tar.gz",
        f"crane {CRANE_VERSION} for {crane_system}_{crane_machine}",
    )
    with tarfile.open(archive) as bundle:
        bundle.extract("crane", CRANE_PATH.parent, filter="data")
    archive.unlink(missing_ok=True)
    CRANE_PATH.chmod(0o755)
    return CRANE_PATH


def gh_program() -> str:
    """Find the GitHub command line, preferring one already on the path."""
    from .commands import which as _which

    found = _which("gh")
    if found:
        return found
    if not GH_PATH.is_file():
        # Fetch it rather than refuse. A job that needs a tool and is told to
        # run a different verb first is a job that fails on its first step.
        fetch_gh()
    return str(GH_PATH)



def fetch_gh(force: bool = False) -> Path:
    """Fetch the pinned GitHub command line into `.deps/bin`."""
    from .commands import which as _which

    if not force and (_which("gh") or GH_PATH.is_file()):
        return GH_PATH

    import tarfile

    system, machine = _platform_target()
    release = f"gh_{GH_VERSION}_{system}_{machine}"
    archive = _download(
        f"https://github.com/cli/cli/releases/download/v{GH_VERSION}/{release}.tar.gz",
        GH_PATH.parent / "gh.tar.gz",
        f"the GitHub command line {GH_VERSION} for {system}-{machine}",
    )
    with tarfile.open(archive) as bundle:
        member = bundle.getmember(f"{release}/bin/gh")
        member.name = "gh"
        bundle.extract(member, GH_PATH.parent, filter="data")
    archive.unlink(missing_ok=True)
    GH_PATH.chmod(0o755)
    return GH_PATH


#: The static Docker client. The release job builds the service image against
#: the BuildKit-free daemon that Reactorcide's `docker` capability provides,
#: and the runner image carries podman rather than a docker client.
DOCKER_CLI_VERSION = "27.5.1"
DOCKER_PATH = DEPENDENCY_DIR / "bin" / "docker"


def fetch_docker_cli(force: bool = False) -> Path:
    """Fetch the pinned Docker client into `.deps/bin`.

    Only a client. The daemon comes from the job's `docker` capability, which
    is how corndogs builds its image too.
    """
    from .commands import which as _which

    if not force and (_which("docker") or DOCKER_PATH.is_file()):
        return DOCKER_PATH

    import tarfile

    system, machine = _platform_target()
    if system != "linux":
        raise ToolFailed(
            "The static Docker client this fetches is a Linux build, and this "
            "is not Linux. Install Docker yourself."
        )
    static = {"amd64": "x86_64", "arm64": "aarch64"}[machine]
    archive = _download(
        f"https://download.docker.com/linux/static/stable/{static}/docker-{DOCKER_CLI_VERSION}.tgz",
        DOCKER_PATH.parent / "docker.tgz",
        f"the Docker client {DOCKER_CLI_VERSION} for {static}",
    )
    with tarfile.open(archive) as bundle:
        member = bundle.getmember("docker/docker")
        member.name = "docker"
        bundle.extract(member, DOCKER_PATH.parent, filter="data")
    archive.unlink(missing_ok=True)
    DOCKER_PATH.chmod(0o755)
    return DOCKER_PATH


# ---------------------------------------------------------------------------
# csilgen
#
# The generator is a released binary now, fetched for this machine's
# architecture, rather than something each person builds. A build somebody
# makes by hand is a build nobody else has: the generated code is checked in,
# `gen-check` compares against it, and the comparison only means something when
# every machine runs the same generator.
# ---------------------------------------------------------------------------

#: The csilgen release this repository generates with. A release rather than
#: "latest": generated output is checked in, so the generator is a pin like any
#: other dependency, and a new one is a deliberate change with a diff to read.
CSILGEN_VERSION = "0.2.7"

#: csilgen tags a core release with this prefix.
CSILGEN_TAG = f"csilgen-core/v{CSILGEN_VERSION}"
CSILGEN_PATH = DEPENDENCY_DIR / "bin" / "csilgen"


def csilgen_binary() -> str:
    """The pinned generator, with its target plugins, fetching what is missing.

    The pinned one comes first, before anything on the path: a generator on a
    developer's path is whatever they built last, and `gen-check` would then
    pass here and fail in CI.

    The binary alone is not enough. csilgen loads a WASM generator for each
    target from the home directory, and without them it refuses every target it
    was asked for.
    """
    if not CSILGEN_PATH.is_file():
        fetch_csilgen_binary()
    fetch_csilgen_generators()
    return str(CSILGEN_PATH)


def fetch_csilgen_binary(force: bool = False) -> Path:
    """Fetch the pinned csilgen command line into `.deps/bin`.

    From the release's own asset list when that release can be read, and from
    the per-platform name this repository has always used when it cannot.
    """
    if not force and CSILGEN_PATH.is_file():
        return CSILGEN_PATH

    from . import csilgen_release

    system, machine = _platform_target()
    # csilgen names the architecture the way the compiler does.
    csilgen_machine = {"amd64": "x86_64", "arm64": "aarch64"}[machine]

    url = (
        f"https://github.com/catalystcommunity/csilgen/releases/download/{CSILGEN_TAG}"
        f"/csilgen-{CSILGEN_VERSION}-{system}-{csilgen_machine}.tar.gz"
    )
    release = csilgen_release.find_release(CSILGEN_VERSION)
    if release:
        chosen = csilgen_release.pick(release, "cli", system, csilgen_machine)
        if chosen:
            url = chosen.url

    archive = _download(
        url,
        CSILGEN_PATH.parent / "csilgen.tar.gz",
        f"csilgen {CSILGEN_VERSION} for {system}-{csilgen_machine}",
    )
    taken = csilgen_release.extract_members(
        archive, __import__("re").compile(r"^csilgen$"), CSILGEN_PATH.parent
    )
    archive.unlink(missing_ok=True)
    if not taken:
        raise ToolFailed(
            f"The csilgen archive at {url} holds no `csilgen` binary."
        )
    CSILGEN_PATH.chmod(0o755)
    return CSILGEN_PATH


# ---------------------------------------------------------------------------
# The audit tools
#
# `cargo audit` and `cargo deny` are the Phase 11 dependency audit, and the
# runner image carries neither. Both publish prebuilt binaries, which is much
# faster than `cargo install` and gives every machine the same one.
# ---------------------------------------------------------------------------

CARGO_AUDIT_VERSION = "0.22.2"
CARGO_AUDIT_PATH = DEPENDENCY_DIR / "bin" / "cargo-audit"

CARGO_DENY_VERSION = "0.20.2"
CARGO_DENY_PATH = DEPENDENCY_DIR / "bin" / "cargo-deny"


def _extract_one(archive: Path, name: str, destination: Path) -> Path:
    """Take one named file out of a release archive, wherever it sits inside."""
    import tarfile

    with tarfile.open(archive) as bundle:
        member = next(m for m in bundle.getmembers() if Path(m.name).name == name)
        member.name = name
        bundle.extract(member, destination.parent, filter="data")
    archive.unlink(missing_ok=True)
    destination.chmod(0o755)
    return destination


def cargo_audit_program() -> str:
    """Find `cargo-audit`, fetching the pinned release when it is not there."""
    from .commands import which as _which

    found = _which("cargo-audit")
    if found:
        return found
    if not CARGO_AUDIT_PATH.is_file():
        fetch_cargo_audit()
    return str(CARGO_AUDIT_PATH)


def fetch_cargo_audit(force: bool = False) -> Path:
    from .commands import which as _which

    if not force and (_which("cargo-audit") or CARGO_AUDIT_PATH.is_file()):
        return CARGO_AUDIT_PATH
    _, machine = _platform_target()
    target = {"amd64": "x86_64", "arm64": "aarch64"}[machine]
    archive = _download(
        "https://github.com/rustsec/rustsec/releases/download/"
        f"cargo-audit/v{CARGO_AUDIT_VERSION}/cargo-audit-{target}-unknown-linux-musl"
        f"-v{CARGO_AUDIT_VERSION}.tgz",
        CARGO_AUDIT_PATH.parent / "cargo-audit.tgz",
        f"cargo-audit {CARGO_AUDIT_VERSION} for {target}",
    )
    return _extract_one(archive, "cargo-audit", CARGO_AUDIT_PATH)


def cargo_deny_program() -> str:
    """Find `cargo-deny`, fetching the pinned release when it is not there."""
    from .commands import which as _which

    found = _which("cargo-deny")
    if found:
        return found
    if not CARGO_DENY_PATH.is_file():
        fetch_cargo_deny()
    return str(CARGO_DENY_PATH)


def fetch_cargo_deny(force: bool = False) -> Path:
    from .commands import which as _which

    if not force and (_which("cargo-deny") or CARGO_DENY_PATH.is_file()):
        return CARGO_DENY_PATH
    _, machine = _platform_target()
    target = {"amd64": "x86_64", "arm64": "aarch64"}[machine]
    archive = _download(
        f"https://github.com/EmbarkStudios/cargo-deny/releases/download/{CARGO_DENY_VERSION}"
        f"/cargo-deny-{CARGO_DENY_VERSION}-{target}-unknown-linux-musl.tar.gz",
        CARGO_DENY_PATH.parent / "cargo-deny.tar.gz",
        f"cargo-deny {CARGO_DENY_VERSION} for {target}",
    )
    return _extract_one(archive, "cargo-deny", CARGO_DENY_PATH)


# ---------------------------------------------------------------------------
# The csilgen generators
#
# csilgen loads a WASM generator for each target from `~/.csilgen/generators`,
# and the release archive carries only the binary. A machine with the binary
# and no generators refuses with "Unknown target 'rust'", which is what a
# run-local job found. The generators are built from the pinned checkout — the
# same one the TypeScript transport comes from — and installed where csilgen
# looks.
#
# **This is the one provisioning step that compiles rather than downloads.**
# csilgen publishes no generator artifact today; when it does, this becomes a
# fetch like every other tool here. See L185.
# ---------------------------------------------------------------------------

#: The three targets this repository generates. Building the other twelve would
#: cost minutes for output nobody here reads.
GENERATOR_TARGETS = ("rust", "go", "typescript")

#: The generator release csilgen publishes each WASM module under. csilgen's
#: release job archives one for each language as
#: `csilgen-generator-<language>-<version>.tar.gz` on the tag
#: `generator-<language>/v<version>`.
#:
#: **Fetched when it is there, built when it is not.** The generator releases
#: for 0.2.0 are drafts with no assets today, so the fallback is what runs; the
#: day a generator release carries its archive, this fetches it and the build
#: disappears without another change here. See L185.
GENERATOR_VERSION = "0.2.7"

#: Where csilgen looks. The path is relative to the home directory of whoever
#: runs it, which is why this is installed rather than kept in `.deps`.
GENERATOR_DIR = Path.home() / ".csilgen" / "generators"


def generator_files() -> list[Path]:
    """What must exist for `csilgen generate` to know a target."""
    return [GENERATOR_DIR / f"csilgen_{name}_generator.wasm" for name in GENERATOR_TARGETS]


def _download_all_generators() -> bool:
    """Take the whole generator tarball out of the combined csilgen release."""
    import re

    from . import csilgen_release

    release = csilgen_release.find_release(CSILGEN_VERSION)
    if release is None:
        return False
    chosen = csilgen_release.pick(release, "generators")
    if chosen is None:
        return False

    archive = DEPENDENCY_DIR / "csilgen-generators.tar.gz"
    try:
        _download(chosen.url, archive, f"the csilgen generators from {release.tag}")
    except ToolFailed:
        archive.unlink(missing_ok=True)
        return False
    taken = csilgen_release.extract_members(
        archive, re.compile(r"^csilgen_.+_generator\.wasm$"), GENERATOR_DIR
    )
    archive.unlink(missing_ok=True)
    if taken:
        say(f"Installed {len(taken)} generators from {chosen.name}.")
    return bool(taken)


def _download_generator(name: str) -> bool:
    """Take one published generator archive, when csilgen has published it."""
    import tarfile

    wasm = GENERATOR_DIR / f"csilgen_{name}_generator.wasm"
    archive = GENERATOR_DIR / f"csilgen-generator-{name}.tar.gz"
    GENERATOR_DIR.mkdir(parents=True, exist_ok=True)
    url = (
        "https://github.com/catalystcommunity/csilgen/releases/download/"
        f"generator-{name}/v{GENERATOR_VERSION}"
        f"/csilgen-generator-{name}-{GENERATOR_VERSION}.tar.gz"
    )
    try:
        _download(url, archive, f"the {name} generator {GENERATOR_VERSION}", quiet=True)
    except ToolFailed:
        archive.unlink(missing_ok=True)
        return False
    try:
        with tarfile.open(archive) as bundle:
            member = next(
                (m for m in bundle.getmembers() if Path(m.name).name == wasm.name), None
            )
            if member is None:
                return False
            member.name = wasm.name
            bundle.extract(member, GENERATOR_DIR, filter="data")
    finally:
        archive.unlink(missing_ok=True)
    return wasm.is_file()


def fetch_csilgen_generators(force: bool = False) -> Path:
    """Put the WASM generators where csilgen looks for them.

    A published archive first. csilgen builds one for each language in its own
    release job, and downloading three files beats compiling them.
    """
    from .commands import require

    if not force and all(path.is_file() for path in generator_files()):
        return GENERATOR_DIR

    missing = [
        name
        for name in GENERATOR_TARGETS
        if not (GENERATOR_DIR / f"csilgen_{name}_generator.wasm").is_file()
    ]

    # One tarball of every generator, which is what the combined release
    # carries. It costs the same as one and covers targets this repository does
    # not generate today.
    if _download_all_generators():
        if all(path.is_file() for path in generator_files()):
            return GENERATOR_DIR

    still_missing = [name for name in missing if not _download_generator(name)]
    if not still_missing:
        say(f"Installed {len(missing)} generators from the csilgen release.")
        return GENERATOR_DIR

    warn(
        "csilgen publishes no archive for "
        + ", ".join(still_missing)
        + f" at {GENERATOR_VERSION}, so they are built from the pinned checkout."
    )

    checkout = fetch_csilgen()
    cargo = require(
        "cargo",
        "Install the Rust toolchain from https://rustup.rs and run this again.",
    )
    rustup = _which_program("rustup")
    if rustup:
        # The generators are WASM. A toolchain without that target builds
        # nothing, and says so in a way nobody expects here.
        run([rustup, "target", "add", "wasm32-unknown-unknown"], check=False)

    say(f"Building the {len(GENERATOR_TARGETS)} csilgen generators this repository uses")
    packages = []
    for name in GENERATOR_TARGETS:
        packages.extend(["--package", f"csilgen-{name}-generator"])
    # `--target-dir` explicitly, rather than trusting the default: a caller
    # with `CARGO_TARGET_DIR` set builds somewhere else entirely, and the
    # generators are then read from a directory that stays empty. A run-local
    # job with a scratch target directory found exactly that.
    built_root = checkout / "target"
    run(
        [
            cargo, "build", "--release",
            "--target", "wasm32-unknown-unknown",
            "--target-dir", str(built_root),
            *packages,
        ],
        cwd=checkout,
    )

    import shutil

    GENERATOR_DIR.mkdir(parents=True, exist_ok=True)
    built = built_root / "wasm32-unknown-unknown" / "release"
    for name in GENERATOR_TARGETS:
        source = built / f"csilgen_{name}_generator.wasm"
        if not source.is_file():
            raise ToolFailed(
                f"The {name} generator did not build at {source}. csilgen's "
                "package list may have moved; see its tools/xtask."
            )
        shutil.copy(source, GENERATOR_DIR / source.name)
    say(f"Installed {len(GENERATOR_TARGETS)} generators into {GENERATOR_DIR}.")
    return GENERATOR_DIR


def _which_program(name: str) -> str | None:
    from .commands import which as _which

    return _which(name)


def show() -> int:
    """Say what the csilgen release holds, and what this would take from it.

    One command to run on the day csilgen publishes a combined release: it
    names the tag it found, the asset it would take for each artifact, and
    every asset the release carries. A pattern that matches nothing is then a
    five-minute fix rather than a failed job.
    """
    from . import csilgen_release

    system, machine = _platform_target()
    csilgen_machine = {"amd64": "x86_64", "arm64": "aarch64"}[machine]
    print(csilgen_release.describe(CSILGEN_VERSION, system, csilgen_machine))
    return 0
