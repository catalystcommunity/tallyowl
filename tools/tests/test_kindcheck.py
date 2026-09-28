"""Tests for the parts of `kind-check` that need no cluster.

The procedure itself runs only in a live cluster. What it decides from what a
cluster says is plain text in, verdict out, and is tested here.
"""

from __future__ import annotations

import unittest

from tallyowl_tools import kindcheck
from tallyowl_tools.commands import ToolFailed
from tallyowl_tools.kindcheck import GroupView


class Metrics(unittest.TestCase):
    def test_a_sample_is_keyed_by_its_name_and_labels(self) -> None:
        samples = kindcheck.parse_metrics(
            "# HELP x A counter.\n"
            "# TYPE x counter\n"
            'tallyowl_tls_handshakes_refused_total{listener="intake"} 4\n'
            "tallyowl_consensus_groups_count 2\n"
            "not_a_number NaNish\n"
        )
        self.assertEqual(samples['tallyowl_tls_handshakes_refused_total{listener="intake"}'], 4)
        self.assertEqual(samples["tallyowl_consensus_groups_count"], 2)
        self.assertNotIn("not_a_number", samples)


class HelmList(unittest.TestCase):
    def test_a_comma_inside_one_value_stays_in_that_value(self) -> None:
        # The role list is one argument to `token create`. Unescaped, Helm made
        # it two, and the verb read the second role as a workspace.
        self.assertEqual(
            kindcheck.helm_list(["token", "create", "collectors", "collector-intake,collector-forwarder"]),
            "{token,create,collectors,collector-intake\\,collector-forwarder}",
        )


class Credentials(unittest.TestCase):
    def test_the_key_and_the_token_are_found_in_their_job_logs(self) -> None:
        logs = "Project: shop\n\ntow_09cb8b7269601939_kZZ1kep-y15Dq\n\nThis is the only time"
        self.assertEqual(kindcheck.credential(logs, "tow_", "key"), "tow_09cb8b7269601939_kZZ1kep-y15Dq")
        token_logs = "towr_277cbf2deee3a6fc_8RSGAvn\nThis role token is printed one time."
        self.assertEqual(kindcheck.credential(token_logs, "towr_", "token"), "towr_277cbf2deee3a6fc_8RSGAvn")

    def test_a_log_with_no_credential_says_so_and_shows_the_log(self) -> None:
        with self.assertRaisesRegex(ToolFailed, "(?s)printed no project key.*is not a verb"):
            kindcheck.credential("`token` is not a verb this binary has.", "tow_", "project key")

    def test_a_key_prefix_does_not_match_inside_a_role_token(self) -> None:
        # `towr_` begins with `tow`, and the key is `tow_`.
        with self.assertRaises(ToolFailed):
            kindcheck.credential("towr_abc", "tow_", "project key")


class CorndogsImage(unittest.TestCase):
    def test_the_pinned_image_is_read_from_the_chart_values(self) -> None:
        values = (kindcheck.REPOSITORY_ROOT / "charts/tallyowl/values.yaml").read_text()
        self.assertRegex(kindcheck.corndogs_image(values), r"/corndogs:\d+\.\d+\.\d+$")

    def test_values_with_no_image_are_refused(self) -> None:
        with self.assertRaises(ToolFailed):
            kindcheck.corndogs_image("corndogsDeployment:\n  enabled: false\n")


def view(pod: str, groups: float = 2, led: float = 0, leaderless: float = 0, lag: float = 0) -> GroupView:
    return GroupView(pod=pod, groups=groups, led=led, leaderless=leaderless, lag=lag)


class Consensus(unittest.TestCase):
    def test_one_head_leading_both_groups_is_agreement(self) -> None:
        self.assertIsNone(kindcheck.consensus_verdict([view("cell-0"), view("cell-1", led=2), view("cell-2")], 2))

    def test_each_way_a_cell_can_disagree_is_named(self) -> None:
        cases = {
            "runs 1 groups": [view("cell-0", groups=1), view("cell-1", led=2), view("cell-2")],
            "no leader": [view("cell-0", leaderless=1), view("cell-1", led=2), view("cell-2")],
            "lag of 40": [view("cell-0", lag=40), view("cell-1", led=2), view("cell-2")],
            "no head": [view("cell-0"), view("cell-1"), view("cell-2")],
            "one head should lead": [view("cell-0", led=1), view("cell-1", led=1), view("cell-2")],
        }
        for fragment, views in cases.items():
            with self.subTest(fragment):
                self.assertIn(fragment, kindcheck.consensus_verdict(views, 2) or "")


if __name__ == "__main__":
    unittest.main()
