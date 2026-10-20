"""Protected acceptance tests for the offline soak-evidence field contract.

Acceptance-owned: preserve this file in the combined implementation change;
return suspected test defects to its author, rather than weakening assertions.
This file is intentionally RED until the implementation satisfies the contract.

Dispatch seq must be a genuine nonnegative integer (bool is not an integer for
this contract). A supplied resume seq has the same requirement; only omission
permits the existing sequence-less resume. Rejected records cannot count as
accepted events, change session/sequence state, or consume an outstanding outage.
Timestamp UTC normalization failures, including explicit UNKNOWN start/end, must
be bounded UNKNOWN evidence, not exceptions or rejected-value diagnostics.
Following valid records still apply. No sorting, threshold changes, live inputs,
network, database, or claim of live soak acceptance is involved here.
"""

import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from soak_evidence import summarize

ROOT = Path(__file__).resolve().parents[1]
SENTINEL = "rejected-field-sentinel"
BAD_SEQUENCES = (
    ("true", True),
    ("false", False),
    ("negative", -1),
    ("large-negative", -1000007),
    ("null", None),
    ("numeric-string", "42"),
    ("zero-string", "0"),
    ("text", SENTINEL),
    ("fraction", 1.5),
    ("integral-float", 2.0),
    ("array", [SENTINEL]),
    ("object", {"value": SENTINEL}),
)
BAD_TIMESTAMPS = (
    ("utc-underflow", "0001-01-01T00:00:00+01:00"),
    ("utc-overflow", "9999-12-31T23:59:59-01:00"),
    ("malformed", SENTINEL),
)


def event(kind, second, session="a", **fields):
    return {
        "ts": f"2026-09-20T00:{second // 60:02d}:{second % 60:02d}Z",
        "event": kind,
        "session": session,
        **fields,
    }


def encode(records):
    return "".join(json.dumps(record) + "\n" for record in records)


def outage_prefix():
    return [
        event("connect", 0),
        event("dispatch", 1, seq=10),
        event("disconnect", 2),
    ]


def run_cli(records):
    scratch = (os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
               or os.environ.get("PAPERCLIP_SCRATCH_DIR"))
    with tempfile.TemporaryDirectory(dir=scratch) as directory:
        path = Path(directory) / "evidence.jsonl"
        path.write_text(encode(records), encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(ROOT / "scripts/soak_evidence.py"), str(path)],
            capture_output=True, text=True, check=False, timeout=10,
        )


def malformed_cases():
    """One matrix drives both the real summarize boundary and CLI process."""
    for label, seq in BAD_SEQUENCES:
        yield f"dispatch-only/{label}", [
            event("connect", 0), event("dispatch", 1, seq=seq),
        ], {
            "connects": 1, "dispatches": 0, "resumes": 0,
            "missed_events": 0, "missed_sequence_windows": [],
            "redeploy_gaps_s": [], "unknowns": 1,
        }, "dispatch-only"
        # A rejected different-session record must not reset a's baseline.
        for session in ("a", "b"):
            gap = [{
                "session": "a", "expected_after": 10, "received": 12,
                "count": 1, "at": "2026-09-20T00:00:04+00:00",
            }]
            for kind in ("dispatch", "resume"):
                yield f"{kind}-state/{session}/{label}", [
                    event("connect", 0), event("dispatch", 1, seq=10),
                    event(kind, 3, session=session, seq=seq),
                    event("dispatch", 4, seq=12),
                    event("dispatch", 5, seq=13),
                ], {
                    "connects": 1, "dispatches": 3, "resumes": 0,
                    "missed_events": 1, "missed_sequence_windows": gap,
                    "redeploy_gaps_s": [], "unknowns": 1,
                }, f"{kind}-state"
            yield f"resume-recovery/{session}/{label}", outage_prefix() + [
                event("resume", 3, session=session, seq=seq),
                event("resume", 14, seq=10),
                event("dispatch", 15, seq=11),
            ], {
                "connects": 1, "dispatches": 2, "disconnects": 1,
                "resumes": 1, "missed_events": 0,
                "missed_sequence_windows": [], "redeploy_gaps_s": [12.0],
                "unknowns": 1,
            }, "resume-recovery"
        yield f"resume-still-open/{label}", outage_prefix() + [
            event("resume", 3, seq=seq),
        ], {
            "connects": 1, "dispatches": 1, "disconnects": 1,
            "resumes": 0, "missed_events": 0,
            "missed_sequence_windows": [], "redeploy_gaps_s": [],
            "unknowns": 2, "open_outage": True,
        }, "resume-still-open"

    for label, ts in BAD_TIMESTAMPS:
        # Bad event timestamps cannot begin an outage or apply an event.
        for kind in ("connect", "disconnect", "dispatch", "resume"):
            yield f"event-ts/{kind}/{label}", [
                event(kind, 0, session="b", seq=1000007, ts=ts),
                event("connect", 1), event("dispatch", 2, seq=0),
                event("dispatch", 3, seq=1),
            ], {
                "connects": 1, "dispatches": 2, "disconnects": 0,
                "resumes": 0, "missed_events": 0,
                "missed_sequence_windows": [], "redeploy_gaps_s": [],
                "unknowns": 1,
                "window": {"start": "2026-09-20T00:00:01+00:00",
                           "end": "2026-09-20T00:00:03+00:00"},
            }, "event-ts"
        bad_resume = event("resume", 3, session="b", seq=1000007, ts=ts)
        for field in ("ts", "start", "end"):
            if field == "ts":
                rejected = bad_resume
            else:
                rejected = event(
                    "unknown", 3, start="2026-09-20T00:00:02Z",
                    end="2026-09-20T00:00:03Z", reason=SENTINEL,
                )
                rejected[field] = ts
            yield f"timestamp-recovery/{field}/{label}", outage_prefix() + [
                rejected, event("resume", 14, seq=10),
                event("dispatch", 15, seq=11),
            ], {
                "connects": 1, "dispatches": 2, "disconnects": 1,
                "resumes": 1, "missed_events": 0,
                "missed_sequence_windows": [], "redeploy_gaps_s": [12.0],
                "unknowns": 1,
            }, f"timestamp-recovery-{field}"


