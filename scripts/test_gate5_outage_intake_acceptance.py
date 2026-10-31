"""Intake bounds for gate5_outage: caps refuse before parsing, valid logs unchanged.

Offline only: temp files; no network, no staging, no host.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_gate5_outage_intake_acceptance.py' -v
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
import gate5_outage as cli


START = "2026-10-01T00:00:00Z"
END = "2026-10-01T00:00:01Z"
INTERVAL = ["--interval-start", START, "--interval-end", END]

CLEAN_LOG = (
    '{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}\n'
    '{"ts": "2026-10-01T00:00:02Z", "event": "readyz_ok"}\n'
    '{"ts": "2026-10-01T00:00:03Z", "event": "readyz_ok"}\n'
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


class Gate5IntakeAcceptanceTests(unittest.TestCase):
    def test_caps_are_the_documented_contract(self):
        self.assertEqual(cli.MAX_INPUT_BYTES, 1024 * 1024)
        self.assertEqual(cli.MAX_LINE_BYTES, 64 * 1024)
        self.assertEqual(cli.MAX_RECORDS, 100_000)
        self.assertEqual(cli.MAX_UNKNOWN_INTERVALS, 1000)

    def assert_invalid(self, path, code):
        out_code, out, err = run_main([path, *INTERVAL])
        self.assertEqual(out_code, 2)
        self.assertEqual(err, "")
        verdict = json.loads(out)
        self.assertEqual(verdict["verdict"], "INVALID")
        self.assertEqual(verdict["code"], code)
        self.assertTrue(verdict["next_action"].strip())
        self.assertNotIn("\n", verdict["next_action"])
        self.assertNotIn("Traceback", out + err)
        return verdict

    def test_oversize_log_refuses_before_parsing(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            line = b"x" * cli.MAX_LINE_BYTES + b"\n"
            raw = line * (cli.MAX_INPUT_BYTES // len(line) + 1)
            path = write_tmp(directory, "big.jsonl", raw)
            with mock.patch.object(cli, "summarize",
                                   side_effect=AssertionError("over-cap log must not be parsed")):
                self.assert_invalid(path, "input_too_large")

    def test_exact_byte_cap_passes(self):
        target, chunk, lines = cli.MAX_INPUT_BYTES, 60_000, []
        remaining, seq = target, 0
        while remaining > 0:
            take = min(chunk, remaining)
            head = '{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok", "note": "'
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
            code, out, err = run_main([path, *INTERVAL])
            self.assertEqual(code, 0, out)
            self.assertEqual(json.loads(out)["verdict"], "PASS")

    def test_long_line_refuses(self):
        head = '{"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok", "note": "'
        over = head + "p" * cli.MAX_LINE_BYTES + '"}\n'
        self.assertGreater(len(over.encode().splitlines()[0]), cli.MAX_LINE_BYTES)
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            path = write_tmp(directory, "long.jsonl", over)
            with mock.patch.object(cli, "summarize",
                                   side_effect=AssertionError("over-long line must not be parsed")):
                self.assert_invalid(path, "line_too_long")

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
            self.assert_invalid("giant.jsonl", "line_too_long")
        self.assertEqual(sizes, [cli.MAX_LINE_BYTES + 2])

    def test_line_cap_excludes_lf_crlf_and_eof_terminators(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            for ending in (b"\n", b"\r\n", b""):
                for extra in (0, 1):
                    with self.subTest(ending=ending, extra=extra):
                        raw = b"x" * (cli.MAX_LINE_BYTES + extra) + ending
                        path = write_tmp(directory, "boundary.jsonl", raw)
                        if extra:
                            self.assert_invalid(path, "line_too_long")
                        else:
                            self.assertEqual(cli._read_log_lines(path),
                                             ["x" * cli.MAX_LINE_BYTES])

    def test_record_flood_refuses(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            raw = b"{}\n" * (cli.MAX_RECORDS + 1)
            self.assertLess(len(raw), cli.MAX_INPUT_BYTES)
            path = write_tmp(directory, "flood.jsonl", raw)
            with mock.patch.object(cli, "summarize",
                                   side_effect=AssertionError("record flood must not be parsed")):
                self.assert_invalid(path, "too_many_records")

    def test_exact_record_cap_passes_intake(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            raw = b"{}\n" * cli.MAX_RECORDS
            self.assertLess(len(raw), cli.MAX_INPUT_BYTES)
            path = write_tmp(directory, "exact-records.jsonl", raw)
            code, out, err = run_main([path, *INTERVAL])
            self.assertEqual(code, 1)
            summary = json.loads(out)
            self.assertEqual(summary["verdict"], "NOT VERIFIED")
            self.assertTrue(summary["truncated"])

    def test_missing_path_refuses_without_traceback(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            self.assert_invalid(str(Path(directory) / "no-such.jsonl"), "unreadable_input")

    def test_directory_refuses_without_traceback(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            self.assert_invalid(directory, "unreadable_input")

    def test_nul_byte_path_refuses_without_traceback(self):
        self.assert_invalid("a\x00b", "unreadable_input")

    def test_clean_log_verdict_unchanged(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            path = write_tmp(directory, "clean.jsonl", CLEAN_LOG)
            code, out, err = run_main([path, *INTERVAL])
            self.assertEqual(code, 0)
            self.assertEqual(err, "")
            summary = json.loads(out)
            self.assertEqual(summary["verdict"], "PASS")
            self.assertEqual(summary["outage_windows"], [])
            self.assertEqual(summary["unknown_intervals"], [])
            self.assertNotIn("truncated", summary)

    def test_bom_still_passes(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            path = write_tmp(directory, "bom.jsonl", b"\xef\xbb\xbf" + CLEAN_LOG.encode())
            code, out, err = run_main([path, *INTERVAL])
            self.assertEqual(code, 0, out)
            self.assertEqual(json.loads(out)["verdict"], "PASS")

    def test_invalid_utf8_line_stays_unknown_not_a_crash(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            raw = (CLEAN_LOG.splitlines()[0] + "\n").encode() + b"\xff\xfe not utf-8\n"
            raw += (CLEAN_LOG.splitlines()[1] + "\n").encode()
            path = write_tmp(directory, "binary.jsonl", raw)
            code, out, err = run_main([path, *INTERVAL])
            self.assertEqual(code, 1)
            self.assertEqual(err, "")
            self.assertNotIn("Traceback", out)
            summary = json.loads(out)
            self.assertEqual(summary["verdict"], "NOT VERIFIED")
            self.assertTrue(any("malformed line 2" in u["reason"]
                                for u in summary["unknown_intervals"]))

    def test_unknown_flood_truncates_with_flag(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            lines = [CLEAN_LOG.splitlines()[0] + "\n"]
            lines += ["not json at all\n"] * (cli.MAX_UNKNOWN_INTERVALS + 50)
            lines += [CLEAN_LOG.splitlines()[1] + "\n"]
            path = write_tmp(directory, "unknowns.jsonl", "".join(lines))
            code, out, err = run_main([path, *INTERVAL])
            self.assertEqual(code, 1)
            self.assertEqual(err, "")
            summary = json.loads(out)
            self.assertEqual(len(summary["unknown_intervals"]), cli.MAX_UNKNOWN_INTERVALS)
            self.assertTrue(summary["truncated"])
            self.assertEqual(summary["verdict"], "NOT VERIFIED")
            self.assertEqual(summary["readyz_ok"], 2)

    def test_help_lists_codes_and_caps(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            with self.assertRaises(SystemExit) as exited:
                cli.main(["--help"])
        self.assertEqual(exited.exception.code, 0)
        text = out.getvalue()
        for token in ("unreadable_input", "input_too_large", "line_too_long",
                      "too_many_records", "1048576", "65536", "100000", "1000"):
            self.assertIn(token, text)


if __name__ == "__main__":
    unittest.main()
