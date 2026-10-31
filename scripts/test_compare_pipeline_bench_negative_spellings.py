"""Pin rare negative float spellings through the bench argparse bound checks.

``BenchParser`` extends argparse's negative-number match so values starting
with "-" reach their type check instead of dying as "expected one argument".
These spellings are all valid ``float()`` input the stdlib match misses:

* ``-1.`` (trailing dot) and ``-.5e3`` (leading-dot scientific) refuse with
  the fixed bound vocabulary before any file read;
* underscore spellings ``-1_0``, ``-1_000.5`` and ``-1e1_0`` (all valid
  ``float()`` input) refuse with the fixed bound vocabulary before any
  file read;
* ``-0.`` and ``-0_0`` (negative zero, which ``compare()`` accepts) pass
  argparse and proceed to the file reads;
* strings that are not numbers (``-x``) still parse as flags, never as values.

Offline only: reports are temp files; no network, no credentials. Routed to
CI by pipeline-benchmark.yml's ``test*pipeline_bench*.py`` wildcard, like
the existing comparator suite.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_compare_pipeline_bench_negative_spellings.py' -v
"""

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import compare_pipeline_bench as bench


TOLERANCE_MESSAGE = "tolerance must be finite and nonnegative"
RSS_MESSAGE = "RSS limit must be positive and at most the 256 MiB lite ceiling"


def valid_report():
    return {
        "schema_version": 1,
        "workload": {"members": 107, "channels": 10, "duration_secs": 30},
        "metrics": {
            "peak_rss_mib": 40,
            "handler_latency_us": {"p50": 100, "p99": 1000},
            "db_round_trips_per_event": 2,
        },
    }


def scratch_dir():
    return (os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
            or os.environ.get("PAPERCLIP_SCRATCH_DIR")
            or os.environ.get("RUNNER_TEMP"))


class NegativeSpellingTests(unittest.TestCase):
    def run_main(self, argv):
        out, err = io.StringIO(), io.StringIO()
        reads = []
        real_open = Path.open

        def counting_open(self, *args, **kwargs):
            reads.append(str(self))
            return real_open(self, *args, **kwargs)

        with patch.object(sys, "argv", argv), \
                patch.object(Path, "open", counting_open), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = bench.main()
            except SystemExit as exit:
                code = exit.code
        return code, out.getvalue(), err.getvalue(), reads

    def write_reports(self, directory):
        baseline_path = Path(directory) / "baseline.json"
        actual_path = Path(directory) / "actual.json"
        baseline_path.write_text(json.dumps(valid_report()))
        actual_path.write_text(json.dumps(valid_report()))
        return baseline_path, actual_path

    def test_trailing_dot_and_leading_dot_scientific_refuse_fixed(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            for flag, value, message in (
                    ("--tolerance", "-1.", TOLERANCE_MESSAGE),
                    ("--tolerance", "-.5e3", TOLERANCE_MESSAGE),
                    ("--tolerance", "-1_0", TOLERANCE_MESSAGE),
                    ("--tolerance", "-1_000.5", TOLERANCE_MESSAGE),
                    ("--tolerance", "-1e1_0", TOLERANCE_MESSAGE),
                    ("--max-rss-mib", "-1.", RSS_MESSAGE),
                    ("--max-rss-mib", "-1_0", RSS_MESSAGE)):
                with self.subTest(flag=flag, value=value):
                    code, out, err, reads = self.run_main(
                        ["compare_pipeline_bench.py", str(actual),
                         "--baseline", str(baseline), flag, value])
                    self.assertEqual(code, 2)
                    self.assertIn(flag, err)
                    self.assertIn(message, err)
                    self.assertEqual(reads, [])
                    self.assertNotIn("Traceback", out + err)

    def test_negative_zero_passes_argparse(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            for value in ("-0.", "-0_0"):
                with self.subTest(value=value):
                    code, out, err, reads = self.run_main(
                        ["compare_pipeline_bench.py", str(actual),
                         "--baseline", str(baseline), "--tolerance", value])
                    self.assertEqual(code, 0, err)
                    self.assertTrue(out.startswith("PASS"), out)
                    self.assertNotEqual(reads, [])

    def test_non_numeric_dash_value_still_parses_as_flag(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            code, out, err, reads = self.run_main(
                ["compare_pipeline_bench.py", str(actual),
                 "--baseline", str(baseline), "--tolerance", "-x"])
            self.assertEqual(code, 2)
            self.assertEqual(reads, [])
            self.assertNotIn(TOLERANCE_MESSAGE, err)
            self.assertNotIn("Traceback", out + err)


if __name__ == "__main__":
    unittest.main()
