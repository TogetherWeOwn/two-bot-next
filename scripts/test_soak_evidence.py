"""Soak-evidence parser regressions: committed fixtures only, no staging."""

import contextlib
import io
import json
import tempfile
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from soak_evidence import main, summarize

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "scripts/fixtures/soak_evidence_sample.jsonl"
EXPECTED = ROOT / "scripts/fixtures/soak_evidence_sample.summary.json"
UNRECOGNISED_EVENT_FIELDS = (
    {"event": "frobnicate"},
    {},
    {"event": None},
    {"event": ""},
    {"event": True},
    {"event": 7},
    {"event": []},
    {"event": {}},
)


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

    def test_invalid_sequence_does_not_advance_timestamp_state(self):
        for event, seq in (("dispatch", True), ("resume", "bad")):
            with self.subTest(event=event):
                records = [
                    {"ts": "2026-09-20T00:00:00Z", "event": "connect", "session": "a"},
                    {"ts": "2026-09-20T00:00:01Z", "event": "dispatch", "session": "a", "seq": 0},
                    {"ts": "2026-09-20T00:00:02Z", "event": "disconnect"},
                    {"ts": "2026-09-20T00:00:10Z", "event": event, "session": "a", "seq": seq},
                    {"ts": "2026-09-20T00:00:04Z", "event": "resume", "session": "a"},
                    {"ts": "2026-09-20T00:00:05Z", "event": "dispatch", "session": "a", "seq": 1},
                ]
                summary = run("".join(json.dumps(record) + "\n" for record in records))
                self.assertEqual(summary["dispatches"], 2)
                self.assertEqual(summary["resumes"], 1)
                self.assertEqual(summary["redeploy_gaps_s"], [2.0])
                self.assertEqual(summary["missed_events"], 0)
                self.assertEqual(summary["window"], {
                    "start": "2026-09-20T00:00:00+00:00",
                    "end": "2026-09-20T00:00:05+00:00",
                })
                self.assertEqual(len(summary["unknown_intervals"]), 1)
                self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_invalid_unknown_endpoint_does_not_advance_timestamp_state(self):
        for field in ("start", "end"):
            for value in ("bad", "0001-01-01T00:00:00+01:00",
                          "9999-12-31T23:59:59-01:00"):
                with self.subTest(field=field, value=value):
                    records = [
                        {"ts": "2026-09-20T00:00:00Z", "event": "connect", "session": "a"},
                        {"ts": "2026-09-20T00:00:01Z", "event": "dispatch", "session": "a", "seq": 0},
                        {"ts": "2026-09-20T00:00:02Z", "event": "disconnect"},
                        {"ts": "2026-09-20T00:00:20Z", "event": "unknown", field: value},
                        {"ts": "2026-09-20T00:00:14Z", "event": "resume", "session": "a"},
                        {"ts": "2026-09-20T00:00:15Z", "event": "dispatch", "session": "a", "seq": 1},
                    ]
                    summary = run("".join(json.dumps(record) + "\n" for record in records))
                    self.assertEqual(summary["resumes"], 1)
                    self.assertEqual(summary["dispatches"], 2)
                    self.assertEqual(summary["redeploy_gaps_s"], [12.0])
                    self.assertEqual(summary["window"]["end"], "2026-09-20T00:00:15+00:00")
                    self.assertEqual(summary["unknown_intervals"], [{
                        "start": "2026-09-20T00:00:20+00:00",
                        "end": "2026-09-20T00:00:20+00:00",
                        "reason": "unknown interval with bad start/end",
                    }])
                    self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_unrecognised_event_does_not_advance_timestamp_state(self):
        for fields in UNRECOGNISED_EVENT_FIELDS:
            records = [
                {"ts": "2026-09-20T00:00:00Z", "event": "connect", "session": "a"},
                {"ts": "2026-09-20T00:00:01Z", "event": "dispatch", "session": "a", "seq": 0},
                {"ts": "2026-09-20T00:00:02Z", "event": "disconnect"},
                {"ts": "2026-09-20T00:00:20Z", **fields},
                {"ts": "2026-09-20T00:00:14Z", "event": "resume", "session": "a"},
                {"ts": "2026-09-20T00:00:15Z", "event": "dispatch", "session": "a", "seq": 1},
            ]
            text = "".join(json.dumps(record) + "\n" for record in records)
            for boundary in ("summarize", "cli"):
                with self.subTest(fields=fields, boundary=boundary):
                    if boundary == "summarize":
                        summary = run(text)
                    else:
                        with tempfile.TemporaryDirectory() as directory:
                            path = Path(directory) / "unrecognised.jsonl"
                            path.write_text(text, encoding="utf-8")
                            result = subprocess.run(
                                [sys.executable, str(ROOT / "scripts/soak_evidence.py"), str(path)],
                                capture_output=True, text=True, check=False, timeout=10,
                            )
                        self.assertEqual(result.returncode, 1)
                        self.assertEqual(result.stderr, "")
                        summary = json.loads(result.stdout)
                    self.assertEqual(summary["connects"], 1)
                    self.assertEqual(summary["disconnects"], 1)
                    self.assertEqual(summary["resumes"], 1)
                    self.assertEqual(summary["dispatches"], 2)
                    self.assertEqual(summary["redeploy_gaps_s"], [12.0])
                    self.assertEqual(summary["max_redeploy_gap_s"], 12.0)
                    self.assertEqual(summary["missed_events"], 0)
                    self.assertEqual(summary["missed_sequence_windows"], [])
                    self.assertEqual(summary["window"], {
                        "start": "2026-09-20T00:00:00+00:00",
                        "end": "2026-09-20T00:00:15+00:00",
                    })
                    self.assertEqual(summary["unknown_intervals"], [{
                        "start": "2026-09-20T00:00:20+00:00",
                        "end": "2026-09-20T00:00:20+00:00",
                        "reason": f"unrecognised event {fields.get('event')!r}",
                    }])
                    self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_unrecognised_events_do_not_define_evidence_window(self):
        good = {"ts": "2026-09-20T00:00:01Z", "event": "dispatch", "session": "a", "seq": 0}
        for fields in UNRECOGNISED_EVENT_FIELDS:
            bad = {"ts": "2026-09-20T00:00:20Z", **fields}
            for records in ([bad], [bad, good], [good, bad]):
                with self.subTest(fields=fields, records=records):
                    summary = run("".join(json.dumps(record) + "\n" for record in records))
                    accepted_ts = "2026-09-20T00:00:01+00:00" if len(records) > 1 else None
                    self.assertEqual(summary["window"], {
                        "start": accepted_ts, "end": accepted_ts,
                    })
                    self.assertEqual(summary["dispatches"], int(len(records) > 1))
                    self.assertEqual(len(summary["unknown_intervals"]), 1)
                    self.assertEqual(summary["verdict"], "NEEDS WORK")

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

    def test_empty_input_refuses_pass_with_honest_nulls(self):
        summary = run("")
        self.assertIsNone(summary["window"]["start"])
        self.assertIsNone(summary["window"]["end"])
        self.assertEqual(summary["missed_events"], 0)
        self.assertEqual(summary["missed_sequence_windows"], [])
        self.assertEqual(summary["connects"], 0)
        self.assertEqual(summary["dispatches"], 0)
        self.assertNotEqual(summary["verdict"], "PASS")
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_blank_only_input_refuses_pass_with_honest_nulls(self):
        summary = run("   \n\n  \n\t\n")
        self.assertIsNone(summary["window"]["start"])
        self.assertIsNone(summary["window"]["end"])
        self.assertEqual(summary["missed_events"], 0)
        self.assertEqual(summary["connects"], 0)
        self.assertEqual(summary["dispatches"], 0)
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_connect_only_input_refuses_pass_without_dispatches(self):
        summary = run(
            '{"ts": "2026-09-20T00:00:01Z", "event": "connect",'
            ' "session": "a"}\n'
        )
        self.assertEqual(summary["connects"], 1)
        self.assertEqual(summary["dispatches"], 0)
        self.assertEqual(summary["missed_events"], 0)
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_connect_plus_resume_without_dispatches_refuses_pass(self):
        summary = run(
            '{"ts": "2026-09-20T00:00:01Z", "event": "connect",'
            ' "session": "a"}\n'
            '{"ts": "2026-09-20T04:00:01Z", "event": "resume",'
            ' "session": "a", "seq": 7}\n'
        )
        self.assertEqual(summary["connects"], 1)
        self.assertEqual(summary["resumes"], 1)
        self.assertEqual(summary["dispatches"], 0)
        self.assertEqual(summary["missed_events"], 0)
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_cli_exit_code_empty_fails_clean_passes(self):
        with tempfile.NamedTemporaryFile(
                mode="w", suffix=".jsonl", delete=True,
                encoding="utf-8") as empty:
            empty.write("")
            empty.flush()
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(main([empty.name]), 1)
        with tempfile.NamedTemporaryFile(
                mode="w", suffix=".jsonl", delete=True,
                encoding="utf-8") as blank:
            blank.write("   \n\n")
            blank.flush()
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(main([blank.name]), 1)
        with tempfile.NamedTemporaryFile(
                mode="w", suffix=".jsonl", delete=True,
                encoding="utf-8") as clean:
            clean.write(
                '{"ts": "2026-09-20T00:00:01Z", "event": "connect",'
                ' "session": "a"}\n'
                '{"ts": "2026-09-20T00:00:02Z", "event": "dispatch",'
                ' "session": "a", "seq": 1}\n'
            )
            clean.flush()
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(main([clean.name]), 0)

    def test_decoder_recursion_overflow_becomes_unknown_then_continues(self):
        # Deep enough to overflow the C JSON decoder on older CPython. The
        # lexical pre-scan refuses it before json.loads on every version;
        # this pins that the run marks it UNKNOWN and continues, not aborts.
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

    def test_timestamp_regression_cannot_close_outage(self):
        summary = run(
            '{"ts": "2026-09-20T00:00:00Z", "event": "dispatch", "session": "a", "seq": 1}\n'
            '{"ts": "2026-09-20T00:00:01Z", "event": "disconnect", "session": "a"}\n'
            '{"ts": "2026-09-20T00:00:00Z", "event": "resume", "session": "a", "seq": 50}\n'
            '{"ts": "2026-09-20T00:00:03Z", "event": "resume", "session": "a", "seq": 2}\n'
            '{"ts": "2026-09-20T00:00:04Z", "event": "dispatch", "session": "a", "seq": 4}\n'
        )
        self.assertEqual(summary["redeploy_gaps_s"], [2.0])
        self.assertTrue(all(gap >= 0 for gap in summary["redeploy_gaps_s"]))
        self.assertEqual(summary["resumes"], 1)
        self.assertEqual(summary["missed_sequence_windows"][0]["count"], 1)
        self.assertEqual(summary["window"], {
            "start": "2026-09-20T00:00:00+00:00",
            "end": "2026-09-20T00:00:04+00:00",
        })
        self.assertEqual(len(summary["unknown_intervals"]), 1)
        self.assertEqual(summary["unknown_intervals"][0]["reason"],
                         "timestamp regression on line 3")
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_timestamp_regression_does_not_advance_sequence(self):
        summary = run(
            '{"ts": "2026-09-20T00:00:01Z", "event": "connect", "session": "a"}\n'
            '{"ts": "2026-09-20T00:00:02Z", "event": "dispatch", "session": "a", "seq": 1}\n'
            '{"ts": "2026-09-20T00:00:01Z", "event": "dispatch", "session": "a", "seq": 50}\n'
            '{"ts": "2026-09-20T00:00:03Z", "event": "dispatch", "session": "a", "seq": 4}\n'
        )
        self.assertEqual(summary["dispatches"], 2)
        self.assertEqual(summary["missed_sequence_windows"], [{
            "session": "a",
            "expected_after": 1,
            "received": 4,
            "count": 2,
            "at": "2026-09-20T00:00:03+00:00",
        }])
        self.assertEqual(summary["unknown_intervals"][0]["reason"],
                         "timestamp regression on line 3")

    def test_timestamp_regression_does_not_reset_session_sequence(self):
        summary = run(
            '{"ts": "2026-09-20T00:00:01Z", "event": "dispatch", "session": "a", "seq": 1}\n'
            '{"ts": "2026-09-20T00:00:03Z", "event": "dispatch", "session": "a", "seq": 2}\n'
            '{"ts": "2026-09-20T00:00:02Z", "event": "connect", "session": "a"}\n'
            '{"ts": "2026-09-20T00:00:04Z", "event": "dispatch", "session": "a", "seq": 4}\n'
        )
        self.assertEqual(summary["connects"], 0)
        self.assertEqual(summary["missed_sequence_windows"][0]["count"], 1)
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_equal_timestamps_and_ordered_duplicate_dispatches_are_accepted(self):
        summary = run(
            '{"ts": "2026-09-20T00:00:01Z", "event": "connect", "session": "a"}\n'
            '{"ts": "2026-09-20T00:00:01Z", "event": "dispatch", "session": "a", "seq": 1}\n'
            '{"ts": "2026-09-20T00:00:01Z", "event": "dispatch", "session": "a", "seq": 1}\n'
            '{"ts": "2026-09-20T00:00:02Z", "event": "dispatch", "session": "a", "seq": 2}\n'
        )
        self.assertEqual(summary["dispatches"], 3)
        self.assertEqual(summary["missed_events"], 0)
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertEqual(summary["verdict"], "PASS")

    def test_regressed_unknown_record_is_not_applied_and_window_ignores_it(self):
        summary = run(
            '{"ts": "2026-09-20T00:00:02Z", "event": "connect", "session": "a"}\n'
            '{"ts": "2026-09-20T00:00:01Z", "event": "unknown", '
            '"start": "2026-09-20T00:00:00Z", '
            '"end": "2026-09-20T00:00:01Z", "reason": "ignored regression"}\n'
            '{"ts": "2026-09-20T00:00:03Z", "event": "dispatch", "session": "a", "seq": 1}\n'
        )
        self.assertEqual(summary["window"], {
            "start": "2026-09-20T00:00:02+00:00",
            "end": "2026-09-20T00:00:03+00:00",
        })
        self.assertEqual(summary["connects"], 1)
        self.assertEqual(summary["dispatches"], 1)
        self.assertEqual(summary["unknown_intervals"], [{
            "start": "2026-09-20T00:00:01+00:00",
            "end": "2026-09-20T00:00:01+00:00",
            "reason": "timestamp regression on line 2",
        }])

    def test_cli_timestamp_regression_exits_nonzero_without_raw_record(self):
        raw_record = (
            '{"ts": "2026-09-20T00:00:00Z", "event": "resume", '
            '"session": "private-session-value", "seq": 999}\n'
        )
        text = (
            '{"ts": "2026-09-20T00:00:01Z", "event": "disconnect", "session": "a"}\n'
            + raw_record
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "regression.jsonl"
            path.write_text(text, encoding="utf-8")
            result = subprocess.run(
                [sys.executable, str(ROOT / "scripts/soak_evidence.py"), str(path)],
                capture_output=True, text=True, check=False,
            )
        self.assertNotEqual(result.returncode, 0)
        summary = json.loads(result.stdout)
        self.assertEqual(summary["verdict"], "NEEDS WORK")
        self.assertNotIn("private-session-value", result.stdout + result.stderr)
        self.assertNotIn("999", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
