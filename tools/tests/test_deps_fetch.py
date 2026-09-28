"""Tests for fetching a pinned dependency.

Every fetch here asks one question before it does any work: is the file there?
That question has two wrong answers, and both reached a machine.

- **The file is there and it is not the pin.** The csilgen pin moved from 0.2.7
  to 0.2.8, CI fetched 0.2.8 from scratch, and every workstation kept the 0.2.7
  it already had. `gen-check` then disagreed between the two.
- **The file is there and it is half a file.** An archive unpacked straight to
  its final path, so a run that stopped left a short binary under the real
  name, and every later run took it for the tool.

Nothing here reaches the network. The download is a stubbed opener, and the
archives are made in a temporary directory.
"""

from __future__ import annotations

import contextlib
import hashlib
import io
import os
import tarfile
import tempfile
import unittest
import unittest.mock
import urllib.error
from pathlib import Path

from tallyowl_tools import csilgen_release, deps
from tallyowl_tools.commands import ToolFailed

URL = "https://example.invalid/releases/tool-1.0.0.tar.gz"


class _Answer(io.BytesIO):
    """What `urlopen` gives back: a body that is also a context manager."""

    def __enter__(self) -> "_Answer":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


def _no_curl():
    """Take the Python path, which is the one a stubbed opener can answer."""
    return unittest.mock.patch("tallyowl_tools.commands.which", return_value=None)


def _answers(body: bytes):
    return unittest.mock.patch("urllib.request.urlopen", side_effect=lambda *_, **__: _Answer(body))


def _refuses(case: unittest.TestCase, code: int):
    body = io.BytesIO(b"")
    error = urllib.error.HTTPError(URL, code, "no", {}, body)
    case.addCleanup(error.close)
    return unittest.mock.patch("urllib.request.urlopen", side_effect=error)


class Scratch(unittest.TestCase):
    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)

    def quietly(self):
        """Keep `say` lines out of the test output."""
        return contextlib.redirect_stdout(io.StringIO())

    def archive(self, members: dict[str, bytes], name: str = "bundle.tar.gz") -> Path:
        path = self.root / name
        with tarfile.open(path, "w:gz") as bundle:
            for member, body in members.items():
                info = tarfile.TarInfo(member)
                info.size = len(body)
                bundle.addfile(info, io.BytesIO(body))
        return path


