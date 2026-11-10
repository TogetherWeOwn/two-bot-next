"""Offline fixtures for the `incident` issue sink; stdlib only, no network.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_incident_issue.py' -v
"""

import importlib
import re
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
sink = importlib.import_module("incident_issue")

ENV = {"GITHUB_TOKEN": "offline-token-not-a-secret", "GITHUB_REPOSITORY": "owner/repo"}
KEY = "synthetic-user-flow:staging"


class FakeGitHub:
    """Stands in for sink._request; records (method, path, body)."""

    def __init__(self, open_issues=(), created=None):
        self.open_issues = list(open_issues)
        self.created = {"number": 9} if created is None else created
        self.calls = []

    def __call__(self, method, path, token, body=None):
        assert token == ENV["GITHUB_TOKEN"]
        self.calls.append((method, path, body))
        if method == "GET":
            page = int(re.search(r"[?&]page=(\d+)", path).group(1))
            return self.open_issues[(page - 1) * 100:page * 100]
        if path.endswith("/issues"):
            return self.created
        return {}


class IncidentIssueTests(unittest.TestCase):
    def run_with(self, fake, func, *args):
        with mock.patch.object(sink, "_request", side_effect=fake):
            return func(ENV, *args)

    def test_alert_opens_one_labelled_issue_with_the_key_marker(self):
        fake = FakeGitHub()
        self.assertEqual(self.run_with(fake, sink.open_incident, KEY, "title", "body"), ("opened", 9))
        (get, _get_path, _), (post, path, body), (reread, _, _) = fake.calls
        self.assertEqual((get, post, reread), ("GET", "POST", "GET"))
        self.assertIn("labels=incident", _get_path)
        self.assertEqual(path, "/repos/owner/repo/issues")
        self.assertEqual(body["labels"], ["incident"])
        self.assertTrue(body["body"].rstrip().endswith(sink.marker(KEY)))

    def test_alert_with_an_open_issue_comments_on_the_lowest_match_only(self):
        tagged = f"old\n\n{sink.marker(KEY)}\n"
        fake = FakeGitHub([
            {"number": 30, "body": tagged},
            {"number": 12, "body": tagged},
            {"number": 5, "body": sink.marker("synthetic-user-flow:prod")},
            {"number": 4, "body": tagged, "pull_request": {}},
            {"number": 3, "body": None},
        ])
        self.assertEqual(self.run_with(fake, sink.open_incident, KEY, "title", "body"), ("commented", 12))
        self.assertEqual(fake.calls[1][:2], ("POST", "/repos/owner/repo/issues/12/comments"))
        self.assertEqual(len(fake.calls), 2)

    def test_alert_finds_an_older_keyed_issue_past_the_first_page(self):
        newer = [{"number": 500 - n, "body": sink.marker("other:check")} for n in range(150)]
        fake = FakeGitHub(newer + [{"number": 7, "body": sink.marker(KEY)}])
        self.assertEqual(self.run_with(fake, sink.open_incident, KEY, "title", "body"), ("commented", 7))
        self.assertEqual([call[0] for call in fake.calls], ["GET", "GET", "POST"])

    def test_search_past_the_page_bound_fails_closed_without_writing(self):
        fake = FakeGitHub([{"number": n + 1, "body": None} for n in range(100 * sink.MAX_PAGES)])
        with self.assertRaises(sink.IncidentError):
            self.run_with(fake, sink.open_incident, KEY, "title", "body")
        self.assertTrue(all(call[0] == "GET" for call in fake.calls))
        self.assertEqual(len(fake.calls), sink.MAX_PAGES)

    def test_alert_that_lost_a_create_race_closes_its_duplicate(self):
        class Racing(FakeGitHub):
            def __call__(self, method, path, token, body=None):
                result = super().__call__(method, path, token, body)
                if method == "POST" and path.endswith("/issues"):
                    # The concurrent run's issue #8 became visible meanwhile.
                    self.open_issues = [{"number": 8, "body": sink.marker(KEY)},
                                        {"number": 9, "body": sink.marker(KEY)}]
                return result

        fake = Racing()
        self.assertEqual(self.run_with(fake, sink.open_incident, KEY, "title", "body"), ("commented", 8))
        self.assertEqual([call[:2] for call in fake.calls[2:]], [
            ("GET", "/repos/owner/repo/issues?state=open&labels=incident&per_page=100&page=1"),
            ("POST", "/repos/owner/repo/issues/9/comments"),
            ("PATCH", "/repos/owner/repo/issues/9"),
            ("POST", "/repos/owner/repo/issues/8/comments")])
        self.assertEqual(fake.calls[4][2], {"state": "closed", "state_reason": "duplicate"})

    def test_recovery_closes_every_open_issue_for_the_key(self):
        fake = FakeGitHub([{"number": 12, "body": sink.marker(KEY)},
                           {"number": 5, "body": sink.marker(KEY)},
                           {"number": 6, "body": sink.marker("other:check")}])
        self.assertEqual(self.run_with(fake, sink.close_incident, KEY, "healthy"), ("closed", 5))
        self.assertEqual([call[:2] for call in fake.calls[1:] if call[0] == "PATCH"], [
            ("PATCH", "/repos/owner/repo/issues/5"), ("PATCH", "/repos/owner/repo/issues/12")])

    def test_recovery_holds_an_issue_with_activity_after_the_evidence_of_health(self):
        from datetime import datetime, timezone
        passed = datetime(2026, 10, 31, 4, 0, tzinfo=timezone.utc)
        for updated, outcome in (("2026-10-31T03:59:59Z", ("closed", 12)),
                                 ("2026-10-31T04:00:00Z", ("closed", 12)),
                                 # A proof that failed after the pass alerted here.
                                 ("2026-10-31T04:10:00Z", ("held", 12)),
                                 (None, ("held", 12)), ("garbage", ("held", 12)),
                                 ("2026-10-31T04:10:00", ("held", 12))):
            issue = {"number": 12, "body": sink.marker(KEY)}
            if updated is not None:
                issue["updated_at"] = updated
            fake = FakeGitHub([issue])
            with self.subTest(updated=updated), mock.patch.object(sink, "_request", side_effect=fake):
                self.assertEqual(sink.close_incident(ENV, KEY, "healthy", quiet_since=passed), outcome)
            writes = [call for call in fake.calls if call[0] != "GET"]
            self.assertEqual(len(writes), 2 if outcome[0] == "closed" else 0)

    def test_recovery_comments_then_closes_and_tolerates_no_open_issue(self):
        fake = FakeGitHub([{"number": 12, "body": sink.marker(KEY)}])
        self.assertEqual(self.run_with(fake, sink.close_incident, KEY, "healthy"), ("closed", 12))
        self.assertEqual([call[:2] for call in fake.calls[1:]], [
            ("POST", "/repos/owner/repo/issues/12/comments"),
            ("PATCH", "/repos/owner/repo/issues/12")])
        self.assertEqual(fake.calls[2][2], {"state": "closed", "state_reason": "completed"})
        fake = FakeGitHub()
        self.assertEqual(self.run_with(fake, sink.close_incident, KEY, "healthy"), ("none-open", None))
        self.assertEqual(len(fake.calls), 1)

    def test_refusals_fail_closed_without_writing(self):
        for open_issues in ({"not": "a list"}, [{"number": "7"}], [{"number": 7, "body": 3}], [{"number": 0}]):
            fake = FakeGitHub(open_issues)
            with self.subTest(open_issues=open_issues), self.assertRaises(sink.IncidentError):
                self.run_with(fake, sink.open_incident, KEY, "title", "body")
            self.assertEqual([call[0] for call in fake.calls], ["GET"])
        fake = FakeGitHub(created={"id": 1})
        with self.assertRaisesRegex(sink.IncidentError, "no issue number"):
            self.run_with(fake, sink.open_incident, KEY, "title", "body")
        for key in ("", "Has Space", "x" * 81, "<!--"):
            with self.subTest(key=key), self.assertRaisesRegex(sink.IncidentError, "invalid incident key"):
                self.run_with(FakeGitHub(), sink.open_incident, key, "title", "body")
        for env in ({}, {"GITHUB_TOKEN": "t"}, {"GITHUB_TOKEN": "t", "GITHUB_REPOSITORY": "a/b/c"}):
            with self.subTest(env=sorted(env)), \
                    mock.patch.object(sink, "_request", side_effect=AssertionError("network")), \
                    self.assertRaisesRegex(sink.IncidentError, "context is missing"):
                sink.open_incident(env, KEY, "title", "body")

    def test_long_text_is_clipped_to_github_limits(self):
        fake = FakeGitHub()
        self.run_with(fake, sink.open_incident, KEY, "t" * 500, "b" * 100000)
        body = fake.calls[1][2]
        self.assertLessEqual(len(body["title"]), sink.MAX_TITLE)
        self.assertLessEqual(len(body["body"]), sink.MAX_BODY)
        self.assertIn(sink.marker(KEY), body["body"])


if __name__ == "__main__":
    unittest.main()
