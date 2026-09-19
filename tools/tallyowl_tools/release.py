"""Cut a version, prove every version site agrees, and plan the publish.

A version lives in nineteen places here: a Cargo workspace and its path
dependencies, four CSIL specifications and everything generated from them, two
Helm charts, three package manifests, two app-driver constants, two Go module
requirement lists, and the tooling's own manifest. Nobody can hold that list in
their head, so the list lives here and one verb writes all of it.

Three rules this module exists to hold:

- **one version for the whole tree.** A chart that says 0.1.0 and an image that
  says 0.0.0 is an installation that pulls a tag which does not exist. `version
  check` fails the build on any disagreement, and CI runs it;
- **the generated packages are checked, never stamped.** `csilgen` writes them
  from `package_version` in the specification. A stamp here would drift from
  the generator, which is what `gen-check` exists to catch;
- **a refusal names what is missing.** The publish job refuses without a grant,
  and the crates.io plan says exactly which crate names a clearance covers.

See docs/CI-CD.md section 6 and D31.
"""

from __future__ import annotations

import json
import re
import tomllib
from dataclasses import dataclass
from pathlib import Path

from . import generate
from .commands import REPOSITORY_ROOT, ToolFailed, say, warn

#: What this project accepts as a version: three numbers, and an optional
#: release-candidate suffix. Cargo, npm, Helm, and a Go module tag all read
#: this shape the same way, which a looser one does not guarantee.
VERSION = re.compile(r"^\d+\.\d+\.\d+(?:-rc\.\d+)?$")


@dataclass(frozen=True)
class Site:
    """One place a version is written, and how to find it there."""

    #: Relative to the repository root.
    path: str
    #: One group, around the version and nothing else.
    pattern: str
    #: What a person calls this place.
    what: str
    #: True when `./tools.sh gen` writes it. Checked here, never stamped.
    generated: bool = False
    #: Set for a JSON file whose version is a key rather than a line. A lock
    #: file holds the version twice and holds every dependency's version in the
    #: same shape, so a regular expression cannot tell them apart. Each entry
    #: is a path of keys, and `""` is npm's name for the package itself.
    json_keys: tuple[tuple[str, ...], ...] | None = None


#: Every version site in the repository. Add a site here the day a file starts
#: carrying a version, because a site nobody listed is a site nobody checks.
SITES: tuple[Site, ...] = (
    Site("Cargo.toml", r'(?m)^version = "([^"]+)"$', "the Rust workspace"),
    Site(
        "Cargo.toml",
        r'(?m)^tallyowl-[a-z-]+ = \{ path = "[^"]+", version = "([^"]+)" \}$',
        "the Rust path dependencies, which crates.io resolves by version",
    ),
    Site("tools/pyproject.toml", r'(?m)^version = "([^"]+)"$', "the tooling"),
    *(
        Site(
            f"csil/tallyowl-{name}.csil",
            r'(?m)^\s*package_version: "([^"]+)"',
            f"the {name} contract",
        )
        for name in ("ingest", "collector", "control", "cluster")
    ),
    *(
        site
        for chart in ("tallyowl", "tallyowl-collector")
        for site in (
            Site(f"charts/{chart}/Chart.yaml", r"(?m)^version: (\S+)$", f"the {chart} chart"),
            Site(
                f"charts/{chart}/Chart.yaml",
                r'(?m)^appVersion: "([^"]+)"$',
                f"the {chart} chart's image tag",
            ),
        )
    ),
    *(
        site
        for package in ("packages/browser", "packages/dashboard", "testbed/webapp")
        for site in (
            Site(f"{package}/package.json", r'(?m)^  "version": "([^"]+)"', f"the {package} package"),
            # npm keeps the version twice in a lock file: at the root, and in
            # the entry for the package itself. `npm install` rewrites both, so
            # a lock file left behind is a diff in somebody's branch that no
            # gate explains — and it lands after the release commit.
            Site(
                f"{package}/package-lock.json",
                "",
                f"the {package} lock file, which npm rewrites",
                json_keys=(("version",), ("packages", "", "version")),
            ),
        )
    ),
    Site(
        "packages/browser/src/capture.ts",
        r'export const SDK_VERSION = "([^"]+)"',
        "the browser package's own envelope stamp",
    ),
    Site(
        "packages/driver-go/item.go",
        r'SDKVersion = "([^"]+)"',
        "the Go app driver's own envelope stamp",
    ),
    *(
        Site(
            f"{module}/go.mod",
            r"(?m)^(?:\t|require )github\.com/CatalystCommunity/tallyowl/\S+ v(\S+)$",
            f"the Go modules {module} requires, which the module proxy resolves by tag",
        )
        for module in ("packages/driver-go", "testbed")
    ),
    *(
        Site(
            f"generated/typescript/tallyowl-{name}-api/package.json",
            r'(?m)^  "version": "([^"]+)"',
            f"the generated {name} client for TypeScript",
            generated=True,
        )
        for name in ("ingest", "collector", "control", "cluster")
    ),
    *(
        Site(
            f"generated/rust/tallyowl-{name}-api/Cargo.toml",
            r'(?m)^version = "([^"]+)"$',
            f"the generated {name} client for Rust",
            generated=True,
        )
        for name in ("ingest", "collector", "control", "cluster")
    ),
    # Cargo writes this one, and a stale entry fails a `--locked` build. The
    # service image builds with `--locked`, so a lock file left behind is a
    # release that cannot be built rather than a detail.
    Site(
        "Cargo.lock",
        r'(?m)^name = "tallyowl-[a-z-]+"\nversion = "([^"]+)"$',
        "the lock file, which a `--locked` build reads",
        generated=True,
    ),
)

