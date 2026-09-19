"""One csilgen release, three kinds of artifact.

csilgen is moving to a single release for each version — one tag carrying the
command line for each platform, the transport library for each language, and
one tarball of every generator. This module knows how to read that release and
pick the three things TallyOwl needs out of it:

| What | Where it goes | Who reads it |
| --- | --- | --- |
| The `csilgen` command line | `.deps/bin/csilgen` | `./tools.sh gen` |
| Every WASM generator | `~/.csilgen/generators/` | csilgen itself, at generation |
| The TypeScript transport | `.deps/csilgen/transports/typescript/` | eight source files, by relative path |

**Assets are chosen by shape, not by a guessed name.** The release is read from
the GitHub API and each asset matched with a pattern, so a name that ends up
slightly different from what this expected still resolves, and a name that
matches nothing produces a refusal that lists what the release actually holds.
That is the difference between a release-day surprise and a release-day
five-minute fix.

**The transport keeps its path.** `packages/browser/tsconfig.json` and seven
other files reach it at `.deps/csilgen/transports/typescript/src`, so whatever
the archive looks like inside, it is extracted to exactly there. Changing that
path is a separate decision from changing where the bytes come from.

Until the new release exists, every one of these falls back to what works
today: the per-platform archive on the old tag, a generator build from the
pinned checkout, and a git clone for the transport. See L186.
"""

from __future__ import annotations

import json
import re
import tarfile
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from .commands import ToolFailed, say

#: The release tag forms this knows, newest shape first. csilgen tagged
#: `csilgen-core/v0.2.1` for the command line alone; the combined release is
#: `csilgen/v<version>`. Both are tried, so this module works before and after
#: the change without an edit.
TAG_FORMS = ("csilgen/v{version}", "csilgen/{version}", "csilgen-core/v{version}")

REPOSITORY = "catalystcommunity/csilgen"


@dataclass(frozen=True)
class Asset:
    name: str
    url: str


@dataclass(frozen=True)
class Release:
    tag: str
    assets: tuple[Asset, ...]

    def names(self) -> list[str]:
        return [asset.name for asset in self.assets]


def _api(url: str) -> dict | None:
    """One GitHub API read, or `None` when there is nothing there."""
    request = urllib.request.Request(
        url,
        headers={"User-Agent": "tallyowl-tools", "Accept": "application/vnd.github+json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:  # noqa: S310
            return json.loads(response.read())
    except urllib.error.HTTPError:
        return None
    except Exception:  # pragma: no cover - a network failure
        return None


def find_release(version: str) -> Release | None:
    """The release for this version, whichever tag form it was cut under.

    A tag holds a slash, so it is escaped: `releases/tags/csilgen/v0.3.0` reads
    as a different endpoint entirely and answers 404 for everything. That cost
    an hour once, and it is why this is a function rather than a URL.
    """
    for form in TAG_FORMS:
        tag = form.format(version=version)
        escaped = tag.replace("/", "%2F")
        found = _api(f"https://api.github.com/repos/{REPOSITORY}/releases/tags/{escaped}")
        if not found or not isinstance(found.get("assets"), list):
            continue
        assets = tuple(
            Asset(name=asset["name"], url=asset["browser_download_url"])
            for asset in found["assets"]
        )
        if assets:
            return Release(tag=tag, assets=assets)
    return None


#: How each artifact is recognised. The patterns are deliberately loose about
#: word order and separators and strict about what the thing is.
PATTERNS = {
    "cli": r"^csilgen-.*{system}[-_]{machine}.*\.(?:tar\.gz|tgz|zip)$",
    "generators": r"generators?.*\.(?:tar\.gz|tgz)$",
    "transport-typescript": r"transport[-_]typescript|typescript[-_]transport",
}


def pick(release: Release, kind: str, system: str = "", machine: str = "") -> Asset | None:
    """The one asset of a kind, or `None` when the release has none.

    More than one match is a refusal rather than a guess: picking the first of
    two plausible archives is how a build ends up running something nobody
    chose.
    """
    pattern = PATTERNS[kind].format(system=system, machine=machine)
    matches = [asset for asset in release.assets if re.search(pattern, asset.name)]
    if len(matches) > 1:
        raise ToolFailed(
            f"The csilgen release {release.tag} holds {len(matches)} assets that "
            f"could be the {kind}: {', '.join(a.name for a in matches)}. "
            "Narrow the pattern in tools/tallyowl_tools/csilgen_release.py."
        )
    return matches[0] if matches else None


def describe(version: str, system: str, machine: str) -> str:
    """What this would install, and from where. `./tools.sh deps show`."""
    release = find_release(version)
    if release is None:
        return (
            f"No csilgen release found for {version}. Tag forms tried: "
            + ", ".join(form.format(version=version) for form in TAG_FORMS)
        )
    lines = [f"csilgen release {release.tag}, {len(release.assets)} assets:"]
    for kind, argument in (
        ("cli", True),
        ("generators", False),
        ("transport-typescript", False),
    ):
        chosen = pick(release, kind, system if argument else "", machine if argument else "")
        lines.append(f"  {kind:22} {chosen.name if chosen else '(none — falls back)'}")
    lines.append("  every asset: " + ", ".join(release.names()))
    return "\n".join(lines)


def extract_members(archive: Path, wanted: re.Pattern[str], into: Path) -> list[Path]:
    """Take every member whose base name matches, flattened into `into`.

    Flattened on purpose: an archive may hold `generators/x.wasm` or `x.wasm`,
    and the caller wants the files rather than the layout.
    """
    into.mkdir(parents=True, exist_ok=True)
    written = []
    with tarfile.open(archive) as bundle:
        for member in bundle.getmembers():
            base = Path(member.name).name
            if not member.isfile() or not wanted.search(base):
                continue
            member.name = base
            bundle.extract(member, into, filter="data")
            written.append(into / base)
    return written


def directory_holding(archive: Path, marker: str) -> str | None:
    """The directory inside an archive that holds `marker`, or `None`.

    Guessing how many leading directories to drop is how an unpack lands one
    level off, which is what happened with the 0.2.7 transport archive: it
    holds `transports/typescript/src/index.ts`, and a fixed strip of one left a
    `typescript` directory in the way. Find the marker and derive the prefix
    from it instead.
    """
    with tarfile.open(archive) as bundle:
        for member in bundle.getmembers():
            if not member.isfile():
                continue
            name = member.name.lstrip("./")
            if name == marker or name.endswith("/" + marker):
                return name[: -len(marker)].rstrip("/")
    return None


def extract_tree(archive: Path, into: Path, strip: int = 1, marker: str = "") -> Path:
    """Unpack an archive into `into`, dropping the leading path parts.

    A release archive names its own top directory, and the eight files that
    reach the TypeScript transport by relative path do not care what that
    directory was called. They care that `src/rpc.ts` is where it has always
    been. Give a `marker` and the prefix is derived from the archive rather
    than assumed.
    """
    if marker:
        prefix = directory_holding(archive, marker)
        if prefix is None:
            raise ToolFailed(
                f"{archive.name} holds no `{marker}`, so there is nothing to "
                "unpack for the thing that needs it."
            )
        strip = len(Path(prefix).parts) if prefix else 0
    import shutil

    if into.exists():
        shutil.rmtree(into)
    into.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive) as bundle:
        for member in bundle.getmembers():
            parts = Path(member.name).parts[strip:]
            if not parts:
                continue
            member.name = str(Path(*parts))
            bundle.extract(member, into, filter="data")
    say(f"Unpacked {archive.name} into {into}")
    return into
