"""Protected acceptance: bench tolerance/bounds refuse at argparse time.

``--tolerance`` and ``--max-rss-mib`` parse as bare floats, so a non-finite or
out-of-range value is only rejected inside ``compare()`` -- after both report
files have been read. A missing-file ``OSError`` then masks the bad flag, and
a defence-in-depth caller cannot trust that refusal precedes I/O. The fix must
validate both flags at argparse time, before any file read:

* ``--tolerance`` refuses with ``TOLERANCE_MESSAGE`` unless finite and >= 0;
* ``--max-rss-mib`` refuses with ``RSS_MESSAGE`` unless finite and in
  ``0 < value <= 256``.

Both messages reuse the exact ``compare()`` vocabulary. CLI level exits 2 via
argparse, names the flag, reads no file, prints no traceback, and echoes no
file content.

Passing controls pin the valid-input behavior byte-for-byte (same PASS /
NEEDS WORK / INVALID texts, same exit codes, same boundary values, same
defaults) so the fix cannot trade one outcome for another.

Offline only: reports are temp files; no network, no credentials.
Builder must not edit this file; build on the pushed head.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_compare_pipeline_bench_argparse_refusal_acceptance.py' -v
"""

import contextlib
import copy
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import compare_pipeline_bench as bench
from compare_pipeline_bench import compare


TOLERANCE_MESSAGE = "tolerance must be finite and nonnegative"
RSS_MESSAGE = "RSS limit must be positive and at most the 256 MiB lite ceiling"
MARKER = "BENCH-FIXTURE-MARKER-NEVER-ECHO"

NONFINITE_TOLERANCES = ("nan", "inf", "-inf", "+Infinity", "1e1000")
BAD_TOLERANCES = NONFINITE_TOLERANCES + ("-1", "-0.5", "-1e3")
NONFINITE_RSS = ("nan", "inf", "-inf", "+Infinity")
BAD_RSS = NONFINITE_RSS + ("0", "0.0", "-5", "257", "256.5", "1e9")


def valid_report(**overrides):
    report = {
        "schema_version": 1,
        "workload": {"members": 107, "channels": 10, "duration_secs": 30},
        "metrics": {
            "peak_rss_mib": 40,
            "handler_latency_us": {"p50": 100, "p99": 1000},
            "db_round_trips_per_event": 2,
        },
    }
    report.update(overrides)
    return report


def scratch_dir():
    return (os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
            or os.environ.get("PAPERCLIP_SCRATCH_DIR")
            or os.environ.get("RUNNER_TEMP"))


