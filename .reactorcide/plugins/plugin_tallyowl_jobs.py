"""Runnerlib lifecycle jobs for the TallyOwl workflows.

One rule holds everything here: a job calls the same Python modules that
`tools.sh` calls, so local and CI cannot drift. See docs/CI-CD.md section 2.
The job to run is selected by `REACTORCIDE_TALLYOWL_JOB`, which is the shape
the trusted-plugin model wants: the command in every job file is the same, and
the work lives in this trusted CI source rather than in a command line the
application source could rewrite.
"""

from __future__ import annotations

import base64
import json
import os
import shlex
import shutil
import subprocess
import tarfile
import tempfile
from pathlib import Path
from typing import Callable, Dict, List

from src.logging import log_stdout
from src.plugins import Plugin, PluginContext, PluginPhase


def _run(
    command: List[str],
    *,
    cwd: Path,
    stdin: str | None = None,
    env: Dict[str, str] | None = None,
    check: bool = True,
) -> bool:
    """Run one command without a command shell, and say whether it worked.

    `stdin` carries a credential to a command that reads one, and `env` carries
    one to a command that reads an environment variable, so a secret never
    appears in an argument list the process table can show.

    `check=False` is for the two steps a re-run legitimately repeats: a GitHub
    release that already exists, and a charts commit with nothing new in it.
    """
    log_stdout(f"Running: {shlex.join(command)}")
    environment = None
    if env:
        environment = {**os.environ, **env}
    finished = subprocess.run(
        command, cwd=cwd, check=check, text=True, input=stdin, env=environment
    )
    return finished.returncode == 0


def _tools(code_dir: Path, *verb: str) -> None:
    """One `tools.sh` verb, through the same module the person types."""
    _run(
        ["uv", "run", "--project", "tools", "python", "-m", "tallyowl_tools", *verb],
        cwd=code_dir,
    )


def validate(code_dir: Path) -> None:
    """Formatting, linting, the contract, and one version everywhere."""
    # First, because it is the cheapest check here and the most expensive one
    # to discover late: a chart that asks for an image tag nobody built.
    _tools(code_dir, "version", "check")
    _tools(code_dir, "csil-validate")
    _tools(code_dir, "lint")
    _tools(code_dir, "test-tools")


def conventional_commits(code_dir: Path) -> None:
    """Refuse a pull request whose commits do not say what they change.

    The version is computed from these subjects, so a subject that matches
    nothing is a release that does not happen for a reason nobody sees.
    """
    _tools(code_dir, "commits", "check")


def gen_check(code_dir: Path) -> None:
    """Generate into a temporary directory and fail on drift."""
    _tools(code_dir, "gen-check")


def test_rust(code_dir: Path) -> None:
    _tools(code_dir, "test-rust")


def test_go(code_dir: Path) -> None:
    _tools(code_dir, "test-go")


def test_ts(code_dir: Path) -> None:
    _tools(code_dir, "test-ts")


def helm_check(code_dir: Path) -> None:
    _tools(code_dir, "helm-check")


def dependency_audit(code_dir: Path) -> None:
    _tools(code_dir, "audit")


def package(code_dir: Path) -> None:
    """Build the publishable artifacts into the artifact directory.

    Immutable and identified by the source commit, per docs/CI-CD.md section 6.
    The release job publishes what this built; it does not build again. The work
    is a `tools.sh` verb, so a person can build the same artifacts and look
    inside them before a release ever runs.
    """
    _tools(code_dir, "release", "package")


#: What the release job needs, and what each one is for. The same names, in the
#: same order, are in `tools/tallyowl_tools/release.py`; a test in
#: `tools/tests/test_release.py` holds the two lists together.
GRANTS = (
    ("REGISTRY", "the container registry host"),
    ("IMAGE_PATH", "the image path inside that registry"),
    ("REGISTRY_USER", "the registry push grant, `catalystcommunity/registry:user`"),
    ("REGISTRY_PASSWORD", "the registry push grant, `catalystcommunity/registry:password`"),
    ("GITHUB_PAT", "the git and release grant, `catalystcommunity/ci:githubpat`"),
    ("CHARTS_REPO", "the chart repository the packaged charts are added to"),
    ("NPM_TOKEN", "the npmjs staging grant, `catalystcommunity/ci:npmpublish`"),
)


