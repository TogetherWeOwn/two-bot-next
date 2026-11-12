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
import production_hold_history as hold
import production_hold_receipts as receipts
import production_hold_timing as timing

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