#: The site the other sites are compared against. The Rust workspace holds the
#: version the services compile in, so it is the one to read when a person asks
#: what this working tree is.
SOURCE = SITES[0]


def _json_at(document: dict, keys: tuple[str, ...]):
    """Follow a path of keys, or `None` when the path is not there."""
    node = document
    for key in keys:
        if not isinstance(node, dict) or key not in node:
            return None
        node = node[key]
    return node


def read_site(site: Site, root: Path = REPOSITORY_ROOT) -> list[str]:
    """Every version this site carries. More than one means they must agree."""
    path = root / site.path
    if not path.is_file():
        raise ToolFailed(f"{site.path} is a version site and it is not there.")
    if site.json_keys:
        document = json.loads(path.read_text())
        found = [
            value
            for keys in site.json_keys
            if isinstance(value := _json_at(document, keys), str)
        ]
        if not found:
            raise ToolFailed(
                f"{site.path} holds {site.what}, and none of its version keys "
                "are there. Either the file changed shape or SITES in "
                "tools/tallyowl_tools/release.py is stale."
            )
        return found
    found = re.findall(site.pattern, path.read_text())
    if not found:
        raise ToolFailed(
            f"{site.path} holds {site.what}, and no version matched there. "
            "Either the file changed shape or SITES in "
            "tools/tallyowl_tools/release.py is stale."
        )
    return list(found)


def current_version(root: Path = REPOSITORY_ROOT) -> str:
    """What this working tree calls itself."""
    return read_site(SOURCE, root)[0]


def stamp(text: str, pattern: str, version: str) -> tuple[str, int]:
    """Write `version` over each match's group, and say how many it wrote.

    The group span is spliced rather than substituted, because a substitution
    template would have to reproduce the rest of the match, and a pattern that
    grows a group later would then write the wrong thing.
    """
    matches = list(re.finditer(pattern, text))
    for match in reversed(matches):
        start, end = match.span(1)
        text = text[:start] + version + text[end:]
    return text, len(matches)


def write_json_version(path: Path, keys: tuple[tuple[str, ...], ...], version: str) -> int:
    """Write the version at each key path, and keep the file byte-for-byte.

    npm writes a lock file as two-space JSON with a trailing newline, and
    Python writes the same bytes back, so a version cut leaves no formatting
    churn for the next person to explain.
    """
    document = json.loads(path.read_text())
    written = 0
    for path_keys in keys:
        node = document
        for key in path_keys[:-1]:
            if not isinstance(node, dict) or key not in node:
                node = None
                break
            node = node[key]
        if isinstance(node, dict) and path_keys[-1] in node:
            node[path_keys[-1]] = version
            written += 1
    path.write_text(json.dumps(document, indent=2) + "\n")
    return written


def set_version(version: str, root: Path = REPOSITORY_ROOT, regenerate: bool = True) -> int:
    """Write one version over every source site, then regenerate the clients."""
    if not VERSION.match(version):
        raise ToolFailed(
            f"`{version}` is not a version this project cuts. Use three numbers, "
            "and `-rc.N` for a release candidate: 0.1.0, or 0.1.0-rc.1. Cargo, "
            "npm, Helm, and a Go module tag all have to read it."
        )
    say(f"Cutting {version}")
    for site in SITES:
        if site.generated:
            continue
        path = root / site.path
        if site.json_keys:
            count = write_json_version(path, site.json_keys, version)
        else:
            written, count = stamp(path.read_text(), site.pattern, version)
            path.write_text(written)
        print(f"  {site.path}: {count} in {site.what}")

    if regenerate:
        # The generated clients carry `package_version`, so they are stale the
        # moment the specification changes. Regenerating here means `version
        # set` leaves a tree that `gen-check` passes.
        #
        # Always regenerate, never stamp the generated files: the generator is
        # a pinned release that fetches itself, so there is no machine without
        # one. An earlier version of this probed the path for `csilgen`, found
        # nothing — the pinned binary lives in `.deps/bin` — and quietly wrote
        # a new version string over generated code that could be stale.
        generate.generate()
        refresh_lock_file(root)

    # Without the regeneration the generated clients still say the old version,
    # and saying so is the point: the tree is not cut until they are rewritten.
    check(root, include_generated=regenerate)
    say(f"The tree is {version}. Nothing is staged, committed, or pushed; that is the owner's.")
    return 0


