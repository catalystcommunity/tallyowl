"""Every shipped alert rule names an instrument that a crate declares.

An alert on an instrument that does not exist can never fire, and it looks like
protection until the day it is needed. docs/FAILURE_MODES.md section 13 named
three such instruments for a release. This keeps the shipped rules from doing
the same.
"""

from __future__ import annotations

import re
import unittest

from tallyowl_tools.commands import REPOSITORY_ROOT

RULES = REPOSITORY_ROOT / "deploy" / "monitoring" / "tallyowl-rules.yaml"
NAME = re.compile(r"\btallyowl_[a-z0-9_]+\b")


def declared() -> set[str]:
    """Each instrument name that appears as a string in a crate's source."""
    names: set[str] = set()
    for source in (REPOSITORY_ROOT / "crates").glob("*/src/**/*.rs"):
        names.update(re.findall(r'"(tallyowl_[a-z0-9_]+)"', source.read_text()))
    return names


def undeclared(expression: str, known: set[str]) -> set[str]:
    """The instruments an expression names that no crate declares."""
    return set(NAME.findall(expression)) - known


def expressions() -> list[tuple[str, str]]:
    import yaml

    document = yaml.safe_load(RULES.read_text())
    return [
        (rule["alert"], str(rule["expr"]))
        for group in document["groups"]
        for rule in group["rules"]
    ]


class ShippedRules(unittest.TestCase):
    def test_the_file_holds_rules(self) -> None:
        self.assertGreater(len(expressions()), 10)

    def test_every_rule_names_only_declared_instruments(self) -> None:
        known = declared()
        for alert, expression in expressions():
            with self.subTest(alert):
                self.assertTrue(NAME.findall(expression), f"{alert} names no TallyOwl instrument")
                missing = undeclared(expression, known)
                self.assertFalse(
                    missing,
                    f"{alert} names {sorted(missing)}, and no crate declares it. "
                    "An alert on it can never fire.",
                )

    def test_a_name_no_crate_declares_is_noticed(self) -> None:
        expression = "tallyowl_commits_total > 0 and tallyowl_no_such_instrument > 0"
        self.assertEqual(
            undeclared(expression, {"tallyowl_commits_total"}),
            {"tallyowl_no_such_instrument"},
        )

    def test_every_rule_says_what_to_do(self) -> None:
        import yaml

        for group in yaml.safe_load(RULES.read_text())["groups"]:
            for rule in group["rules"]:
                with self.subTest(rule["alert"]):
                    self.assertTrue(rule["annotations"]["summary"])
                    self.assertTrue(rule["annotations"]["description"])
                    self.assertIn(rule["labels"]["severity"], ("warning", "critical"))


if __name__ == "__main__":
    unittest.main()
