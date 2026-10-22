"""Intake bounds for compare_pipeline_bench: caps and duplicate keys refuse first.

Over-cap, unreadable, malformed, or duplicate-keyed reports refuse with exit 2,
a JSON verdict on stdout, and the historical ``INVALID benchmark:`` stderr
prefix (pinned by the argparse-refusal suite). Valid PASS / NEEDS WORK output
is byte-identical.

Offline only: reports are temp files; no network, no credentials.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_compare_pipeline_bench_intake_acceptance.py' -v
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
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import compare_pipeline_bench as bench


MARKER = "BENCH-INTAKE-MARKER-NEVER-ECHO"
SCRIPT = str(Path(__file__).with_name("compare_pipeline_bench.py"))


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


def run_cli(actual, baseline):
    return subprocess.run(
        [sys.executable, SCRIPT, str(actual), "--baseline", str(baseline)],
        capture_output=True, text=True,
        env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"), timeout=60)


def run_main_inprocess(argv):
    out, err = io.StringIO(), io.StringIO()
    with mock.patch.object(sys, "argv", argv), \
            contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        try:
            code = bench.main()
        except SystemExit as exit:
            code = exit.code
    return code, out.getvalue(), err.getvalue()


def refusing_loads(real_loads):
    """json.loads that explodes only if the refused MARKER content is decoded."""

    def loads(text, *args, **kwargs):
        body = text.decode() if isinstance(text, bytes) else text
        if MARKER in body:
            raise AssertionError("over-cap intake must not reach JSON decode")
        return real_loads(text, *args, **kwargs)

    return loads


class CompareIntakeAcceptanceTests(unittest.TestCase):
    def test_caps_are_the_documented_contract(self):
        self.assertEqual(bench.MAX_INPUT_BYTES, 1024 * 1024)
        self.assertEqual(bench.MAX_LINE_BYTES, 64 * 1024)
        self.assertEqual(bench.MAX_RECORDS, 100_000)

    def assert_invalid(self, result, code):
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertTrue(result.stderr.startswith("INVALID benchmark: "),
                        result.stderr)
        verdict = json.loads(result.stdout)
        self.assertEqual(verdict["verdict"], "INVALID")
        self.assertEqual(verdict["code"], code)
        self.assertTrue(verdict["next_action"].strip())
        self.assertNotIn("\n", verdict["next_action"])
        self.assertNotIn("Traceback", result.stdout + result.stderr)
        self.assertNotIn(MARKER, result.stdout + result.stderr)
        return verdict

    def write_reports(self, directory, baseline=None, actual=None):
        baseline_path = Path(directory) / "baseline.json"
        actual_path = Path(directory) / "actual.json"
        baseline_path.write_text(json.dumps(
            valid_report() if baseline is None else baseline))
        if actual is not None:
            if isinstance(actual, bytes):
                actual_path.write_bytes(actual)
            else:
                actual_path.write_text(actual)
        else:
            actual_path.write_text(json.dumps(valid_report()))
        return baseline_path, actual_path

    def test_oversize_actual_refuses_before_decode(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            big = {"note": MARKER + "x" * bench.MAX_INPUT_BYTES}
            actual.write_text(json.dumps(valid_report(**big)))
            self.assertGreater(actual.stat().st_size, bench.MAX_INPUT_BYTES)
            with mock.patch.object(bench.json, "loads",
                                   side_effect=refusing_loads(json.loads)):
                code, out, err = run_main_inprocess(
                    ["compare_pipeline_bench.py", str(actual),
                     "--baseline", str(baseline)])
            self.assertEqual(code, 2, err)
            self.assertTrue(err.startswith("INVALID benchmark: "), err)
            self.assertEqual(json.loads(out)["code"], "input_too_large")
            self.assertNotIn(MARKER, out + err)

    def test_oversize_baseline_refuses(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            big = {"note": MARKER + "x" * bench.MAX_INPUT_BYTES}
            baseline.write_text(json.dumps(valid_report(**big)))
            self.assert_invalid(run_cli(actual, baseline), "input_too_large")

    def test_exact_byte_cap_passes(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            base = json.dumps(valid_report(), indent=2)
            head, tail = base.rsplit("}", 1)
            # Pad entries of ~60 KiB each, one per line, sized to the cap exactly.
            # used starts one past len(base): the closing "\n}" below adds a byte.
            pieces, index, used = [head], 0, len(base.encode()) + 1
            while True:
                entry = ',\n  "pad%04d": "%%s"' % index
                room = bench.MAX_INPUT_BYTES - used - len((entry % "").encode())
                if room <= 0:
                    break
                take = min(60_000, room)
                pieces.append(entry % ("x" * take))
                used += len((entry % ("x" * take)).encode())
                index += 1
            raw = ("".join(pieces) + "\n}" + tail).encode()
            self.assertEqual(len(raw), bench.MAX_INPUT_BYTES)
            self.assertTrue(all(len(line) <= bench.MAX_LINE_BYTES
                                for line in raw.splitlines()))
            actual.write_bytes(raw)
            result = run_cli(actual, baseline)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(result.stdout.startswith("PASS"), result.stdout)

    def test_long_line_refuses(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            actual.write_text('{"note": "' + MARKER + "x" * bench.MAX_LINE_BYTES + '"}')
            raw = actual.read_bytes()
            self.assertLess(len(raw), bench.MAX_INPUT_BYTES)
            self.assertGreater(len(raw.splitlines()[0]), bench.MAX_LINE_BYTES)
            with mock.patch.object(bench.json, "loads",
                                   side_effect=refusing_loads(json.loads)):
                code, out, err = run_main_inprocess(
                    ["compare_pipeline_bench.py", str(actual),
                     "--baseline", str(baseline)])
            self.assertEqual(code, 2, err)
            self.assertEqual(json.loads(out)["code"], "line_too_long")
            self.assertNotIn(MARKER, out + err)

    def test_exact_line_cap_passes(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            report = valid_report()
            stub = json.dumps(report, separators=(",", ":"))
            head, tail = stub.rsplit("}", 1)
            pad = bench.MAX_LINE_BYTES - len((head + ',"pad":""').encode()) - len(tail.encode()) - 1
            raw = (head + ',"pad":"' + "x" * pad + '"}' + tail).encode()
            self.assertEqual(len(raw.splitlines()), 1)
            self.assertEqual(len(raw.splitlines()[0]), bench.MAX_LINE_BYTES)
            actual.write_bytes(raw)
            result = run_cli(actual, baseline)
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_record_flood_refuses(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            raw = (MARKER + "\n").encode() + b"x\n" * bench.MAX_RECORDS
            self.assertLess(len(raw), bench.MAX_INPUT_BYTES)
            actual.write_bytes(raw)
            with mock.patch.object(bench.json, "loads",
                                   side_effect=refusing_loads(json.loads)):
                code, out, err = run_main_inprocess(
                    ["compare_pipeline_bench.py", str(actual),
                     "--baseline", str(baseline)])
            self.assertEqual(code, 2, err)
            self.assertEqual(json.loads(out)["code"], "too_many_records")
            self.assertNotIn(MARKER, out + err)

    def test_exact_record_cap_reaches_decode(self):
        # 100000 short lines pass intake; the refusal below comes from the
        # JSON stage instead, proving the record count itself was accepted.
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            raw = b"x\n" * bench.MAX_RECORDS
            self.assertLess(len(raw), bench.MAX_INPUT_BYTES)
            actual.write_bytes(raw)
            self.assert_invalid(run_cli(actual, baseline), "invalid_json")

    def test_missing_actual_refuses(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, _ = self.write_reports(directory)
            missing = Path(directory) / "no-actual.json"
            self.assert_invalid(run_cli(missing, baseline), "unreadable_input")

    def test_missing_baseline_refuses(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            _, actual = self.write_reports(directory)
            missing = Path(directory) / "no-baseline.json"
            self.assert_invalid(run_cli(actual, missing), "unreadable_input")

    def test_directory_refuses(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, _ = self.write_reports(directory)
            self.assert_invalid(run_cli(Path(directory), baseline), "unreadable_input")

    def test_nul_byte_path_refuses_without_traceback(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, _ = self.write_reports(directory)
            code, out, err = run_main_inprocess(
                ["compare_pipeline_bench.py", "a\x00b",
                 "--baseline", str(baseline)])
            self.assertEqual(code, 2)
            self.assertTrue(err.startswith("INVALID benchmark: "), err)
            self.assertEqual(json.loads(out)["code"], "unreadable_input")
            self.assertNotIn("Traceback", out + err)

    def test_duplicate_top_level_key_refuses(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            report = json.dumps(valid_report())
            head, tail = report.rsplit("}", 1)
            actual.write_text(head + ', "schema_version": 1}' + tail)
            result = run_cli(actual, baseline)
            self.assert_invalid(result, "duplicate_keys")

    def test_duplicate_nested_and_escaped_keys_refuse(self):
        cases = [
            # Nested repeat inside metrics.
            lambda report: report.replace('"db_round_trips_per_event": 2',
                                          '"db_round_trips_per_event": 2, "p50": 1, "p50": 2'),
            # Escaped repeat: the decoder sees the same key twice.
            lambda report: report.replace('"schema_version": 1',
                                          '"schema_version": 1, "\\u0073chema_version": 1'),
        ]
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            for index, mutate in enumerate(cases):
                with self.subTest(case=index):
                    actual.write_text(mutate(json.dumps(valid_report())))
                    self.assert_invalid(run_cli(actual, baseline), "duplicate_keys")

    def test_duplicate_key_in_baseline_refuses(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            report = json.dumps(valid_report())
            head, tail = report.rsplit("}", 1)
            baseline.write_text(head + ', "schema_version": 1}' + tail)
            self.assert_invalid(run_cli(actual, baseline), "duplicate_keys")

    def test_malformed_json_refuses(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            actual.write_text("{" + MARKER + " not json")
            self.assert_invalid(run_cli(actual, baseline), "invalid_json")

    def test_checked_bytes_are_parsed_without_a_second_read(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            checked = actual.read_bytes()
            real_open = Path.open
            opens = []

            @contextlib.contextmanager
            def swapping_open(path, *args, **kwargs):
                opens.append(str(path))
                with real_open(path, *args, **kwargs) as handle:
                    yield handle
                # Simulate a producer replacing the report after its first
                # bounded read. Parsing must use only the checked snapshot.
                with real_open(path, "wb") as handle:
                    handle.write(b"x" * (bench.MAX_INPUT_BYTES + 1))

            with mock.patch.object(Path, "open", swapping_open):
                text = bench._read_report(actual)
            self.assertEqual(text, checked.decode("utf-8"))
            self.assertEqual(opens, [str(actual)])

    def test_bad_flags_never_open_reports(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            for flag, value in (("--tolerance", "nan"), ("--max-rss-mib", "257")):
                with self.subTest(flag=flag), mock.patch.object(
                        Path, "open", side_effect=AssertionError("flag refusal must precede I/O")):
                    code, out, err = run_main_inprocess([
                        "compare_pipeline_bench.py", str(actual),
                        "--baseline", str(baseline), flag, value])
                    self.assertEqual(code, 2)
                    self.assertIn(flag, err)
                    self.assertNotIn("Traceback", out + err)

    def test_huge_integer_metrics_refuse_without_traceback(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            for report_side in ("baseline", "actual"):
                for metric in bench.METRICS:
                    with self.subTest(report_side=report_side, metric=metric):
                        report = valid_report()
                        container = report["metrics"]
                        for key in metric[:-1]:
                            container = container[key]
                        container[metric[-1]] = 10 ** 400
                        baseline, actual = self.write_reports(directory)
                        path = baseline if report_side == "baseline" else actual
                        path.write_text(json.dumps(report))
                        self.assert_invalid(run_cli(actual, baseline), "invalid_benchmark")

    def test_validation_error_keeps_its_message(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            regressed = valid_report()
            regressed["workload"]["members"] = 1000
            baseline, actual = self.write_reports(directory, actual=json.dumps(regressed))
            result = run_cli(actual, baseline)
            verdict = self.assert_invalid(result, "invalid_benchmark")
            self.assertIn("workload differs", result.stderr)

    def test_valid_pass_and_needs_work_byte_identical(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            baseline, actual = self.write_reports(directory)
            result = run_cli(actual, baseline)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "PASS: same workload, metrics within "
                                            "tolerance and lite RSS budget\n")
            self.assertEqual(result.stderr, "")
            regressed = valid_report()
            regressed["metrics"]["peak_rss_mib"] = 60
            actual.write_text(json.dumps(regressed))
            result = run_cli(actual, baseline)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertEqual(result.stdout, "NEEDS WORK\npeak_rss_mib: 60.000 > "
                                            "50.000 (25% tolerance)\n")

    def test_help_lists_codes_and_caps(self):
        result = subprocess.run(
            [sys.executable, SCRIPT, "--help"],
            capture_output=True, text=True,
            env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"), timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        for token in ("unreadable_input", "input_too_large", "line_too_long",
                      "too_many_records", "duplicate_keys", "invalid_json",
                      "invalid_benchmark", "1048576", "65536", "100000"):
            self.assertIn(token, result.stdout)


if __name__ == "__main__":
    unittest.main()
