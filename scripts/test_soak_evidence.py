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

    def test_duplicate_event_refused_both_orders(self):
        lines = [
            '{"ts": "2026-09-20T00:00:01Z", "event": "connect", '
            '"session": "a", "event": "dispatch", "seq": 1}\n',
            '{"ts": "2026-09-20T00:00:01Z", "event": "dispatch", "seq": 1, '
            '"session": "a", "event": "connect"}\n',
        ]
        for text in lines:
            summary = run(text)
            self.assertEqual(len(summary["unknown_intervals"]), 1)
            reason = summary["unknown_intervals"][0]["reason"]
            self.assertEqual(reason, "malformed line 1: duplicate field")
            self.assertNotIn("connect", reason)
            self.assertNotIn("dispatch", reason)
            # Refused instead of last-wins: neither side is counted.
            self.assertEqual(summary["connects"], 0)
            self.assertEqual(summary["dispatches"], 0)
            self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_duplicate_identity_and_time_refused_both_orders(self):
        cases = [
            # duplicate identity (session) in both orders
            ('{"ts": "2026-09-20T00:00:01Z", "event": "connect", '
             '"session": "sess-AAA-111", "session": "sess-BBB-222"}\n',
             "sess-AAA-111", "sess-BBB-222"),
            ('{"ts": "2026-09-20T00:00:01Z", "event": "connect", '
             '"session": "sess-BBB-222", "session": "sess-AAA-111"}\n',
             "sess-AAA-111", "sess-BBB-222"),
            # duplicate time (ts) in both orders
            ('{"ts": "2026-09-20T00:00:01Z", "event": "connect", '
             '"session": "sess-AAA-111", "ts": "2026-09-20T00:00:02Z"}\n',
             "2026-09-20T00:00:01Z", "2026-09-20T00:00:02Z"),
            ('{"ts": "2026-09-20T00:00:02Z", "event": "connect", '
             '"session": "sess-AAA-111", "ts": "2026-09-20T00:00:01Z"}\n',
             "2026-09-20T00:00:01Z", "2026-09-20T00:00:02Z"),
        ]
        for text, first, second in cases:
            summary = run(text)
            self.assertEqual(len(summary["unknown_intervals"]), 1)
            reason = summary["unknown_intervals"][0]["reason"]
            self.assertEqual(reason, "malformed line 1: duplicate field")
            self.assertNotIn(first, reason)
            self.assertNotIn(second, reason)
            self.assertEqual(summary["connects"], 0)
            self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_deeply_nested_record_becomes_unknown_then_continues(self):
        nested = "[" * 500 + "]" * 500
        bad = (
            '{"ts": "2026-09-20T00:00:01Z", "event": "connect", '
            f'"session": "a", "payload": {nested}}}\n'
        )
        good = (
            '{"ts": "2026-09-20T00:00:02Z", "event": "connect", '
            '"session": "a"}\n'
        )
        # Must not raise (no traceback); the bad line is UNKNOWN and the
        # good line still counts.
        summary = run(bad + good)
        self.assertEqual(len(summary["unknown_intervals"]), 1)
        reason = summary["unknown_intervals"][0]["reason"]
        self.assertEqual(reason, "malformed line 1: too deeply nested")
        self.assertNotIn(nested, reason)
        self.assertNotIn("payload", reason)
        self.assertEqual(summary["connects"], 1)
        self.assertEqual(summary["verdict"], "NEEDS WORK")
        self.assertEqual(summary["window"]["start"],
                         "2026-09-20T00:00:02+00:00")
        self.assertEqual(summary["window"]["end"],
                         "2026-09-20T00:00:02+00:00")
        dumped = json.dumps(summary)
        self.assertNotIn(nested, dumped)
        self.assertNotIn("Traceback", dumped)

    def test_decoder_recursion_overflow_becomes_unknown_then_continues(self):
        # Deep enough to overflow the C JSON decoder itself (default
        # recursion limit 1000), so json.loads raises RecursionError before
        # _too_deeply_nested is reached. This pins the except RecursionError
        # branch in summarize(): without it the whole run aborts.
        bad = "[" * 100000 + "\n"
        good = (
            '{"ts": "2026-09-20T00:00:02Z", "event": "connect", '
            '"session": "a"}\n'
        )
        # Must not raise; the bad line is UNKNOWN and the good line counts.
        summary = run(bad + good)
        self.assertEqual(len(summary["unknown_intervals"]), 1)
        reason = summary["unknown_intervals"][0]["reason"]
        self.assertEqual(reason, "malformed line 1: too deeply nested")
        self.assertEqual(summary["connects"], 1)
        self.assertEqual(summary["verdict"], "NEEDS WORK")
        self.assertEqual(summary["window"]["start"],
                         "2026-09-20T00:00:02+00:00")
        self.assertEqual(summary["window"]["end"],
                         "2026-09-20T00:00:02+00:00")

    def test_invalid_session_identities_become_unknown_then_continues(self):
        # State-bearing events require a nonempty string session. Array,
        # object, null, missing and empty identities must not raise, must
        # not touch counters/sequence/recovery state, and later valid
        # records are still summarized. Reasons are fixed per event with
        # no identity/value echo.
        bad = (
            '{"ts": "2026-09-20T00:00:01Z", "event": "connect", "session": []}\n'
            '{"ts": "2026-09-20T00:00:02Z", "event": "dispatch", '
            '"session": {}, "seq": 1}\n'
            '{"ts": "2026-09-20T00:00:03Z", "event": "dispatch", "seq": 1}\n'
            '{"ts": "2026-09-20T00:00:04Z", "event": "dispatch", '
            '"session": null, "seq": 1}\n'
            '{"ts": "2026-09-20T00:00:05Z", "event": "dispatch", '
            '"session": "", "seq": 1}\n'
            '{"ts": "2026-09-20T00:00:06Z", "event": "resume", '
            '"session": [], "seq": 1}\n'
        )
        good = (
            '{"ts": "2026-09-20T00:00:07Z", "event": "connect", "session": "a"}\n'
            '{"ts": "2026-09-20T00:00:08Z", "event": "dispatch", '
            '"session": "a", "seq": 1}\n'
            '{"ts": "2026-09-20T00:00:09Z", "event": "dispatch", '
            '"session": "a", "seq": 2}\n'
        )
        # Must not raise (no traceback); bad lines are UNKNOWN and the
        # good lines still count.
        summary = run(bad + good)
        self.assertEqual(len(summary["unknown_intervals"]), 6)
        reasons = [u["reason"] for u in summary["unknown_intervals"]]
        self.assertEqual(reasons.count("connect with invalid session"), 1)
        self.assertEqual(reasons.count("dispatch with invalid session"), 4)
        self.assertEqual(reasons.count("resume with invalid session"), 1)
        for reason in reasons:
            self.assertNotIn("[]", reason)
            self.assertNotIn("{}", reason)
            self.assertNotIn("None", reason)
            self.assertNotIn("null", reason)
            self.assertNotIn("Traceback", reason)
        # Invalid identities leave no counter, sequence or recovery trace.
        self.assertEqual(summary["connects"], 1)
        self.assertEqual(summary["dispatches"], 2)
        self.assertEqual(summary["resumes"], 0)
        self.assertEqual(summary["missed_sequence_windows"], [])
        self.assertEqual(summary["missed_events"], 0)
        self.assertEqual(summary["redeploy_gaps_s"], [])
        self.assertEqual(summary["verdict"], "NEEDS WORK")
        dumped = json.dumps(summary)
        self.assertNotIn("Traceback", dumped)

    def test_invalid_session_does_not_clear_reconnect_gap(self):
        # A malformed connect/resume must not consume the pending
        # disconnect: only the later valid connect closes the gap.
        summary = run(
            '{"ts": "2026-09-20T00:00:01Z", "event": "connect", "session": "a"}\n'
            '{"ts": "2026-09-20T00:00:02Z", "event": "disconnect", "session": "a"}\n'
            '{"ts": "2026-09-20T00:00:03Z", "event": "connect", "session": []}\n'
            '{"ts": "2026-09-20T00:00:04Z", "event": "resume", "session": ""}\n'
            '{"ts": "2026-09-20T00:00:12Z", "event": "connect", "session": "b"}\n'
        )
        reasons = [u["reason"] for u in summary["unknown_intervals"]]
        self.assertIn("connect with invalid session", reasons)
        self.assertIn("resume with invalid session", reasons)
        self.assertEqual(summary["connects"], 2)
        self.assertEqual(summary["resumes"], 0)
        # Gap measured from the disconnect to the valid connect only.
        self.assertEqual(summary["redeploy_gaps_s"], [10.0])
        self.assertEqual(summary["verdict"], "NEEDS WORK")


if __name__ == "__main__":
    unittest.main()
