import copy
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from compare_pipeline_bench import compare


class ComparisonTests(unittest.TestCase):
    def setUp(self):
        self.baseline = {
            "schema_version": 1,
            "workload": {"members": 107, "channels": 10, "duration_secs": 30},
            "metrics": {
                "peak_rss_mib": 40,
                "handler_latency_us": {"p50": 100, "p99": 1000},
                "db_round_trips_per_event": 2,
            },
        }
        self.actual = copy.deepcopy(self.baseline)

    def test_identical_and_tolerance_boundary_pass(self):
        self.assertEqual(compare(self.baseline, self.actual), [])
        self.actual["metrics"]["peak_rss_mib"] = 50
        self.assertEqual(compare(self.baseline, self.actual), [])

    def test_each_metric_regression_fails(self):
        for key in ("peak_rss_mib", "db_round_trips_per_event"):
            with self.subTest(key=key):
                actual = copy.deepcopy(self.baseline)
                actual["metrics"][key] *= 1.26
                self.assertTrue(compare(self.baseline, actual))
        for key in ("p50", "p99"):
            with self.subTest(key=key):
                actual = copy.deepcopy(self.baseline)
                actual["metrics"]["handler_latency_us"][key] *= 1.26
                self.assertTrue(compare(self.baseline, actual))

    def test_zero_query_baseline_is_strict(self):
        self.baseline["metrics"]["db_round_trips_per_event"] = 0
        self.actual["metrics"]["db_round_trips_per_event"] = 0.01
        self.assertTrue(compare(self.baseline, self.actual))

    def test_absolute_lite_budget_even_with_high_baseline(self):
        self.baseline["metrics"]["peak_rss_mib"] = 190
        self.actual["metrics"]["peak_rss_mib"] = 200
        self.assertTrue(compare(self.baseline, self.actual))

    def test_unlike_workload_is_invalid(self):
        self.actual["workload"]["members"] = 1000
        with self.assertRaises(ValueError):
            compare(self.baseline, self.actual)

    def test_invalid_metric_is_not_green(self):
        for value in (float("nan"), float("inf"), -1, True, "40", None):
            with self.subTest(value=value):
                self.actual["metrics"]["peak_rss_mib"] = value
                with self.assertRaises(ValueError):
                    compare(self.baseline, self.actual)

    def test_missing_measurement_is_invalid(self):
        del self.actual["metrics"]["db_round_trips_per_event"]
        with self.assertRaises(KeyError):
            compare(self.baseline, self.actual)

    def test_invalid_limits(self):
        for value in (-1, float("nan"), float("inf")):
            with self.assertRaises(ValueError):
                compare(self.baseline, self.actual, tolerance=value)
        for value in (0, 257, float("nan")):
            with self.assertRaises(ValueError):
                compare(self.baseline, self.actual, max_rss_mib=value)

    def test_cli_exit_codes(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("PAPERCLIP_SCRATCH_DIR")
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            baseline = Path(directory) / "baseline.json"
            actual = Path(directory) / "actual.json"
            baseline.write_text(json.dumps(self.baseline))
            command = [sys.executable, str(Path(__file__).with_name("compare_pipeline_bench.py")), str(actual), "--baseline", str(baseline)]
            actual.write_text(json.dumps(self.actual))
            result = subprocess.run(command, capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.actual["metrics"]["peak_rss_mib"] = 60
            actual.write_text(json.dumps(self.actual))
            result = subprocess.run(command, capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 1, result.stderr)
            actual.write_text("not JSON")
            result = subprocess.run(command, capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 2, result.stderr)
            actual.unlink()
            result = subprocess.run(command, capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 2, result.stderr)

    def test_percentile_order_is_validated(self):
        self.actual["metrics"]["handler_latency_us"]["p99"] = 1
        with self.assertRaises(ValueError):
            compare(self.baseline, self.actual)


if __name__ == "__main__":
    unittest.main()
