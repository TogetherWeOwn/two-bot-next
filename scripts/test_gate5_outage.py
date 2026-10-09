"""Gate 5 outage measurement regressions: synthetic fixtures only, no staging."""

import contextlib
import io
import json
import os
import tempfile
import unittest
from datetime import datetime, timedelta, timezone

from gate5_outage import main, summarize

EPOCH = datetime(2026, 10, 1, tzinfo=timezone.utc)


def at(second):
    return EPOCH + timedelta(seconds=second)


def line(second, event, **extra):
    stamp = at(second).strftime("%Y-%m-%dT%H:%M:%SZ")
    return json.dumps({"ts": stamp, "event": event, **extra}) + "\n"


def ok(second):
    return line(second, "readyz_ok", status=200)


def fail(second):
    return line(second, "readyz_fail", status=503)


def run(*lines, expect=(0, 60)):
    return summarize(io.StringIO("".join(lines)), at(expect[0]), at(expect[1]))


class Gate5OutageTests(unittest.TestCase):
    def test_clean_run_passes(self):
        summary = run(ok(0), ok(5), ok(10), ok(15), expect=(0, 15))
        self.assertEqual(summary["outage_windows"], [])
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertIsNone(summary["max_outage_seconds"])
        self.assertEqual(summary["verdict"], "PASS")

    def test_empty_log_needs_work(self):
        self.assertEqual(run("")["verdict"], "NEEDS WORK")

    def test_blank_lines_only_need_work(self):
        self.assertEqual(run("\n\n")["verdict"], "NEEDS WORK")

    def test_silent_gap_between_healthy_readings_needs_work(self):
        summary = run(ok(0), ok(3600), expect=(0, 3600))
        self.assertEqual(summary["outage_windows"], [])
        self.assertEqual(len(summary["unknown_intervals"]), 1)
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_records_must_cover_the_declared_interval(self):
        summary = run(ok(0), ok(5), ok(10), expect=(0, 3600))
        self.assertEqual(len(summary["unknown_intervals"]), 1)
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_mid_log_outage_with_recovery(self):
        summary = run(
            ok(1), fail(10), fail(20), fail(30), fail(40), fail(50),
            ok(55), ok(60), ok(65), expect=(1, 65),
        )
        self.assertEqual(len(summary["outage_windows"]), 1)
        window = summary["outage_windows"][0]
        # The window spans the FIRST failure to the first confirmed recovery.
        self.assertEqual(window["start"], "2026-10-01T00:00:10+00:00")
        self.assertEqual(window["end"], "2026-10-01T00:00:55+00:00")
        self.assertEqual(window["outage_seconds"], 45.0)
        self.assertEqual(window["outage_seconds_max"], 54.0)
        self.assertEqual(window["status"], "recovered")
        self.assertEqual(summary["max_outage_seconds"], 54.0)
        self.assertEqual(summary["verdict"], "PASS")

    def test_missed_tick_opens_outage(self):
        summary = run(ok(1), line(2, "tick_missed"), ok(12), ok(13), ok(14), expect=(1, 14))
        self.assertEqual(len(summary["outage_windows"]), 1)
        window = summary["outage_windows"][0]
        self.assertEqual(window["outage_seconds"], 10.0)
        self.assertEqual(window["status"], "recovered")
        self.assertEqual(summary["verdict"], "PASS")

    def test_single_healthy_record_is_not_a_recovery(self):
        summary = run(
            ok(0), fail(5), ok(10),
            *(fail(second) for second in range(15, 60, 5)),
            ok(60), ok(65), ok(70), expect=(0, 70),
        )
        self.assertEqual(len(summary["outage_windows"]), 1)
        window = summary["outage_windows"][0]
        self.assertEqual(window["start"], "2026-10-01T00:00:05+00:00")
        self.assertEqual(window["outage_seconds"], 55.0)
        self.assertEqual(window["outage_seconds_max"], 60.0)
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_start_between_samples_is_bounded_by_last_healthy_record(self):
        summary = run(
            ok(0), *(fail(second) for second in range(5, 60, 5)),
            ok(64), ok(69), ok(74), expect=(0, 74),
        )
        window = summary["outage_windows"][0]
        self.assertEqual(window["outage_seconds"], 59.0)
        self.assertEqual(window["outage_seconds_max"], 64.0)
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_upper_bound_exactly_at_budget_needs_work(self):
        summary = run(
            ok(0), *(fail(second) for second in range(5, 60, 5)),
            ok(60), ok(65), ok(70), expect=(0, 70),
        )
        window = summary["outage_windows"][0]
        self.assertEqual(window["outage_seconds"], 55.0)
        self.assertEqual(window["outage_seconds_max"], 60.0)
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_over_budget_recovery_needs_work(self):
        summary = run(
            ok(0), *(fail(second) for second in range(5, 95, 5)),
            ok(95), ok(100), ok(105), expect=(0, 105),
        )
        window = summary["outage_windows"][0]
        self.assertEqual(window["outage_seconds"], 90.0)
        self.assertEqual(summary["max_outage_seconds"], 95.0)
        # The budget alone fails this log: no silent gaps, no unknowns.
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_two_confirmations_are_not_a_recovery(self):
        summary = run(ok(0), fail(5), ok(10), ok(15), expect=(0, 15))
        self.assertEqual(len(summary["outage_windows"]), 1)
        window = summary["outage_windows"][0]
        self.assertEqual(window["status"], "unknown")
        self.assertIsNone(window["end"])
        self.assertIsNone(summary["max_outage_seconds"])
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_still_down_at_end_is_unknown_not_zero(self):
        summary = run(ok(1), fail(10), expect=(1, 10))
        self.assertEqual(len(summary["outage_windows"]), 1)
        window = summary["outage_windows"][0]
        self.assertEqual(window["status"], "unknown")
        self.assertIsNone(window["end"])
        # Explicitly UNKNOWN: never scored as zero.
        self.assertIsNone(window["outage_seconds"])
        self.assertNotEqual(window["outage_seconds"], 0)
        self.assertIsNone(summary["max_outage_seconds"])
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_failure_before_first_healthy_reading_is_unknown_not_recovered(self):
        summary = run(fail(0), fail(5), ok(10), ok(15), ok(20), expect=(0, 20))
        self.assertEqual(len(summary["outage_windows"]), 1)
        window = summary["outage_windows"][0]
        self.assertEqual(window["status"], "unknown")
        self.assertIsNone(window["start"])
        self.assertIsNone(window["outage_seconds"])
        self.assertIsNone(summary["max_outage_seconds"])
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_silent_gap_inside_outage_needs_work(self):
        summary = run(ok(1), fail(10), fail(25), ok(30), ok(35), ok(40), expect=(1, 40))
        self.assertEqual(summary["outage_windows"][0]["outage_seconds"], 20.0)
        self.assertEqual(len(summary["unknown_intervals"]), 1)
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_out_of_order_record_needs_work_and_never_scores_negative(self):
        summary = run(ok(1), fail(10), ok(5), ok(15), ok(20), ok(25), expect=(1, 25))
        window = summary["outage_windows"][0]
        self.assertEqual(window["outage_seconds"], 5.0)
        self.assertTrue(any("out-of-order" in item["reason"]
                            for item in summary["unknown_intervals"]))
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_record_contradicting_its_status_is_unknown(self):
        summary = run(
            ok(0), *(fail(second) for second in range(5, 40, 5)),
            line(40, "readyz_ok", status=503),
            ok(45), ok(50), ok(55), expect=(0, 55),
        )
        self.assertTrue(any("contradicts" in item["reason"]
                            for item in summary["unknown_intervals"]))
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_timestamp_without_time_of_day_is_unknown(self):
        summary = run('{"ts": "2026-10-01", "event": "readyz_ok", "status": 200}\n')
        self.assertEqual(len(summary["unknown_intervals"]), 1)
        self.assertIsNone(summary["window"]["start"])
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_non_utf8_and_out_of_range_lines_are_unknown_not_crashes(self):
        raw = (ok(0).encode("utf-8") + b"\xff\n"
               + b'{"ts": "0001-01-01T00:00:00+01:00", "event": "readyz_ok"}\n'
               + ok(5).encode("utf-8"))
        summary = summarize(io.BytesIO(raw), at(0), at(5))
        self.assertEqual(len(summary["unknown_intervals"]), 2)
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_unknown_events_are_preserved_not_dropped(self):
        summary = run(ok(1), line(2, "frobnicate"), "not json at all\n", expect=(1, 2))
        self.assertEqual(len(summary["unknown_intervals"]), 2)
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_cli_exit_code_follows_verdict(self):
        interval = ["--expect-start", "2026-10-01T00:00:00Z",
                    "--expect-end", "2026-10-01T00:00:15Z"]
        with tempfile.TemporaryDirectory() as tmp:
            clean = os.path.join(tmp, "clean.jsonl")
            with open(clean, "w", encoding="utf-8") as handle:
                handle.write(ok(0) + ok(5) + ok(10) + ok(15))
            empty = os.path.join(tmp, "empty.jsonl")
            with open(empty, "w", encoding="utf-8"):
                pass
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(main([*interval, clean]), 0)
                self.assertEqual(main([*interval, empty]), 1)

    def test_cli_rejects_reversed_declared_interval(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "clean.jsonl")
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(ok(0) + ok(5))
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                main(["--expect-start", "2026-10-01T00:00:05Z",
                      "--expect-end", "2026-10-01T00:00:00Z", path])


if __name__ == "__main__":
    unittest.main()
