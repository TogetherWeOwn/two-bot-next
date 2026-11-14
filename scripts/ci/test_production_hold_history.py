"""Transport tests extracted from production hold reporter PR #2023."""

import importlib
import io
import os
import sys
import unittest
import urllib.parse
from contextlib import redirect_stderr, redirect_stdout
from datetime import datetime, timezone
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import incident_issue
import production_hold_diagnostic as diagnostic
import production_hold_history as hold
import production_hold_receipts as receipts
import production_hold_timing as timing
from test_production_hold_import_probe import assert_inert_import

REPO = "TogetherWeOwn/two-bot-next"
ENV = {"GITHUB_TOKEN": "fixture-token", "GITHUB_REPOSITORY": REPO}


def paged(rows, key=None):
    def serve(query):
        size, page = int(query["per_page"]), int(query["page"])
        chunk = rows[(page - 1) * size:page * size]
        return {key: chunk} if key else chunk
    return serve


class FakeGitHub:
    def __init__(self):
        self.routes = {}

    def request(self, method, path, token, body=None):
        assert method == "GET" and token == ENV["GITHUB_TOKEN"] and body is None
        url = urllib.parse.urlsplit(path)
        prefix = f"/repos/{REPO}/"
        assert url.path.startswith(prefix)
        query = dict(urllib.parse.parse_qsl(url.query))
        return self.routes[url.path[len(prefix):]](query)


def history(gh):
    return hold.History(ENV)


class EvidenceBoundTests(unittest.TestCase):
    def run_history(self, gh):
        with mock.patch.object(incident_issue, "_request", gh.request):
            return list(history(gh).pages("deployments", environment="production"))

    def test_malformed_pages_are_rejected(self):
        for payload in (None, {"deployments": []}, "text", [1], [[]], [None]):
            gh = FakeGitHub()
            gh.routes["deployments"] = lambda query, value=payload: value
            with self.subTest(payload=payload), self.assertRaises(incident_issue.IncidentError):
                self.run_history(gh)

    def test_jobs_wrapper_must_carry_a_list(self):
        for payload in ({"jobs": None}, {"jobs": {}}, {}):
            gh = FakeGitHub()
            gh.routes["x"] = lambda query, value=payload: value
            with self.subTest(payload=payload), self.assertRaises(incident_issue.IncidentError), \
                    mock.patch.object(incident_issue, "_request", gh.request):
                list(history(gh).pages("x", key="jobs"))

    def test_oversized_page_is_rejected(self):
        gh = FakeGitHub()
        gh.routes["deployments"] = lambda query: [{} for _ in range(hold.PAGE_SIZE + 1)]
        with self.assertRaises(incident_issue.IncidentError):
            self.run_history(gh)

    def test_pagination_walks_multiple_pages(self):
        gh = FakeGitHub()
        rows = [{"n": i} for i in range(hold.PAGE_SIZE * 2 + 5)]
        gh.routes["deployments"] = paged(rows)
        self.assertEqual(self.run_history(gh), rows)

    def test_full_last_page_is_followed_by_one_more_read(self):
        gh = FakeGitHub()
        calls = []

        def serve(query):
            calls.append(int(query["page"]))
            return [{} for _ in range(hold.PAGE_SIZE)] if query["page"] == "1" else []
        gh.routes["deployments"] = serve
        self.assertEqual(len(self.run_history(gh)), hold.PAGE_SIZE)
        self.assertEqual(calls, [1, 2])

    def test_page_cap_is_enforced(self):
        gh = FakeGitHub()
        calls = []

        def serve(query):
            calls.append(int(query["page"]))
            return [{} for _ in range(hold.PAGE_SIZE)]
        gh.routes["deployments"] = serve
        with self.assertRaises(incident_issue.IncidentError):
            self.run_history(gh)
        self.assertEqual(calls, list(range(1, hold.MAX_PAGES + 1)))


