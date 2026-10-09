"""Gate 5 outage measurement regressions: synthetic fixtures only, no staging."""

import io
import json
import unittest
from datetime import datetime, timedelta, timezone

from gate5_outage import summarize

EPOCH = datetime(2026, 10, 1, tzinfo=timezone.utc)


def line(second, event, **extra):
    stamp = (EPOCH + timedelta(seconds=second)).strftime("%Y-%m-%dT%H:%M:%SZ")
    return json.dumps({"ts": stamp, "event": event, **extra}) + "\n"


def run(*lines):
    return summarize(io.StringIO("".join(lines)))


class Gate5OutageTests(unittest.TestCase):
    def test_clean_run_passes(self):
        summary = run(line(1, "readyz_ok"), line(2, "readyz_ok"), line(3, "readyz_ok"))
        self.assertEqual(summary["outage_windows"], [])
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertIsNone(summary["max_outage_seconds"])
        self.assertEqual(summary["verdict"], "PASS")

    def test_empty_log_needs_work(self):
        self.assertEqual(run("")["verdict"], "NEEDS WORK")

    def test_blank_lines_only_need_work(self):
        self.assertEqual(run("\n\n")["verdict"], "NEEDS WORK")

    def test_silent_gap_between_healthy_readings_needs_work(self):
        summary = run(line(0, "readyz_ok"), line(3600, "readyz_ok"))
        self.assertEqual(summary["outage_windows"], [])
        self.assertEqual(len(summary["unknown_intervals"]), 1)
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_mid_log_outage_with_recovery(self):
        summary = run(
            line(1, "readyz_ok"),
            line(10, "readyz_fail", status=503),
            line(20, "readyz_fail", status=503),
            line(30, "readyz_fail", status=503),
            line(40, "readyz_fail", status=503),
            line(50, "readyz_fail", status=503),
            line(55, "readyz_ok"),
        )
        self.assertEqual(len(summary["outage_windows"]), 1)
        window = summary["outage_windows"][0]
        # The window spans the FIRST failure to the first recovery.
        self.assertEqual(window["start"], "2026-10-01T00:00:10+00:00")
        self.assertEqual(window["end"], "2026-10-01T00:00:55+00:00")
        self.assertEqual(window["outage_seconds"], 45.0)
        self.assertEqual(window["status"], "recovered")
        self.assertEqual(summary["max_outage_seconds"], 45.0)
        self.assertEqual(summary["verdict"], "PASS")

    def test_silent_gap_inside_outage_needs_work(self):
        summary = run(
            line(1, "readyz_ok"),
            line(10, "readyz_fail", status=503),
            line(25, "readyz_fail", status=503),
            line(30, "readyz_ok"),
        )
        self.assertEqual(summary["outage_windows"][0]["outage_seconds"], 20.0)
        self.assertEqual(len(summary["unknown_intervals"]), 1)
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_out_of_order_record_needs_work_and_never_scores_negative(self):
        summary = run(
            line(1, "readyz_ok"),
            line(10, "readyz_fail", status=503),
            line(5, "readyz_ok"),
            line(15, "readyz_ok"),
        )
        self.assertEqual(summary["max_outage_seconds"], 5.0)
        self.assertTrue(any("out-of-order" in item["reason"]
                            for item in summary["unknown_intervals"]))
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_missed_tick_opens_outage(self):
        summary = run(
            line(1, "readyz_ok"),
            line(2, "tick_missed"),
            line(12, "readyz_ok"),
        )
        self.assertEqual(len(summary["outage_windows"]), 1)
        window = summary["outage_windows"][0]
        self.assertEqual(window["outage_seconds"], 10.0)
        self.assertEqual(window["status"], "recovered")
        self.assertEqual(summary["verdict"], "PASS")

    def test_failure_before_first_healthy_reading_is_unknown_not_recovered(self):
        summary = run(
            line(0, "readyz_fail", status=503),
            line(5, "readyz_fail", status=503),
            line(10, "readyz_ok"),
        )
        self.assertEqual(len(summary["outage_windows"]), 1)
        window = summary["outage_windows"][0]
        self.assertEqual(window["status"], "unknown")
        self.assertIsNone(window["start"])
        self.assertIsNone(window["outage_seconds"])
        self.assertIsNone(summary["max_outage_seconds"])
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_still_down_at_end_is_unknown_not_zero(self):
        summary = run(line(1, "readyz_ok"), line(10, "readyz_fail", status=503))
        self.assertEqual(len(summary["outage_windows"]), 1)
        window = summary["outage_windows"][0]
        self.assertEqual(window["status"], "unknown")
        self.assertIsNone(window["end"])
        # Explicitly UNKNOWN: never scored as zero.
        self.assertIsNone(window["outage_seconds"])
        self.assertNotEqual(window["outage_seconds"], 0)
        self.assertIsNone(summary["max_outage_seconds"])
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_over_budget_recovery_needs_work(self):
        summary = run(
            line(0, "readyz_ok"),
            *(line(s, "readyz_fail", status=503) for s in range(5, 95, 5)),
            line(95, "readyz_ok"),
        )
        window = summary["outage_windows"][0]
        self.assertEqual(window["outage_seconds"], 90.0)
        self.assertEqual(summary["max_outage_seconds"], 90.0)
        # The budget alone fails this log: no silent gaps, no unknowns.
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_unknown_events_are_preserved_not_dropped(self):
        summary = run(
            line(1, "readyz_ok"),
            line(2, "frobnicate"),
            "not json at all\n",
        )
        self.assertEqual(len(summary["unknown_intervals"]), 2)
        self.assertEqual(summary["verdict"], "NEEDS WORK")


if __name__ == "__main__":
    unittest.main()
