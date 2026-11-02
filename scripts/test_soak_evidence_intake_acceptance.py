"""Intake bounds for soak_evidence: caps refuse before parsing, valid logs unchanged.

Offline only: temp files; no network, no staging, no host.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_soak_evidence_intake_acceptance.py' -v
"""

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import soak_evidence as cli


ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "scripts/fixtures/soak_evidence_sample.jsonl"
EXPECTED = ROOT / "scripts/fixtures/soak_evidence_sample.summary.json"

CLEAN_LOG = (
    '{"ts": "2026-09-20T00:00:01Z", "event": "connect", "session": "a"}\n'
    '{"ts": "2026-09-20T00:00:02Z", "event": "dispatch", "session": "a", "seq": 1}\n'
    '{"ts": "2026-09-20T00:00:03Z", "event": "dispatch", "session": "a", "seq": 2}\n'
)


def scratch_dir():
    return (os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
            or os.environ.get("PAPERCLIP_SCRATCH_DIR")
            or os.environ.get("RUNNER_TEMP"))


def run_main(argv):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = cli.main(argv)
    return code, out.getvalue(), err.getvalue()


def write_tmp(directory, name, data):
    path = Path(directory) / name
    if isinstance(data, str):
        path.write_text(data, encoding="utf-8")
    else:
        path.write_bytes(data)
    return str(path)