class ReaderContractTests(unittest.TestCase):
    def test_keyed_pages_reject_bare_lists(self):
        for payload in ([], [{}]):
            with self.subTest(payload=payload), \
                    mock.patch.object(incident_issue, "_request", return_value=payload), \
                    self.assertRaises(incident_issue.IncidentError):
                list(hold.History(ENV).pages("x", key="jobs"))

    def test_keyed_pages_traverse_wrapped_lists(self):
        rows = [{"n": i} for i in range(hold.PAGE_SIZE + 1)]
        gh = FakeGitHub()
        gh.routes["x"] = paged(rows, key="jobs")
        with mock.patch.object(incident_issue, "_request", gh.request):
            self.assertEqual(list(history(gh).pages("x", key="jobs")), rows)

    def test_import_is_inert(self):
        output = io.StringIO()
        with mock.patch.dict(os.environ, {}, clear=True), \
                mock.patch.object(incident_issue, "_context", side_effect=AssertionError), \
                mock.patch.object(incident_issue, "_request", side_effect=AssertionError), \
                redirect_stdout(output), redirect_stderr(output):
            importlib.reload(hold)
        self.assertEqual(output.getvalue(), "")

    def test_context_and_shared_request_budget(self):
        with self.assertRaises(incident_issue.IncidentError):
            hold.History({})
        reader = hold.History(ENV)
        with mock.patch.object(incident_issue, "_request", return_value=[]) as request:
            for _ in range(99):
                reader.get("x", branch="main")
            self.assertEqual(list(reader.pages("y")), [])
            with self.assertRaisesRegex(incident_issue.IncidentError, "request cap"):
                reader.get("z")
        self.assertEqual(request.call_count, 100)
        request.assert_any_call("GET", f"/repos/{REPO}/x?branch=main", ENV["GITHUB_TOKEN"])

    def test_shared_transport_bounds_and_duplicate_json(self):
        reader = hold.History(ENV)
        for raw, message in ((b'{"x":1,"x":2}', "not valid JSON"),
                             (b"x" * (incident_issue.MAX_RESPONSE_BYTES + 1), "size cap")):
            with self.subTest(message=message), \
                    mock.patch.object(incident_issue.urllib.request, "build_opener") as build:
                build.return_value.open.return_value = io.BytesIO(raw)
                with self.assertRaisesRegex(incident_issue.IncidentError, message):
                    reader.get("x")
                args, kwargs = build.return_value.open.call_args
                self.assertEqual(args[0].get_method(), "GET")
                self.assertEqual(kwargs["timeout"], 10)
                self.assertTrue(any(isinstance(handler, incident_issue._NoRedirectHandler)
                                    for handler in build.call_args.args))

    def test_shared_transport_refuses_redirect(self):
        reader = hold.History(ENV)
        error = incident_issue.urllib.error.HTTPError("https://example.invalid", 302, "", {}, None)
        with mock.patch.object(incident_issue.urllib.request, "build_opener") as build:
            build.return_value.open.side_effect = error
            with self.assertRaisesRegex(incident_issue.IncidentError, "refused redirect"):
                reader.get("x")
        self.assertIsNone(incident_issue._NoRedirectHandler().redirect_request(None, None, 302, "", {}, "https://example.invalid"))

    def test_consumers_exhaust_reader_and_share_budget(self):
        reader = hold.History(ENV)
        revision = "a" * 40
        cutoff = datetime(2026, 11, 10, tzinfo=timezone.utc)
        row = {"merge_commit_sha": revision, "base": {"ref": "main", "repo": {"full_name": REPO}},
               "merged_at": "2026-11-10T00:00:00Z"}
        event = {"id": 1, "event": "reopened", "created_at": "2026-11-11T00:00:00Z"}
        gh = FakeGitHub()
        gh.routes[f"commits/{revision}/pulls"] = paged([row])
        gh.routes["issues/1/events"] = paged([event])
        with mock.patch.object(incident_issue, "_request", gh.request):
            self.assertEqual(timing.held_since(reader, revision, cutoff, cutoff), cutoff)
            self.assertFalse(receipts.stale_activity(reader, {"number": 1, "updated_at": "2026-11-10T00:00:00Z"},
                                                       {"last_promoted_at": "2026-11-10T00:00:00Z"}))
        self.assertEqual(reader.requests, 2)

    def test_timing_rejects_duplicate_match_on_later_page(self):
        reader = hold.History(ENV)
        revision = "a" * 40
        cutoff = datetime(2026, 11, 10, tzinfo=timezone.utc)
        row = {"merge_commit_sha": revision, "base": {"ref": "main", "repo": {"full_name": REPO}},
               "merged_at": "2026-11-10T00:00:00Z"}
        other = dict(row, merge_commit_sha="b" * 40)
        gh = FakeGitHub()
        gh.routes[f"commits/{revision}/pulls"] = paged([row] + [other] * 99 + [row])
        with mock.patch.object(incident_issue, "_request", gh.request), \
                self.assertRaisesRegex(incident_issue.IncidentError, "not unique"):
            timing.held_since(reader, revision, cutoff, cutoff)
        self.assertEqual(reader.requests, 2)

    def test_subclass_and_patch_get_share_base_reader_counter(self):
        class ReporterHistory(hold.History):
            pass

        reader = ReporterHistory(ENV)
        with mock.patch.object(incident_issue, "_request", return_value=[]) as transport, \
                mock.patch.object(ReporterHistory, "get", autospec=True, wraps=reader.get) as get:
            get.side_effect = lambda instance, path, **query: hold.History.get(instance, path, **query)
            self.assertEqual(list(reader.pages("x")), [])
            self.assertEqual(reader.requests, 1)
            self.assertEqual(transport.call_count, 1)

    def test_consumers_do_not_accept_incomplete_pages(self):
        reader = hold.History(ENV)
        gh = FakeGitHub()
        calls = []

        def serve(query):
            page = int(query["page"])
            calls.append(page)
            return [{"id": (page - 1) * hold.PAGE_SIZE + i + 1,
                     "event": "reopened", "created_at": "2026-11-11T00:00:00Z"}
                    for i in range(hold.PAGE_SIZE)]

        gh.routes["issues/1/events"] = serve
        with mock.patch.object(incident_issue, "_request", gh.request), \
                self.assertRaisesRegex(incident_issue.IncidentError, "page cap"):
            receipts.stale_activity(reader, {"number": 1, "updated_at": "2026-11-10T00:00:00Z"},
                                    {"last_promoted_at": "2026-11-10T00:00:00Z"})
        self.assertEqual(calls, list(range(1, hold.MAX_PAGES + 1)))


