"""Synthetic policy adapter regressions; no workflow or live reads."""
import sys
from pathlib import Path
import unittest
from unittest import mock

from test_attestation_event_identity import event
from test_attestation_snapshot import inventory

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / ".github/scripts"))
import pr_standards as policy


BODY = "## Issue Description\n\nA synthetic change with a meaningful description.\n\n## Verification\n\nTests passed.\n"
ATTESTATION = (
    "\n## Attestation\n\nrisk_tier: T0\n\n### Claims\n\n- C1: Safe behavior\n\n"
    "### Tests\n\n- C1 | test=tests/example.py | failed_before=not-run | proof=synthetic\n\n"
    "### High-risk checklist\n\n" + "".join(f"- [x] D{i}: Synthetic evidence\n" for i in range(1, 9))
)


def metadata(body=BODY):
    data = event()
    data["pull_request"].update(title="feat(ci): exercise adapter", body=body, changed_files=1)
    return data


def evaluate(data, paths):
    return inventory.evaluate_metadata(data, "org/repo", "a" * 40, paths, policy)


class PolicyAdapter(unittest.TestCase):
    def assert_refused(self, data, paths):
        with self.assertRaises(inventory.InventoryError) as error:
            evaluate(data, paths)
        self.assertEqual(str(error.exception), "Metadata policy evaluation refused")
        self.assertIsNone(error.exception.__cause__)

    def test_valid_policy_and_t0_attestation(self):
        self.assertEqual(evaluate(metadata(), ["docs/example.md"]),
                         {"head_sha": "b" * 40, "base_sha": "a" * 40,
                          "file_count": 1, "path_count": 1})
        self.assertEqual(evaluate(metadata(BODY + ATTESTATION), ["sql/change.sql"])["path_count"], 1)
        for path in inventory.SELF_GATE_PATHS:
            with self.subTest(path=path):
                self.assert_refused(metadata(), [path])
                self.assertEqual(evaluate(metadata(BODY + ATTESTATION), [path])["path_count"], 1)

    def test_invalid_attestation_and_rename_source_floor(self):
        self.assert_refused(metadata(), ["sql/change.sql"])
        self.assert_refused(metadata(BODY + ATTESTATION.replace("[x] D8", "[ ] D8")),
                            ["sql/change.sql"])
        self.assert_refused(metadata(), ["docs/new.md", "sql/old.sql"])
        renamed = inventory.changed_paths(
            lambda route: [{"status": "renamed", "filename": "docs/new.md",
                            "previous_filename": "sql/old.sql"}], "pulls/1", 1)
        self.assert_refused(metadata(), renamed)
        self.assertEqual(evaluate(metadata(BODY + ATTESTATION), renamed)["path_count"], 2)

    def test_policy_errors_and_release_truncation_refuse(self):
        data = metadata()
        data["pull_request"]["title"] = "bad SENTINEL title"
        self.assert_refused(data, ["docs/example.md"])
        self.assert_refused(metadata("  This release is too large to preview in the pull request body.\nSENTINEL"),
                            ["docs/example.md"])
        self.assert_refused(metadata(None), ["docs/example.md"])

    def test_malformed_inputs_and_policy_exceptions_are_redacted(self):
        for paths in (None, [], ["../SENTINEL"], ["docs/a", "docs/a"],
                      ["docs/a", "docs/b", "docs/c"], ["docs/a\nSENTINEL"]):
            with self.subTest(paths=paths):
                self.assert_refused(metadata(), paths)
        data = metadata()
        data["repository"]["full_name"] = "SENTINEL"
        self.assert_refused(data, ["docs/example.md"])
        with mock.patch.object(policy, "evaluate", side_effect=RuntimeError("SENTINEL")):
            self.assert_refused(metadata(), ["docs/example.md"])


if __name__ == "__main__":
    unittest.main()