def refresh_lock_file(root: Path = REPOSITORY_ROOT) -> None:
    """Write the new workspace versions into Cargo.lock.

    The lock file names every crate and its version, including this
    workspace's own. The service image builds with `--locked`, which refuses a
    lock file that does not match the manifests, so a version cut that leaves
    the lock behind is a release that cannot be built. Offline first: this
    changes only the workspace members' own versions, and nothing here wants a
    dependency update as a side effect of a release.
    """
    from .commands import run, which

    cargo = which("cargo")
    if not cargo:
        warn("`cargo` is not installed, so Cargo.lock keeps the old version.")
        return
    say("Writing the new version into Cargo.lock")
    offline = run([cargo, "update", "--workspace", "--offline"], cwd=root, check=False)
    if not offline.ok:
        run([cargo, "update", "--workspace"], cwd=root)


def disagreements(
    root: Path = REPOSITORY_ROOT, include_generated: bool = True
) -> dict[str, list[str]]:
    """Each version found, against the sites that carry it."""
    by_version: dict[str, list[str]] = {}
    for site in SITES:
        if site.generated and not include_generated:
            continue
        for found in read_site(site, root):
            by_version.setdefault(found, []).append(f"{site.path} ({site.what})")
    return by_version


def check(root: Path = REPOSITORY_ROOT, include_generated: bool = True) -> int:
    """Fail unless every version site says the same thing."""
    by_version = disagreements(root, include_generated)
    if len(by_version) == 1:
        version = next(iter(by_version))
        say(f"Every version site says {version}.")
        return 0

    lines = [
        "The version sites disagree, so a chart would ask for an image tag "
        "that nobody built. Found:"
    ]
    for version, sites in sorted(by_version.items()):
        lines.append(f"  {version}")
        for site in sites:
            lines.append(f"    {site}")
    lines.append("Run `./tools.sh version set <version>` to write one version over all of them.")
    raise ToolFailed("\n".join(lines))


# ---------------------------------------------------------------------------
# The next version, and cutting it
#
# semver-tags reads the conventional commits since the last tag and says what
# the next version is. Every repository here releases that way, so TallyOwl
# does too rather than inventing a second answer to the same question.
# ---------------------------------------------------------------------------

#: The Go modules this repository publishes. A Go module in a subdirectory
#: resolves through a tag that is the module directory and then the version,
#: so each of these needs its own tag beside the repository tag.
GO_MODULES = (
    "packages/driver-go",
    "generated/go/tallyowl-ingest-api",
    "generated/go/tallyowl-collector-api",
    "generated/go/tallyowl-control-api",
    "generated/go/tallyowl-cluster-api",
)

#: What `release stamp` leaves behind, so the steps after it know whether there
#: is anything to publish. One file, in the artifact directory the same job
#: packages into.
MARKER = "release.json"

#: The release job sets this. Without it, `release stamp` computes and stops:
#: nothing in this repository commits, tags, or pushes on somebody's
#: workstation, and that includes this module.
RELEASE_ENVIRONMENT = "TALLYOWL_RELEASE"


def next_release(root: Path = REPOSITORY_ROOT) -> dict:
    """Ask semver-tags what the next version is. It changes nothing.

    Returns the tool's own JSON object, with `New_release_published` telling
    whether the commits since the last tag call for a release at all.
    """
    from . import deps
    from .commands import run

    program = deps.semver_tags_program()
    result = run(
        [program, "run", "--dry_run", "--output_json"],
        cwd=root,
        capture=True,
    )
    # The tool logs to stderr and prints one JSON object last.
    for line in reversed(result.stdout.strip().splitlines()):
        if line.startswith("{"):
            return json.loads(line)
    raise ToolFailed(
        "semver-tags printed no result. Its output was:\n" + result.stdout + result.stderr
    )


