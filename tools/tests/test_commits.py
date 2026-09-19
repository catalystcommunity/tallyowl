"""Tests for the conventional-commit gate.

The gate exists to stop a silent outcome: a commit subject that matches nothing
produces no version change, so a merge publishes nothing and says nothing about
why. These tests hold the gate's list of types **equal to the calculator's**,
because a gate that accepts a type semver-tags ignores would pass a pull
request and then produce exactly the silence it was built to prevent.
"""

from __future__ import annotations

import subprocess
import unittest
import unittest.mock
from tallyowl_tools import commits, deps
from tallyowl_tools.commands import REPOSITORY_ROOT, ToolFailed


def _commit(subject: str, body: str = "") -> commits.Commit:
    return commits.Commit(hash="a" * 40, subject=subject, body=body)


class Subjects(unittest.TestCase):
    def test_a_type_and_a_subject_are_enough(self) -> None:
        for subject in ("fix: a thing", "feat: another", "docs: a page"):
            self.assertTrue(_commit(subject).conventional, subject)

    def test_a_scope_and_a_breaking_mark_are_read(self) -> None:
        commit = _commit("feat(store)!: change the segment footer")
        self.assertTrue(commit.conventional)
        self.assertEqual(commit.kind, "feat")
        self.assertTrue(commit.breaking)
        self.assertEqual(commit.effect, "major")

    def test_a_breaking_change_in_the_body_is_read(self) -> None:
        commit = _commit("fix: read the footer", "BREAKING CHANGE: the footer moved")
        self.assertTrue(commit.breaking)
        self.assertEqual(commit.effect, "major")

    def test_what_is_not_a_conventional_commit(self) -> None:
        for subject in (
            "fixed the thing",
            "Fix: capitalised",
            "fix:no space",
            "fix: ",
            "feat",
            "",
            "wip: a type nobody configured",
        ):
            self.assertFalse(_commit(subject).conventional, subject)

    def test_each_type_says_what_it_does_to_the_version(self) -> None:
        self.assertEqual(_commit("feat: a").effect, "minor")
        self.assertEqual(_commit("fix: a").effect, "patch")
        self.assertEqual(_commit("chore: a").effect, "patch")
        self.assertEqual(_commit("norelease: a").effect, "no release, deliberately")
        self.assertEqual(_commit("nope: a").effect, "nothing, and this is a refusal")


class TheCalculatorAgrees(unittest.TestCase):
    """The gate's types must be the calculator's types, and nothing else."""

    def setUp(self) -> None:
        self.program = deps.semver_tags_program()

    def _flag_default(self, flag: str) -> set[str]:
        """What semver-tags itself says the default list for a flag is.

        Read from the tool rather than copied from its source: the source is a
        checkout that may not be there, and the binary is the thing that will
        compute the version.
        """
        import re

        help_text = subprocess.run(
            [self.program, "run", "--help"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
        found = re.search(rf"--{flag}\s+\S+.*?\(default \[([^\]]*)\]\)", help_text)
        if not found:
            raise AssertionError(f"semver-tags help does not describe --{flag}")
        return {word.strip() for word in found.group(1).split(",") if word.strip()}

    def test_the_patch_types_are_the_calculators(self) -> None:
        self.assertEqual(set(commits.PATCH_TYPES), self._flag_default("patch_types"))

    def test_the_minor_types_are_the_calculators(self) -> None:
        self.assertEqual(set(commits.MINOR_TYPES), self._flag_default("minor_types"))

    def test_a_type_the_gate_accepts_either_releases_or_says_it_will_not(self) -> None:
        releasing = {*commits.PATCH_TYPES, *commits.MINOR_TYPES}
        for kind in commits.ALL_TYPES:
            if kind in commits.NO_RELEASE_TYPES:
                self.assertNotIn(kind, releasing, f"{kind} cannot both release and not")
            else:
                self.assertIn(kind, releasing, f"{kind} would pass the gate and release nothing")


class Parsing(unittest.TestCase):
    """A commit body has blank lines in it, and a log is one string."""

    def _log(self, records: list[tuple[str, str, str]]) -> str:
        return "".join(f"{h}\x1f{s}\x1f{b}\x1e\n" for h, s, b in records)

    def test_a_body_with_blank_lines_does_not_run_into_the_next_commit(self) -> None:
        log = self._log(
            [
                ("a" * 40, "feat: one", "why it happened\n\nBREAKING CHANGE: it moved\n"),
                ("b" * 40, "fix: two", ""),
            ]
        )
        with unittest.mock.patch.object(
            commits, "run", return_value=type("R", (), {"stdout": log, "ok": True})()
        ):
            parsed = commits.read_commits("base")
        self.assertEqual(len(parsed), 2)
        self.assertEqual([c.subject for c in parsed], ["feat: one", "fix: two"])
        # The hash must not carry the newline the previous record ended with.
        for commit in parsed:
            self.assertEqual(len(commit.hash), 40, repr(commit.hash))
            self.assertTrue(commit.hash.isalnum())
        self.assertTrue(parsed[0].breaking, "the marker in the body is read")
        self.assertFalse(parsed[1].breaking)

    def test_an_empty_log_is_no_commits(self) -> None:
        with unittest.mock.patch.object(
            commits, "run", return_value=type("R", (), {"stdout": "", "ok": True})()
        ):
            self.assertEqual(commits.read_commits("base"), [])


class Refusals(unittest.TestCase):
    def test_the_refusal_says_how_to_write_one(self) -> None:
        with unittest.mock.patch.object(
            commits, "read_commits", return_value=[_commit("just some words")]
        ):
            with self.assertRaises(ToolFailed) as refusal:
                commits.check()
        message = str(refusal.exception)
        self.assertIn("type: subject", message)
        self.assertIn("feat", message)
        self.assertIn("norelease", message)

    def test_a_branch_of_good_commits_passes(self) -> None:
        good = [_commit("feat: add a thing"), _commit("docs: say what it does")]
        with unittest.mock.patch.object(commits, "read_commits", return_value=good):
            self.assertEqual(commits.check(), 0)

    def test_one_bad_commit_among_good_ones_refuses(self) -> None:
        mixed = [_commit("feat: fine"), _commit("oops"), _commit("fix: fine")]
        with unittest.mock.patch.object(commits, "read_commits", return_value=mixed):
            with self.assertRaises(ToolFailed) as refusal:
                commits.check()
        self.assertIn("1 of 3", str(refusal.exception))

    def test_a_branch_that_releases_nothing_passes_and_says_so(self) -> None:
        with unittest.mock.patch.object(
            commits, "read_commits", return_value=[_commit("norelease: notes only")]
        ):
            self.assertEqual(commits.check(), 0)


class TheRepositoryItself(unittest.TestCase):
    def test_this_repositorys_own_history_passes_the_gate(self) -> None:
        """The gate has to accept the commits that are already here.

        A gate nobody could have merged the existing history through is a gate
        that will be turned off rather than obeyed.
        """
        log = subprocess.run(
            ["git", "log", "--pretty=format:%s", "-20"],
            cwd=REPOSITORY_ROOT,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.splitlines()
        for subject in log:
            self.assertTrue(_commit(subject).conventional, f"already merged: {subject}")


if __name__ == "__main__":
    unittest.main()