def _git_credentials(code_dir: Path, token: str) -> None:
    """Let git push as the CI identity, without the token in an argument list.

    A credential file is read by git itself. A token inside a remote URL shows
    up in `git remote -v`, in the process table, and in an error message.

    The helper is configured **globally**, because a release touches two
    repositories: this one, and the charts repository it clones into a
    temporary directory. A per-repository helper is not read by the clone, and
    the push there would then stop for a username nobody can type.
    """
    credentials = Path.home() / ".git-credentials"
    credentials.write_text(f"https://x-access-token:{token}@github.com\n")
    credentials.chmod(0o600)
    _run(["git", "config", "--global", "credential.helper", "store"], cwd=code_dir)
    _run(["git", "config", "--global", "user.name", "catalystcommunityci"], cwd=code_dir)
    _run(["git", "config", "--global", "user.email", "ci@catalystcommunity.org"], cwd=code_dir)


def release(code_dir: Path) -> None:
    """Build the release, publish it, and tag it last.

    The order is the whole design. A git tag is the one thing here that cannot
    be taken back — the Go module proxy caches it for ever — so nothing is
    tagged until every artifact exists and the image is pushed:

    1. bring the tree up to date with main and **stamp** the computed version.
       Nothing is committed;
    2. build every artifact from that tree, and verify each one;
    3. push the image, which is the only artifact a later step cannot rebuild;
    4. **commit, tag, and push**, atomically. Main moving under the job is a
       refusal rather than a wrong tag;
    5. publish what the tag now names: the GitHub release, the charts
       repository, and the staged npm package.

    semver-tags reads the conventional commits since the last tag and says what
    the next version is, exactly as corndogs and linkkeys release. This is the
    one place where CI writes to the source, by the owner's decision of
    2026-08-12; see docs/CI-CD.md section 1.

    Everything that touches a secret is here, in the trusted CI source.
    Everything else is a `tools.sh` verb that a person can run.
    """
    values = {name: os.environ.get(name, "").strip() for name, _ in GRANTS}
    missing = [name for name, _ in GRANTS if not values[name]]
    if missing:
        wanted = dict(GRANTS)
        raise RuntimeError(
            "The release refuses rather than half-publishing. Missing:\n"
            + "\n".join(f"  {name}: {wanted[name]}" for name in missing)
            + "\nThe coordinates are decided (D31). Each grant is a repository "
            "secret the owner creates. See docs/CI-CD.md section 6."
        )

    artifacts = Path(os.environ.get("RC_ARTIFACT_DIR", code_dir / "dist"))

    # 1. Stamp the version. No commit, no tag, no push.
    _git_credentials(code_dir, values["GITHUB_PAT"])
    os.environ["TALLYOWL_RELEASE"] = "1"
    _tools(code_dir, "release", "stamp")

    plan = json.loads((artifacts / "release.json").read_text())
    if not plan.get("released"):
        log_stdout("No release: no commit since the last tag asks for a version change.")
        return
    version, tag = plan["version"], plan["tag"]

    # 2. Build every artifact, once, and verify each. Anything that raises here
    #    leaves the repository exactly as it was.
    _tools(code_dir, "release", "package")

    charts = sorted((artifacts / "charts").glob("*.tgz"))
    tarballs = sorted((artifacts / "npm").glob("*.tgz"))
    binaries = sorted((artifacts / "bin").glob("*.tar.gz"))
    checksums = sorted((artifacts / "bin").glob("SHA256SUMS"))
    archive = artifacts / "images" / f"tallyowl-{version}.tar"
    if not charts or not tarballs or not binaries or not archive.is_file():
        raise RuntimeError(
            f"The package step wrote {len(charts)} charts, {len(tarballs)} "
            f"client packages, and {len(binaries)} binary archives into "
            f"{artifacts}, and the image archive is "
            f"{'there' if archive.is_file() else 'missing'}. Nothing is tagged."
        )

    # 3. Every credential is written, and every publisher is asked to prove it
    #    works. This costs seconds and it is why the step exists: release
    #    0.2.0 pushed the image, cut six tags, made the release page and
    #    committed the charts, then stopped on `npm stage publish` — a
    #    subcommand the pinned npm was three minors too old to have. None of
    #    those four steps could be taken back. A publisher that cannot publish
    #    must be found before the first one that cannot be undone, not after
    #    the fourth.
    registry, image_path = values["REGISTRY"], values["IMAGE_PATH"]
    reference = f"{registry}/{image_path}:{version}"
    docker_config = Path.home() / ".docker"
    docker_config.mkdir(parents=True, exist_ok=True)
    auth = base64.b64encode(
        f"{values['REGISTRY_USER']}:{values['REGISTRY_PASSWORD']}".encode()
    ).decode()
    config = docker_config / "config.json"
    config.write_text(json.dumps({"auths": {registry: {"auth": auth}}}))
    config.chmod(0o600)

    npmrc = Path.home() / ".npmrc"
    npmrc.write_text(f"//registry.npmjs.org/:_authToken={values['NPM_TOKEN']}\n")
    npmrc.chmod(0o600)

    crane = _program(code_dir, "crane")
    gh = _program(code_dir, "gh")
    npm = _program(code_dir, "npm")
    node_environment = _node_environment(code_dir)
    gh_environment = {"GH_TOKEN": values["GITHUB_PAT"]}

    # Each probe names one thing that must be true: the binary is there, the
    # credential is accepted, and the subcommand exists with the arguments this
    # job gives it. `npm stage list` reads the staging endpoint, so it proves
    # the subcommand and the token together — it is the probe that would have
    # answered `Unknown command: "stage"` while nothing was public. `--dry-run`
    # then does everything a staged publish does except upload.
    _run([crane, "version"], cwd=code_dir)
    _run([gh, "auth", "status"], cwd=code_dir, env=gh_environment)
    _run([npm, "stage", "list"], cwd=code_dir, env=node_environment)
    _refuse_unpublished_packages(npm, tarballs, code_dir, node_environment)
    for tarball in tarballs:
        _run(
            [npm, "stage", "publish", str(tarball), "--access", "public", "--dry-run"],
            cwd=code_dir,
            env=node_environment,
        )
    log_stdout("Every publisher answered. Nothing is public yet.")

    # 4. The image. crane pushes a saved archive with no daemon and no build.
    _run([crane, "push", str(archive), reference], cwd=code_dir)
    log_stdout(f"Pushed {reference}.")

    # 5. Now, and only now, the version becomes public in git.
    _tools(code_dir, "release", "tag")

    # 6. Publish what the tag names. Each of these is repeatable if it fails:
    #    the release page takes an upload, and the charts repository takes
    #    another commit.
    repository = os.environ.get("REACTORCIDE_REPO", "CatalystCommunity/tallyowl")
    assets = [str(asset) for asset in [*binaries, *checksums, *charts]]
    made = _run(
        [
            gh, "release", "create", tag,
            "--repo", repository,
            "--title", tag,
            "--notes", f"TallyOwl {version}. See docs/RELEASE_NOTES.md.",
            *assets,
        ],
        cwd=code_dir,
        env=gh_environment,
        check=False,
    )
    if not made:
        # A re-run after a later step failed finds the release already there.
        _run(
            [gh, "release", "upload", tag, "--repo", repository, "--clobber", *assets],
            cwd=code_dir,
            env=gh_environment,
        )

    charts_repo = values["CHARTS_REPO"]
    checkout = Path(tempfile.mkdtemp())
    _run(
        ["git", "clone", "--depth", "1", f"https://github.com/{charts_repo}.git", str(checkout)],
        cwd=code_dir,
    )
    for chart in charts:
        shutil.copy(chart, checkout / chart.name)
    _run(["git", "add", *[chart.name for chart in charts]], cwd=checkout)
    committed = _run(
        ["git", "commit", "--message", f"chore: add tallyowl {version}"],
        cwd=checkout,
        check=False,
    )
    if committed:
        _run(["git", "push", "origin", "main"], cwd=checkout)
        log_stdout(f"Added {len(charts)} charts to {charts_repo}.")
    else:
        log_stdout(f"{charts_repo} already holds these charts.")
    shutil.rmtree(checkout, ignore_errors=True)

    # npm, staged. The token cannot publish outright and should not be able to:
    # a maintainer approves the staged version with 2FA on npmjs.com or with
    # `npm stage approve <stage-id>`. `npm stage list` says what is waiting.
    #
    # This needs npm 11.16 or newer, which is why `deps.NODE_VERSION` is pinned
    # to a Node that carries one. See L180 and L190.
    for tarball in tarballs:
        _run(
            [npm, "stage", "publish", str(tarball), "--access", "public"],
            cwd=code_dir,
            env=node_environment,
        )
    log_stdout(
        f"Staged {len(tarballs)} package(s) on npmjs. A maintainer approves each "
        "one with 2FA before it is installable."
    )

    log_stdout(
        f"Released {tag}: image {reference}, {len(charts)} charts in "
        f"{charts_repo}, {len(binaries)} binary archive(s) on the release page, "
        f"{len(tarballs)} package(s) staged on npmjs, and a tag for each Go "
        "module. crates.io is off by the owner's decision until the release "
        "pages have proved themselves; `./tools.sh release crates-plan` says "
        "what turning it on would publish."
    )