class SoakEvidenceFieldAcceptance(unittest.TestCase):
    def assert_summary(self, summary, expected):
        for key, value in expected.items():
            if key not in ("unknowns", "open_outage"):
                self.assertEqual(summary[key], value, key)
        self.assertEqual(summary["verdict"], "NEEDS WORK")
        self.assertEqual(len(summary["unknown_intervals"]), expected["unknowns"])
        for interval in summary["unknown_intervals"]:
            self.assertIsInstance(interval["reason"], str)
            self.assertTrue(interval["reason"].strip())
            self.assertLessEqual(len(interval["reason"]), 256)
        if expected.get("open_outage"):
            self.assertTrue(any(
                interval["start"] == "2026-09-20T00:00:02+00:00"
                and interval["end"] is None
                and "disconnected" in interval["reason"]
                for interval in summary["unknown_intervals"]
            ), "the original outage must remain open")
        dumped = json.dumps(summary)
        self.assertNotIn(SENTINEL, dumped)
        self.assertNotIn("0001-01-01T00:00:00+01:00", dumped)
        self.assertNotIn("9999-12-31T23:59:59-01:00", dumped)
        self.assertNotIn("Traceback", dumped)

    def summarize_without_exception(self, records):
        try:
            return summarize(io.StringIO(encode(records)))
        except Exception as error:
            self.fail(f"malformed evidence escaped as {type(error).__name__}")

    def test_summarize_rejects_bundled_malformed_fields_before_state_changes(self):
        reasons = {}
        for name, records, expected, reason_group in malformed_cases():
            with self.subTest(case=name):
                summary = self.summarize_without_exception(records)
                self.assert_summary(summary, expected)
                reason = summary["unknown_intervals"][0]["reason"]
                self.assertEqual(reason, reasons.setdefault(reason_group, reason),
                                 "diagnostics must be fixed, not value-dependent")

    def test_real_cli_rejects_same_matrix_without_traceback_or_rejected_values(self):
        reasons = {}
        for name, records, expected, reason_group in malformed_cases():
            with self.subTest(case=name):
                result = run_cli(records)
                output = result.stdout + result.stderr
                self.assertEqual(result.returncode, 1)
                self.assertNotIn("Traceback", output)
                self.assertNotIn(SENTINEL, output)
                self.assertNotIn("0001-01-01T00:00:00+01:00", output)
                self.assertNotIn("9999-12-31T23:59:59-01:00", output)
                summary = json.loads(result.stdout)
                self.assert_summary(summary, expected)
                reason = summary["unknown_intervals"][0]["reason"]
                self.assertEqual(reason, reasons.setdefault(reason_group, reason))


