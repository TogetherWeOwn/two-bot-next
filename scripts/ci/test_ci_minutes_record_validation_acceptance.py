"""Protected acceptance tests: ``ci_minutes.record`` input validation.

Contract under test (the fix in ``ci_minutes.py`` must make these pass
without changing the ledger row shape or the ``report`` path):

* Minutes must be a finite, non-negative number. NaN, positive or
  negative infinity (including values that overflow to infinity when
  parsed, such as ``1e999``), and negative minutes are refused with
  ``SystemExit`` -- a clean, readable refusal, never a traceback, and
  never a written ledger row. Today NaN/Inf are written into the
  ledger as the bare tokens ``NaN`` / ``Infinity``, which are not
  valid JSON, so one bad measurement corrupts every later comparison
  that parses the ledger strictly.
* The PR number must be a positive integer with no whitespace in or
  around it. Alphabetic text, decimal text, negative numbers, zero,
  the empty string, whitespace-only, whitespace-padded and
  internally-spaced values are refused with ``SystemExit``. Today
  non-numeric text escapes as an uncaught ``ValueError`` traceback
  from ``int(pr)``, while negative, zero and padded values are
  silently recorded.
* A refused call leaves the ledger untouched: it creates no file
  when none existed and appends nothing when one did, so a bad
  invocation can be corrected and retried against a clean ledger.
* Existing accepted inputs keep working byte for byte: string and
  typed numeric minutes (including ``0`` and scientific notation)
  and string or typed-int PR numbers append one strictly-JSON line
  per call, ``{"minutes": <float>, "pr": <int>, "sha": <sha>}``.

Hermetic only: temp-dir ledgers, no network, no ``gh``, no clock.
"""

import io
import json
import math
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

import importlib.util

SPEC = importlib.util.spec_from_file_location(
    "ci_minutes_under_test", Path(__file__).with_name("ci_minutes.py")
)
ci_minutes = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ci_minutes)

SCRIPT = Path(__file__).with_name("ci_minutes.py")
SHA = "deadbeef"


def _reject_constant(token):
    raise ValueError(f"non-JSON constant in ledger line: {token}")