def _npm_package_name(tarball: Path) -> str:
    """The package name inside a packed npm tarball.

    `npm pack` writes the name into the file name with the scope flattened, so
    `@catalystcommunity/tallyowl-browser` becomes
    `catalystcommunity-tallyowl-browser-0.2.0.tgz` and the scope cannot be read
    back out of it. The manifest inside carries the real name.
    """
    with tarfile.open(tarball, "r:gz") as archive:
        member = archive.extractfile("package/package.json")
        if member is None:
            raise RuntimeError(f"{tarball.name} holds no package/package.json.")
        return json.loads(member.read())["name"]


def _refuse_unpublished_packages(
    npm: str, tarballs: List[Path], code_dir: Path, environment: Dict[str, str]
) -> None:
    """Refuse a release that would stage a version of a package npm does not have.

    **Staging cannot create a package.** `npm stage publish` defers the 2FA on
    a new *version*; the staging endpoint answers `404 Package "<name>" not
    found` when the package itself has never been published. The first publish
    of each package is a person with `npm publish`, once.

    Without this, that 404 arrives at the end of the release, after the image,
    the tags, the release page and the charts are all public. With it, the
    release refuses while nothing has been published and says what to run.
    """
    missing = []
    for tarball in tarballs:
        name = _npm_package_name(tarball)
        found = _run(
            [npm, "view", name, "version"],
            cwd=code_dir,
            env=environment,
            check=False,
        )
        if not found:
            missing.append((name, tarball))

    if not missing:
        return

    lines = [
        "These packages are not on npmjs, and `npm stage publish` cannot create",
        "one — it defers the 2FA on a new version of a package that already",
        "exists. A maintainer publishes each of these once, by hand, with 2FA:",
        "",
    ]
    lines += [f"  npm publish {tarball} --access public" for _, tarball in missing]
    lines += [
        "",
        "Every release after that stages through this job. Nothing is published",
        "and nothing is tagged.",
    ]
    raise RuntimeError("\n".join(lines))