class Downloading(Scratch):
    def test_a_download_that_matches_its_pin_is_installed(self) -> None:
        body = b"the whole file"
        pins = {URL: hashlib.sha256(body).hexdigest()}
        destination = self.root / "tool.tar.gz"
        with _no_curl(), _answers(body), unittest.mock.patch.dict(deps.PINNED_SHA256, pins):
            deps._download(URL, destination, "the tool", quiet=True)
        self.assertEqual(destination.read_bytes(), body)

    def test_a_download_that_does_not_match_its_pin_is_not_installed(self) -> None:
        pins = {URL: hashlib.sha256(b"what was pinned").hexdigest()}
        destination = self.root / "tool.tar.gz"
        with _no_curl(), _answers(b"something else"), unittest.mock.patch.dict(
            deps.PINNED_SHA256, pins
        ):
            with self.assertRaises(ToolFailed) as refusal:
                deps._download(URL, destination, "the tool", quiet=True)
        self.assertIn("does not match its pinned SHA-256", str(refusal.exception))
        self.assertIn("PINNED_SHA256", str(refusal.exception))
        self.assertFalse(destination.exists())
        self.assertEqual(list(self.root.iterdir()), [], "a partial file was left behind")

    def test_a_mismatch_keeps_the_file_that_was_already_there(self) -> None:
        pins = {URL: hashlib.sha256(b"what was pinned").hexdigest()}
        destination = self.root / "tool.tar.gz"
        destination.write_bytes(b"the last good one")
        with _no_curl(), _answers(b"something else"), unittest.mock.patch.dict(
            deps.PINNED_SHA256, pins
        ):
            with self.assertRaises(ToolFailed):
                deps._download(URL, destination, "the tool", quiet=True)
        self.assertEqual(destination.read_bytes(), b"the last good one")

    def test_a_download_with_no_pin_prints_the_digest_to_pin(self) -> None:
        body = b"nobody pinned this yet"
        said = io.StringIO()
        with _no_curl(), _answers(body), contextlib.redirect_stdout(said):
            deps._download(URL, self.root / "tool.tar.gz", "the tool")
        self.assertIn(hashlib.sha256(body).hexdigest(), said.getvalue())
        self.assertIn("PINNED_SHA256", said.getvalue())

    def test_no_digest_in_the_table_is_invented(self) -> None:
        """An entry is 64 hexadecimal characters, keyed by the URL it came from."""
        for url, digest in deps.PINNED_SHA256.items():
            self.assertTrue(url.startswith("https://"), url)
            self.assertRegex(digest, r"^[0-9a-f]{64}$")

    def test_a_404_is_not_published_and_nothing_else_is(self) -> None:
        with _no_curl(), _refuses(self, 404):
            with self.assertRaises(deps.NotPublished):
                deps._download(URL, self.root / "a.tar.gz", "the tool", quiet=True)
        with _no_curl(), _refuses(self, 503):
            with self.assertRaises(ToolFailed) as failure:
                deps._download(URL, self.root / "b.tar.gz", "the tool", quiet=True)
        self.assertNotIsInstance(failure.exception, deps.NotPublished)
        self.assertIn("503", str(failure.exception))

    def test_a_failed_download_says_what_to_run(self) -> None:
        lost = urllib.error.URLError("no route to host")
        with _no_curl(), unittest.mock.patch("urllib.request.urlopen", side_effect=lost):
            with self.assertRaises(ToolFailed) as failure:
                deps._download(
                    URL,
                    self.root / "csilgen.tar.gz",
                    "csilgen",
                    quiet=True,
                    advice=deps.CSILGEN_DOWNLOAD_ADVICE,
                )
        message = str(failure.exception)
        self.assertIn("./tools.sh deps", message)
        # The pinned csilgen is the only one used, so this advice would be wrong.
        self.assertNotIn("install it yourself", message.lower())

    def test_curl_reporting_a_404_is_not_published(self) -> None:
        from tallyowl_tools.commands import Result

        with unittest.mock.patch("tallyowl_tools.commands.which", return_value="/usr/bin/curl"):
            with unittest.mock.patch.object(deps, "run", return_value=Result(22, "404", "")):
                with self.assertRaises(deps.NotPublished):
                    deps._download(URL, self.root / "a.tar.gz", "the tool", quiet=True)


class InstallingABinary(Scratch):
    def test_the_program_lands_executable_under_its_name(self) -> None:
        archive = self.archive({"linux-amd64/helm": b"#!/bin/sh\n"})
        destination = self.root / "bin" / "helm"
        destination.parent.mkdir()
        deps._install_binary(archive, destination, "linux-amd64/helm")
        self.assertTrue(os.access(destination, os.X_OK))
        self.assertFalse(archive.exists(), "the archive is removed once it is used")

    def test_an_archive_without_the_program_installs_nothing(self) -> None:
        archive = self.archive({"README.md": b"no program here"})
        destination = self.root / "helm"
        destination.write_bytes(b"the last good one")
        with self.assertRaises(ToolFailed) as refusal:
            deps._install_binary(archive, destination, "linux-amd64/helm")
        self.assertIn("linux-amd64/helm", str(refusal.exception))
        self.assertEqual(destination.read_bytes(), b"the last good one")

    def test_an_unpack_that_stops_leaves_no_short_program(self) -> None:
        archive = self.archive({"crane": b"#!/bin/sh\n"})
        destination = self.root / "crane"

        def write_half(bundle, member, path, **kwargs):
            (Path(path) / member.name).write_bytes(b"#!")
            raise OSError("the disk filled")

        with unittest.mock.patch.object(tarfile.TarFile, "extract", write_half):
            with self.assertRaises(OSError):
                deps._install_binary(archive, destination, "crane")
        self.assertFalse(destination.exists())


def _csilgen_script(version: str) -> bytes:
    return f"#!/bin/sh\necho 'csilgen {version}'\n".encode()


