"""Soak-evidence parser regressions: committed fixtures only, no staging."""

import io
import json
from pathlib import Path
import unittest

from soak_evidence import summarize

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "scripts/fixtures/soak_evidence_sample.jsonl"
EXPECTED = ROOT / "scripts/fixtures/soak_evidence_sample.summary.json"


def run(text):
    return summarize(io.StringIO(text))


class SoakEvidenceTests(unittest.TestCase):
    def test_fixture_pair_matches_committed_summary(self):
        with FIXTURE.open(encoding="utf-8") as handle:
            actual = summarize(handle)
        expected = json.loads(EXPECTED.read_text(encoding="utf-8"))
        self.assertEqual(actual, expected)
        # Spot-check the acceptance shape, not just the blob.
        self.assertEqual(actual["connects"], 2)
        self.assertEqual(actual["disconnects"], 1)
        self.assertEqual(actual["resumes"], 1)
        self.assertEqual(actual["missed_events"], 2)
        self.assertEqual(len(actual["unknown_intervals"]), 2)
        self.assertEqual(actual["verdict"], "NEEDS WORK")

    def test_clean_week_passes(self):
        summary = run(
            '{"ts": "2026-09-20T00:00:01Z", "event": "connect", "session": "a"}\n'
            '{"ts": "2026-09-20T00:00:02Z", "event": "dispatch", "session": "a", "seq": 1}\n'
            '{"ts": "2026-09-20T00:00:03Z", "event": "dispatch", "session": "a", "seq": 2}\n'
        )
        self.assertEqual(summary["missed_events"], 0)
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertEqual(summary["verdict"], "PASS")

    def test_new_session_resets_sequence(self):
        summary = run(
            '{"ts": "2026-09-20T00:00:01Z", "event": "connect", "session": "a"}\n'
            '{"ts": "2026-09-20T00:00:02Z", "event": "dispatch", "session": "a", "seq": 50}\n'
            '{"ts": "2026-09-20T00:00:03Z", "event": "disconnect", "session": "a"}\n'
            '{"ts": "2026-09-20T00:00:04Z", "event": "connect", "session": "b"}\n'
            '{"ts": "2026-09-20T00:00:05Z", "event": "dispatch", "session": "b", "seq": 1}\n'
        )
        self.assertEqual(summary["missed_sequence_windows"], [])
        self.assertEqual(summary["missed_events"], 0)
        self.assertEqual(summary["redeploy_gaps_s"], [1.0])

    def test_unknown_events_are_preserved_not_dropped(self):
        summary = run(
            '{"ts": "2026-09-20T00:00:01Z", "event": "connect", "session": "a"}\n'
            '{"ts": "2026-09-20T00:00:02Z", "event": "frobnicate", "session": "a"}\n'
            "not json at all\n"
            '{"ts": "2026-09-20T00:00:03Z", "event": "dispatch", "session": "a"}\n'
        )
        reasons = [u["reason"] for u in summary["unknown_intervals"]]
        self.assertEqual(len(reasons), 3)
        self.assertTrue(any("frobnicate" in r for r in reasons))
        self.assertTrue(any("not JSON" in r for r in reasons))
        self.assertTrue(any("without integer seq" in r for r in reasons))
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_log_ending_disconnected_stays_unknown(self):
        summary = run(
            '{"ts": "2026-09-20T00:00:01Z", "event": "connect", "session": "a"}\n'
            '{"ts": "2026-09-20T00:00:02Z", "event": "disconnect", "session": "a"}\n'
        )
        self.assertEqual(len(summary["unknown_intervals"]), 1)
        self.assertIn("disconnected", summary["unknown_intervals"][0]["reason"])
        self.assertIsNone(summary["unknown_intervals"][0]["end"])
        self.assertEqual(summary["verdict"], "NEEDS WORK")


if __name__ == "__main__":
    unittest.main()
