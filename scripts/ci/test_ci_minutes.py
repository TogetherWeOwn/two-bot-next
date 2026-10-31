"""Offline regressions for the CI-minutes baseline/record helper.

Hermetic file I/O only: no `gh`, no network, no clock. Pins the report
skeleton shape the advisory workflow uploads and the ledger append format
maintainers fill in post-merge, so a drifted baseline key fails here.
"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/ci/ci_minutes.py"
BASELINE = ROOT / "docs/ci-speed-baseline.json"


class CiMinutesTests(unittest.TestCase):
    def test_checked_in_baseline_has_the_contract_key(self):
        data = json.loads(BASELINE.read_text(encoding="utf-8"))
        self.assertIn("minutes_per_merged_pr", data)
        self.assertIsInstance(data["minutes_per_merged_pr"], (int, float))
        self.assertTrue(data.get("source"))

    def test_report_writes_a_pending_delta_skeleton(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "ci-minutes.json"
            run = subprocess.run(
                [sys.executable, str(SCRIPT), "report", "--baseline", str(BASELINE),
                 "--run-id", "123", "--sha", "abc123", "--event", "pull_request",
                 "--output", str(output)],
                capture_output=True,
                text=True,
            )
            self.assertEqual(run.returncode, 0, run.stderr)
            record = json.loads(output.read_text(encoding="utf-8"))
            baseline = json.loads(BASELINE.read_text(encoding="utf-8"))
            self.assertEqual(record["baseline_minutes_per_merged_pr"], baseline["minutes_per_merged_pr"])
            self.assertEqual(record["delta_minutes_per_merged_pr"], "pending")
            self.assertEqual(record["this_run"], {"run_id": "123", "sha": "abc123", "event": "pull_request"})

    def test_report_refuses_a_baseline_without_the_key(self):
        with tempfile.TemporaryDirectory() as tmp:
            baseline = Path(tmp) / "baseline.json"
            baseline.write_text('{"source": "x"}', encoding="utf-8")
            run = subprocess.run(
                [sys.executable, str(SCRIPT), "report", "--baseline", str(baseline),
                 "--run-id", "1", "--sha", "s", "--event", "push",
                 "--output", str(Path(tmp) / "out.json")],
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(run.returncode, 0)

    def test_record_appends_one_json_row_per_call(self):
        with tempfile.TemporaryDirectory() as tmp:
            ledger = Path(tmp) / "ledger.jsonl"
            for pr, minutes in (("101", "42.5"), ("102", "40")):
                run = subprocess.run(
                    [sys.executable, str(SCRIPT), "record", "--ledger", str(ledger),
                     "--pr", pr, "--minutes", minutes, "--sha", "deadbeef"],
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(run.returncode, 0, run.stderr)
            rows = [json.loads(line) for line in ledger.read_text(encoding="utf-8").splitlines()]
            self.assertEqual(rows, [
                {"pr": 101, "minutes": 42.5, "sha": "deadbeef"},
                {"pr": 102, "minutes": 40.0, "sha": "deadbeef"},
            ])

    def test_record_refuses_negative_minutes(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = subprocess.run(
                [sys.executable, str(SCRIPT), "record", "--ledger", str(Path(tmp) / "l.jsonl"),
                 "--pr", "1", "--minutes", "-3", "--sha", "s"],
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(run.returncode, 0)


if __name__ == "__main__":
    unittest.main()
