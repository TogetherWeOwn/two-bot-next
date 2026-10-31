"""Offline regressions for the deterministic JUnit-to-flake-ledger parser.

Hermetic fixtures only: no cargo, no nextest, no network, no clock. Each
case writes a small JUnit document to a temp dir and asserts the exact
ledger rows, so a schema or rule change fails here before it can turn an
advisory CI ledger into silent greens.

Fixtures mirror what cargo-nextest 0.9.99 actually emits with
`--profile ci` (retries = 1): ONE <testcase> per test, with failed attempts
as child elements. A fail-then-pass test carries a flakyFailure child; a
test that fails every retry carries its terminal failure plus a
rerunFailure child. Repeated <testcase> elements (which nextest never
emits) are covered only by the merged-input test below.
"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/ci/flake_ledger.py"

PASS_SUITE = """<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="unit" tests="1" failures="0" errors="0" skipped="0">
    <testcase classname="ledger" name="steady_pass" time="0.012"/>
  </testsuite>
</testsuites>
"""

# nextest 0.9.99 shape: one <testcase> per test; the failed first attempt is a
# flakyFailure child carrying its own time. The testcase time is the final
# (passing) attempt, so the ledger must sum both.
FLAKY_SUITE = """<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="unit" tests="2" failures="0" errors="0" skipped="1">
    <testcase classname="ledger" name="flaky_retry" time="0.005">
      <flakyFailure message="first attempt failed" type="assert" time="0.020">boom</flakyFailure>
    </testcase>
    <testcase classname="ledger" name="skipped_case" time="0">
      <skipped/>
    </testcase>
  </testsuite>
</testsuites>
"""

# nextest 0.9.99 shape: the terminal failure plus one rerunFailure child per
# exhausted retry. This is a deterministic failure, not a flake: the test
# failed on every attempt, so flaky stays false while attempts records the
# spent retries.
STUCK_FAIL_SUITE = """<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="integration" tests="1" failures="1" errors="0" skipped="0">
  <testcase classname="db" name="still_red" time="0.110">
    <failure message="final attempt">boom</failure>
    <rerunFailure message="attempt 1" type="assert" time="0.100">boom</rerunFailure>
  </testcase>
</testsuite>
"""

# Non-nextest fallback: merged JUnit may repeat a <testcase> for one test.
# Grouping still counts each element as an attempt; disagreeing outcomes
# mean the retry changed the result, so the test is flaky.
MERGED_SUITE = """<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="unit" tests="2" failures="1" errors="0" skipped="0">
    <testcase classname="ledger" name="merged_retry" time="0.020">
      <failure message="first attempt">boom</failure>
    </testcase>
    <testcase classname="ledger" name="merged_retry" time="0.005"/>
  </testsuite>
</testsuites>
"""


# Non-finite / overflowing durations: a corrupted or non-nextest JUnit
# producer can emit time values float() parses but int() cannot convert
# (inf, or finite seconds whose millisecond scaling overflows to inf).
# Those must contribute 0 ms, not abort the whole ledger run.
BAD_TIMES = ["NaN", "inf", "-inf", "1e308", "-1e308", "1e999"]


def nonfinite_suite(bad):
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="unit" tests="3" failures="0" errors="0" skipped="0">
    <testcase classname="ledger" name="bad_case_time" time="{bad}">
      <flakyFailure message="first attempt failed" type="assert" time="0.020">boom</flakyFailure>
    </testcase>
    <testcase classname="ledger" name="bad_retry_time" time="0.005">
      <flakyFailure message="first attempt failed" type="assert" time="{bad}">boom</flakyFailure>
    </testcase>
    <testcase classname="ledger" name="ordinary_truncation" time="0.0129"/>
  </testsuite>
</testsuites>
"""


def run_ledger(junit_text):
    with tempfile.TemporaryDirectory() as tmp:
        junit = Path(tmp) / "junit.xml"
        output = Path(tmp) / "ledger.jsonl"
        junit.write_text(junit_text, encoding="utf-8")
        run = subprocess.run(
            [sys.executable, str(SCRIPT), "--junit", str(junit), "--output", str(output)],
            capture_output=True,
            text=True,
        )
        rows = []
        if output.exists():
            rows = [json.loads(line) for line in output.read_text(encoding="utf-8").splitlines()]
        return run.returncode, rows


