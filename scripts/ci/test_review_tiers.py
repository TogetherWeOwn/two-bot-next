"""Offline contracts for the audited review-tier allowlist, not a classifier."""

from fnmatch import fnmatchcase
from pathlib import Path
import re
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
AUDITED_ORDINARY = ["crates/discord/src/brand_tokens.rs"]


def matches(path, pattern):
    """Match this map's literal/star-only paths, with slash-bounded stars."""
    parts = path.split("/") if path else []
    globs = pattern.split("/") if pattern else []
    if not globs:
        return not parts
    if globs[0] == "**":
        return matches(path, "/".join(globs[1:])) or bool(parts) and matches(
            "/".join(parts[1:]), pattern)
    return bool(parts) and fnmatchcase(parts[0], globs[0]) and matches(
        "/".join(parts[1:]), "/".join(globs[1:]))


class ReviewTierTests(unittest.TestCase):
    def test_path_matching_keeps_single_stars_inside_one_component(self):
        self.assertTrue(matches("crates/discord/tests/deep/case.rs", "crates/*/tests/**"))
        self.assertFalse(matches("crates/discord/src/internal_actions/tests/case.rs",
                                 "crates/*/tests/**"))
        self.assertTrue(matches("tests/case.py", "tests/**/*.py"))
        self.assertTrue(matches("tests/deep/case.py", "tests/**/*.py"))

    def setUp(self):
        self.text = (ROOT / ".github/review-tiers.yml").read_text()
        self.policy = yaml.safe_load(self.text)
        self.tiers = self.policy["tiers"]

    def test_ordinary_is_only_the_exact_audited_module(self):
        self.assertEqual(self.tiers["ordinary"], AUDITED_ORDINARY)
        for path in self.tiers["ordinary"]:
            self.assertTrue((ROOT / path).is_file(), path)
            self.assertFalse(any(char in path for char in "*?[]{}!()"), path)

    def test_audited_module_stays_numeric_constants_only(self):
        lines = (ROOT / AUDITED_ORDINARY[0]).read_text().splitlines()
        constants = [line for line in lines if line and not line.startswith("//")]
        self.assertTrue(constants)
        for line in constants:
            self.assertIsNotNone(re.fullmatch(
                r"pub const [A-Z0-9_]+: u(?:32|64) = (?:0x[0-9A-Fa-f]+|[0-9_]+);",
                line), line)

    def test_ordinary_has_no_sensitive_overlap(self):
        for patterns in self.tiers.values():
            for pattern in patterns:
                self.assertFalse(any(char in pattern for char in "?[]{}!()"), pattern)
        for pattern in self.tiers["sensitive"]:
            for path in AUDITED_ORDINARY:
                self.assertFalse(matches(path, pattern), (path, pattern))

    def test_sensitive_floor_and_fail_closed_default_remain(self):
        self.assertEqual(self.policy["version"], 1)
        self.assertEqual(self.policy["default_tier"], "sensitive")
        for pattern in [
            "src/**", "crates/bot/src/**", "crates/core/src/**",
            "crates/store/src/**", "crates/cutover/**",
            "crates/voice-probe/src/**", "crates/testsupport/src/**",
            "wrangler/src/**", "sql/**", "crates/*/migrations/**",
            ".github/workflows/**", ".github/review-tiers.yml",
            ".github/scripts/**", "scripts/**", "bin/**", "deploy/**",
            "CONTRIBUTING.md", "CONSTRAINTS.md", "REVIEW.md", "SECURITY.md",
            "assets/brand/two-icon-1-dark-512.png",
        ]:
            with self.subTest(pattern=pattern):
                self.assertIn(pattern, self.tiers["sensitive"])

    def test_other_discord_source_cannot_fall_into_a_lower_tier(self):
        # Removing the blanket Discord-source glob only exposes the exact
        # ordinary entry. Every other module, including future paths, inherits
        # default_tier=sensitive; no broad low/ordinary entry may capture it.
        paths = [
            str(path.relative_to(ROOT))
            for path in (ROOT / "crates/discord/src").rglob("*")
            if path.is_file()
        ] + [
            "crates/discord/src/future.rs",
            "crates/discord/src/new_boundary/parser.rs",
            "crates/discord/src/nested/brand_tokens.rs",
            "crates/discord/src/brand_tokens_extra.rs",
        ]
        for path in paths:
            if path in AUDITED_ORDINARY:
                continue
            for pattern in self.tiers["low"] + self.tiers["ordinary"]:
                with self.subTest(path=path, pattern=pattern):
                    self.assertFalse(matches(path, pattern))

    def test_content_escalation_and_security_review_are_retained(self):
        for rule in [
            "Content triggers escalate any tier to sensitive",
            "auth/authz/session", "crypto/keys", "secrets/credentials",
            "permission checks", "SQL/schema/migrations",
            "dependency or", "lockfile changes", "CI/workflow changes",
            "weakened test assertions", "QA verifies", "may",
            "escalate any PR", "This file itself is always sensitive",
        ]:
            with self.subTest(rule=rule):
                self.assertIn(rule, self.text)


if __name__ == "__main__":
    unittest.main()
