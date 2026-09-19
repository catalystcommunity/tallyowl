"""Tests for reading a csilgen release.

The download cannot be tested before the release exists. The **choosing** can,
and choosing is where this will go wrong: an asset named slightly differently
from what somebody expected, or two assets that both look like the answer.

Each test names the asset shapes explicitly — today's release, and the
combined one csilgen is moving to — so the day the new release lands, a failure
here says which pattern to widen instead of a job saying "unknown target".
"""

from __future__ import annotations

import tarfile
import tempfile
import unittest
from pathlib import Path

from tallyowl_tools import csilgen_release as release
from tallyowl_tools.commands import ToolFailed


def _release(names: list[str], tag: str = "csilgen/v0.3.0") -> release.Release:
    return release.Release(
        tag=tag,
        assets=tuple(
            release.Asset(name=name, url=f"https://example.invalid/{name}") for name in names
        ),
    )


#: What the release carries today: one command line for each platform.
TODAY = [
    "csilgen-0.2.1-darwin-aarch64.tar.gz",
    "csilgen-0.2.1-linux-aarch64.tar.gz",
    "csilgen-0.2.1-linux-x86_64.tar.gz",
    "csilgen-0.2.1-windows-x86_64.tar.gz",
]

#: What the combined release is expected to carry: a command line for each
#: platform, a transport for each language, and one generators tarball.
COMBINED = TODAY + [
    "csilgen-generators-0.3.0.tar.gz",
    "csilgen-transport-typescript-0.3.0.tar.gz",
    "csilgen-transport-rust-0.3.0.tar.gz",
    "csilgen-transport-go-0.3.0.tar.gz",
    "csilgen-transport-python-0.3.0.tar.gz",
]


class Choosing(unittest.TestCase):
    def test_the_command_line_for_this_platform(self) -> None:
        chosen = release.pick(_release(COMBINED), "cli", "linux", "x86_64")
        self.assertIsNotNone(chosen)
        self.assertEqual(chosen.name, "csilgen-0.2.1-linux-x86_64.tar.gz")

    def test_another_platform_takes_another_asset(self) -> None:
        for system, machine, expected in (
            ("darwin", "aarch64", "csilgen-0.2.1-darwin-aarch64.tar.gz"),
            ("linux", "aarch64", "csilgen-0.2.1-linux-aarch64.tar.gz"),
        ):
            chosen = release.pick(_release(COMBINED), "cli", system, machine)
            self.assertEqual(chosen.name, expected)

    def test_the_generators_tarball(self) -> None:
        chosen = release.pick(_release(COMBINED), "generators")
        self.assertEqual(chosen.name, "csilgen-generators-0.3.0.tar.gz")

    def test_the_typescript_transport_and_not_another_language(self) -> None:
        chosen = release.pick(_release(COMBINED), "transport-typescript")
        self.assertEqual(chosen.name, "csilgen-transport-typescript-0.3.0.tar.gz")

    def test_todays_release_has_no_generators_or_transport(self) -> None:
        """It falls back, and saying so is the point of returning None."""
        today = _release(TODAY, tag="csilgen-core/v0.2.1")
        self.assertIsNone(release.pick(today, "generators"))
        self.assertIsNone(release.pick(today, "transport-typescript"))
        self.assertIsNotNone(release.pick(today, "cli", "linux", "x86_64"))

    def test_two_plausible_assets_are_a_refusal_rather_than_a_guess(self) -> None:
        ambiguous = _release(
            ["csilgen-generators-0.3.0.tar.gz", "csilgen-generators-debug-0.3.0.tar.gz"]
        )
        with self.assertRaises(ToolFailed) as refusal:
            release.pick(ambiguous, "generators")
        self.assertIn("could be the generators", str(refusal.exception))

    def test_a_zip_command_line_is_still_a_command_line(self) -> None:
        """Windows ships a zip, and one day another platform might."""
        chosen = release.pick(_release(["csilgen-0.3.0-linux-x86_64.zip"]), "cli", "linux", "x86_64")
        self.assertIsNotNone(chosen)


class Tags(unittest.TestCase):
    def test_every_tag_form_escapes_its_slash(self) -> None:
        """A tag holds a slash, and an unescaped one reads as another endpoint.

        That mistake made an existing release look absent for an hour.
        """
        for form in release.TAG_FORMS:
            tag = form.format(version="0.3.0")
            self.assertIn("/", tag)
            self.assertEqual(tag.replace("/", "%2F").count("%2F"), tag.count("/"))

    def test_the_combined_form_is_tried_before_the_old_one(self) -> None:
        forms = [form.format(version="0.3.0") for form in release.TAG_FORMS]
        self.assertLess(forms.index("csilgen/v0.3.0"), forms.index("csilgen-core/v0.3.0"))


class Unpacking(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.addCleanup(self.directory.cleanup)

    def _archive(self, members: dict[str, str]) -> Path:
        archive = self.root / "bundle.tar.gz"
        staging = self.root / "staging"
        with tarfile.open(archive, "w:gz") as bundle:
            for name, body in members.items():
                path = staging / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(body)
                bundle.add(path, arcname=name)
        return archive

    def test_generators_are_taken_whatever_directory_they_sit_in(self) -> None:
        import re

        archive = self._archive(
            {
                "generators/csilgen_rust_generator.wasm": "r",
                "csilgen_go_generator.wasm": "g",
                "generators/README.md": "not a generator",
            }
        )
        taken = release.extract_members(
            archive, re.compile(r"^csilgen_.+_generator\.wasm$"), self.root / "out"
        )
        self.assertEqual(
            sorted(path.name for path in taken),
            ["csilgen_go_generator.wasm", "csilgen_rust_generator.wasm"],
        )
        self.assertFalse((self.root / "out" / "README.md").exists())

    def test_a_transport_keeps_its_inner_layout_and_loses_its_wrapper(self) -> None:
        """Eight files import `src/rpc.ts` by relative path. That must survive."""
        archive = self._archive(
            {
                "csilgen-transport-typescript-0.3.0/package.json": "{}",
                "csilgen-transport-typescript-0.3.0/src/rpc.ts": "export {};",
                "csilgen-transport-typescript-0.3.0/src/index.ts": "export {};",
            }
        )
        into = self.root / "transports" / "typescript"
        release.extract_tree(archive, into)
        self.assertTrue((into / "package.json").is_file())
        self.assertTrue((into / "src" / "rpc.ts").is_file())
        self.assertTrue((into / "src" / "index.ts").is_file())

    def test_unpacking_twice_leaves_no_file_from_the_first_time(self) -> None:
        into = self.root / "transports" / "typescript"
        release.extract_tree(self._archive({"a/src/old.ts": "x"}), into)
        self.assertTrue((into / "src" / "old.ts").is_file())
        release.extract_tree(self._archive({"b/src/new.ts": "y"}), into)
        self.assertTrue((into / "src" / "new.ts").is_file())
        self.assertFalse((into / "src" / "old.ts").exists())


class TheRealRelease(unittest.TestCase):
    """Read the release that exists now, so the reader is exercised for real."""

    def test_the_current_release_resolves_and_offers_a_command_line(self) -> None:
        from tallyowl_tools import deps

        found = release.find_release(deps.CSILGEN_VERSION)
        if found is None:
            self.skipTest("no network, or the pinned release is gone")
        self.assertTrue(found.assets)
        chosen = release.pick(found, "cli", "linux", "x86_64")
        self.assertIsNotNone(chosen, f"assets were: {found.names()}")


if __name__ == "__main__":
    unittest.main()