class FlakeLedgerTests(unittest.TestCase):
    def test_passing_test_is_not_flaky(self):
        code, rows = run_ledger(PASS_SUITE)
        self.assertEqual(code, 0)
        self.assertEqual(
            rows,
            [
                {
                    "suite": "unit",
                    "classname": "ledger",
                    "name": "steady_pass",
                    "status": "pass",
                    "flaky": False,
                    "attempts": 1,
                    "time_ms": 12,
                }
            ],
        )

    def test_fail_then_pass_retry_is_flaky_pass(self):
        code, rows = run_ledger(FLAKY_SUITE)
        self.assertEqual(code, 0)
        by_name = {row["name"]: row for row in rows}
        self.assertEqual(
            by_name["flaky_retry"],
            {
                "suite": "unit",
                "classname": "ledger",
                "name": "flaky_retry",
                "status": "pass",
                "flaky": True,
                "attempts": 2,
                "time_ms": 25,
            },
        )
        self.assertEqual(by_name["skipped_case"]["status"], "skipped")
        self.assertFalse(by_name["skipped_case"]["flaky"])
        # Deterministic order: sorted by (suite, classname, name).
        self.assertEqual([row["name"] for row in rows], ["flaky_retry", "skipped_case"])

    def test_fail_on_every_retry_stays_failed_not_flaky(self):
        code, rows = run_ledger(STUCK_FAIL_SUITE)
        self.assertEqual(code, 0)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["status"], "fail")
        self.assertFalse(rows[0]["flaky"])
        self.assertEqual(rows[0]["attempts"], 2)
        # Retry time lives on the rerunFailure child; dropping it would
        # undercount the test's cost (0.110 final + 0.100 retry).
        self.assertEqual(rows[0]["time_ms"], 210)

    def test_merged_repeated_testcases_stay_supported(self):
        code, rows = run_ledger(MERGED_SUITE)
        self.assertEqual(code, 0)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["status"], "pass")
        self.assertTrue(rows[0]["flaky"])
        self.assertEqual(rows[0]["attempts"], 2)
        self.assertEqual(rows[0]["time_ms"], 25)

    def test_nonfinite_and_overflowing_times_contribute_zero(self):
        for bad in BAD_TIMES:
            with self.subTest(bad=bad):
                code, rows = run_ledger(nonfinite_suite(bad))
                self.assertEqual(code, 0)
                by_name = {row["name"]: row for row in rows}
                # Bad testcase time contributes 0; the finite retry time
                # on the flakyFailure child still counts.
                self.assertEqual(
                    by_name["bad_case_time"],
                    {
                        "suite": "unit",
                        "classname": "ledger",
                        "name": "bad_case_time",
                        "status": "pass",
                        "flaky": True,
                        "attempts": 2,
                        "time_ms": 20,
                    },
                )
                # Bad retry-child time contributes 0; the finite testcase
                # time still counts.
                self.assertEqual(by_name["bad_retry_time"]["time_ms"], 5)
                self.assertEqual(by_name["bad_retry_time"]["attempts"], 2)
                self.assertTrue(by_name["bad_retry_time"]["flaky"])
                # Ordinary finite values keep truncating to whole ms.
                self.assertEqual(by_name["ordinary_truncation"]["time_ms"], 12)

    def test_identical_input_bytes_give_identical_output_bytes(self):
        first = run_ledger(FLAKY_SUITE)
        second = run_ledger(FLAKY_SUITE)
        self.assertEqual(first, second)

    def test_missing_junit_file_exits_nonzero_without_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "ledger.jsonl"
            run = subprocess.run(
                [sys.executable, str(SCRIPT), "--junit", str(Path(tmp) / "absent.xml"),
                 "--output", str(output)],
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(run.returncode, 0)
            self.assertFalse(output.exists())

    def test_malformed_xml_exits_nonzero(self):
        code, _ = run_ledger("<testsuites><testsuite>")
        self.assertNotEqual(code, 0)


if __name__ == "__main__":
    unittest.main()