def release_plan(root: Path = REPOSITORY_ROOT) -> dict:
    """What a release from this commit would be: version, tag, and every tag."""
    computed = next_release(root)
    published = computed.get("New_release_published") == "true"
    version = computed.get("New_release_version", "")
    tag = computed.get("New_release_git_tag", "")
    return {
        "released": published,
        "version": version,
        "tag": tag,
        "go_tags": [f"{module}/v{version}" for module in GO_MODULES] if published else [],
        "notes": computed.get("New_release_notes", ""),
    }


def plan_report(root: Path = REPOSITORY_ROOT) -> int:
    """Say what the next release would be, and change nothing."""
    plan = release_plan(root)
    if not plan["released"]:
        say("No release. No commit since the last tag asks for a version change.")
        return 0
    say(f"The next release is {plan['tag']} ({plan['version']}).")
    print("  Tags it would create:")
    for tag in [plan["tag"], *plan["go_tags"]]:
        print(f"    {tag}")
    print(f"  The tree is {current_version(root)} and would be written to {plan['version']}.")
    return 0


def _git_head(root: Path) -> str:
    from .commands import run

    return run(["git", "rev-parse", "HEAD"], cwd=root, capture=True, quiet=True).stdout.strip()


def stamp_release(root: Path = REPOSITORY_ROOT) -> int:
    """Bring the tree up to date with main, then write the next version into it.

    **This commits nothing and pushes nothing.** The artifacts are built from
    what this leaves behind, and only when every one of them exists does
    `release tag` make the version public. A tag is the one thing in a release
    that cannot be taken back: the module proxy caches it, so a tag with no
    artifacts behind it is a dead version for ever.

    The rebase happens **before** the version is computed, not after. The other
    order releases a commit that the version was not computed from: a `feat:`
    that merges while this job runs would ship inside a patch release, and
    because the next run only reads commits after the last tag, the minor bump
    it asked for would never be applied.

    It refuses without `TALLYOWL_RELEASE=1`, so running it by hand on a
    workstation computes the version and stops.
    """
    import os

    from .commands import run

    artifacts = artifact_directory(root)
    artifacts.mkdir(parents=True, exist_ok=True)

    releasing = os.environ.get(RELEASE_ENVIRONMENT) == "1"
    if releasing:
        # Everything on main, first. What follows is computed from it.
        run(["git", "fetch", "--tags", "--force", "origin"], cwd=root)
        pull = run(["git", "pull", "--rebase", "origin", "main"], cwd=root, check=False)
        if not pull.ok:
            run(["git", "rebase", "--abort"], cwd=root, check=False)
            raise ToolFailed(
                "This working tree could not be brought up to date with main, so "
                "the version cannot be computed from what main holds. Nothing is "
                "stamped and nothing is published."
            )

    plan = release_plan(root)
    plan["base"] = _git_head(root) if releasing else ""
    (artifacts / MARKER).write_text(json.dumps(plan, indent=2) + "\n")

    if not plan["released"]:
        say("No release. No commit since the last tag asks for a version change.")
        return 0

    version, tag = plan["version"], plan["tag"]
    if not releasing:
        say(f"The next release is {tag}. The tree stays as it is.")
        say(
            f"Only the release job cuts a version. Set {RELEASE_ENVIRONMENT}=1 "
            "if you are that job."
        )
        return 0

    set_version(version, root)
    say(f"The tree is stamped {version}. Nothing is committed until the artifacts exist.")
    return 0