def strict_rows(ledger):
    """Parse ledger lines as strict JSON (``NaN``/``Infinity`` refused)."""
    return [
        json.loads(line, parse_constant=_reject_constant)
        for line in Path(ledger).read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


def call_record(ledger, pr, minutes):
    with redirect_stdout(io.StringIO()):
        return ci_minutes.record(str(ledger), pr, minutes, SHA)


class _LedgerCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.ledger = Path(self._tmp.name) / "ledger.jsonl"

    def assert_refused(self, pr, minutes):
        """SystemExit with a message, and no ledger file is created."""
        with self.assertRaises(SystemExit) as caught:
            call_record(self.ledger, pr, minutes)
        self.assertTrue(str(caught.exception), "refusal must carry a message")
        self.assertFalse(
            self.ledger.exists(), "a refused call must not create the ledger"
        )


class MinutesRefusalTests(_LedgerCase):
    def test_nan_minutes_refused_in_every_spelling(self):
        for minutes in ("nan", "NaN", "NAN", float("nan"), math.nan):
            with self.subTest(minutes=repr(minutes)):
                self.assert_refused("101", minutes)

    def test_infinite_minutes_refused_in_every_spelling(self):
        for minutes in (
            "inf",
            "+inf",
            "Infinity",
            "1e999",
            float("inf"),
            math.inf,
        ):
            with self.subTest(minutes=repr(minutes)):
                self.assert_refused("101", minutes)

    def test_negative_minutes_refused(self):
        # Positive control for the pre-existing guard: already a clean
        # SystemExit on main, and it must stay one after the fix.
        for minutes in ("-3", "-0.5", "-inf", -1, -0.25, float("-inf")):
            with self.subTest(minutes=repr(minutes)):
                self.assert_refused("101", minutes)

    def test_nonnumeric_minutes_refused(self):
        # Positive control for the pre-existing guard.
        for minutes in ("abc", "", None):
            with self.subTest(minutes=repr(minutes)):
                self.assert_refused("101", minutes)


class PrRefusalTests(_LedgerCase):
    def test_alphabetic_and_decimal_pr_refused(self):
        for pr in ("abc", "12a", "a12", "1.5", "1.0", "1e3"):
            with self.subTest(pr=pr):
                self.assert_refused(pr, "10")

    def test_negative_and_zero_pr_refused(self):
        for pr in ("-1", "-5", "0", "00", -7, 0):
            with self.subTest(pr=repr(pr)):
                self.assert_refused(pr, "10")

    def test_whitespace_pr_refused(self):
        for pr in ("", " ", "   ", " 101", "101 ", " 101 ", "1 01", "\t101", "101\n"):
            with self.subTest(pr=repr(pr)):
                self.assert_refused(pr, "10")


class LedgerIntegrityTests(_LedgerCase):
    def test_refusal_appends_nothing_to_an_existing_ledger(self):
        self.assertEqual(call_record(self.ledger, "101", "42.5"), 0)
        before = self.ledger.read_bytes()
        for pr, minutes in (
            ("101", "nan"),
            ("101", "inf"),
            ("abc", "10"),
            ("0", "10"),
            ("-2", "10"),
            (" 101", "10"),
        ):
            with self.subTest(pr=pr, minutes=minutes):
                with self.assertRaises(SystemExit):
                    call_record(self.ledger, pr, minutes)
                self.assertEqual(self.ledger.read_bytes(), before)

    def test_valid_row_after_refusals_is_the_first_row(self):
        for pr, minutes in (("101", "nan"), ("nope", "10")):
            with self.assertRaises(SystemExit):
                call_record(self.ledger, pr, minutes)
        self.assertEqual(call_record(self.ledger, "7", "3.5"), 0)
        self.assertEqual(
            strict_rows(self.ledger), [{"minutes": 3.5, "pr": 7, "sha": SHA}]
        )


class PositiveControlTests(_LedgerCase):
    def test_valid_rows_append_one_strict_json_line_per_call(self):
        calls = [("101", "42.5"), ("102", "40"), (103, 12.25), ("104", 9)]
        for pr, minutes in calls:
            self.assertEqual(call_record(self.ledger, pr, minutes), 0)
        self.assertEqual(
            strict_rows(self.ledger),
            [
                {"minutes": 42.5, "pr": 101, "sha": SHA},
                {"minutes": 40.0, "pr": 102, "sha": SHA},
                {"minutes": 12.25, "pr": 103, "sha": SHA},
                {"minutes": 9.0, "pr": 104, "sha": SHA},
            ],
        )

    def test_zero_and_scientific_minutes_stay_valid(self):
        self.assertEqual(call_record(self.ledger, "101", "0"), 0)
        self.assertEqual(call_record(self.ledger, "102", "1e2"), 0)
        rows = strict_rows(self.ledger)
        self.assertEqual([row["minutes"] for row in rows], [0.0, 100.0])

    def test_recorded_row_types_are_int_pr_and_float_minutes(self):
        self.assertEqual(call_record(self.ledger, "55", "6"), 0)
        (row,) = strict_rows(self.ledger)
        self.assertIs(type(row["pr"]), int)
        self.assertIs(type(row["minutes"]), float)
        self.assertEqual(row["sha"], SHA)


class CliRefusalTests(_LedgerCase):
    def run_cli(self, pr, minutes):
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "record",
                "--ledger",
                str(self.ledger),
                "--pr",
                pr,
                "--minutes",
                minutes,
                "--sha",
                SHA,
            ],
            capture_output=True,
            text=True,
        )

    def test_cli_refuses_cleanly_without_a_traceback(self):
        for pr, minutes in (("101", "nan"), ("101", "inf"), ("abc", "10"), ("0", "10")):
            with self.subTest(pr=pr, minutes=minutes):
                run = self.run_cli(pr, minutes)
                self.assertNotEqual(run.returncode, 0, run.stdout)
                self.assertNotIn("Traceback", run.stderr)
                self.assertFalse(self.ledger.exists())

    def test_cli_valid_row_still_records(self):
        run = self.run_cli("101", "42.5")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(
            strict_rows(self.ledger),
            [{"minutes": 42.5, "pr": 101, "sha": SHA}],
        )


if __name__ == "__main__":
    unittest.main()
