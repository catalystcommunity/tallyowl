"""Tests for finding a language toolchain.

These exist because of one production failure. `test-go` passed on a
workstation and on `run-local`, then failed in CI with `KeyError: 'PATH'`. The
difference was `.deps/node/bin`: it existed locally, so the environment
dictionary carried a `PATH`, and it did not exist in a fresh CI clone, so the
dictionary carried only the Go cache settings and the lookup read a key that
was not there.

A workstation is the one machine where this cannot reproduce. So the tests
below build the CI shape on purpose: no toolchain bundle, no fetched Node, and
a `GOPATH` that cannot be written.
"""

from __future__ import annotations

import os
import unittest
import unittest.mock
from pathlib import Path

from tallyowl_tools import deps, packages
from tallyowl_tools.commands import ToolFailed


class ToolchainEnvironment(unittest.TestCase):
    def _ci_shape(self):
        """No catalyst-tools, no fetched Node, an unwritable GOPATH."""
        return (
            unittest.mock.patch.object(packages, "CATALYST_TOOLS", Path("/nonexistent")),
            unittest.mock.patch.object(deps, "node_bin", return_value=None),
            unittest.mock.patch.dict(os.environ, {"GOPATH": "/go"}),
        )

    def test_the_environment_can_carry_a_go_cache_and_no_path(self) -> None:
        with self._ci_shape()[0], self._ci_shape()[1], self._ci_shape()[2]:
            environment = packages._toolchain_environment()
        self.assertIn("GOPATH", environment, "an unwritable GOPATH must be replaced")
        self.assertNotIn("PATH", environment, "there is no toolchain directory to add")

    def test_finding_a_program_survives_an_environment_with_no_path(self) -> None:
        """The production failure, as a test: this raised `KeyError: 'PATH'`."""
        with self._ci_shape()[0], self._ci_shape()[1], self._ci_shape()[2]:
            found, environment = packages._program("sh", "install a shell")
        self.assertTrue(found.endswith("sh"))
        self.assertIn("GOPATH", environment)

    def test_the_path_is_restored_whatever_happens(self) -> None:
        before = os.environ.get("PATH", "")
        with self._ci_shape()[0], self._ci_shape()[1], self._ci_shape()[2]:
            with self.assertRaises(ToolFailed):
                packages._program("a-program-nobody-installs", "install it")
        self.assertEqual(os.environ.get("PATH", ""), before)

    def test_a_writable_gopath_is_left_alone(self) -> None:
        """Nothing here overrides a developer's own cache."""
        with unittest.mock.patch.dict(os.environ, {"GOPATH": "/tmp"}):
            self.assertEqual(packages.go_environment(), {})


if __name__ == "__main__":
    unittest.main()
