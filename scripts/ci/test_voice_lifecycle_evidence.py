"""Offline contract for voice measurement lifecycle evidence."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("voice_lifecycle_evidence.py")


class LifecycleEvidenceTests(unittest.TestCase):
    def test_receipt_and_refusal_cases(self):
        success = [{"check": name, "verdict": "PASS"} for name in ("create", "moved", "delete")]
        cases = (
            (None, "evidence_missing"),
            ([], "evidence_missing"),
            ([success, success], "retry_inflated_or_unexpected_files"),
            ([success], "single_successful_lifecycle"),
            ([success[:-1]], "lifecycle_incomplete"),
            ([success + [{"check": "other", "verdict": "FAIL"}]], "lifecycle_failed"),
            ([success + [{"check": "create", "verdict": "PASS"}]], "evidence_invalid"),
            ([success + [{"check": "other", "verdict": "UNKNOWN"}]], "evidence_invalid"),
            (["malformed"], "evidence_invalid"),
        )
        for payloads, expected in cases:
            with self.subTest(expected=expected, payloads=payloads), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp) / "evidence"
                if payloads is not None:
                    root.mkdir()
                    for attempt, checks in enumerate(payloads, 1):
                        (root / f"voice-synthetic-123-{attempt}.json").write_text(
                            json.dumps({"checks": checks}), encoding="utf-8")
                output = Path(tmp) / "result" / "receipt.json"
                result = subprocess.run([sys.executable, "-I", str(SCRIPT), str(root), "123", str(output)],
                                        capture_output=True, text=True, check=False)
                receipt = json.loads(output.read_text(encoding="utf-8"))
                valid = expected == "single_successful_lifecycle"
                self.assertEqual(result.returncode, 0 if valid else 1)
                self.assertEqual(receipt["schema"], "voice-rest-lifecycle-v1")
                self.assertEqual((receipt["valid"], receipt["code"]), (valid, expected))
                self.assertNotIn("detail", result.stdout + output.read_text(encoding="utf-8"))

    def test_duplicate_json_keys_are_rejected(self):
        valid = '[{"check":"create","verdict":"PASS"},{"check":"moved","verdict":"PASS"},{"check":"delete","verdict":"PASS"}]'
        cases = (
            '{"checks":[],"checks":' + valid + '}',
            '{"checks":[{"check":"create","verdict":"FAIL","verdict":"PASS"},'
            '{"check":"moved","verdict":"PASS"},{"check":"delete","verdict":"PASS"}]}',
        )
        for raw in cases:
            with self.subTest(raw=raw), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp) / "evidence"
                root.mkdir()
                (root / "voice-synthetic-123-1.json").write_text(raw, encoding="utf-8")
                output = Path(tmp) / "receipt.json"
                result = subprocess.run([sys.executable, "-I", str(SCRIPT), str(root), "123", str(output)],
                                        capture_output=True, text=True, check=False)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(json.loads(output.read_text(encoding="utf-8"))["code"], "evidence_invalid")

    def test_unexpected_names_and_symlink_are_not_published(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "evidence"
            root.mkdir()
            output = Path(tmp) / "receipt.json"
            (root / "private-name").write_text("secret", encoding="utf-8")
            command = [sys.executable, "-I", str(SCRIPT), str(root), "123", str(output)]
            result = subprocess.run(command, capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 1)
            self.assertNotIn("private-name", output.read_text(encoding="utf-8") + result.stdout)
            (root / "private-name").unlink()
            (root / "voice-synthetic-123-1.json").symlink_to(output)
            result = subprocess.run(command, capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(json.loads(output.read_text(encoding="utf-8"))["code"], "evidence_invalid")


if __name__ == "__main__":
    unittest.main()