class SoakIntakeAcceptanceTests(unittest.TestCase):
    def test_caps_are_the_documented_contract(self):
        self.assertEqual(cli.MAX_INPUT_BYTES, 1024 * 1024)
        self.assertEqual(cli.MAX_LINE_BYTES, 64 * 1024)
        self.assertEqual(cli.MAX_RECORDS, 100_000)
        self.assertEqual(cli.MAX_UNKNOWN_INTERVALS, 1000)
        self.assertEqual(cli.MAX_REDEPLOY_GAPS, 10_000)

    def assert_invalid(self, path_argv, code):
        code_out, out, err = run_main(path_argv)
        self.assertEqual(code_out, 2)
        self.assertEqual(err, "")
        verdict = json.loads(out)
        self.assertEqual(verdict["verdict"], "INVALID")
        self.assertEqual(verdict["code"], code)
        self.assertTrue(verdict["next_action"].strip())
        self.assertNotIn("\n", verdict["next_action"])
        self.assertNotIn("Traceback", out + err)
        return verdict

    def test_oversize_log_refuses_before_json_decode(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            line = b"x" * cli.MAX_LINE_BYTES + b"\n"
            raw = line * (cli.MAX_INPUT_BYTES // len(line) + 1)
            path = write_tmp(directory, "big.jsonl", raw)
            with mock.patch.object(cli, "summarize",
                                   side_effect=AssertionError("over-cap log must not be parsed")):
                self.assert_invalid([path], "input_too_large")

    def test_exact_byte_cap_passes(self):
        # Dispatch lines padded to ~60 KiB each, sized to hit the cap exactly.
        # Same timestamp and rising seqs: no gaps, no unknowns, a clean PASS.
        target, chunk, seq, lines = cli.MAX_INPUT_BYTES, 60_000, 1, []
        remaining = target
        while remaining > 0:
            take = min(chunk, remaining)
            head = ('{"ts": "2026-09-20T00:00:01Z", "event": "dispatch", '
                    f'"session": "a", "seq": {seq}, "note": "')
            tail = '"}\n'
            pad = take - len(head.encode()) - len(tail.encode())
            lines.append(head + "p" * pad + tail)
            remaining -= take
            seq += 1
        raw = "".join(lines).encode()
        self.assertEqual(len(raw), cli.MAX_INPUT_BYTES)
        self.assertTrue(all(len(line.encode()) <= cli.MAX_LINE_BYTES for line in lines))
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            path = write_tmp(directory, "exact.jsonl", raw)
            code, out, err = run_main([path])
            self.assertEqual(code, 0, out)
            self.assertEqual(err, "")
            self.assertEqual(json.loads(out)["verdict"], "PASS")

    def test_long_line_refuses(self):
        line = '{"ts": "2026-09-20T00:00:01Z", "event": "connect", "session": "a%s"}\n'
        over = line % ("a" * cli.MAX_LINE_BYTES)
        self.assertGreater(len(over.encode().splitlines()[0]), cli.MAX_LINE_BYTES)
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            path = write_tmp(directory, "long.jsonl", over)
            with mock.patch.object(cli, "summarize",
                                   side_effect=AssertionError("over-long line must not be parsed")):
                self.assert_invalid([path], "line_too_long")

    def test_giant_line_read_is_bounded_before_refusal(self):
        sizes = []

        class BoundedReader(io.BytesIO):
            def __iter__(self):
                raise AssertionError("unbounded line iteration")

            def readline(self, size=-1):
                sizes.append(size)
                return super().readline(size)

        handle = BoundedReader(b"x" * (cli.MAX_LINE_BYTES * 4))
        with mock.patch.object(cli, "open", return_value=handle), \
                mock.patch.object(cli.os.path, "isfile", return_value=True):
            self.assert_invalid(["giant.jsonl"], "line_too_long")
        self.assertEqual(sizes, [cli.MAX_LINE_BYTES + 2])

    def test_line_cap_excludes_lf_crlf_and_eof_terminators(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            for ending in (b"\n", b"\r\n", b""):
                for extra in (0, 1):
                    with self.subTest(ending=ending, extra=extra):
                        raw = b"x" * (cli.MAX_LINE_BYTES + extra) + ending
                        path = write_tmp(directory, "boundary.jsonl", raw)
                        if extra:
                            self.assert_invalid([path], "line_too_long")
                        else:
                            self.assertEqual(cli._read_log_lines(path),
                                             ["x" * cli.MAX_LINE_BYTES])

    def test_exact_line_cap_passes(self):
        line = '{"ts": "2026-09-20T00:00:01Z", "event": "connect", "session": "a%s"}\n'
        exact = line % ("a" * (cli.MAX_LINE_BYTES - len((line % "").encode().splitlines()[0])))
        self.assertEqual(len(exact.encode().splitlines()[0]), cli.MAX_LINE_BYTES)
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            path = write_tmp(directory, "exact-line.jsonl", exact)
            code, out, err = run_main([path])
            # connect-only input is missing dispatch evidence, not a clean run.
            self.assertEqual(code, 1)
            self.assertEqual(json.loads(out)["verdict"], "NEEDS WORK")

    def test_record_flood_refuses(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            raw = b"{}\n" * (cli.MAX_RECORDS + 1)
            self.assertLess(len(raw), cli.MAX_INPUT_BYTES)
            path = write_tmp(directory, "flood.jsonl", raw)
            with mock.patch.object(cli, "summarize",
                                   side_effect=AssertionError("record flood must not be parsed")):
                self.assert_invalid([path], "too_many_records")

    def test_exact_record_cap_passes_intake(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            raw = b"{}\n" * cli.MAX_RECORDS
            self.assertLess(len(raw), cli.MAX_INPUT_BYTES)
            path = write_tmp(directory, "exact-records.jsonl", raw)
            code, out, err = run_main([path])
            # Every line is malformed (bad ts), so the verdict is NEEDS WORK
            # with a truncated unknown list -- but intake itself passes.
            self.assertEqual(code, 1)
            summary = json.loads(out)
            self.assertEqual(summary["verdict"], "NEEDS WORK")
            self.assertTrue(summary["truncated"])

    def test_missing_path_refuses_without_traceback(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            self.assert_invalid([str(Path(directory) / "no-such.jsonl")], "unreadable_input")

    def test_directory_refuses_without_traceback(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            self.assert_invalid([directory], "unreadable_input")

    def test_nul_byte_path_refuses_without_traceback(self):
        self.assert_invalid(["a\x00b"], "unreadable_input")

    def test_committed_fixture_verdict_byte_identical(self):
        code, out, err = run_main([str(FIXTURE)])
        self.assertEqual(code, 1)
        self.assertEqual(err, "")
        self.assertEqual(out, EXPECTED.read_text(encoding="utf-8"))

    def test_clean_log_golden_stdout(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            path = write_tmp(directory, "clean.jsonl", CLEAN_LOG)
            code, out, err = run_main([path])
            self.assertEqual(code, 0)
            self.assertEqual(err, "")
            summary = json.loads(out)
            self.assertEqual(summary["verdict"], "PASS")
            self.assertNotIn("truncated", summary)
            self.assertEqual(summary["unknown_intervals"], [])
            self.assertEqual(summary["redeploy_gaps_s"], [])

    def test_unknown_flood_truncates_with_flag(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            lines = [CLEAN_LOG.splitlines()[0] + "\n"]
            lines += ["not json at all\n"] * (cli.MAX_UNKNOWN_INTERVALS + 50)
            lines += [CLEAN_LOG.splitlines()[1] + "\n"]
            path = write_tmp(directory, "unknowns.jsonl", "".join(lines))
            code, out, err = run_main([path])
            self.assertEqual(code, 1)
            self.assertEqual(err, "")
            summary = json.loads(out)
            self.assertEqual(len(summary["unknown_intervals"]), cli.MAX_UNKNOWN_INTERVALS)
            self.assertTrue(summary["truncated"])
            self.assertEqual(summary["verdict"], "NEEDS WORK")
            # The valid dispatch after the flood is still summarized.
            self.assertEqual(summary["dispatches"], 1)

    def test_redeploy_gap_flood_truncates_but_keeps_max(self):
        # Direct summarize: 10025 gaps need 20051 lines, which the CLI byte
        # cap already refuses; the stored-list cap is the library backstop.
        from datetime import datetime, timedelta, timezone
        base = datetime(2026, 9, 20, tzinfo=timezone.utc)
        lines = []
        stamp = base
        # Alternate disconnect/connect: every pair closes one gap.
        lines.append(json.dumps({"ts": stamp.isoformat(), "event": "connect", "session": "a"}))
        stamp += timedelta(seconds=1)
        for index in range(cli.MAX_REDEPLOY_GAPS + 25):
            lines.append(json.dumps({"ts": stamp.isoformat(), "event": "disconnect"}))
            # Only a dropped gap breaches the budget; max-over-stored would
            # wrongly report 2 seconds and PASS.
            stamp += timedelta(seconds=120 if index == cli.MAX_REDEPLOY_GAPS + 24 else 2)
            lines.append(json.dumps({"ts": stamp.isoformat(), "event": "connect", "session": "a"}))
            stamp += timedelta(seconds=1)
        lines.append(json.dumps({"ts": stamp.isoformat(), "event": "dispatch",
                                 "session": "a", "seq": 1}))
        summary = cli.summarize(io.StringIO("\n".join(lines) + "\n"))
        self.assertEqual(summary["verdict"], "NEEDS WORK")
        self.assertEqual(len(summary["redeploy_gaps_s"]), cli.MAX_REDEPLOY_GAPS)
        self.assertTrue(summary["truncated"])
        self.assertEqual(max(summary["redeploy_gaps_s"]), 2.0)
        self.assertEqual(summary["unknown_intervals"], [])
        self.assertEqual(summary["dispatches"], 1)
        # Only the dropped gap can cause this breach.
        self.assertEqual(summary["max_redeploy_gap_s"], 120.0)

    def test_invalid_utf8_line_is_unknown_not_a_crash(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            raw = (CLEAN_LOG.splitlines()[0] + "\n").encode() + b"\xff\xfe not utf-8\n"
            raw += (CLEAN_LOG.splitlines()[1] + "\n").encode()
            path = write_tmp(directory, "binary.jsonl", raw)
            code, out, err = run_main([path])
            self.assertEqual(code, 1)
            self.assertEqual(err, "")
            self.assertNotIn("Traceback", out)
            summary = json.loads(out)
            self.assertEqual(summary["verdict"], "NEEDS WORK")
            self.assertEqual(len(summary["unknown_intervals"]), 1)
            self.assertEqual(summary["dispatches"], 1)

    def test_help_lists_codes_and_caps(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            with self.assertRaises(SystemExit) as exited:
                cli.main(["--help"])
        self.assertEqual(exited.exception.code, 0)
        text = out.getvalue()
        for token in ("unreadable_input", "input_too_large", "line_too_long",
                      "too_many_records", "1048576", "65536", "100000",
                      "1000", "10000"):
            self.assertIn(token, text)


if __name__ == "__main__":
    unittest.main()