def tag_release(root: Path = REPOSITORY_ROOT) -> int:
    """Commit the stamped version, tag it, and push — the last step of a release.

    Every artifact is built and verified before this runs, so a tag never names
    a version nothing was built for.

    The push is atomic and carries the branch and every tag together. If main
    moved while the artifacts were building, the push is refused and so is the
    release: the version was computed from a commit that is no longer the head,
    and the honest answer is to run again rather than to tag something else.
    """
    import os

    from .commands import run

    if os.environ.get(RELEASE_ENVIRONMENT) != "1":
        raise ToolFailed(
            f"Only the release job tags a version. It sets {RELEASE_ENVIRONMENT}=1."
        )

    plan = marker(root)
    if not plan["released"]:
        say("No release to tag.")
        return 0

    version, tag = plan["version"], plan["tag"]
    names = [tag, *plan["go_tags"]]

    # The tree must carry the version this is about to name. A stamp that did
    # not happen, or a file that changed after it, is caught here rather than
    # by whoever downloads the tag.
    check(root)
    if current_version(root) != version:
        raise ToolFailed(
            f"The tree says {current_version(root)} and this release is {version}. "
            "`release stamp` did not leave what it should have."
        )

    # Nothing but the stamp may have happened since. `stamp_release` recorded
    # the commit it computed the version from, and if something else committed
    # in between then this tag would name a tree nobody planned.
    base = plan.get("base", "")
    if base and _git_head(root) != base:
        raise ToolFailed(
            f"The version was computed from {base[:12]} and this working tree is "
            f"at {_git_head(root)[:12]}. Something committed between the stamp "
            "and the tag. Nothing is tagged."
        )

    run(["git", "add", "--all"], cwd=root)
    status = run(["git", "status", "--porcelain"], cwd=root, capture=True, quiet=True)
    if status.stdout.strip():
        # A commit that fails leaves the stamp staged and the tree unchanged, and
        # the tags would then name the version before this one. Checked, not
        # assumed: a hook, an unset identity, or a full disk all land here.
        run(["git", "commit", "--message", f"ci: release {version}"], cwd=root)
    else:
        say("The tree already holds this version. No commit was needed.")

    for name in names:
        run(["git", "tag", "--force", name], cwd=root)

    push = run(
        ["git", "push", "--atomic", "origin", "HEAD:main", *(f"refs/tags/{name}" for name in names)],
        cwd=root,
        check=False,
    )
    if not push.ok:
        for name in names:
            run(["git", "tag", "--delete", name], cwd=root, check=False)
        raise ToolFailed(
            f"{tag} could not be pushed: main moved while the artifacts were "
            "building. Nothing is tagged. Run the release again, and it will "
            "compute the version from what main holds now."
        )

    say(f"Released {tag}, with a tag for each of the {len(GO_MODULES)} Go modules.")
    return 0


def marker(root: Path = REPOSITORY_ROOT) -> dict:
    """What `release stamp` decided, for the steps that follow it."""
    path = artifact_directory(root) / MARKER
    if not path.is_file():
        raise ToolFailed(
            f"{path} is not there. `release stamp` writes it, and the steps after "
            "it read it to know whether there is a release at all."
        )
    return json.loads(path.read_text())


def version_of_tag(tag: str) -> str:
    """The version a git tag names. A Go module tag needs the `v`."""
    return tag[1:] if tag.startswith("v") else tag


def check_tag(tag: str, root: Path = REPOSITORY_ROOT) -> int:
    """Fail unless the tag being released is the version the tree carries.

    A Go consumer takes the tag itself and nothing else, so a tag that does not
    match the compiled-in version ships a driver that misreports itself for as
    long as the tag exists. A tag cannot be moved after somebody fetches it.
    """
    version = current_version(root)
    check(root)
    if not tag.startswith("v"):
        raise ToolFailed(
            f"The tag is `{tag}` and a release tag starts with `v`, because a Go "
            f"module resolves this tag directly. Use `v{version}`."
        )
    if version_of_tag(tag) != version:
        raise ToolFailed(
            f"The tag `{tag}` and the tree disagree: the tree is {version}. "
            f"Either tag `v{version}`, or run "
            f"`./tools.sh version set {version_of_tag(tag)}` first."
        )
    say(f"The tag {tag} is the version this tree carries.")
    return 0


# ---------------------------------------------------------------------------
# crates.io
# ---------------------------------------------------------------------------

#: The crate a Rust application depends on. Publishing it publishes everything
#: it depends on, which is why the clearance is a decision and not a command.
DRIVER_CRATE = "tallyowl-driver-rust"


def _manifest_paths(root: Path) -> dict[str, Path]:
    """Every workspace member, by crate name."""
    workspace = tomllib.loads((root / "Cargo.toml").read_text())
    members: dict[str, Path] = {}
    for member in workspace["workspace"]["members"]:
        manifest = root / member / "Cargo.toml"
        name = tomllib.loads(manifest.read_text())["package"]["name"]
        members[name] = manifest
    return members


def _dependencies(manifest: Path, known: set[str]) -> list[str]:
    """The workspace crates this manifest needs to build, in name order.

    Development dependencies are left out on purpose: crates.io does not need
    a dev-dependency to be published for a release to build.
    """
    parsed = tomllib.loads(manifest.read_text())
    names = set(parsed.get("dependencies", {})) | set(
        name
        for table in parsed.get("target", {}).values()
        for name in table.get("dependencies", {})
    )
    return sorted(names & known)


@dataclass(frozen=True)
class CratePlan:
    """One crate on the way to publishing the Rust app driver."""

    name: str
    #: False when the manifest says `publish = false`. Clearing it is the
    #: owner's decision, because it takes a public name on a public registry.
    publishable: bool


