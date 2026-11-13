"""Controls for the inert producing-attempt verifier."""
import importlib
import io
import os
import sys
import unittest
from contextlib import redirect_stderr, redirect_stdout
from datetime import datetime, timezone
from pathlib import Path
from unittest import mock
from urllib.parse import parse_qs, urlsplit

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import incident_issue
import production_hold_attempts as attempts
import production_hold_history as reader
from production_hold_import_probe import assert_inert_import
from test_production_hold_history import ENV, REPO, FakeGitHub, paged

RUN = "actions/runs/7"
START = "2026-01-01T00:00:00Z"
END = "2026-01-01T01:00:00Z"
MID = "2026-01-01T00:30:00Z"
JOB = "deploy"


def run_row(attempt=1):
    return {"head_branch": "main", "path": attempts.WORKFLOW + "@refs/heads/main",
            "repository": {"full_name": REPO}, "head_repository": {"full_name": REPO},
            "run_attempt": attempt}


def job(**changes):
    row = {"name": JOB, "status": "completed", "conclusion": "success",
           "started_at": START, "completed_at": END}
    row.update(changes)
    return row


class AttemptTests(unittest.TestCase):
    def setUp(self):
        self.gh = FakeGitHub()
        self.calls = []
        self.gh.routes[RUN] = lambda query: run_row()
        self.gh.routes[RUN + "/attempts/1/jobs"] = paged([job()], key="jobs")
        self.history = reader.History(ENV)
        self.patch = mock.patch.object(incident_issue, "_request", side_effect=self.request)
        self.patch.start()
        self.addCleanup(self.patch.stop)

    def request(self, method, path, token, body=None):
        self.calls.append(path.removeprefix(f"/repos/{REPO}/"))
        return self.gh.request(method, path, token, body)

    def verify(self, recorded=MID):
        return attempts.verified_attempt(self.history, 7, recorded, JOB)

    def test_success_and_inclusive_window(self):
        expected = (1, datetime(2026, 1, 1, 1, tzinfo=timezone.utc))
        for stamp in (START, MID, END):
            with self.subTest(stamp=stamp):
                self.assertEqual(self.verify(stamp), expected)
        for stamp in ("2025-12-31T23:59:59Z", "2026-01-01T01:00:01Z"):
            with self.subTest(stamp=stamp):
                self.assertIsNone(self.verify(stamp))

    def test_provenance_and_attempt_rejection_precedes_jobs(self):
        cases = [("head_branch", "other"), ("path", "other"),
                 ("repository", {"full_name": "other"}),
                 ("head_repository", {"full_name": "other"})]
        for field, bad in cases:
            with self.subTest(field=field):
                self.calls.clear()
                row = run_row(); row[field] = bad
                self.gh.routes[RUN] = lambda query, row=row: row
                with self.assertRaisesRegex(incident_issue.IncidentError, "run provenance is malformed"):
                    self.verify("bad-time")
                self.assertEqual(self.calls, [RUN])
                self.gh.routes[RUN] = lambda query: run_row()
                self.assertEqual(self.verify()[0], 1)
        for bad in (None, 0, -1, True, "1"):
            with self.subTest(attempt=bad):
                self.calls.clear()
                self.gh.routes[RUN] = lambda query, bad=bad: run_row(bad)
                with self.assertRaisesRegex(incident_issue.IncidentError, "run attempt is malformed"):
                    self.verify("bad-time")
                self.assertEqual(self.calls, [RUN])
                self.gh.routes[RUN] = lambda query: run_row()
                self.assertEqual(self.verify()[0], 1)

    def test_cap_and_recorded_time_validation_order(self):
        self.gh.routes[RUN] = lambda query: run_row(51)
        with self.assertRaisesRegex(incident_issue.IncidentError, "production hold timestamp is unreadable"):
            self.verify("not-a-time")
        self.assertEqual(self.calls, [RUN])
        self.calls.clear()
        with self.assertRaisesRegex(incident_issue.IncidentError, "attempt history exceeds the cap"):
            self.verify()
        self.assertEqual(self.calls, [RUN])

    def test_missing_duplicate_and_non_successful_jobs(self):
        for jobs in ([], [job(), job()], [job(conclusion="skipped")],
                     [job(status="in_progress")], [job(conclusion="failure")],
                     [job(name="other")]):
            with self.subTest(jobs=jobs):
                self.gh.routes[RUN + "/attempts/1/jobs"] = paged(jobs, key="jobs")
                self.assertIsNone(self.verify())
                self.gh.routes[RUN + "/attempts/1/jobs"] = paged([job()], key="jobs")
                self.assertEqual(self.verify()[0], 1)

    def test_skipped_rerun_does_not_hide_earlier_producer(self):
        self.gh.routes[RUN] = lambda query: run_row(2)
        self.gh.routes[RUN + "/attempts/2/jobs"] = paged([job(conclusion="skipped")], key="jobs")
        self.assertEqual(self.verify()[0], 1)
        self.assertEqual([p.split("?")[0] for p in self.calls],
                         [RUN, RUN + "/attempts/2/jobs", RUN + "/attempts/1/jobs"])

    def test_later_success_cannot_certify_earlier_failed_status(self):
        self.gh.routes[RUN] = lambda query: run_row(2)
        self.gh.routes[RUN + "/attempts/2/jobs"] = paged([job(started_at=END,
            completed_at="2026-01-01T02:00:00Z")], key="jobs")
        self.gh.routes[RUN + "/attempts/1/jobs"] = paged([job(conclusion="failure")], key="jobs")
        self.assertIsNone(self.verify())

    def test_later_malformed_jobs_page_and_page_cap(self):
        route = RUN + "/attempts/1/jobs"
        for cap in (False, True):
            with self.subTest(cap=cap):
                self.calls.clear()
                self.gh.routes[route] = lambda q: ({"jobs": [job()] * 100} if cap or q["page"] == "1" else {"jobs": None})
                message = "page cap" if cap else "history is malformed"
                with self.assertRaisesRegex(incident_issue.IncidentError, message):
                    self.verify()
                self.assertEqual([int(parse_qs(urlsplit(p).query)["page"][0]) for p in self.calls[1:]],
                                 list(range(1, 11 if cap else 3)))

    def test_shared_budget_and_dynamic_get(self):
        self.gh.routes["prior"] = lambda query: []
        for count, success in ((98, True), (99, False)):
            with self.subTest(count=count):
                self.history = reader.History(ENV); self.calls.clear()
                for _ in range(count):
                    self.history.get("prior")
                with mock.patch.object(reader.History, "get", autospec=True,
                        side_effect=lambda instance, path, **kw: original(instance, path, **kw)) as patched:
                    if success:
                        self.assertEqual(self.verify()[0], 1)
                    else:
                        with self.assertRaisesRegex(incident_issue.IncidentError, "request cap"):
                            self.verify()
                    self.assertEqual(patched.call_count, 2)
                self.assertEqual(self.history.requests, 100 if success else 101)
                self.assertEqual(len(self.calls), 100)
                expected = [RUN, RUN + "/attempts/1/jobs"] if success else [RUN]
                self.assertEqual([p.split("?")[0] for p in self.calls[count:]], expected)

    def test_import_does_not_access_context_or_transport(self):
        output = io.StringIO()
        with mock.patch.dict(os.environ, {}, clear=True), \
             mock.patch.object(incident_issue, "_context", side_effect=AssertionError), \
             mock.patch.object(incident_issue, "_request", side_effect=AssertionError), \
             redirect_stdout(output), redirect_stderr(output):
            importlib.reload(attempts)
        self.assertEqual(output.getvalue(), "")

    def test_malformed_provenance_type_behavior_and_valid_pair(self):
        for field, value, error, text in (
                ("path", 1, AttributeError, "split"),
                ("repository", [1], AttributeError, "get"),
                ("head_repository", "bad", AttributeError, "get"),
                ("path", None, AttributeError, "split"),
                ("repository", [], incident_issue.IncidentError, "run provenance is malformed"),
                ("head_repository", None, incident_issue.IncidentError, "run provenance is malformed")):
            with self.subTest(field=field, value=value):
                self.calls.clear()
                row = run_row(); row[field] = value
                self.gh.routes[RUN] = lambda query, row=row: row
                with self.assertRaisesRegex(error, text):
                    self.verify()
                self.assertEqual(self.calls, [RUN])
                self.gh.routes[RUN] = lambda query: run_row()
                self.assertEqual(self.verify()[0], 1)

    def test_timestamp_failures_are_specific_and_precede_jobs(self):
        for raw in (None, 1, "bad", "2026-01-01T00:00:00", "99999-01-01T00:00:00Z"):
            with self.subTest(raw=raw):
                self.calls.clear()
                with self.assertRaisesRegex(incident_issue.IncidentError,
                                            "production hold timestamp is unreadable"):
                    self.verify(raw)
                self.assertEqual(self.calls, [RUN])
                self.assertEqual(self.verify()[0], 1)
        for field in ("started_at", "completed_at"):
            for raw in (None, 1, "bad", "2026-01-01T00:00:00"):
                with self.subTest(field=field, raw=raw):
                    self.calls.clear()
                    self.gh.routes[RUN + "/attempts/1/jobs"] = paged([job(**{field: raw})], key="jobs")
                    with self.assertRaisesRegex(incident_issue.IncidentError,
                                                "production hold timestamp is unreadable"):
                        self.verify()
                    self.assertEqual([p.split("?")[0] for p in self.calls],
                                     [RUN, RUN + "/attempts/1/jobs"])
                    self.gh.routes[RUN + "/attempts/1/jobs"] = paged([job()], key="jobs")
                    self.assertEqual(self.verify()[0], 1)

    def test_attempt_50_is_allowed_and_walks_to_first(self):
        self.gh.routes[RUN] = lambda query: run_row(50)
        for number in range(2, 51):
            self.gh.routes[RUN + f"/attempts/{number}/jobs"] = paged([], key="jobs")
        self.assertEqual(self.verify()[0], 1)
        self.assertEqual(self.history.requests, 51)
        self.assertEqual([p.split("?")[0] for p in self.calls],
                         [RUN] + [RUN + f"/attempts/{number}/jobs"
                                  for number in range(50, 0, -1)])

    def test_missing_fields_and_non_object_run(self):
        for field in ("head_branch", "path", "repository", "head_repository", "run_attempt"):
            with self.subTest(field=field):
                row = run_row()
                del row[field]
                self.gh.routes[RUN] = lambda query, row=row: row
                self.calls.clear()
                message = ("run attempt is malformed" if field == "run_attempt"
                           else "run provenance is malformed")
                with self.assertRaisesRegex(incident_issue.IncidentError, message):
                    self.verify()
                self.assertEqual(self.calls, [RUN])
                self.gh.routes[RUN] = lambda query: run_row()
                self.assertEqual(self.verify()[0], 1)
        for row in (None, [], "bad", 1):
            with self.subTest(row=row):
                self.gh.routes[RUN] = lambda query, row=row: row
                self.calls.clear()
                with self.assertRaisesRegex(incident_issue.IncidentError,
                                            "run provenance is malformed"):
                    self.verify()
                self.assertEqual(self.calls, [RUN])
                self.gh.routes[RUN] = lambda query: run_row()
                self.assertEqual(self.verify()[0], 1)

    def test_two_page_jobs_and_malformed_later_evidence(self):
        route = RUN + "/attempts/1/jobs"
        first = [job()] + [job(name="other")] * 99
        for second in ({"jobs": []}, {"jobs": [None]}, {"jobs": None}, [], {}):
            with self.subTest(second=second):
                self.history = reader.History(ENV)
                self.calls.clear()
                self.gh.routes[route] = lambda q, second=second: (
                    {"jobs": first} if q["page"] == "1" else second)
                if second == {"jobs": []}:
                    self.assertEqual(self.verify()[0], 1)
                else:
                    with self.assertRaisesRegex(incident_issue.IncidentError,
                                                "production hold history is malformed"):
                        self.verify()
                self.assertEqual([int(parse_qs(urlsplit(p).query)["page"][0])
                                  for p in self.calls[1:]], [1, 2])
                self.assertEqual(self.calls[0], RUN)
                self.assertEqual([p.split("?")[0] for p in self.calls[1:]],
                                 [route, route])
                self.gh.routes[route] = paged([job()], key="jobs")
                self.assertEqual(self.verify()[0], 1)

    def test_malformed_first_jobs_page_stops_before_page_two(self):
        route = RUN + "/attempts/1/jobs"
        for malformed in (None, [], "bad"):
            with self.subTest(malformed=malformed):
                first = [job(), malformed] + [job(name="other")] * 98
                self.history = reader.History(ENV)
                self.calls.clear()
                self.gh.routes[route] = lambda q, first=first: (
                    {"jobs": first} if q["page"] == "1" else {"jobs": []})
                with self.assertRaisesRegex(incident_issue.IncidentError,
                                            "production hold history is malformed"):
                    self.verify()
                self.assertEqual([p.split("?")[0] for p in self.calls], [RUN, route])
                self.assertEqual(parse_qs(urlsplit(self.calls[1]).query)["page"], ["1"])

                first[1] = job(name="other")
                self.history = reader.History(ENV)
                self.calls.clear()
                self.assertEqual(self.verify()[0], 1)
                self.assertEqual([p.split("?")[0] for p in self.calls], [RUN, route, route])
                self.assertEqual([parse_qs(urlsplit(p).query)["page"][0]
                                  for p in self.calls[1:]], ["1", "2"])

    def test_missing_job_timestamp_keys_fail_after_job_read(self):
        route = RUN + "/attempts/1/jobs"
        for field in ("started_at", "completed_at"):
            with self.subTest(field=field):
                row = job()
                del row[field]
                self.calls.clear()
                self.gh.routes[route] = paged([row], key="jobs")
                with self.assertRaisesRegex(incident_issue.IncidentError,
                                            "production hold timestamp is unreadable"):
                    self.verify()
                self.assertEqual([p.split("?")[0] for p in self.calls], [RUN, route])
                self.gh.routes[route] = paged([job()], key="jobs")
                self.assertEqual(self.verify()[0], 1)

    def test_fresh_process_import_has_no_side_effects(self):
        assert_inert_import(
            self, 'production_hold_attempts',
            ['production_hold_attempts', 'production_hold_receipts', 'incident_issue'],
            ['incident_issue', 'production_hold_receipts'],
        )


original = reader.History.get