class TheCsilgenPin(Scratch):
    """The file being there is not the pin being there."""

    def setUp(self) -> None:
        super().setUp()
        self.program = self.root / "bin" / "csilgen"
        self.program.parent.mkdir()
        patches = (
            unittest.mock.patch.object(deps, "CSILGEN_PATH", self.program),
            unittest.mock.patch.object(deps, "CSILGEN_VERSION", "0.2.8"),
            unittest.mock.patch.object(csilgen_release, "find_release", return_value=None),
        )
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)

    def _install(self, version: str) -> None:
        self.program.write_bytes(_csilgen_script(version))
        self.program.chmod(0o755)

    def _serves(self, version: str):
        """A `_download` that writes an archive holding a csilgen of `version`."""
        fetched = []

        def download(url, destination, what, quiet=False, advice=""):
            fetched.append(url)
            built = self.archive({"csilgen": _csilgen_script(version)}, name="served.tar.gz")
            os.replace(built, destination)
            return destination

        return fetched, unittest.mock.patch.object(deps, "_download", download)

    def test_a_release_build_reports_its_release(self) -> None:
        self._install("0.2.7")
        self.assertEqual(deps.installed_csilgen_version(), "0.2.7")

    def test_a_file_that_is_not_a_program_reports_nothing(self) -> None:
        self.program.write_bytes(b"\x00\x01 half an archive")
        self.program.chmod(0o755)
        self.assertIsNone(deps.installed_csilgen_version())

    def test_the_pinned_version_already_there_fetches_nothing(self) -> None:
        self._install("0.2.8")
        fetched, serving = self._serves("0.2.8")
        with serving, self.quietly():
            deps.fetch_csilgen_binary()
        self.assertEqual(fetched, [])

    def test_an_older_version_already_there_is_fetched_over(self) -> None:
        """The 0.2.7 a workstation kept while CI ran 0.2.8."""
        self._install("0.2.7")
        fetched, serving = self._serves("0.2.8")
        with serving, self.quietly():
            deps.fetch_csilgen_binary()
        self.assertEqual(len(fetched), 1)
        self.assertIn("csilgen-0.2.8-", fetched[0])
        self.assertEqual(deps.installed_csilgen_version(), "0.2.8")

    def test_a_download_that_is_not_the_pin_is_refused(self) -> None:
        fetched, serving = self._serves("0.2.6")
        with serving, self.quietly():
            with self.assertRaises(ToolFailed) as refusal:
                deps.fetch_csilgen_binary()
        self.assertIn("0.2.6", str(refusal.exception))
        self.assertIn("CSILGEN_VERSION", str(refusal.exception))


class TheTransport(Scratch):
    """Which transport a machine gets must not depend on whether the API answered."""

    def setUp(self) -> None:
        super().setUp()
        self.transport = self.root / "csilgen" / "transports" / "typescript"
        patches = (
            unittest.mock.patch.object(deps, "DEPENDENCY_DIR", self.root),
            unittest.mock.patch.object(deps, "TYPESCRIPT_TRANSPORT", self.transport),
            unittest.mock.patch.object(deps, "TRANSPORT_STAMP", self.transport / ".csilgen-release"),
            unittest.mock.patch.object(deps, "CSILGEN_VERSION", "0.2.8"),
            unittest.mock.patch.object(deps, "CSILGEN_TAG", "csilgen/v0.2.8"),
        )
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)

    def _serves(self):
        fetched = []

        def download(url, destination, what, quiet=False, advice=""):
            fetched.append(url)
            built = self.archive(
                {
                    "transports/typescript/package.json": b"{}",
                    "transports/typescript/src/index.ts": b"export {};",
                },
                name="served.tar.gz",
            )
            os.replace(built, destination)
            return destination

        return fetched, unittest.mock.patch.object(deps, "_download", download)

    def test_a_rate_limited_api_still_takes_the_release_transport(self) -> None:
        limited = ToolFailed("GitHub answered 403")
        fetched, serving = self._serves()
        with serving, unittest.mock.patch.object(
            csilgen_release, "find_release", side_effect=limited
        ), self.quietly(), contextlib.redirect_stderr(io.StringIO()):
            self.assertTrue(deps.fetch_transport())
        self.assertEqual(
            fetched,
            [
                "https://github.com/catalystcommunity/csilgen/releases/download/"
                "csilgen/v0.2.8/csilgen-transport-typescript-0.2.8.tar.gz"
            ],
        )
        self.assertTrue(deps.transport_is_pinned())

    def test_a_transport_from_another_release_is_fetched_again(self) -> None:
        (self.transport / "src").mkdir(parents=True)
        (self.transport / "package.json").write_text("{}")
        (self.transport / ".csilgen-release").write_text("0.2.7\n")
        self.assertFalse(deps.transport_is_pinned())
        fetched, serving = self._serves()
        with serving, unittest.mock.patch.object(
            csilgen_release, "find_release", return_value=None
        ), self.quietly():
            self.assertTrue(deps.fetch_transport())
        self.assertEqual(len(fetched), 1)

        # And the pinned one costs nothing the second time.
        with serving, self.quietly():
            self.assertTrue(deps.fetch_transport())
        self.assertEqual(len(fetched), 1)

    def test_only_a_404_sends_the_caller_to_the_clone(self) -> None:
        with unittest.mock.patch.object(csilgen_release, "find_release", return_value=None):
            with unittest.mock.patch.object(
                deps, "_download", side_effect=deps.NotPublished("no asset")
            ):
                self.assertFalse(deps.fetch_transport())
            with unittest.mock.patch.object(
                deps, "_download", side_effect=ToolFailed("the network went away")
            ):
                with self.assertRaises(ToolFailed):
                    deps.fetch_transport()