def crates_plan(root: Path = REPOSITORY_ROOT) -> list[CratePlan]:
    """The crates a crates.io publication covers, dependencies first.

    `cargo publish` needs every dependency on the registry already, so the
    order is the order the pushes must happen in. This is the list the owner's
    dependency clearance decides on (L167).
    """
    manifests = _manifest_paths(root)
    order: list[str] = []
    visiting: set[str] = set()

    def visit(name: str) -> None:
        if name in order:
            return
        if name in visiting:
            raise ToolFailed(f"The crate graph has a cycle at `{name}`.")
        visiting.add(name)
        for dependency in _dependencies(manifests[name], set(manifests)):
            visit(dependency)
        visiting.discard(name)
        order.append(name)

    visit(DRIVER_CRATE)
    plan = []
    for name in order:
        parsed = tomllib.loads(manifests[name].read_text())
        plan.append(CratePlan(name, parsed["package"].get("publish", True) is not False))
    return plan


def crates_report(root: Path = REPOSITORY_ROOT) -> int:
    """Say what a crates.io publication would cover, and what stops it today."""
    plan = crates_plan(root)
    version = current_version(root)
    say(f"Publishing {DRIVER_CRATE} {version} to crates.io covers {len(plan)} crates, in order:")
    for index, crate in enumerate(plan, start=1):
        state = "publishable" if crate.publishable else "`publish = false` today"
        print(f"  {index}. {crate.name} ({state})", flush=True)
    refused = [crate.name for crate in plan if not crate.publishable]
    print(
        "\nNothing here is published today. The owner turned crates.io off on\n"
        "2026-08-12 until the GitHub releases have proved themselves, and a\n"
        "release attaches the binaries and the charts instead. A Rust\n"
        "application depends on this driver by Git revision meanwhile.\n"
        "\nWhen it is turned on: each name above becomes a public name on a\n"
        "public registry, in this order, and the first push of a name cannot be\n"
        "taken back. See D31, L167, and L182.",
        flush=True,
    )
    if refused:
        warn(
            "These say `publish = false` in their manifests, which is the flag "
            "the clearance would lift: " + ", ".join(refused) + "."
        )
    return 0


# ---------------------------------------------------------------------------
# The publish grants
# ---------------------------------------------------------------------------

#: What the release job needs, and what each thing is for. The values are
#: secret references that Reactorcide resolves; nothing here reads a value.
#: The same names are in `.reactorcide/plugins/plugin_tallyowl_jobs.py`, which
#: is the copy that runs, and a test holds the two together.
GRANTS = (
    ("REGISTRY", "the container registry host"),
    ("IMAGE_PATH", "the image path inside that registry"),
    ("REGISTRY_USER", "the registry push grant, `catalystcommunity/registry:user`"),
    ("REGISTRY_PASSWORD", "the registry push grant, `catalystcommunity/registry:password`"),
    ("GITHUB_PAT", "the git and release grant, `catalystcommunity/ci:githubpat`"),
    ("CHARTS_REPO", "the chart repository the packaged charts are added to"),
    ("NPM_TOKEN", "the npmjs staging grant, `catalystcommunity/ci:npmpublish`"),
)


def missing_grants(environment: dict[str, str]) -> list[str]:
    """The names the release job has no value for.

    A refusal that names the missing grant is the difference between a person
    creating one secret and a person reading a job log to guess which.
    """
    return [name for name, _ in GRANTS if not environment.get(name, "").strip()]


def grant_refusal(missing: list[str]) -> str:
    """What the release job says when it will not run."""
    wanted = dict(GRANTS)
    lines = [
        "The release refuses rather than half-publishing. Missing:",
        *(f"  {name}: {wanted[name]}" for name in missing),
        "The coordinates are decided (D31). Each grant is a repository "
        "secret the owner creates. See docs/CI-CD.md section 6.",
    ]
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# The container images
# ---------------------------------------------------------------------------

#: Both charts run this one image and choose a binary with `command`, so there
#: is one image to build and one tag to push.
IMAGE_NAME = "tallyowl"


def image_reference(registry: str, version: str) -> str:
    """The tag the charts ask for, which is `appVersion` and therefore this."""
    return f"{registry.rstrip('/')}/{IMAGE_NAME}:{version}"


#: The container tools this project builds with, in the order it tries them.
#: `podman` comes first because the Reactorcide runner image carries podman and
#: carries no other one. Every command used here reads the same in all three.
CONTAINER_TOOLS = ("podman", "docker", "nerdctl")


def container_tool() -> str:
    """The container tool to build the image with.

    A release job declares Reactorcide's `docker` capability, which gives it a
    daemon and `DOCKER_HOST`. There the client to use is Docker's, and the
    runner image has no Docker client, so the pinned static one is fetched.
    Everywhere else the first tool on the path wins, and podman leads because
    the runner image carries it.
    """
    import os

    from . import deps
    from .commands import which

    if os.environ.get("DOCKER_HOST"):
        found = which("docker")
        return found if found else str(deps.fetch_docker_cli())

    for name in CONTAINER_TOOLS:
        found = which(name)
        if found:
            return found
    raise ToolFailed(
        "No container tool is installed, and the service image needs one. "
        "Install one of: " + ", ".join(CONTAINER_TOOLS) + "."
    )


