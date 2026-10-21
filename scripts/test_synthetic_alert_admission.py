"""Supplemental offline admission and JSON-mode boundary checks."""

import copy
import io
import json
import os
import unittest
from contextlib import redirect_stderr, redirect_stdout
from unittest import mock
from urllib.parse import parse_qs, urlparse

import synthetic_alert as alert
import synthetic_alert_history as history


class AdmissionBoundaryTests(unittest.TestCase):
    def setUp(self):
        for obj, name in ((alert, "post"), (alert.urllib.request, "build_opener")):
            guard = mock.patch.object(obj, name, side_effect=AssertionError("network forbidden"))
            guard.start()
            self.addCleanup(guard.stop)

    def test_filtered_full_pages_still_obey_reconstruction_budget(self):
        calls = []

        def get(path, _token):
            self.assertNotIn("/jobs?", path)
            calls.append(path)
            # Stop a broken implementation without hanging the offline test.
            self.assertLessEqual(len(calls), 4, "filtered rows escaped the scan budget")
            page = int(parse_qs(urlparse(path).query)["page"][0])
            return {"total_count": 100, "workflow_runs": [
                {"id": 100 - page * 2 - i, "run_number": 100 - page * 2 - i,
                 "event": "workflow_dispatch"} for i in range(2)]}

        with mock.patch.object(history, "MAX_HISTORY_RUNS", 5), \
                mock.patch.object(history, "RUNS_PAGE_SIZE", 2), \
                mock.patch.object(history, "_get", side_effect=get):
            with self.assertRaises(history.HistoryError):
                history.previous_probe_outcomes(
                    {"GITHUB_TOKEN": "fixture", "GITHUB_REPOSITORY": "owner/repo",
                     "GITHUB_RUN_ID": "999"}, "staging")
        self.assertLessEqual(len(calls), 4)

    def test_optional_fields_are_validated_when_present(self):
        run = {"id": 100, "run_number": 1, "event": "schedule"}
        job = {"name": history.JOB_NAMES["staging"], "steps": [
            {"name": history.PROBE_STEPS["staging"], "conclusion": "success"}]}
        for target, field, bad in (
                ("run", "status", None), ("run", "conclusion", "unsupported"),
                ("job", "status", "in_progress"), ("job", "id", True),
                ("step", "number", 1.0), ("step", "status", [])):
            with self.subTest(target=target, field=field):
                row, item = copy.deepcopy(run), copy.deepcopy(job)
                {"run": row, "job": item, "step": item["steps"][0]}[target][field] = bad
                with mock.patch.object(history, "_get", return_value={"jobs": [item]}):
                    with self.assertRaises(history.HistoryError):
                        history._run_outcome("owner/repo", row, "staging", "fixture")

    def test_json_has_one_stdout_record_even_without_github_output(self):
        for run_id in ("123", "REMOTE_SENTINEL" * 500, "\n123", "１２３"):
            out, err = io.StringIO(), io.StringIO()
            with self.subTest(run_id_length=len(run_id)), \
                    mock.patch.dict(os.environ, {"PROBE_OUTCOME": "skipped",
                                                 "GITHUB_RUN_ID": run_id}, clear=True), \
                    mock.patch.object(alert, "previous_probe_outcomes",
                                      side_effect=AssertionError("no sample cannot read history")), \
                    redirect_stdout(out), redirect_stderr(err):
                result = alert.main(["--env", "staging", "--evidence", "absent",
                                     "--decide-only", "--json"])
            self.assertEqual(result, 1)
            receipt = json.loads(out.getvalue())
            self.assertEqual(receipt["run_id"], "123" if run_id == "123" else "unknown")
            self.assertEqual(receipt["code"], "no_probe_sample")
            self.assertIsNone(receipt["event"])
            self.assertEqual(len(out.getvalue().splitlines()), 1)
            self.assertLess(len(out.getvalue()), 512)
            self.assertIn("event=\n", err.getvalue())
            self.assertNotIn("REMOTE_SENTINEL", out.getvalue() + err.getvalue())

    def test_json_cannot_enable_post_or_dry_run(self):
        for flags in ([], ["--post-only"], ["--dry-run"], ["--decide-only", "--dry-run"]):
            with self.subTest(flags=flags), redirect_stderr(io.StringIO()), \
                    mock.patch.object(alert, "previous_probe_outcomes",
                                      side_effect=AssertionError("invalid modes cannot read history")):
                self.assertEqual(alert.main(["--env", "staging", "--evidence", "absent",
                                             "--json", *flags]), 2)


if __name__ == "__main__":
    unittest.main()
