"""Tests for the order in which `release package` provisions its inputs.

These exist because of one production failure. Every gate passed — including
the TypeScript suite — and the release job then stopped at `npm run build`
with `Cannot find module '../../../.deps/csilgen/transports/typescript/
src/index.ts'`.

The test job fetches the CSIL transport and the package job did not. Four
TypeScript packages reach that directory by relative path, the compiler copies
it into each package's `dist`, and the image build copies the same directory
out of the build context. So the transport is an input to the packaging step,
not only to the tests, and nothing proved it was there.

A green TypeScript suite is the one signal that cannot catch this, because the
suite is what leaves the transport on disk. The test below removes that help:
it records the order of the calls and refuses a build that starts before the
transport is asked for.
"""

from __future__ import annotations

import tempfile
import unittest
import unittest.mock
from pathlib import Path

from tallyowl_tools import commands, deps, packages, release


class PackageOrder(unittest.TestCase):
    def _run_package(self, root: Path) -> list[str]:
        """Run `release package` against stubs and return what it called, in order."""
        order: list[str] = []

        def record(name):
            def called(*_args, **_kwargs):
                order.append(name)
                return None

            return called

        def recorded_run(command, *_args, **_kwargs):
            # The command is a list whose first element is the program. Only
            # the npm steps matter here; everything else is noise.
            program = Path(str(command[0])).name
            if program.startswith("npm"):
                order.append(f"npm {command[1]}")
            return commands.Result(exit_code=0, stdout="", stderr="")

        with (
            unittest.mock.patch.object(release, "check", return_value=0),
            unittest.mock.patch.object(release, "current_version", return_value="9.9.9"),
            unittest.mock.patch.object(release, "artifact_directory", return_value=root),
            unittest.mock.patch.object(release, "check_tarballs", return_value=0),
            unittest.mock.patch.object(release, "build_binary_archive", return_value=0),
            unittest.mock.patch.object(release, "container_tool", return_value="/bin/true"),
            unittest.mock.patch.object(
                release, "default_image_registry", return_value="example.invalid/tallyowl"
            ),
            unittest.mock.patch.object(deps, "helm_program", return_value="/bin/true"),
            unittest.mock.patch.object(deps, "fetch_csilgen", record("fetch_csilgen")),
            unittest.mock.patch.object(deps, "fetch_node", record("fetch_node")),
            unittest.mock.patch.object(
                packages, "npm_program", return_value=("/usr/bin/npm", {})
            ),
            unittest.mock.patch.object(commands, "run", recorded_run),
        ):
            release.package(root)
        return order

    def test_the_transport_arrives_before_anything_typescript_runs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with unittest.mock.patch.object(release, "MARKER", "no-such-marker.json"):
                order = self._run_package(Path(directory))

        self.assertIn(
            "fetch_csilgen",
            order,
            "the packaging step must fetch the CSIL transport; four TypeScript "
            "packages and the image build read it by relative path",
        )
        builds = [index for index, name in enumerate(order) if name.startswith("npm")]
        self.assertTrue(builds, "the packaging step must build the npm packages")
        self.assertLess(
            order.index("fetch_csilgen"),
            builds[0],
            "the transport must be on disk before the first npm command, or "
            "`tsc` cannot resolve it",
        )


if __name__ == "__main__":
    unittest.main()
