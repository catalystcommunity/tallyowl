"""Refuse a commit that does not say what kind of change it is.

The version is not typed by a person here; `semver-tags` reads the conventional
commits since the last tag and computes it. That makes a commit subject part of
the release contract, and a subject that matches nothing is not a style
complaint — it is a commit that changes no version, silently, and a release
that does not happen for a reason nobody sees.

**The types below are semver-tags' own defaults, not a list somebody liked.**
A gate that accepted a type the calculator ignores would pass a pull request
and then produce no release, which is exactly the failure it exists to prevent.
They are checked against the calculator in `tools/tests/test_commits.py`.

See docs/CI-CD.md section 5 and D31.
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass

from .commands import REPOSITORY_ROOT, ToolFailed, run, say, warn

#: A minor release. `semver-tags --minor_types`.
MINOR_TYPES = ("feat",)

#: A patch release. `semver-tags --patch_types`.
PATCH_TYPES = (
    "build",
    "chore",
    "ci",
    "docs",
    "fix",
    "perf",
    "refactor",
    "revert",
    "style",
    "test",
)

#: Deliberately no release. It is not one of semver-tags' types, which is the
#: point: a commit marked this way is one the author decided should not move a
#: version, and the gate lets it through rather than making them invent a type.
NO_RELEASE_TYPES = ("norelease",)

#: What a major release is marked with. semver-tags has no major *type*: a
#: breaking change is a `!` after the type, or `BREAKING CHANGE` in the body.
BREAKING_MARKER = "BREAKING CHANGE"

ALL_TYPES = tuple(sorted({*MINOR_TYPES, *PATCH_TYPES, *NO_RELEASE_TYPES}))

#: `type(scope)!: subject`. The scope and the breaking mark are optional, and
#: the subject is not: a commit with an empty subject says nothing.
SUBJECT = re.compile(
    r"^(?P<type>[a-z]+)(?P<scope>\([^)]+\))?(?P<breaking>!)?: (?P<subject>.+)$"
)


@dataclass(frozen=True)
class Commit:
    """One commit, and what it would do to the version."""

    hash: str
    subject: str
    body: str

    @property
    def conventional(self) -> bool:
        return bool(SUBJECT.match(self.subject)) and self.kind in ALL_TYPES

    @property
    def kind(self) -> str:
        match = SUBJECT.match(self.subject)
        return match.group("type") if match else ""

    @property
    def breaking(self) -> bool:
        match = SUBJECT.match(self.subject)
        marked = bool(match and match.group("breaking"))
        return marked or BREAKING_MARKER in self.body

    @property
    def effect(self) -> str:
        """What this commit does to the next version."""
        if not self.conventional:
            return "nothing, and this is a refusal"
        if self.breaking:
            return "major"
        if self.kind in MINOR_TYPES:
            return "minor"
        if self.kind in PATCH_TYPES:
            return "patch"
        return "no release, deliberately"


def _range(base: str | None) -> str:
    """The commits to read: what this branch adds, and nothing else.

    Reactorcide names the base of a pull request in `REACTORCIDE_DIFF_BASE`. A
    person running this by hand gets the same answer from the merge base with
    `main`, which is what a pull request would be opened against.
    """
    if base:
        return f"{base}..HEAD"
    from_ci = os.environ.get("REACTORCIDE_DIFF_BASE", "").strip()
    if from_ci:
        return f"{from_ci}..HEAD"
    for candidate in ("origin/main", "main"):
        found = run(
            ["git", "merge-base", candidate, "HEAD"],
            cwd=REPOSITORY_ROOT,
            capture=True,
            check=False,
            quiet=True,
        )
        if found.ok and found.stdout.strip():
            return f"{found.stdout.strip()}..HEAD"
    raise ToolFailed(
        "There is no `main` to compare against, so there is no set of commits "
        "this branch adds. Name a base: `./tools.sh commits check <base>`."
    )


def read_commits(base: str | None = None) -> list[Commit]:
    """Every commit this branch adds, newest first."""
    span = _range(base)
    # A record separator that cannot appear in a subject, so a body with blank
    # lines in it stays one commit.
    result = run(
        ["git", "log", span, "--pretty=format:%H%x1f%s%x1f%b%x1e"],
        cwd=REPOSITORY_ROOT,
        capture=True,
        quiet=True,
    )
    commits = []
    for record in result.stdout.split("\x1e"):
        record = record.strip("\n")
        if not record:
            continue
        parts = record.split("\x1f")
        if len(parts) != 3:
            continue
        commits.append(Commit(hash=parts[0], subject=parts[1], body=parts[2]))
    return commits


def check(base: str | None = None) -> int:
    """Fail unless every commit this branch adds says what kind of change it is."""
    commits = read_commits(base)
    if not commits:
        say("This branch adds no commit, so there is nothing to check.")
        return 0

    refused = [commit for commit in commits if not commit.conventional]
    for commit in commits:
        mark = "  " if commit.conventional else "->"
        print(f"{mark} {commit.hash[:12]} {commit.subject}")
        print(f"     version: {commit.effect}")

    if refused:
        raise ToolFailed(
            f"{len(refused)} of {len(commits)} commits do not say what kind of "
            "change they are, and the version is computed from that. Write "
            "`type: subject`, or `type(scope)!: subject` for a breaking "
            "change.\n"
            f"  Types that release: {', '.join(sorted({*MINOR_TYPES, *PATCH_TYPES}))}.\n"
            f"  `{NO_RELEASE_TYPES[0]}:` for a change that should move no version.\n"
            "  A `!` after the type, or `BREAKING CHANGE` in the body, makes it "
            "major."
        )

    releasing = [c for c in commits if c.effect in ("major", "minor", "patch")]
    if not releasing:
        warn(
            "Every commit here is marked to release nothing, so merging this "
            "publishes nothing. That is a decision, and it is recorded as one."
        )
        return 0

    say(f"{len(commits)} commits, and the strongest asks for a {_strongest(commits)} release.")
    return 0


def _strongest(commits: list[Commit]) -> str:
    for effect in ("major", "minor", "patch"):
        if any(commit.effect == effect for commit in commits):
            return effect
    return "no"