def build_image(registry: str | None = None, root: Path = REPOSITORY_ROOT) -> int:
    """Build the service image from source, and tag it with this version.

    The build compiles inside the image rather than copying a binary from this
    machine, because a binary built against this machine's libraries is not the
    binary the chart runs.
    """
    from .commands import run

    version = current_version(root)
    check(root)
    target = image_reference(registry or default_image_registry(root), version)
    tool = container_tool()
    say(f"Building {target}")
    run([tool, "build", "--tag", target, "--file", "Containerfile", "."], cwd=root)
    say(f"Built {target}. Pushing it is the release job's, under the owner's grant.")
    return 0


def default_image_registry(root: Path = REPOSITORY_ROOT) -> str:
    """Where the charts say the image lives, so one file decides it."""
    values = (root / "charts" / "tallyowl" / "values.yaml").read_text()
    match = re.search(r"(?m)^  repository: (\S+)$", values)
    if not match:
        raise ToolFailed(
            "charts/tallyowl/values.yaml does not name an image repository, and "
            "the package job takes the registry from it."
        )
    return match.group(1).rsplit("/", 1)[0]


# ---------------------------------------------------------------------------
# The package artifacts
# ---------------------------------------------------------------------------

#: The charts, in the order a person reads them.
CHARTS = ("charts/tallyowl", "charts/tallyowl-collector")

#: The npm packages this project publishes.
#:
#: The browser package is the one an application installs. It ships compiled
#: output, and the compiler puts the generated client and the CSIL transport
#: under the same `dist`, so the tarball needs no path outside itself.
#:
#: **The four generated TypeScript clients are not here**, for two reasons.
#: An application includes the ingest contract in its own CSIL build and
#: generates its own client, which is what `csil/tallyowl-ingest.csil` says at
#: the top. And a generated package does not build on its own today: the
#: emitted server dispatch throws `{ code: 404 } satisfies ServiceError`, and
#: this contract's `ServiceError.code` is a text enum rather than a number. See
#: L177.
NPM_PACKAGES = ("packages/browser",)

#: What a packed tarball must hold to be worth publishing. `npm pack` will
#: happily produce a tarball with nothing in it but a manifest, and a package
#: like that reaches a registry and cannot be taken back.
TARBALL_ENTRY = "package/dist/packages/browser/src/index.js"


def tarball_members(tarball: Path) -> list[str]:
    """What is inside a packed tarball."""
    import tarfile

    with tarfile.open(tarball) as bundle:
        return bundle.getnames()


def check_tarballs(directory: Path) -> None:
    """Refuse a tarball that holds a manifest and no code.

    This is not hypothetical. `npm pack` on a package whose `files` names a
    build directory produces exactly that when nobody built first, and the
    published package is broken for everybody who installs it.
    """
    tarballs = sorted(directory.glob("*.tgz"))
    if not tarballs:
        raise ToolFailed(f"No client package was packed into {directory}.")
    for tarball in tarballs:
        members = tarball_members(tarball)
        if TARBALL_ENTRY not in members:
            raise ToolFailed(
                f"{tarball.name} holds {len(members)} files and none of them is "
                f"{TARBALL_ENTRY}. A package like that installs and imports "
                "nothing. Build the package before it is packed."
            )
        say(f"{tarball.name}: {len(members)} files, and the entry point is one of them.")


#: The two service binaries, and where the image keeps them.
BINARIES = ("tallyowl-head", "tallyowl-collector")
BINARY_DIRECTORY = "/usr/local/bin"


def binary_archive_name(version: str, target: str = "linux-amd64") -> str:
    """What the downloadable archive is called on the release page."""
    return f"tallyowl-{version}-{target}.tar.gz"


def checksum_line(path: Path) -> str:
    """One `sha256sum` line, in the format the command itself writes."""
    import hashlib

    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    return f"{digest}  {path.name}"


def extract_binaries(reference: str, into: Path, root: Path = REPOSITORY_ROOT) -> list[Path]:
    """Copy the service binaries out of the image that was just built.

    Out of the image rather than off this machine: the image is what an
    operator runs, so the binary a person downloads is the same binary, built
    against the same libraries. It needs a glibc as new as the image's.
    """
    from .commands import run

    tool = container_tool()
    into.mkdir(parents=True, exist_ok=True)
    created = run([tool, "create", reference], cwd=root, capture=True)
    container = created.stdout.strip().splitlines()[-1]
    try:
        for binary in BINARIES:
            run(
                [tool, "cp", f"{container}:{BINARY_DIRECTORY}/{binary}", str(into / binary)],
                cwd=root,
            )
    finally:
        run([tool, "rm", container], cwd=root, check=False, quiet=True)
    return [into / binary for binary in BINARIES]