class ArgparseRefusalAcceptanceTests(unittest.TestCase):
    def run_main(self, argv, read_side_effect=None):
        """Run main() with captured streams; optionally guard every file read."""
        out, err = io.StringIO(), io.StringIO()
        reads = []
        real_read_text = Path.read_text

        def counting_read_text(self, *args, **kwargs):
            reads.append(str(self))
            if read_side_effect is not None:
                raise read_side_effect
            return real_read_text(self, *args, **kwargs)

        with patch.object(sys, "argv", argv), \
                patch.object(Path, "read_text", counting_read_text), \
                contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = bench.main()
            except SystemExit as exit:
                code = exit.code
        return code, out.getvalue(), err.getvalue(), reads

    def write_reports(self, directory, baseline=None, actual=None):
        baseline_path = Path(directory) / "baseline.json"
        actual_path = Path(directory) / "actual.json"
        baseline_path.write_text(json.dumps(
            valid_report() if baseline is None else baseline))
        actual_path.write_text(json.dumps(
            valid_report() if actual is None else actual))
        return baseline_path, actual_path

    def assert_argparse_refusal(self, argv, flag, message, label, read_side_effect=None):
        code, out, err, reads = self.run_main(argv, read_side_effect)
        self.assertEqual(code, 2, f"bad flag accepted in case {label}")
        self.assertIn(flag, err, f"flag not named in case {label}")
        self.assertIn(message, err, f"fixed refusal missing in case {label}")
        self.assertEqual(reads, [], f"file read before argparse refusal in case {label}")
        self.assertNotIn("Traceback", out + err, f"traceback leaked in case {label}")
        self.assertNotIn(MARKER, out + err, f"file content echoed in case {label}")

    # --- tolerance refuses before any file read -----------------------------

    def test_nonfinite_tolerance_refused_before_any_file_read(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            for value in NONFINITE_TOLERANCES:
                with self.subTest(value=value):
                    self.assert_argparse_refusal(
                        ["compare_pipeline_bench.py", str(actual),
                         "--baseline", str(baseline), "--tolerance", value],
                        "--tolerance", TOLERANCE_MESSAGE, f"tolerance:{value}")

    def test_negative_tolerance_refused_before_any_file_read(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            for value in ("-1", "-0.5", "-1e3"):
                with self.subTest(value=value):
                    self.assert_argparse_refusal(
                        ["compare_pipeline_bench.py", str(actual),
                         "--baseline", str(baseline), "--tolerance", value],
                        "--tolerance", TOLERANCE_MESSAGE, f"tolerance:{value}")

    def test_bad_tolerance_refused_when_files_are_missing(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            missing_baseline = Path(directory) / "no-baseline.json"
            missing_actual = Path(directory) / "no-actual.json"
            for value in ("nan", "-1"):
                with self.subTest(value=value):
                    self.assert_argparse_refusal(
                        ["compare_pipeline_bench.py", str(missing_actual),
                         "--baseline", str(missing_baseline),
                         "--tolerance", value],
                        "--tolerance", TOLERANCE_MESSAGE, f"missing-files:{value}")

    def test_bad_tolerance_refused_when_files_are_unreadable(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            for value in ("inf", "-2"):
                with self.subTest(value=value):
                    self.assert_argparse_refusal(
                        ["compare_pipeline_bench.py", str(actual),
                         "--baseline", str(baseline), "--tolerance", value],
                        "--tolerance", TOLERANCE_MESSAGE, f"unreadable:{value}",
                        read_side_effect=OSError("disk gone"))

    def test_bad_tolerance_refused_when_files_hold_garbage(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline = Path(directory) / "baseline.json"
            actual = Path(directory) / "actual.json"
            baseline.write_text("{" + MARKER + " not json")
            actual.write_text("[" + MARKER + " not json")
            self.assert_argparse_refusal(
                ["compare_pipeline_bench.py", str(actual),
                 "--baseline", str(baseline), "--tolerance", "nan"],
                "--tolerance", TOLERANCE_MESSAGE, "garbage-files")

    # --- bounds refuse before any file read ----------------------------------

    def test_nonfinite_bounds_refused_before_any_file_read(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            for value in NONFINITE_RSS:
                with self.subTest(value=value):
                    self.assert_argparse_refusal(
                        ["compare_pipeline_bench.py", str(actual),
                         "--baseline", str(baseline), "--max-rss-mib", value],
                        "--max-rss-mib", RSS_MESSAGE, f"max-rss:{value}")

    def test_out_of_range_bounds_refused_before_any_file_read(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            for value in ("0", "0.0", "-5", "257", "256.5", "1e9"):
                with self.subTest(value=value):
                    self.assert_argparse_refusal(
                        ["compare_pipeline_bench.py", str(actual),
                         "--baseline", str(baseline), "--max-rss-mib", value],
                        "--max-rss-mib", RSS_MESSAGE, f"max-rss:{value}")

    def test_bad_bounds_refused_when_files_are_missing(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            missing_baseline = Path(directory) / "no-baseline.json"
            missing_actual = Path(directory) / "no-actual.json"
            for value in ("nan", "999"):
                with self.subTest(value=value):
                    self.assert_argparse_refusal(
                        ["compare_pipeline_bench.py", str(missing_actual),
                         "--baseline", str(missing_baseline),
                         "--max-rss-mib", value],
                        "--max-rss-mib", RSS_MESSAGE, f"missing-files:{value}")

    # --- subprocess surface -----------------------------------------------------

    def test_subprocess_missing_files_report_bad_flag_not_io(self):
        script = str(Path(__file__).with_name("compare_pipeline_bench.py"))
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            missing_baseline = str(Path(directory) / "no-baseline.json")
            missing_actual = str(Path(directory) / "no-actual.json")
            for flag, value, message in (
                    ("--tolerance", "nan", TOLERANCE_MESSAGE),
                    ("--max-rss-mib", "999", RSS_MESSAGE)):
                with self.subTest(flag=flag):
                    result = subprocess.run(
                        [sys.executable, script, missing_actual,
                         "--baseline", missing_baseline, flag, value],
                        capture_output=True, text=True,
                        env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"),
                        timeout=60)
                    self.assertEqual(result.returncode, 2)
                    self.assertIn(flag, result.stderr)
                    self.assertIn(message, result.stderr)
                    self.assertNotIn("Traceback", result.stdout + result.stderr)

    # --- passing controls: valid behavior byte-identical -------------------------

    def test_control_non_float_still_refused_at_argparse(self):
        # Exact float() wording is intentionally unpinned: a custom argparse
        # type may rephrase it, but garbage must still refuse before any read.
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            code, out, err, reads = self.run_main(
                ["compare_pipeline_bench.py", str(actual),
                 "--baseline", str(baseline), "--tolerance", "abc"])
            self.assertEqual(code, 2)
            self.assertIn("--tolerance", err)
            self.assertEqual(reads, [])
            self.assertNotIn("Traceback", out + err)

    def test_control_boundary_values_pass_through(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            code, out, err, _ = self.run_main(
                ["compare_pipeline_bench.py", str(actual),
                 "--baseline", str(baseline),
                 "--tolerance", "0", "--max-rss-mib", "256"])
            self.assertEqual(code, 0, err)
            self.assertEqual(out, "PASS: same workload, metrics within tolerance "
                                  "and lite RSS budget\n")

    def test_control_cli_outcomes_byte_identical(self):
        script = str(Path(__file__).with_name("compare_pipeline_bench.py"))
        env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1")
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            command = [sys.executable, script, str(actual),
                       "--baseline", str(baseline)]
            result = subprocess.run(command, capture_output=True, text=True,
                                    env=env, timeout=60)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "PASS: same workload, metrics within "
                                            "tolerance and lite RSS budget\n")
            regressed = valid_report()
            regressed["metrics"]["peak_rss_mib"] = 60
            actual.write_text(json.dumps(regressed))
            result = subprocess.run(command, capture_output=True, text=True,
                                    env=env, timeout=60)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertEqual(result.stdout, "NEEDS WORK\npeak_rss_mib: 60.000 > "
                                            "50.000 (25% tolerance)\n")
            actual.write_text("not JSON")
            result = subprocess.run(command, capture_output=True, text=True,
                                    env=env, timeout=60)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertTrue(result.stderr.startswith("INVALID benchmark: "),
                            result.stderr)
            actual.unlink()
            result = subprocess.run(command, capture_output=True, text=True,
                                    env=env, timeout=60)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertTrue(result.stderr.startswith("INVALID benchmark: "),
                            result.stderr)

    def test_control_compare_limit_messages_unchanged(self):
        baseline, actual = valid_report(), valid_report()
        for value in (-1, float("nan"), float("inf")):
            with self.subTest(tolerance=value):
                with self.assertRaises(ValueError) as caught:
                    compare(baseline, actual, tolerance=value)
                self.assertEqual(str(caught.exception), TOLERANCE_MESSAGE)
        for value in (0, 257, float("nan"), float("inf")):
            with self.subTest(max_rss=value):
                with self.assertRaises(ValueError) as caught:
                    compare(baseline, actual, max_rss_mib=value)
                self.assertEqual(str(caught.exception), RSS_MESSAGE)

    def test_control_default_tolerance_is_quarter(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            over = valid_report()
            over["metrics"]["peak_rss_mib"] = 50
            actual.write_text(json.dumps(over))
            code, out, err, _ = self.run_main(
                ["compare_pipeline_bench.py", str(actual),
                 "--baseline", str(baseline)])
            self.assertEqual(code, 0, err)
            self.assertTrue(out.startswith("PASS"), out)
            over["metrics"]["peak_rss_mib"] = 51
            actual.write_text(json.dumps(over))
            code, out, err, _ = self.run_main(
                ["compare_pipeline_bench.py", str(actual),
                 "--baseline", str(baseline)])
            self.assertEqual(code, 1, err)
            self.assertTrue(out.startswith("NEEDS WORK"), out)

    def test_control_default_rss_budget_is_200(self):
        baseline = valid_report()
        baseline["metrics"]["peak_rss_mib"] = 170
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline_path, actual_path = self.write_reports(directory, baseline=baseline)
            # 200 passes the 25% tolerance (limit 212.5) but trips the budget.
            over = valid_report()
            over["metrics"]["peak_rss_mib"] = 200
            actual_path.write_text(json.dumps(over))
            code, out, err, _ = self.run_main(
                ["compare_pipeline_bench.py", str(actual_path),
                 "--baseline", str(baseline_path)])
            self.assertEqual(code, 1, err)
            self.assertIn("lite budget 200.000", out)
            over["metrics"]["peak_rss_mib"] = 199
            actual_path.write_text(json.dumps(over))
            code, out, err, _ = self.run_main(
                ["compare_pipeline_bench.py", str(actual_path),
                 "--baseline", str(baseline_path)])
            self.assertEqual(code, 0, err)


if __name__ == "__main__":
    unittest.main()