class TheGenerators(Scratch):
    def setUp(self) -> None:
        super().setUp()
        self.link = self.root / "csil" / ".generators"
        self.link.parent.mkdir()
        self.store = self.root / ".deps" / "csilgen-generators"
        self.pin("0.2.8")
        patches = (
            unittest.mock.patch.object(deps, "DEPENDENCY_DIR", self.root / ".deps"),
            unittest.mock.patch.object(deps, "GENERATOR_STORE", self.store),
            unittest.mock.patch.object(deps, "GENERATOR_LINK", self.link),
            unittest.mock.patch.object(csilgen_release, "find_release", return_value=None),
        )
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)

    def pin(self, version: str) -> None:
        for name, value in (
            ("GENERATOR_VERSION", version),
            ("CSILGEN_VERSION", version),
            ("GENERATOR_DIR", self.store / version),
        ):
            patch = unittest.mock.patch.object(deps, name, value)
            patch.start()
            self.addCleanup(patch.stop)

    def _serves(self, targets=("rust", "go", "typescript", "python")):
        def download(url, destination, what, quiet=False, advice=""):
            built = self.archive(
                {f"generators/csilgen_{t}_generator.wasm": t.encode() for t in targets},
                name="served.tar.gz",
            )
            destination.parent.mkdir(parents=True, exist_ok=True)
            os.replace(built, destination)
            return destination

        return unittest.mock.patch.object(deps, "_download", download)

    def test_the_generators_are_installed_for_this_repository(self) -> None:
        with self._serves(), self.quietly():
            deps.fetch_csilgen_generators()
        self.assertTrue(deps.generators_are_pinned())
        self.assertTrue(self.link.is_symlink())
        self.assertEqual(
            (self.link / "csilgen_rust_generator.wasm").read_text(), "rust"
        )
        self.assertNotIn(
            Path.home() / ".csilgen", deps.GENERATOR_DIR.parents,
            "the shared directory holds one version for every repository",
        )

    def test_a_fetch_that_stops_part_way_installs_nothing(self) -> None:
        gone = ToolFailed("the network went away")
        with unittest.mock.patch.object(deps, "_download", side_effect=gone):
            with self.assertRaises(ToolFailed), self.quietly():
                deps.fetch_csilgen_generators()
        self.assertFalse(deps.GENERATOR_DIR.exists())
        self.assertFalse(deps.generators_are_pinned())
        self.assertFalse(self.link.exists())

    def test_generators_without_the_complete_file_are_not_the_pin(self) -> None:
        deps.GENERATOR_DIR.mkdir(parents=True)
        for path in deps.generator_files():
            path.write_text("half")
        self.assertFalse(deps.generators_are_pinned())

    def test_a_new_pin_gets_its_own_directory_and_the_link_moves(self) -> None:
        with self._serves(), self.quietly():
            deps.fetch_csilgen_generators()
        first = deps.GENERATOR_DIR

        self.pin("0.2.9")
        self.assertFalse(deps.generators_are_pinned())
        with self._serves(), self.quietly():
            deps.fetch_csilgen_generators()

        self.assertEqual(self.link.resolve(), (self.store / "0.2.9").resolve())
        self.assertTrue((first / "csilgen_rust_generator.wasm").is_file())

    def test_a_directory_somebody_made_is_not_replaced(self) -> None:
        self.link.mkdir()
        (self.link / "csilgen_rust_generator.wasm").write_text("somebody's own")
        with self._serves(), self.quietly():
            with self.assertRaises(ToolFailed) as refusal:
                deps.fetch_csilgen_generators()
        self.assertIn("./tools.sh deps", str(refusal.exception))
        self.assertEqual(
            (self.link / "csilgen_rust_generator.wasm").read_text(), "somebody's own"
        )


if __name__ == "__main__":
    unittest.main()