class SoakEvidenceValidControls(unittest.TestCase):
    def test_zero_positive_duplicates_equal_times_and_new_session_stay_valid(self):
        records = [
            event("connect", 0), event("dispatch", 0, seq=0),
            event("dispatch", 0, seq=0), event("dispatch", 1, seq=1),
            event("dispatch", 2, seq=2), event("disconnect", 3),
            event("connect", 4, session="b"),
            event("dispatch", 4, session="b", seq=0),
            event("dispatch", 5, session="b", seq=1),
        ]
        summary = summarize(io.StringIO(encode(records)))
        self.assertEqual(summary["connects"], 2)
        self.assertEqual(summary["disconnects"], 1)
        self.assertEqual(summary["dispatches"], 6)
        self.assertEqual(summary["resumes"], 0)
        self.assertEqual(summary["missed_events"], 0)
        self.assertEqual(summary["missed_sequence_windows"], [])
        self.assertEqual(summary["redeploy_gaps_s"], [1.0])
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertEqual(summary["verdict"], "PASS")
        result = run_cli(records)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(json.loads(result.stdout), summary)
        self.assertEqual(result.stderr, "")

    def test_omitted_resume_seq_preserves_the_previous_sequence_baseline(self):
        records = outage_prefix() + [
            event("resume", 4), event("dispatch", 5, seq=12),
        ]
        summary = summarize(io.StringIO(encode(records)))
        self.assertEqual(summary["resumes"], 1)
        self.assertEqual(summary["dispatches"], 2)
        self.assertEqual(summary["redeploy_gaps_s"], [2.0])
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertEqual(summary["missed_events"], 1)
        self.assertEqual(summary["missed_sequence_windows"], [{
            "session": "a", "expected_after": 10, "received": 12,
            "count": 1, "at": "2026-09-20T00:00:05+00:00",
        }])
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_optional_resume_and_valid_supplied_zero_or_positive_sequences(self):
        for fields, next_seq in (({}, 11), ({"seq": 0}, 1), ({"seq": 10}, 11)):
            with self.subTest(fields=fields):
                records = outage_prefix() + [
                    event("resume", 4, **fields),
                    event("dispatch", 5, seq=next_seq),
                ]
                summary = summarize(io.StringIO(encode(records)))
                self.assertEqual(summary["resumes"], 1)
                self.assertEqual(summary["dispatches"], 2)
                self.assertEqual(summary["redeploy_gaps_s"], [2.0])
                self.assertEqual(summary["missed_events"], 0)
                self.assertEqual(summary["unknown_intervals"], [])
                self.assertEqual(summary["verdict"], "PASS")
                result = run_cli(records)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(json.loads(result.stdout), summary)

    def test_representable_timezone_normalizations_remain_valid(self):
        for ts, utc in (
            ("0001-01-01T01:00:00+01:00", "0001-01-01T00:00:00+00:00"),
            ("9999-12-31T22:59:59-01:00", "9999-12-31T23:59:59+00:00"),
            ("2026-09-20T01:00:00+01:00", "2026-09-20T00:00:00+00:00"),
        ):
            with self.subTest(ts=ts):
                records = [event("connect", 0, ts=ts),
                           event("dispatch", 0, seq=0, ts=ts)]
                summary = summarize(io.StringIO(encode(records)))
                self.assertEqual(summary["window"], {"start": utc, "end": utc})
                self.assertEqual(summary["dispatches"], 1)
                self.assertEqual(summary["missed_events"], 0)
                self.assertEqual(summary["unknown_intervals"], [])
                self.assertEqual(summary["verdict"], "PASS")
                result = run_cli(records)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(json.loads(result.stdout), summary)

    def test_valid_explicit_unknown_interval_is_retained_without_claiming_pass(self):
        records = [
            event("connect", 0), event("dispatch", 1, seq=0),
            event("unknown", 3, start="2026-09-20T01:00:01+01:00",
                  end="2026-09-20T01:00:02+01:00", reason="fixture gap"),
            event("dispatch", 4, seq=1),
        ]
        summary = summarize(io.StringIO(encode(records)))
        self.assertEqual(summary["dispatches"], 2)
        self.assertEqual(summary["missed_events"], 0)
        self.assertEqual(summary["unknown_intervals"], [{
            "start": "2026-09-20T00:00:01+00:00",
            "end": "2026-09-20T00:00:02+00:00", "reason": "fixture gap",
        }])
        self.assertEqual(summary["verdict"], "NEEDS WORK")
        result = run_cli(records)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(json.loads(result.stdout), summary)

    def test_outage_budget_is_not_relaxed(self):
        for seconds, verdict in ((59, "PASS"), (61, "NEEDS WORK")):
            with self.subTest(seconds=seconds):
                records = outage_prefix() + [
                    event("resume", 2 + seconds, seq=10),
                    event("dispatch", 3 + seconds, seq=11),
                ]
                summary = summarize(io.StringIO(encode(records)))
                self.assertEqual(summary["redeploy_gap_budget_s"], 60)
                self.assertEqual(summary["redeploy_gaps_s"], [float(seconds)])
                self.assertEqual(summary["resumes"], 1)
                self.assertEqual(summary["unknown_intervals"], [])
                self.assertEqual(summary["verdict"], verdict)

    def test_empty_and_connect_only_inputs_still_refuse_pass(self):
        for records in ([], [event("connect", 0)]):
            with self.subTest(records=records):
                summary = summarize(io.StringIO(encode(records)))
                self.assertEqual(summary["dispatches"], 0)
                self.assertEqual(summary["missed_events"], 0)
                self.assertEqual(summary["verdict"], "NEEDS WORK")
                result = run_cli(records)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(json.loads(result.stdout), summary)
                self.assertNotIn("Traceback", result.stderr)


if __name__ == "__main__":
    unittest.main()