ACTIVE_RUNS = "actions/workflows/deploy-production.yml/runs"

def job(name=diagnostic.PRODUCTION_JOB, status="completed", conclusion=None):
    if status == "completed" and conclusion is None:
        conclusion = "success"
    return {"name": name, "status": status, "conclusion": conclusion}

def install_active(gh, runs, jobs_by_run):
    gh.routes[ACTIVE_RUNS] = lambda query: {"workflow_runs": runs}
    for run in runs:
        if not isinstance(run, dict) or type(run.get("id")) is not int or type(run.get("run_attempt")) is not int:
            continue
        path = f"actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs"
        gh.routes[path] = paged(jobs_by_run.get(run["id"], []), "jobs")

def diagnostic_history(gh):
    return diagnostic.History(ENV)

class PendingReasonTests(unittest.TestCase):
    def reason_for(self, gh, own="777"):
        with mock.patch.object(incident_issue, "_request", gh.request):
            return diagnostic_history(gh).pending_reason(own)


    def test_waiting_and_queued_production_job_is_reported_with_its_run(self):
        for state in ("waiting", "queued", "pending", "requested", "in_progress"):
            gh = FakeGitHub()
            install_active(gh, [{"id": 888, "run_attempt": 2}],
                           {888: [job(name="guard"),
                                  job(status=state, conclusion=None)]})
            with self.subTest(state=state):
                self.assertEqual(self.reason_for(gh),
                                 (f"production {state}",
                                  f"https://github.com/{REPO}/actions/runs/888"))


    def test_older_approval_wait_takes_priority_over_newer_terminal_run(self):
        for conclusion in ("skipped", "failure"):
            gh = FakeGitHub()
            install_active(gh, [{"id": 999, "run_attempt": 1}, {"id": 888, "run_attempt": 2}],
                           {999: [job(conclusion=conclusion)],
                            888: [job(status="waiting", conclusion=None)]})
            with self.subTest(conclusion=conclusion):
                self.assertEqual(self.reason_for(gh),
                                 ("production waiting",
                                  f"https://github.com/{REPO}/actions/runs/888"))


    def test_no_active_run_keeps_the_newest_terminal_fallback(self):
        gh = FakeGitHub()
        install_active(gh, [{"id": 999, "run_attempt": 1}, {"id": 888, "run_attempt": 1}],
                       {999: [job(conclusion="skipped")], 888: [job(conclusion="failure")]})
        self.assertEqual(self.reason_for(gh),
                         ("production skipped or pending - inspect promotion run",
                          f"https://github.com/{REPO}/actions/runs/999"))


    def test_waiting_rollback_job_is_reported(self):
        gh = FakeGitHub()
        install_active(gh, [{"id": 888, "run_attempt": 1}],
                       {888: [job(name="wrangler rollback + readyz gate",
                                  status="waiting", conclusion=None)]})
        self.assertEqual(self.reason_for(gh)[0], "production waiting")


    def test_own_run_is_excluded(self):
        gh = FakeGitHub()
        install_active(gh, [{"id": 777, "run_attempt": 1}],
                       {777: [job(status="waiting", conclusion=None)]})
        self.assertEqual(self.reason_for(gh), ("no successful production promotion", None))
        install_active(gh, [{"id": 777, "run_attempt": 1}, {"id": 888, "run_attempt": 1}],
                       {777: [job(status="queued", conclusion=None)],
                        888: [job(status="queued", conclusion=None)]})
        self.assertEqual(self.reason_for(gh)[1],
                         f"https://github.com/{REPO}/actions/runs/888")


    def test_failed_gate_and_skipped_production_have_generic_reasons(self):
        gh = FakeGitHub()
        install_active(gh, [{"id": 888, "run_attempt": 1}],
                       {888: [job(name="schema-gate", conclusion="failure")]})
        self.assertIn("gate not green", self.reason_for(gh)[0])
        install_active(gh, [{"id": 888, "run_attempt": 1}],
                       {888: [job(name="guard"), job(conclusion="skipped")]})
        self.assertIn("skipped or pending", self.reason_for(gh)[0])


    def test_no_other_run_has_the_generic_fallback(self):
        gh = FakeGitHub()
        install_active(gh, [], {})
        self.assertEqual(self.reason_for(gh), ("no successful production promotion", None))


    def test_malformed_active_runs_fail_closed(self):
        bad = [None, [], {"workflow_runs": None}, {"workflow_runs": [1]},
               {"workflow_runs": [{"id": "9", "run_attempt": 1}]},
               {"workflow_runs": [{"id": 0, "run_attempt": 1}]},
               {"workflow_runs": [{"id": 9, "run_attempt": 0}]},
               {"workflow_runs": [{"id": 9, "run_attempt": True}]},
               {"workflow_runs": [{"id": 9, "run_attempt": 1}] * 21}]
        for index, payload in enumerate(bad):
            gh = FakeGitHub()
            gh.routes[ACTIVE_RUNS] = lambda query, value=payload: value
            trace = []
            def request(method, path, token, body=None):
                trace.append((method, path))
                return gh.request(method, path, token, body)
            kind = "runs are" if index in (0, 1, 2, 8) else (
                "attempt is" if index in (6, 7) else "run is")
            with self.subTest(payload=payload), mock.patch.object(incident_issue, "_request", request):
                with self.assertRaisesRegex(incident_issue.IncidentError,
                        f"^production hold active {kind} malformed$"):
                    diagnostic_history(gh).pending_reason("777")
            self.assertEqual(trace, [("GET", f"/repos/{REPO}/{ACTIVE_RUNS}?branch=main&per_page=20")])


    def test_active_run_query_is_bounded_to_main(self):
        seen = []
        gh = FakeGitHub()
        gh.routes[ACTIVE_RUNS] = lambda query: seen.append(query) or {"workflow_runs": []}
        self.reason_for(gh)
        self.assertEqual(seen, [{"branch": "main", "per_page": "20"}])


    def test_later_malformed_row_after_waiting_run_fails_closed(self):
        gh = FakeGitHub()
        install_active(gh, [{"id": 888, "run_attempt": 1}],
                       {888: [job(status="waiting", conclusion=None)]})
        gh.routes[ACTIVE_RUNS] = lambda query: {"workflow_runs": [
            {"id": 888, "run_attempt": 1}, {"id": 0, "run_attempt": 1}]}
        trace = []
        def request(method, path, token, body=None):
            trace.append((method, path))
            return gh.request(method, path, token, body)
        with mock.patch.object(incident_issue, "_request", request):
            with self.assertRaisesRegex(incident_issue.IncidentError,
                    "^production hold active run is malformed$"):
                diagnostic_history(gh).pending_reason("777")
        self.assertEqual(trace, [("GET", f"/repos/{REPO}/{ACTIVE_RUNS}?branch=main&per_page=20")])

    def test_active_job_history_rejects_malformed_later_page(self):
        gh = FakeGitHub()
        seen = []
        gh.routes[ACTIVE_RUNS] = lambda query: {"workflow_runs": [{"id": 888, "run_attempt": 1}]}
        path = "actions/runs/888/attempts/1/jobs"
        def jobs(query):
            seen.append(query["page"])
            return {"jobs": [job(status="waiting", conclusion=None)] + [job()] * 99} if query["page"] == "1" else {"jobs": None}
        gh.routes[path] = jobs
        with self.assertRaisesRegex(incident_issue.IncidentError, "^production hold history is malformed$"):
            self.reason_for(gh)
        self.assertEqual(seen, ["1", "2"])

    def test_active_job_history_enforces_page_cap(self):
        gh = FakeGitHub()
        seen = []
        gh.routes[ACTIVE_RUNS] = lambda query: {"workflow_runs": [{"id": 888, "run_attempt": 1}]}
        gh.routes["actions/runs/888/attempts/1/jobs"] = lambda query: seen.append(query["page"]) or {"jobs": [job(status="waiting", conclusion=None)] + [job()] * 99}
        with self.assertRaisesRegex(incident_issue.IncidentError, "^production hold history exceeds the page cap$"):
            self.reason_for(gh)
        self.assertEqual(seen, [str(page) for page in range(1, 11)])

    def test_valid_waiting_first_page_survives_complete_history(self):
        gh = FakeGitHub()
        seen = []
        install_active(gh, [{"id": 777}, {"id": 888, "run_attempt": 1}], {})
        gh.routes["actions/runs/888/attempts/1/jobs"] = lambda query: seen.append(query["page"]) or {
            "jobs": ([job(status="waiting", conclusion=None)] + [job()] * 99
                     if query["page"] == "1" else [])}
        self.assertEqual(self.reason_for(gh),
                         ("production waiting", f"https://github.com/{REPO}/actions/runs/888"))
        self.assertEqual(seen, ["1", "2"])

    def test_active_jobs_share_prior_request_budget(self):
        for full_page in (False, True):
            with self.subTest(full_page=full_page):
                gh = FakeGitHub()
                seen = []
                gh.routes["prior"] = lambda query: {}
                gh.routes[ACTIVE_RUNS] = lambda query: {"workflow_runs": [{"id": 888, "run_attempt": 1}]}
                gh.routes["actions/runs/888/attempts/1/jobs"] = lambda query: seen.append(query["page"]) or {
                    "jobs": [job()] * (100 if full_page else 1)}
                reader = diagnostic_history(gh)
                with mock.patch.object(incident_issue, "_request", gh.request):
                    for _ in range(98):
                        reader.get("prior")
                    if full_page:
                        with self.assertRaisesRegex(incident_issue.IncidentError, "^production hold history exceeds the request cap$"):
                            reader.pending_reason("777")
                    else:
                        self.assertEqual(reader.pending_reason("777"),
                                         ("production skipped or pending - inspect promotion run",
                                          f"https://github.com/{REPO}/actions/runs/888"))
                self.assertEqual(seen, ["1"])
                self.assertEqual(reader.requests, 101 if full_page else 100)

    def test_invalid_batch_is_fully_validated_before_jobs(self):
        valid = {"id": 888, "run_attempt": 1}
        bad_ids = [None, {}, {"id": None}, {"id": True}, {"id": "9"},
                   {"id": 0}, {"id": -1}]
        bad_attempts = [{"id": 9}, *[{"id": 9, "run_attempt": value}
                        for value in (None, True, "1", 0, -1)]]
        for bad, message in ([(row, "run") for row in bad_ids] +
                             [(row, "attempt") for row in bad_attempts]):
            for runs in ([bad, valid], [valid, bad], [valid] * 19 + [bad],
                         [{"id": 777}, bad]):
                with self.subTest(bad=bad, runs=runs):
                    gh = FakeGitHub()
                    trace = []
                    gh.routes[ACTIVE_RUNS] = lambda query: {"workflow_runs": runs}
                    def request(method, path, token, body=None):
                        trace.append((method, path))
                        return gh.request(method, path, token, body)
                    with mock.patch.object(incident_issue, "_request", request):
                        with self.assertRaisesRegex(incident_issue.IncidentError,
                                                    f"^production hold active {message} is malformed$"):
                            diagnostic_history(gh).pending_reason("777")
                    self.assertEqual(trace, [("GET", f"/repos/{REPO}/{ACTIVE_RUNS}?branch=main&per_page=20")])

    def test_valid_batch_excludes_current_and_selects_first_wait(self):
        gh = FakeGitHub()
        trace = []
        runs = [{"id": 777}, {"id": 888, "run_attempt": 1},
                {"id": 999, "run_attempt": 2}]
        install_active(gh, runs, {888: [job(status="waiting")],
                                  999: [job(status="queued")]})
        def request(method, path, token, body=None):
            trace.append(path)
            return gh.request(method, path, token, body)
        with mock.patch.object(incident_issue, "_request", request):
            self.assertEqual(diagnostic_history(gh).pending_reason("777"),
                             ("production waiting", f"https://github.com/{REPO}/actions/runs/888"))
        self.assertEqual(trace, [f"/repos/{REPO}/{ACTIVE_RUNS}?branch=main&per_page=20",
                                 f"/repos/{REPO}/actions/runs/888/attempts/1/jobs?per_page=100&page=1"])

    def test_transport_failure_propagates_at_jobs_boundary(self):
        gh = FakeGitHub()
        install_active(gh, [{"id": 888, "run_attempt": 1}], {})
        trace = []
        def request(method, path, token, body=None):
            trace.append(path)
            if "/jobs?" in path:
                raise incident_issue.IncidentError("offline transport failure")
            return gh.request(method, path, token, body)
        with mock.patch.object(incident_issue, "_request", request):
            with self.assertRaisesRegex(incident_issue.IncidentError, "^offline transport failure$"):
                diagnostic_history(gh).pending_reason("777")
        self.assertEqual(trace, [f"/repos/{REPO}/{ACTIVE_RUNS}?branch=main&per_page=20",
                                 f"/repos/{REPO}/actions/runs/888/attempts/1/jobs?per_page=100&page=1"])


    def test_valid_replacements_for_invalid_run_fields(self):
        for field in ("id", "run_attempt"):
            for invalid in (None, True, "1", 0, -1):
                with self.subTest(field=field, invalid=invalid):
                    row = {"id": 888, "run_attempt": 1}
                    row[field] = invalid
                    gh = FakeGitHub()
                    install_active(gh, [row], {})
                    with self.assertRaisesRegex(incident_issue.IncidentError,
                            "^production hold active (run|attempt) is malformed$"):
                        self.reason_for(gh)
                    row[field] = 888 if field == "id" else 1
                    install_active(gh, [row], {888: [job(status="waiting")]})
                    self.assertEqual(self.reason_for(gh),
                        ("production waiting", f"https://github.com/{REPO}/actions/runs/888"))

    def test_wrapper_errors_and_active_transport_fail_before_jobs(self):
        route = f"/repos/{REPO}/{ACTIVE_RUNS}?branch=main&per_page=20"
        for payload in (None, [], {}, {"workflow_runs": None},
                        {"workflow_runs": [{"id": 1, "run_attempt": 1}] * 21}):
            with self.subTest(payload=payload):
                trace = []
                def request(method, path, token, body=None):
                    trace.append((method, path))
                    return payload
                with mock.patch.object(incident_issue, "_request", request):
                    with self.assertRaisesRegex(incident_issue.IncidentError,
                            "^production hold active runs are malformed$"):
                        diagnostic_history(None).pending_reason("777")
                self.assertEqual(trace, [("GET", route)])
        trace = []
        def fail(method, path, token, body=None):
            trace.append((method, path))
            raise incident_issue.IncidentError("offline active transport failure")
        with mock.patch.object(incident_issue, "_request", fail):
            with self.assertRaisesRegex(incident_issue.IncidentError,
                    "^offline active transport failure$"):
                diagnostic_history(None).pending_reason("777")
        self.assertEqual(trace, [("GET", route)])

    def test_constructor_validation_and_import_are_inert(self):
        with mock.patch.object(incident_issue, "_request") as request,\
                mock.patch.object(incident_issue, "_context") as context:
            importlib.reload(diagnostic)
            context.assert_not_called()
            request.assert_not_called()
        for env in ({}, {"GITHUB_TOKEN": "x"},
                    {"GITHUB_TOKEN": "x", "GITHUB_REPOSITORY": "invalid"}):
            with self.subTest(env=env), mock.patch.object(incident_issue, "_request") as request:
                with self.assertRaisesRegex(incident_issue.IncidentError,
                        "^incident sink context is missing"):
                    diagnostic.History(env)
                request.assert_not_called()
        self.assertEqual(diagnostic.History(ENV).requests, 0)

    def test_invalid_rows_and_wrappers_have_valid_counterparts(self):
        for bad in (None, [], {}, {"id": 888}, {"run_attempt": 1}):
            with self.subTest(bad=bad):
                gh = FakeGitHub()
                install_active(gh, [bad], {})
                error = "attempt" if bad == {"id": 888} else "run"
                with self.assertRaisesRegex(incident_issue.IncidentError,
                        f"^production hold active {error} is malformed$"):
                    self.reason_for(gh)
                install_active(gh, [{"id": 888, "run_attempt": 1}],
                               {888: [job(status="waiting")]})
                self.assertEqual(self.reason_for(gh),
                        ("production waiting", f"https://github.com/{REPO}/actions/runs/888"))
        for invalid in (None, [], {}, {"workflow_runs": None}):
            with self.subTest(wrapper=invalid):
                gh = FakeGitHub()
                gh.routes[ACTIVE_RUNS] = lambda query: invalid
                with self.assertRaisesRegex(incident_issue.IncidentError,
                        "^production hold active runs are malformed$"):
                    self.reason_for(gh)
                install_active(gh, [], {})
                self.assertEqual(self.reason_for(gh), ("no successful production promotion", None))

    def test_exact_twenty_rows_accepted_and_twenty_one_rejected(self):
        rows = [{"id": 888 + i, "run_attempt": 1} for i in range(21)]
        gh = FakeGitHub()
        install_active(gh, rows[:20], {888: [job(status="waiting")]})
        self.assertEqual(self.reason_for(gh),
                ("production waiting", f"https://github.com/{REPO}/actions/runs/888"))
        install_active(gh, rows, {})
        with self.assertRaisesRegex(incident_issue.IncidentError,
                "^production hold active runs are malformed$"):
            self.reason_for(gh)


    def test_malformed_consumed_job_evidence_fails_before_a_result(self):
        for invalid in ({"name": diagnostic.PRODUCTION_JOB, "status": None, "conclusion": None},
                        {"status": "waiting", "conclusion": None},
                        {"name": 1, "status": "waiting", "conclusion": None},
                        {"name": "guard", "status": "completed", "conclusion": None}):
            for rows in ([invalid], [job(status="waiting"), invalid]):
                with self.subTest(invalid=invalid, rows=rows):
                    gh = FakeGitHub()
                    install_active(gh, [{"id": 888, "run_attempt": 1}], {888: rows})
                    with self.assertRaisesRegex(incident_issue.IncidentError,
                                                "^production hold active job is malformed$"):
                        self.reason_for(gh)
                    install_active(gh, [{"id": 888, "run_attempt": 1}],
                                   {888: [job(status="waiting")]})
                    self.assertEqual(self.reason_for(gh)[0], "production waiting")

    def test_unsuccessful_terminal_outcomes_are_not_skipped_or_pending(self):
        for conclusion in ("failure", "cancelled", "timed_out", "action_required",
                           "startup_failure", "stale"):
            with self.subTest(conclusion=conclusion):
                gh = FakeGitHub()
                install_active(gh, [{"id": 888, "run_attempt": 1}],
                               {888: [job(conclusion=conclusion)]})
                self.assertEqual(self.reason_for(gh),
                    ("promotion gate not green - inspect promotion run",
                     f"https://github.com/{REPO}/actions/runs/888"))

    def test_fresh_process_import_has_no_side_effects(self):
        assert_inert_import(
            self, 'production_hold_diagnostic',
            ['production_hold_diagnostic', 'production_hold_receipts', 'incident_issue',
             'production_hold_history'],
            ['incident_issue', 'production_hold_history'],
        )