def _node_environment(code_dir: Path) -> Dict[str, str]:
    """The PATH npm needs to find its own interpreter.

    The fetched `npm` is a shim whose first line is `#!/usr/bin/env node`, and
    the runner image has no node. Without this it dies with `env: 'node': No
    such file or directory` — which is what `tallyowl_tools.packages` builds
    the same way for every other npm call.
    """
    fetched = code_dir / ".deps" / "node" / "bin"
    if not fetched.is_dir():
        return {}
    return {"PATH": os.pathsep.join([str(fetched), os.environ.get("PATH", "")])}


def _program(code_dir: Path, name: str) -> str:
    """Find a release tool, or fetch the pinned one.

    The runner image carries podman, git, uv, Go, and Rust, and carries no
    crane, no gh, no helm, and no Node. `./tools.sh deps` fetches the pinned
    ones into `.deps`, so a release job and a workstation use the same
    versions. That was read out of the image rather than assumed (L176).
    """
    found = shutil.which(name)
    if found:
        return found
    fetched = code_dir / ".deps" / ("node/bin" if name in ("npm", "node") else "bin") / name
    if not fetched.is_file():
        _tools(code_dir, "deps")
    if not fetched.is_file():
        raise RuntimeError(
            f"`{name}` is not in this job image and `./tools.sh deps` did not "
            f"fetch it to {fetched}."
        )
    return str(fetched)


JOBS: Dict[str, Callable[[Path], None]] = {
    "release": release,
    "validate": validate,
    "conventional-commits": conventional_commits,
    "gen-check": gen_check,
    "test-rust": test_rust,
    "test-go": test_go,
    "test-ts": test_ts,
    "helm-check": helm_check,
    "audit": dependency_audit,
    "package": package,
}


class TallyOwlJobsPlugin(Plugin):
    """Run one selected TallyOwl job after source preparation."""

    def __init__(self):
        super().__init__(name="tallyowl_jobs", priority=50)

    def supported_phases(self):
        return [PluginPhase.POST_SOURCE_PREP]

    def execute(self, context: PluginContext) -> None:
        if context.phase != PluginPhase.POST_SOURCE_PREP:
            return

        name = os.environ.get("REACTORCIDE_TALLYOWL_JOB", "").strip()
        if not name:
            return
        job = JOBS.get(name)
        if job is None:
            names = ", ".join(sorted(JOBS))
            raise RuntimeError(
                f"Unknown REACTORCIDE_TALLYOWL_JOB '{name}'. Valid jobs: {names}"
            )

        code_dir = Path(context.config.code_dir)
        if not code_dir.is_dir():
            raise RuntimeError(f"Code directory does not exist: {code_dir}")

        log_stdout(f"Starting TallyOwl job: {name}")
        job(code_dir)
        log_stdout(f"Completed TallyOwl job: {name}")
