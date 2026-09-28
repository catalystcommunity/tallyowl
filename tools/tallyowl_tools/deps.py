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

#: Which csilgen release the unpacked transport came from. It is written after
#: the unpack, so its presence says the unpack finished and its content says
#: which pin it finished for. A transport from a clone has no stamp.
TRANSPORT_STAMP = TYPESCRIPT_TRANSPORT / ".csilgen-release"


def transport_is_pinned() -> bool:
    """Whether the transport on disk is the one `CSILGEN_VERSION` names."""
    if not (TYPESCRIPT_TRANSPORT / "package.json").is_file():
        return False
    try:
        return TRANSPORT_STAMP.read_text().strip() == CSILGEN_VERSION
    except OSError:
        return False


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
    clones the repository instead. "Carries no asset" is a 404 and nothing
    else. A rate limit or a lost network is raised: read as "no asset", it
    gives one machine the release transport and the next one a clone of an
    older tag, and the two then build different packages.
    """
    from . import csilgen_release

    if not force and transport_is_pinned():
        return True

    url = _csilgen_asset_url(
        "transport-typescript", f"csilgen-transport-typescript-{CSILGEN_VERSION}.tar.gz"
    )
    archive = DEPENDENCY_DIR / "csilgen-transport-typescript.tar.gz"
    try:
        _download(
            url,
            archive,
            f"the TypeScript transport from {CSILGEN_TAG}",
            advice=CSILGEN_DOWNLOAD_ADVICE,
        )
    except NotPublished:
        return False
    csilgen_release.extract_tree(archive, TYPESCRIPT_TRANSPORT, marker="src/index.ts")
    archive.unlink(missing_ok=True)

    if not (TYPESCRIPT_TRANSPORT / "src" / "index.ts").is_file():
        raise ToolFailed(
            f"{url} unpacked into {TYPESCRIPT_TRANSPORT} and there is no "
            "`src/index.ts` in it. Eight files import that path; check what the "
            "archive holds and how many leading directories to drop."
        )
    TRANSPORT_STAMP.write_text(CSILGEN_VERSION + "\n")
    return True


def fetch_csilgen(force: bool = False) -> Path:
    """Put the pinned csilgen revision in `.deps/csilgen`.

    The transport is the only thing this checkout is for. When the csilgen
    release publishes the transport as an asset, that is taken instead and this
    clones nothing: a 200 MB checkout to read one directory is a poor trade
    when the directory is downloadable.

    This is idempotent. A transport already unpacked from the pinned release
    costs one file read, and a checkout already at the pinned revision costs
    one `rev-parse`.
    """
    if fetch_transport(force):
        return CSILGEN_CHECKOUT

    has_transport = (TYPESCRIPT_TRANSPORT / "package.json").is_file()
    if not force and has_transport and current_revision() == _resolved_transport_revision():
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

#: `kind-check` makes a disposable cluster with these. The runner image has
#: neither. kind decides the Kubernetes version of the node image it makes, and
#: this kubectl is within the supported skew of it.
KIND_VERSION = "0.31.0"
KIND_PATH = DEPENDENCY_DIR / "bin" / "kind"
KUBECTL_VERSION = "1.36.4"
KUBECTL_PATH = DEPENDENCY_DIR / "bin" / "kubectl"

#: The first npm with `npm stage publish`, to the minor.
#:
#: npm deprecated the 2FA-bypass granular token in August 2026 and removes its
#: publish capability in January 2027, so a release token stages a publish and
#: a person approves it with 2FA. A token that can publish outright is a token
#: this project would rather not hold. See L180.
#:
#: **The major is not enough.** `npm stage` arrived in 11.16.0, not in 11.0.
#: Release 0.2.0 shipped `npm stage publish` against the npm 11.13.0 that Node
#: 26.1.0 carries, and npm answered `Unknown command: "stage"` after four
#: publishers had already made the release public. See L190.
NPM_STAGE_MINIMUM = "11.16.0"

#: The Node version the client packages are packed and published with, and the
#: npm it carries. Both are written down because the second is the one that
#: matters and neither is visible in the other. `tools/tests/test_deps.py`
#: refuses a Node whose npm is older than `NPM_STAGE_MINIMUM`.
#:
#: Node 26.9.0 carries npm 11.19.1. Check
#: https://nodejs.org/dist/index.json before changing this: the `npm` field of
#: the release is the figure to copy here.
NODE_VERSION = "26.9.0"
NODE_NPM_VERSION = "11.19.1"
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


#: The SHA-256 of each pinned download, by URL. `_download` refuses a file
#: whose digest differs, and installs nothing from it.
#:
#: By URL rather than by file name, because the Docker client has one file name
#: for every architecture. An entry moves with its version constant: a version
#: bump without a new digest here fails at the download, which is the point.
#:
#: A download with no entry is still installed, and `_download` prints its
#: digest so that the entry is one paste. Never write a digest here that did
#: not come from a download somebody checked.
#: Each value was measured from two separate downloads over TLS that agreed.
#: The release publishes no checksum file, so this is trust on first use: it
#: proves that a later download is the file that was checked, not that the
#: first one was genuine. A platform that is missing here is not measured yet,
#: and its download prints the digest to add.
_CSILGEN_RELEASE = "https://github.com/catalystcommunity/csilgen/releases/download/csilgen/v0.2.9"
PINNED_SHA256: dict[str, str] = {
    # kind and kubectl publish a SHA-256 file beside each binary, and these
    # values are copied from those files, not measured here.
    "https://github.com/kubernetes-sigs/kind/releases/download/v0.31.0/kind-linux-amd64":
        "eb244cbafcc157dff60cf68693c14c9a75c4e6e6fedaf9cd71c58117cb93e3fa",
    "https://github.com/kubernetes-sigs/kind/releases/download/v0.31.0/kind-linux-arm64":
        "8e1014e87c34901cc422a1445866835d1e666f2a61301c27e722bdeab5a1f7e4",
    "https://dl.k8s.io/release/v1.36.4/bin/linux/amd64/kubectl":
        "8b8f088da2dab964f853b38464033b1be15ede2839eca751482357c45abdd05a",
    "https://dl.k8s.io/release/v1.36.4/bin/linux/arm64/kubectl":
        "0ecf44450ee6063bf19dd166a103ee6df4a9034455c2abce626e6eea657d73fb",
    # Measured from two downloads that agreed. The Corndogs release publishes
    # no checksum file.
    "https://github.com/catalystcommunity/corndogs/releases/download/helm_chart%2Fv0.5.7/corndogs-0.5.7.tgz":
        "331363fc8b45c10486ee4b506637a57faa55861e108fe91cad1bf7195f01d454",
    f"{_CSILGEN_RELEASE}/csilgen-0.2.9-x86_64-unknown-linux-gnu.tar.gz":
        "9977d7f4dd9b2ccffc395d4ec968e18517b75821c96d67f349842028b8e81bf9",
    f"{_CSILGEN_RELEASE}/csilgen-generators-0.2.9.tar.gz":
        "34b892b1c84147a2da00e5b25381bc2c277b3b060651cc309d7caa31e9b0af0d",
    f"{_CSILGEN_RELEASE}/csilgen-transport-typescript-0.2.9.tar.gz":
        "1ca0e88405f3b4cb0ee2ab57caaf91f44ca3847c4f7505d34705d5c46567ecc9",
}

#: What a person does when a fetch fails. csilgen has its own, because the
#: pinned csilgen is the only one this repository runs.
DOWNLOAD_ADVICE = (
    "Check the network and run `./tools.sh deps` again, or install it yourself "
    "and put it on the path."
)
CSILGEN_DOWNLOAD_ADVICE = (
    "Check the network and run `./tools.sh deps` again. A csilgen on the path "
    "is not used, because generated code is compared against the pinned one."
)


class NotPublished(ToolFailed):
    """The host answered 404: this file does not exist at this URL.

    Separate from every other failure, because only this one permits a caller
    to use another source. A timeout that is read as "not published" sends one
    machine to the release asset and the next to a build from a checkout.
    """


def _sha256(path: Path) -> str:
    import hashlib

    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _download(
    url: str,
    destination: Path,
    what: str,
    quiet: bool = False,
    advice: str = DOWNLOAD_ADVICE,
) -> Path:
    """Fetch one pinned release archive.

    `curl` first, because a release host answers a Python library's request
    with a 503 and answers curl with the file. Python is the fallback for a
    machine that has no curl, which the release runner is not.

    The bytes land under a temporary name. They move to `destination` only when
    the download is complete and its digest agrees with `PINNED_SHA256`, so a
    file at `destination` is always a whole, checked file.
    """
    from .commands import which as _which

    destination.parent.mkdir(parents=True, exist_ok=True)
    partial = destination.with_name(f".{destination.name}.part")
    partial.unlink(missing_ok=True)
    if not quiet:
        say(f"Fetching {what}")

    fetched = False
    curl = _which("curl")
    if curl:
        result = run(
            [curl, "--fail", "--silent", "--show-error", "--location",
             "--max-time", "300", "--write-out", "%{http_code}",
             "--output", str(partial), url],
            check=False,
            quiet=quiet,
            capture=True,
        )
        fetched = result.ok and partial.is_file() and partial.stat().st_size > 0
        if not fetched and result.stdout.strip().endswith("404"):
            partial.unlink(missing_ok=True)
            raise NotPublished(f"{what} is not published at {url}.")

    if not fetched:
        import urllib.error
        import urllib.request

        request = urllib.request.Request(url, headers={"User-Agent": "tallyowl-tools"})
        try:
            with urllib.request.urlopen(request, timeout=300) as response:  # noqa: S310
                partial.write_bytes(response.read())
        except urllib.error.HTTPError as error:
            partial.unlink(missing_ok=True)
            if error.code == 404:
                raise NotPublished(f"{what} is not published at {url}.") from error
            raise ToolFailed(
                f"{what} could not be fetched from {url}: the host answered "
                f"{error.code}. {advice}"
            ) from error
        except Exception as error:
            partial.unlink(missing_ok=True)
            raise ToolFailed(
                f"{what} could not be fetched from {url}: {error}. {advice}"
            ) from error

    found = _sha256(partial)
    wanted = PINNED_SHA256.get(url)
    if wanted is None:
        if not quiet:
            say(
                f"No SHA-256 is pinned for {url}. This download has {found}. To "
                "pin it, add that to PINNED_SHA256 in tools/tallyowl_tools/deps.py."
            )
    elif found != wanted.lower():
        partial.unlink(missing_ok=True)
        raise ToolFailed(
            f"{what} does not match its pinned SHA-256, and it was not "
            f"installed. Pinned: {wanted}. Downloaded: {found}. When the version "
            "moved on purpose, put the new digest in PINNED_SHA256 in "
            "tools/tallyowl_tools/deps.py. Otherwise do not use this download."
        )
    import os

    os.replace(partial, destination)
    return destination


def _install_binary(archive: Path, destination: Path, member: str | None = None) -> Path:
    """Take one program out of a release archive and put it at `destination`.

    `member` is its path inside the archive. Without one, the first member with
    the destination's file name is taken, wherever it sits.

    The program is written under a temporary name, made executable, and moved
    into place. Every fetch here reads "the file is there" as "the tool is
    installed", so a short file under the real name would be run as the tool.
    """
    import os
    import tarfile

    partial = f".{destination.name}.part"
    with tarfile.open(archive) as bundle:
        if member is None:
            taken = next(
                (m for m in bundle.getmembers() if Path(m.name).name == destination.name),
                None,
            )
        else:
            taken = next((m for m in bundle.getmembers() if m.name == member), None)
        if taken is None or not taken.isfile():
            archive.unlink(missing_ok=True)
            raise ToolFailed(
                f"{archive.name} holds no `{member or destination.name}`, so "
                f"{destination.name} was not installed. The release may have "
                "changed how it lays out its archive."
            )
        taken.name = partial
        bundle.extract(taken, destination.parent, filter="data")
    archive.unlink(missing_ok=True)
    staged = destination.parent / partial
    staged.chmod(0o755)
    os.replace(staged, destination)
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

    system, machine = _platform_target()
    archive = _download(
        f"https://get.helm.sh/helm-v{HELM_VERSION}-{system}-{machine}.tar.gz",
        HELM_PATH.parent / "helm.tar.gz",
        f"Helm {HELM_VERSION} for {system}-{machine}",
    )
    return _install_binary(archive, HELM_PATH, f"{system}-{machine}/helm")


def _install_plain(download: Path, destination: Path) -> Path:
    """Put a program that is published as one bare file at `destination`.

    The same rule as `_install_binary`: executable first, then moved into place
    under its real name, so a short file is never run as the tool.
    """
    import os

    download.chmod(0o755)
    os.replace(download, destination)
    return destination


def fetch_kind(force: bool = False) -> Path:
    """Fetch the pinned kind release into `.deps/bin`."""
    from .commands import which as _which

    if not force and (_which("kind") or KIND_PATH.is_file()):
        return Path(_which("kind") or KIND_PATH)
    system, machine = _platform_target()
    download = _download(
        f"https://github.com/kubernetes-sigs/kind/releases/download/v{KIND_VERSION}/kind-{system}-{machine}",
        KIND_PATH.parent / ".kind.download",
        f"kind {KIND_VERSION} for {system}-{machine}",
    )
    return _install_plain(download, KIND_PATH)


def fetch_kubectl(force: bool = False) -> Path:
    """Fetch the pinned kubectl release into `.deps/bin`."""
    from .commands import which as _which

    if not force and (_which("kubectl") or KUBECTL_PATH.is_file()):
        return Path(_which("kubectl") or KUBECTL_PATH)
    system, machine = _platform_target()
    download = _download(
        f"https://dl.k8s.io/release/v{KUBECTL_VERSION}/bin/{system}/{machine}/kubectl",
        KUBECTL_PATH.parent / ".kubectl.download",
        f"kubectl {KUBECTL_VERSION} for {system}-{machine}",
    )
    return _install_plain(download, KUBECTL_PATH)


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
    return _install_binary(archive, SEMVER_TAGS_PATH, "semver-tags")


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
    return _install_binary(archive, CRANE_PATH, "crane")


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

    system, machine = _platform_target()
    release = f"gh_{GH_VERSION}_{system}_{machine}"
    archive = _download(
        f"https://github.com/cli/cli/releases/download/v{GH_VERSION}/{release}.tar.gz",
        GH_PATH.parent / "gh.tar.gz",
        f"the GitHub command line {GH_VERSION} for {system}-{machine}",
    )
    return _install_binary(archive, GH_PATH, f"{release}/bin/gh")


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
    return _install_binary(archive, DOCKER_PATH, "docker/docker")


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
CSILGEN_VERSION = "0.2.9"

#: csilgen tags the combined release with this prefix.
CSILGEN_TAG = f"csilgen/v{CSILGEN_VERSION}"

#: The vendor and libc parts of the Rust target triple in a csilgen asset name,
#: for each system. The Linux build links glibc; csilgen has no musl build.
CSILGEN_TRIPLE_TAILS = {"linux": "unknown-linux-gnu", "darwin": "apple-darwin"}
CSILGEN_PATH = DEPENDENCY_DIR / "bin" / "csilgen"


def csilgen_binary() -> str:
    """The pinned generator, with its target plugins, fetching what is missing.

    The pinned one comes first, before anything on the path: a generator on a
    developer's path is whatever they built last, and `gen-check` would then
    pass here and fail in CI.

    The binary alone is not enough. csilgen loads a WASM generator for each
    target, and without them it refuses every target it was asked for.
    """
    fetch_csilgen_binary()
    fetch_csilgen_generators()
    return str(CSILGEN_PATH)


def installed_csilgen_version(program: Path | None = None) -> str | None:
    """The version the csilgen at `.deps/bin` reports, or `None`.

    A release build prints `csilgen <version>`, and the version is the release
    it came from. A build somebody made by hand prints the crate version, which
    is never the pin, so it is fetched over. That is the intent: the pinned
    generator is the only one this repository runs.
    """
    import re

    program = program or CSILGEN_PATH
    if not program.is_file():
        return None
    try:
        result = run([str(program), "--version"], capture=True, check=False, quiet=True)
    except (ToolFailed, OSError):
        # A short or foreign file under the name. It is not the pin.
        return None
    found = re.search(r"csilgen\s+(\d+\.\d+\.\d+\S*)", result.stdout) if result.ok else None
    return found.group(1) if found else None


def _csilgen_asset_url(kind: str, name: str, system: str = "", machine: str = "") -> str:
    """Where one csilgen release asset is: from the asset list, or by its name.

    The asset list is the better source, because it survives a renamed asset.
    It is not the only source. When GitHub does not answer, the published name
    gives the same file, so what a machine installs does not depend on whether
    the API answered.
    """
    from . import csilgen_release

    url = f"https://github.com/catalystcommunity/csilgen/releases/download/{CSILGEN_TAG}/{name}"
    try:
        release = csilgen_release.find_release(CSILGEN_VERSION)
    except ToolFailed as error:
        warn(f"{error} Taking {name} by its published name instead.")
        return url
    if release:
        chosen = csilgen_release.pick(release, kind, system, machine)
        if chosen:
            return chosen.url
    return url


def fetch_csilgen_binary(force: bool = False) -> Path:
    """Fetch the pinned csilgen command line into `.deps/bin`.

    From the release's own asset list when that release can be read, and from
    the target-triple name csilgen publishes since 0.2.8 when it cannot.

    "It is there" is not "it is the pin". A machine that fetched 0.2.7 keeps
    that file for ever, and `gen-check` then disagrees with a machine that
    started clean. So the file is asked for its version, and fetched again when
    the answer is not `CSILGEN_VERSION`.
    """
    have = installed_csilgen_version()
    if not force and have == CSILGEN_VERSION:
        return CSILGEN_PATH
    if CSILGEN_PATH.is_file() and have != CSILGEN_VERSION:
        say(
            f"The csilgen at {CSILGEN_PATH} is {have or 'not a release build'} and "
            f"the pin is {CSILGEN_VERSION}. Fetching {CSILGEN_VERSION}."
        )

    system, machine = _platform_target()
    # csilgen names the architecture the way the compiler does.
    csilgen_machine = {"amd64": "x86_64", "arm64": "aarch64"}[machine]

    url = _csilgen_asset_url(
        "cli",
        f"csilgen-{CSILGEN_VERSION}-{csilgen_machine}-{CSILGEN_TRIPLE_TAILS[system]}.tar.gz",
        system,
        csilgen_machine,
    )
    archive = _download(
        url,
        CSILGEN_PATH.parent / "csilgen.tar.gz",
        f"csilgen {CSILGEN_VERSION} for {system}-{csilgen_machine}",
        advice=CSILGEN_DOWNLOAD_ADVICE,
    )
    _install_binary(archive, CSILGEN_PATH)

    now = installed_csilgen_version()
    if now != CSILGEN_VERSION:
        raise ToolFailed(
            f"The csilgen from {url} reports version {now or 'nothing'}, and the "
            f"pin is {CSILGEN_VERSION}. Check CSILGEN_VERSION and CSILGEN_TAG in "
            "tools/tallyowl_tools/deps.py against the csilgen release page."
        )
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
    return _install_binary(archive, CARGO_AUDIT_PATH)


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
    return _install_binary(archive, CARGO_DENY_PATH)


# ---------------------------------------------------------------------------
# The csilgen generators
#
# csilgen loads a WASM generator for each target, and the release archive
# carries only the binary. A machine with the binary and no generators refuses
# with "Unknown target 'rust'", which is what a run-local job found.
#
# **They are installed for this repository, not for the user.** csilgen reads
# `.generators` under its working directory before `~/.csilgen/generators`, and
# the first one it finds for a target wins. `./tools.sh gen` runs csilgen from
# `csil/`, so `csil/.generators` is a link to `.deps/csilgen-generators/<pin>`.
#
# The home directory is one flat directory for every repository on the machine.
# It cannot hold two versions, so a pin written there is either ignored (what
# happened: a 0.2.8 pin and 0.2.7 generators) or it takes another repository's
# generators away. This repository no longer writes there, and no longer
# depends on what is there.
#
# **The fallback is the one provisioning step that compiles.** When csilgen
# publishes no generator archive for the pin, the three generators are built
# from the csilgen source at the pinned tag. See L185.
# ---------------------------------------------------------------------------

#: The three targets this repository generates. Building the other twelve would
#: cost minutes for output nobody here reads.
GENERATOR_TARGETS = ("rust", "go", "typescript")

#: The generator release csilgen publishes each WASM module under. csilgen's
#: release job archives one for each language as
#: `csilgen-generator-<language>-<version>.tar.gz` on the tag
#: `generator-<language>/v<version>`.
#:
#: **Fetched when it is there, built when it is not.** "Not there" is a 404. A
#: failure of any other kind stops the run, because a build from the checkout
#: is a different generator from the published one.
GENERATOR_VERSION = "0.2.9"

#: One directory for each pin, so a new pin never overwrites the old one in
#: place, and "is the pin installed" is "does its directory hold `.complete`".
GENERATOR_STORE = DEPENDENCY_DIR / "csilgen-generators"
GENERATOR_DIR = GENERATOR_STORE / GENERATOR_VERSION

#: Written last, after every generator is in the directory.
GENERATOR_COMPLETE = ".complete"

#: Where csilgen looks first when it runs from `csil/`.
GENERATOR_LINK = REPOSITORY_ROOT / "csil" / ".generators"


def _generator_names() -> list[str]:
    return [f"csilgen_{name}_generator.wasm" for name in GENERATOR_TARGETS]


def generator_files() -> list[Path]:
    """What must exist for `csilgen generate` to know a target."""
    return [GENERATOR_DIR / name for name in _generator_names()]


def generators_are_pinned() -> bool:
    """Whether the generators for `GENERATOR_VERSION` are installed and whole."""
    return (GENERATOR_DIR / GENERATOR_COMPLETE).is_file() and all(
        path.is_file() for path in generator_files()
    )


def link_generators() -> Path:
    """Point `csil/.generators` at the pinned generators.

    A link rather than a copy, so the 9 MB of generators stay under `.deps`,
    which Git and the image build already ignore.
    """
    import os

    target = os.path.relpath(GENERATOR_DIR, GENERATOR_LINK.parent)
    if GENERATOR_LINK.is_symlink():
        if os.readlink(GENERATOR_LINK) == target:
            return GENERATOR_LINK
    elif GENERATOR_LINK.exists():
        raise ToolFailed(
            f"{GENERATOR_LINK} is a directory that `./tools.sh` did not make. "
            "csilgen reads it before the pinned generators, so the generated "
            "code would come from whatever it holds. Move it away and run "
            "`./tools.sh deps` again."
        )
    staged = GENERATOR_LINK.with_name(".generators.part")
    staged.unlink(missing_ok=True)
    staged.symlink_to(target, target_is_directory=True)
    os.replace(staged, GENERATOR_LINK)
    return GENERATOR_LINK


def _download_all_generators(into: Path) -> bool:
    """Take the whole generator tarball out of the combined csilgen release."""
    import re

    from . import csilgen_release

    url = _csilgen_asset_url("generators", f"csilgen-generators-{CSILGEN_VERSION}.tar.gz")
    archive = DEPENDENCY_DIR / "csilgen-generators.tar.gz"
    try:
        _download(
            url,
            archive,
            f"the csilgen generators from {CSILGEN_TAG}",
            advice=CSILGEN_DOWNLOAD_ADVICE,
        )
    except NotPublished:
        return False
    taken = csilgen_release.extract_members(
        archive, re.compile(r"^csilgen_.+_generator\.wasm$"), into
    )
    archive.unlink(missing_ok=True)
    if taken:
        say(f"Took {len(taken)} generators from {url.rsplit('/', 1)[-1]}.")
    return bool(taken)


def _download_generator(name: str, into: Path) -> bool:
    """Take one published generator archive, when csilgen has published it."""
    import re

    from . import csilgen_release

    wasm = f"csilgen_{name}_generator.wasm"
    archive = DEPENDENCY_DIR / f"csilgen-generator-{name}.tar.gz"
    url = (
        "https://github.com/catalystcommunity/csilgen/releases/download/"
        f"generator-{name}/v{GENERATOR_VERSION}"
        f"/csilgen-generator-{name}-{GENERATOR_VERSION}.tar.gz"
    )
    try:
        _download(
            url,
            archive,
            f"the {name} generator {GENERATOR_VERSION}",
            quiet=True,
            advice=CSILGEN_DOWNLOAD_ADVICE,
        )
    except NotPublished:
        return False
    try:
        csilgen_release.extract_members(archive, re.compile(f"^{re.escape(wasm)}$"), into)
    finally:
        archive.unlink(missing_ok=True)
    return (into / wasm).is_file()


def fetch_csilgen_generators(force: bool = False) -> Path:
    """Put the pinned WASM generators where csilgen looks for them first.

    A published archive first. csilgen builds one for each language in its own
    release job, and downloading three files beats compiling them.

    The generators are gathered in a directory beside the real one, and that
    directory takes the real name only when it holds all of them and its
    `.complete` file. A run that stops part way leaves nothing a later run
    could take for an installed pin.
    """
    import shutil

    from . import csilgen_release

    if not force and generators_are_pinned():
        link_generators()
        return GENERATOR_DIR

    staged = GENERATOR_STORE / f".{GENERATOR_VERSION}.part"
    if staged.exists():
        shutil.rmtree(staged)
    staged.mkdir(parents=True)

    def missing() -> list[str]:
        return [
            name
            for name in GENERATOR_TARGETS
            if not (staged / f"csilgen_{name}_generator.wasm").is_file()
        ]

    # One tarball of every generator, which is what the combined release
    # carries. It costs the same as one and covers targets this repository does
    # not generate today.
    _download_all_generators(staged)

    wanted = missing()
    still_missing = [name for name in wanted if not _download_generator(name, staged)]
    if wanted and not still_missing:
        say(f"Took {len(wanted)} generators from the csilgen generator releases.")

    if still_missing:
        warn(
            "csilgen publishes no archive for "
            + ", ".join(still_missing)
            + f" at {GENERATOR_VERSION}, so they are built from the csilgen source at {CSILGEN_TAG}."
        )
        _build_generators(still_missing, staged)

    (staged / GENERATOR_COMPLETE).write_text(GENERATOR_VERSION + "\n")
    csilgen_release.replace_tree(staged, GENERATOR_DIR)
    link_generators()
    say(f"Installed the {GENERATOR_VERSION} generators into {GENERATOR_DIR}.")
    return GENERATOR_DIR


#: The csilgen source the generators are built from, when they must be built.
#: Apart from `.deps/csilgen`, which is the TypeScript transport and, since the
#: release carries it, holds no source at all.
CSILGEN_SOURCE = DEPENDENCY_DIR / "csilgen-source"


def _csilgen_source() -> Path:
    """A csilgen checkout at the tag the generator pin names."""
    git = _git()
    DEPENDENCY_DIR.mkdir(parents=True, exist_ok=True)
    if not (CSILGEN_SOURCE / ".git").exists():
        say(f"Fetching the csilgen source at {CSILGEN_TAG}")
        sibling = REPOSITORY_ROOT.parent / "csilgen"
        source = str(sibling) if (sibling / ".git").exists() else CSILGEN_REMOTE
        run([git, "clone", "--quiet", source, str(CSILGEN_SOURCE)])
    else:
        run([git, "fetch", "--quiet", "--tags", "origin"], cwd=CSILGEN_SOURCE, check=False)
    result = run([git, "checkout", "--quiet", CSILGEN_TAG], cwd=CSILGEN_SOURCE, check=False)
    if not result.ok:
        raise ToolFailed(
            f"csilgen {CSILGEN_TAG} could not be checked out in {CSILGEN_SOURCE}. "
            "The pin is CSILGEN_VERSION in tools/tallyowl_tools/deps.py."
        )
    return CSILGEN_SOURCE


def _build_generators(names: list[str], into: Path) -> None:
    """Build the named generators from the csilgen source at the pin, into `into`."""
    import shutil

    checkout = _csilgen_source()
    cargo = require(
        "cargo",
        "Install the Rust toolchain from https://rustup.rs and run this again.",
    )
    rustup = _which_program("rustup")
    if rustup:
        # The generators are WASM. A toolchain without that target builds
        # nothing, and says so in a way nobody expects here.
        run([rustup, "target", "add", "wasm32-unknown-unknown"], check=False)

    say(f"Building the {len(names)} csilgen generators that are not published")
    packages = []
    for name in names:
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

    built = built_root / "wasm32-unknown-unknown" / "release"
    for name in names:
        source = built / f"csilgen_{name}_generator.wasm"
        if not source.is_file():
            raise ToolFailed(
                f"The {name} generator did not build at {source}. csilgen's "
                "package list may have moved; see its tools/xtask."
            )
        shutil.copy(source, into / source.name)


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
