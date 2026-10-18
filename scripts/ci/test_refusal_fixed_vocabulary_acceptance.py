"""Acceptance pins for the fixed-vocabulary refusal ast-grep rule.

QA-owned acceptance suite for the build card `ci: pin fixed-vocabulary
refusal errors with an ast-grep rule` (merged as PR #1083). The Builder
owns `ast-grep-rules/refusal-fixed-vocabulary.yml`,
`ast-grep-rule-tests/refusal-fixed-vocabulary-test.yml` and
`scripts/ci/test_refusal_fixed_vocabulary.py`; this file only reads them
and never edits builder-owned files (owner 2026-10-12 pairing rule).

Acceptance criteria covered here (offline, no network, no DB, no
secrets in fixtures):

- an interpolated-input refusal fixture trips the rule;
- fixed-vocabulary fixtures (fixed strings, error-class enums) pass;
- the rule is green on the clean tree.

Failing-first proof: with the rule file hidden (simulating the pre-fix
tree, where `git show <merge>^:...refusal-fixed-vocabulary.yml` does not
exist) `test_rule_exists_and_is_wired` and the trip tests fail; with the
rule present the full file is green. The scratch-mutant test reproves the
gap without mutating the tree.
"""

import importlib.util
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
RULE_PATH = ROOT / "ast-grep-rules/refusal-fixed-vocabulary.yml"
FIXTURE_PATH = ROOT / "ast-grep-rule-tests/refusal-fixed-vocabulary-test.yml"
SGCONFIG_PATH = ROOT / "sgconfig.yml"
PIN_PATH = ROOT / "scripts/ci/test_refusal_fixed_vocabulary.py"

RULE_ID = "refusal-fixed-vocabulary"
PINNED_BINARY = Path("/paperclip/.cache/ast-grep-0.50.0/ast-grep")


def load_pin():
    """Load the builder's offline pin module without importing test globs."""
    spec = importlib.util.spec_from_file_location("refusal_pin", PIN_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class RulePresenceTests(unittest.TestCase):
    def test_rule_exists_and_is_wired(self):
        """Fails on the pre-fix tree (no rule file); passes with the fix."""
        self.assertTrue(RULE_PATH.is_file(), "rule file missing (pre-fix gap)")
        rule = yaml.safe_load(RULE_PATH.read_text())
        self.assertEqual(rule["id"], RULE_ID)
        self.assertEqual(rule["language"], "python")
        config = yaml.safe_load(SGCONFIG_PATH.read_text())
        self.assertIn("ast-grep-rules", config["ruleDirs"])
        test_dirs = [e.get("testDir") for e in config.get("testConfigs", [])]
        self.assertIn("ast-grep-rule-tests", test_dirs)
        fixture = yaml.safe_load(FIXTURE_PATH.read_text())
        self.assertEqual(fixture["id"], RULE_ID)


class RefusalAcceptanceTests(unittest.TestCase):
    # The exact regressed shape removed by the ops fix: echoing the
    # rejected query key back into the refusal message.
    INTERPOLATED = (
        'raise Refused(f"refusing: the database login may not set {key}")',
        'raise Refused(f"plan database port {port} is refused")',
        'raise Refused(f"psql said: {done.stderr}")',
    )
    FIXED = (
        'raise Refused("refusing: the database login may not set '
        'that query parameter")',
        'raise ReadFailed("psql timed out")',
        "raise Refused(DatabaseError.InvalidPort)",
    )

    def test_interpolated_input_refusal_trips(self):
        pin = load_pin()
        for snippet in self.INTERPOLATED:
            with self.subTest(snippet=snippet):
                self.assertEqual(pin.snippet_verdict(snippet), "flagged")

    def test_fixed_vocabulary_fixtures_pass(self):
        pin = load_pin()
        for snippet in self.FIXED:
            with self.subTest(snippet=snippet):
                self.assertIn(
                    pin.snippet_verdict(snippet), ("clean", "exempted")
                )

    def test_rule_green_on_clean_tree(self):
        pin = load_pin()
        flagged, _ = pin.scan_tree()
        self.assertEqual(flagged, [])

    def test_scratch_mutant_reproves_the_gap(self):
        """Inject the regressed line into a scratch copy; exactly it trips."""
        pin = load_pin()
        fixed = (
            'raise Refused("refusing: the database login may not set '
            'that query parameter")'
        )
        regressed = (
            'raise Refused(f"refusing: the database login may not set {key} '
            'query parameter")'
        )
        with tempfile.TemporaryDirectory() as scratch:
            mutant = Path(scratch) / "staging_events_read.py"
            shutil.copy(ROOT / "scripts/staging_events_read.py", mutant)
            text = mutant.read_text()
            self.assertIn(fixed, text)
            mutant.write_text(text.replace(fixed, regressed))
            broad, exempts = pin.broad_and_exempts()
            flagged = [
                pin.canon(segment)
                for _, segment, message, probes in pin.refusal_statements(
                    mutant
                )
                if pin.classify(segment, message, broad, exempts, probes)
                == "flagged"
            ]
            self.assertEqual(len(flagged), 1)
            self.assertIn("may not set {key}", flagged[0])


class FixtureHygieneTests(unittest.TestCase):
    def test_engine_fixture_suite_passes(self):
        binary = PINNED_BINARY if PINNED_BINARY.is_file() else shutil.which(
            "ast-grep"
        )
        if binary is None:
            self.skipTest("ast-grep binary not available offline")
        proc = subprocess.run(
            [str(binary), "test", "-c", str(SGCONFIG_PATH)],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=120,
        )
        self.assertEqual(
            proc.returncode, 0, f"ast-grep test failed:\n{proc.stdout}"
        )

    def test_fixtures_carry_no_secrets_or_network(self):
        text = FIXTURE_PATH.read_text()
        for marker in ("ghp_", "github_token", "passwd", "BEGIN PRIVATE KEY"):
            self.assertNotIn(marker, text)
        for marker in ("http://", "https://", "socket.", "urllib"):
            self.assertNotIn(marker, text)


if __name__ == "__main__":
    unittest.main()
