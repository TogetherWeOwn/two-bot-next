"""Gate 5 outage measurement regressions: synthetic fixtures only, no staging."""

import io
import unittest

from gate5_outage import summarize


def run(text):
    return summarize(io.StringIO(text))


class Gate5OutageTests(unittest.TestCase):
    def test_clean_run_passes(self):
        summary = run(
            '{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}\n'
            '{"ts": "2026-10-01T00:00:02Z", "event": "readyz_ok"}\n'
            '{"ts": "2026-10-01T00:00:03Z", "event": "readyz_ok"}\n'
        )
        self.assertEqual(summary["outage_windows"], [])
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertIsNone(summary["max_outage_seconds"])
        self.assertEqual(summary["verdict"], "PASS")

    def test_mid_log_outage_with_recovery(self):
        summary = run(
            '{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}\n'
            '{"ts": "2026-10-01T00:00:10Z", "event": "readyz_fail", "status": 503}\n'
            '{"ts": "2026-10-01T00:00:20Z", "event": "readyz_fail", "status": 503}\n'
            '{"ts": "2026-10-01T00:00:55Z", "event": "readyz_ok"}\n'
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

    def test_missed_tick_opens_outage(self):
        summary = run(
            '{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}\n'
            '{"ts": "2026-10-01T00:00:02Z", "event": "tick_missed"}\n'
            '{"ts": "2026-10-01T00:00:12Z", "event": "readyz_ok"}\n'
        )
        self.assertEqual(len(summary["outage_windows"]), 1)
        window = summary["outage_windows"][0]
        self.assertEqual(window["outage_seconds"], 10.0)
        self.assertEqual(window["status"], "recovered")
        self.assertEqual(summary["verdict"], "PASS")

    def test_still_down_at_end_is_unknown_not_zero(self):
        summary = run(
            '{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}\n'
            '{"ts": "2026-10-01T00:00:10Z", "event": "readyz_fail", "status": 503}\n'
        )
        self.assertEqual(len(summary["outage_windows"]), 1)
        window = summary["outage_windows"][0]
        self.assertEqual(window["status"], "unknown")
        self.assertIsNone(window["end"])
        # Explicitly UNKNOWN: never scored as zero.
        self.assertIsNone(window["outage_seconds"])
        self.assertNotEqual(window["outage_seconds"], 0)
        self.assertIsNone(summary["max_outage_seconds"])
        self.assertEqual(summary["verdict"], "NOT VERIFIED")

    def test_over_budget_recovery_needs_work(self):
        summary = run(
            '{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}\n'
            '{"ts": "2026-10-01T00:00:05Z", "event": "readyz_fail", "status": 503}\n'
            '{"ts": "2026-10-01T00:01:35Z", "event": "readyz_ok"}\n'
        )
        window = summary["outage_windows"][0]
        self.assertEqual(window["outage_seconds"], 90.0)
        self.assertEqual(summary["max_outage_seconds"], 90.0)
        self.assertEqual(summary["verdict"], "NEEDS WORK")

    def test_unknown_events_are_preserved_not_dropped(self):
        summary = run(
            '{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}\n'
            '{"ts": "2026-10-01T00:00:02Z", "event": "frobnicate"}\n'
            "not json at all\n"
        )
        self.assertEqual(len(summary["unknown_intervals"]), 2)
        self.assertEqual(summary["verdict"], "NOT VERIFIED")

    def test_empty_log_is_not_verified(self):
        for text in ("", "\n\n"):
            with self.subTest(text=text):
                summary = run(text)
                self.assertEqual(summary["verdict"], "NOT VERIFIED")
                self.assertEqual(summary["unknown_intervals"][0]["reason"],
                                 "no readiness events in log")

    def test_silent_gap_of_budget_is_not_verified(self):
        summary = run(
            '{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}\n'
            '{"ts": "2026-10-01T00:01:01Z", "event": "readyz_ok"}\n'
        )
        self.assertEqual(len(summary["unknown_intervals"]), 1)
        self.assertEqual(summary["verdict"], "NOT VERIFIED")

    def test_gap_under_budget_passes(self):
        summary = run(
            '{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}\n'
            '{"ts": "2026-10-01T00:01:00Z", "event": "readyz_ok"}\n'
        )
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertEqual(summary["verdict"], "PASS")

    def test_out_of_order_line_never_scores_negative(self):
        summary = run(
            '{"ts": "2026-10-01T00:00:00Z", "event": "readyz_ok"}\n'
            '{"ts": "2026-10-01T00:00:10Z", "event": "readyz_fail", "status": 503}\n'
            '{"ts": "2026-10-01T00:00:05Z", "event": "readyz_ok"}\n'
            '{"ts": "2026-10-01T00:00:20Z", "event": "readyz_ok"}\n'
        )
        self.assertEqual([w["outage_seconds"] for w in summary["outage_windows"]], [10.0])
        self.assertEqual(len(summary["unknown_intervals"]), 1)
        self.assertEqual(summary["verdict"], "NOT VERIFIED")

    def test_log_starting_mid_outage_is_not_scored(self):
        summary = run(
            '{"ts": "2026-10-01T00:00:00Z", "event": "readyz_fail", "status": 503}\n'
            '{"ts": "2026-10-01T00:00:30Z", "event": "readyz_ok"}\n'
        )
        self.assertEqual(summary["outage_windows"], [])
        self.assertIsNone(summary["max_outage_seconds"])
        self.assertEqual(len(summary["unknown_intervals"]), 1)
        self.assertEqual(summary["verdict"], "NOT VERIFIED")

    def test_start_not_pinned_by_last_healthy_sample_is_not_verified(self):
        summary = run(
            '{"ts": "2026-10-01T00:00:00Z", "event": "readyz_ok"}\n'
            '{"ts": "2026-10-01T00:00:40Z", "event": "readyz_fail", "status": 503}\n'
            '{"ts": "2026-10-01T00:01:05Z", "event": "readyz_ok"}\n'
        )
        self.assertEqual(summary["outage_windows"], [])
        self.assertEqual(len(summary["unknown_intervals"]), 1)
        self.assertEqual(summary["verdict"], "NOT VERIFIED")


if __name__ == "__main__":
    unittest.main()