def build_binary_archive(version: str, reference: str, artifacts: Path, root: Path) -> Path:
    """Put both binaries and the license in one archive, and check it.

    The Rust app driver does not publish to crates.io yet, so a person who
    wants TallyOwl without Kubernetes downloads this. See L182.
    """
    import tarfile
    import tempfile

    archive = artifacts / "bin" / binary_archive_name(version)
    archive.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as staging:
        directory = Path(staging)
        binaries = extract_binaries(reference, directory, root)
        with tarfile.open(archive, "w:gz") as bundle:
            for binary in binaries:
                binary.chmod(0o755)
                bundle.add(binary, arcname=f"tallyowl-{version}/{binary.name}")
            bundle.add(root / "LICENSE", arcname=f"tallyowl-{version}/LICENSE")

    inside = tarball_members(archive)
    for binary in BINARIES:
        if f"tallyowl-{version}/{binary}" not in inside:
            raise ToolFailed(f"{archive.name} does not hold {binary}.")
    (archive.parent / "SHA256SUMS").write_text(checksum_line(archive) + "\n")
    say(f"{archive.name}: {len(inside)} files, both services, and a checksum beside it.")
    return archive


def artifact_directory(root: Path = REPOSITORY_ROOT) -> Path:
    """Where the package artifacts go. Reactorcide names one; `dist/` is local."""
    import os

    return Path(os.environ.get("RC_ARTIFACT_DIR", root / "dist"))


def image_archive(artifacts: Path, version: str) -> Path:
    """The saved image the publish job pushes without building again."""
    return artifacts / "images" / f"tallyowl-{version}.tar"


def package(root: Path = REPOSITORY_ROOT) -> int:
    """Build every publishable artifact, identified by this version.

    A chart and an npm package are both a `.tgz`, so they go to separate
    subdirectories: the publish job must never guess which registry one takes.
    The image is saved rather than pushed, because the release job publishes
    what this built and builds nothing itself. See docs/CI-CD.md section 6.
    """
    from . import deps, packages
    from .commands import run

    version = current_version(root)
    check(root)
    artifacts = artifact_directory(root)
    if (artifacts / MARKER).is_file() and not marker(root)["released"]:
        say("No release to package. `release stamp` found no version change.")
        return 0
    charts = artifacts / "charts"
    npm = artifacts / "npm"
    for directory in (charts, npm, image_archive(artifacts, version).parent):
        directory.mkdir(parents=True, exist_ok=True)

    say(f"Packaging {version} into {artifacts}")

    helm = deps.helm_program()
    for chart in CHARTS:
        run([helm, "package", str(root / chart), "--destination", str(charts)], cwd=root)

    # `npm pack` produces the same tarball `npm publish` would send, so the
    # publish job ships an artifact somebody could have inspected. It packs
    # what is on disk and builds nothing, so the build happens first.
    deps.fetch_node()
    npm_program, environment = packages.npm_program()
    for member in NPM_PACKAGES:
        package_root = root / member
        if not (package_root / "node_modules").is_dir():
            run(
                [npm_program, "install", "--no-audit", "--no-fund", "--silent"],
                cwd=package_root,
                env=environment,
            )
        run([npm_program, "run", "build"], cwd=package_root, env=environment)
        run(
            [npm_program, "pack", "--pack-destination", str(npm)],
            cwd=package_root,
            env=environment,
        )
    check_tarballs(npm)

    tool = container_tool()
    reference = image_reference(default_image_registry(root), version)
    say(f"Building {reference}")
    run([tool, "build", "--tag", reference, "--file", "Containerfile", "."], cwd=root)
    archive = image_archive(artifacts, version)
    run([tool, "save", "--output", str(archive), reference], cwd=root)

    # The binaries come out of that image, so a downloaded TallyOwl and a
    # deployed TallyOwl are the same build.
    build_binary_archive(version, reference, artifacts, root)

    say(
        f"Packaged {version}: {len(CHARTS)} charts, {len(NPM_PACKAGES)} client "
        f"package, the service image, and the binaries."
    )
    return 0


def json_version(path: Path) -> str:
    """Read a package manifest's version the way a package manager reads it.

    The stamp is a regular expression, so this is the second reader that
    proves the file is still valid JSON afterwards.
    """
    return json.loads(path.read_text())["version"]
