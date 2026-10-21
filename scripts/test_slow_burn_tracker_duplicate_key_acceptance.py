"""Protected offline duplicate-key refusal for the slow-burn tracker intake.

Failing-first acceptance for `slow_burn_tracker._intake_json`: bare
`json.loads` parses duplicate object keys last-wins, so a corrupted driver
verdict can flip the firing decision and corrupted issue rows can misroute
the file/refresh/close actuation. Refused input exits 1 with the existing
fixed vocabulary and never reconciles. Offline only: gh and node are
mocked, no network, no credentials.
"""

import contextlib
import importlib
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
tracker = importlib.import_module("slow_burn_tracker")

NOW = 1_700_000_000
SENTINEL = "hostile-dupkey-tracker-sentinel"
TITLE = "[voice-join-slow-burn] fixture"
EXPECTED_DRIVER = "burn evaluation failed: driver answer is not bounded JSON"
EXPECTED_ISSUES = "issue reconcile failed: open issues are unreadable"
VERDICT = {"slow": True, "fast": False, "held": False,
           "slowCovered": True, "ring": []}
ENV = {"GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next",
       "GITHUB_SERVER_URL": "https://github.com", "GITHUB_RUN_ID": "1",
       "GH_TOKEN": "synthetic-fixture-only"}


def duplicate_verdicts():
    base = '"fast": false, "held": false, "slowCovered": true, "ring": []'
    conflicting = '{"slow": true, ' + base + ', "slow": false}'
    reversed_order = '{"slow": false, ' + base + ', "slow": true}'
    held_conflict = '{"slow": true, "fast": false, "held": false, "held": true, "slowCovered": true, "ring": []}'
    identical = '{"slow": true, ' + base + ', "slow": true}'
    nested = ('{"slow": true, "fast": false, "held": false, "slowCovered": true,'
              ' "ring": [{"t": 1699999700, "t": 1699999400, "received": 25,'
              ' "duplicate": 0, "fastMoves": 24}]}')
    hostile = ('{"slow": true, ' + base + f', "{SENTINEL}": "{SENTINEL}",'
               f' "{SENTINEL}": "{SENTINEL}"}}')
    return {"conflicting": conflicting, "reversed": reversed_order,
            "held": held_conflict, "identical": identical,
            "nested": nested, "hostile": hostile}


def duplicate_rows():
    title = json.dumps(TITLE)
    conflicting = ('[{"number": 7, "title": ' + title + ', "body": "seven",'
                   ' "number": 8}]')
    reversed_order = ('[{"number": 8, "title": ' + title + ', "body": "eight",'
                      ' "number": 7}]')
    identical = ('[{"number": 7, "title": ' + title + ', "body": "same",'
                 ' "body": "same"}]')
    nested = ('[{"number": 7, "title": ' + title + ', "body": "nested",'
              ' "meta": {"a": 1, "a": 2}}]')
    hostile = ('[{"number": 7, "title": ' + json.dumps(SENTINEL) + ','
               f' "body": "{SENTINEL}", "title": {title}}}]')
    return {"conflicting": conflicting, "reversed": reversed_order,
            "identical": identical, "nested": nested, "hostile": hostile}


class TrackerDuplicateKeyAcceptanceTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
            or os.environ.get("PAPERCLIP_SCRATCH_DIR"))
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        previous = Path.cwd()
        os.chdir(self.dir)
        self.addCleanup(os.chdir, previous)
        self.no_network = mock.patch.object(
            tracker.urllib.request, "build_opener",
            side_effect=AssertionError("offline acceptance must not scrape"))
        self.no_network.start()
        self.addCleanup(self.no_network.stop)
        self.no_process = mock.patch.object(
            tracker.subprocess, "run",
            side_effect=AssertionError("offline acceptance must not execute gh/node"))
        self.no_process.start()
        self.addCleanup(self.no_process.stop)

    def expect_track_error(self, call, expected):
        try:
            result = call()
        except tracker.TrackError as error:
            message = str(error)
            self.assertEqual(message, expected)
            self.assertLess(len(message), 160)
            self.assertNotIn(SENTINEL, message)
            return
        except Exception as error:
            self.fail(f"expected fixed-vocabulary TrackError, got {type(error).__name__}")
        self.fail(f"duplicate keys must raise TrackError, not parse last-wins: {result!r}")

    def driver_answer(self, raw):
        completed = mock.Mock(returncode=0, stdout=raw, stderr="")
        with mock.patch.object(tracker.subprocess, "run", return_value=completed):
            return tracker.run_driver("node", "driver.mjs", "expo.prom", "ring.json", NOW)

    def issue_answer(self, raw):
        with mock.patch.object(tracker, "gh", return_value=raw):
            return tracker.open_tracking_issues("TogetherWeOwn/two-bot-next", "fixture",
                                                "slow-burn", "[voice-join-slow-burn]")

    def main_with_answer(self, boundary, raw, slow=True):
        output = io.StringIO()
        evidence = self.dir / "receipt.json"
        evidence.unlink(missing_ok=True)
        with contextlib.ExitStack() as stack:
            stack.enter_context(contextlib.redirect_stdout(output))
            stack.enter_context(contextlib.redirect_stderr(output))
            if boundary == "driver":
                completed = mock.Mock(returncode=0, stdout=raw, stderr="")
                stack.enter_context(mock.patch.object(tracker.subprocess, "run",
                                                      return_value=completed))
                stack.enter_context(mock.patch.object(
                    tracker, "reconcile",
                    side_effect=AssertionError("bad driver output must never reconcile")))
            else:
                stack.enter_context(mock.patch.object(
                    tracker, "run_driver",
                    return_value=dict(VERDICT, slow=slow)))

                def list_only(args, *unused):
                    self.assertEqual(args[0], "list", "bad issue rows must never produce a write")
                    return raw

                stack.enter_context(mock.patch.object(tracker, "gh", side_effect=list_only))
            try:
                code = tracker.main([
                    "--fixture", "fire" if slow else "clear", "--evidence-out", str(evidence),
                    "--exposition-out", str(self.dir / "expo.prom"),
                    "--ring", str(self.dir / "ring.json"),
                    "--coverage-path", str(self.dir / "coverage.json")], ENV)
            except Exception as error:
                self.fail(f"main must write a receipt and exit 1, not escape {type(error).__name__}")
        self.assertEqual(code, 1)
        self.assertTrue(evidence.is_file(), "a refused input still needs an evidence receipt")
        receipt = json.loads(evidence.read_text(encoding="utf-8"))
        self.assertEqual(receipt["action"], "none")
        self.assertIsNone(receipt["issue"])
        if boundary == "driver":
            self.assertEqual(receipt["verdict"], "unknown")
        self.assertIn("::error::", output.getvalue())
        self.assertNotIn("Traceback", output.getvalue())
        self.assertNotIn(SENTINEL, output.getvalue())
        return output.getvalue()

    def test_driver_refuses_conflicting_verdict_keys_both_orders(self):
        verdicts = duplicate_verdicts()
        for family in ("conflicting", "reversed", "held"):
            with self.subTest(family=family):
                self.expect_track_error(lambda: self.driver_answer(verdicts[family]),
                                        EXPECTED_DRIVER)

    def test_driver_refuses_identical_duplicate_values(self):
        self.expect_track_error(
            lambda: self.driver_answer(duplicate_verdicts()["identical"]), EXPECTED_DRIVER)

    def test_driver_refuses_nested_duplicate_keys(self):
        self.expect_track_error(
            lambda: self.driver_answer(duplicate_verdicts()["nested"]), EXPECTED_DRIVER)

    def test_driver_refusal_echoes_no_hostile_bytes(self):
        self.expect_track_error(
            lambda: self.driver_answer(duplicate_verdicts()["hostile"]), EXPECTED_DRIVER)

    def test_driver_valid_verdict_stays_byte_identical(self):
        raw = json.dumps(VERDICT)
        self.assertEqual(self.driver_answer(raw), VERDICT)
        self.assertEqual(self.driver_answer(raw), json.loads(raw))

    def test_issue_rows_refuse_conflicting_keys_both_orders(self):
        rows = duplicate_rows()
        for family in ("conflicting", "reversed"):
            with self.subTest(family=family):
                self.expect_track_error(lambda: self.issue_answer(rows[family]),
                                        EXPECTED_ISSUES)

    def test_issue_rows_refuse_identical_duplicate_values(self):
        self.expect_track_error(
            lambda: self.issue_answer(duplicate_rows()["identical"]), EXPECTED_ISSUES)

    def test_issue_rows_refuse_nested_duplicate_keys(self):
        self.expect_track_error(
            lambda: self.issue_answer(duplicate_rows()["nested"]), EXPECTED_ISSUES)

    def test_issue_rows_refusal_echoes_no_hostile_bytes(self):
        self.expect_track_error(
            lambda: self.issue_answer(duplicate_rows()["hostile"]), EXPECTED_ISSUES)

    def test_issue_rows_valid_stay_byte_identical(self):
        rows = [{"number": 7, "title": TITLE, "body": "fixture"}]
        raw = json.dumps(rows)
        self.assertEqual(self.issue_answer(raw), rows)
        self.assertEqual(self.issue_answer(raw), json.loads(raw))

    def test_main_refuses_duplicate_driver_answers(self):
        for family, raw in duplicate_verdicts().items():
            with self.subTest(family=family):
                output = self.main_with_answer("driver", raw)
                self.assertEqual(output.strip(), "::error::" + EXPECTED_DRIVER)

    def test_main_refuses_duplicate_issue_rows(self):
        for family, raw in duplicate_rows().items():
            for slow in (True, False):
                with self.subTest(family=family, slow=slow):
                    output = self.main_with_answer("gh", raw, slow=slow)
                    self.assertEqual(output.strip(), "::error::" + EXPECTED_ISSUES)


if __name__ == "__main__":
    unittest.main()
