"""Tests for the pinned tool versions.

These exist because of one production failure. Release 0.2.0 pushed the image,
cut six tags, made the release page and committed the charts, and then ran
`npm stage publish`. npm answered `Unknown command: "stage"` and the release
stopped with four irreversible steps behind it.

The command was right. The npm was not: `npm stage` arrived in 11.16.0, and
Node 26.1.0 carries npm 11.13.0. The pin named a major that was new enough and
a minor that was three short, and nothing compared the two.

So the two figures are written down next to each other, and this compares
them. A Node bump that carries npm backwards fails here rather than at the end
of a release.
"""

from __future__ import annotations

import unittest

from tallyowl_tools import deps


def _version(text: str) -> tuple[int, ...]:
    """A dotted version as numbers, so 11.16.0 sorts above 11.9.0."""
    return tuple(int(part) for part in text.split("."))


class PinnedNpm(unittest.TestCase):
    def test_the_pinned_node_carries_an_npm_that_can_stage(self) -> None:
        self.assertGreaterEqual(
            _version(deps.NODE_NPM_VERSION),
            _version(deps.NPM_STAGE_MINIMUM),
            f"Node {deps.NODE_VERSION} carries npm {deps.NODE_NPM_VERSION}, and "
            f"`npm stage publish` needs {deps.NPM_STAGE_MINIMUM}. The release job "
            "publishes nothing to npm without it. Pick a Node whose `npm` field "
            "in https://nodejs.org/dist/index.json is new enough.",
        )

    def test_a_major_on_its_own_does_not_settle_it(self) -> None:
        """The comparison must read the minor, which is what 0.2.0 missed."""
        self.assertLess(_version("11.13.0"), _version(deps.NPM_STAGE_MINIMUM))
        self.assertGreaterEqual(_version("11.16.0"), _version(deps.NPM_STAGE_MINIMUM))


if __name__ == "__main__":
    unittest.main()
