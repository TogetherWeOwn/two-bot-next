"""Offline tests for the independent production-hold timing boundary."""

import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import incident_issue
import production_hold_timing as timing
import production_hold_pending as pending
import test_production_hold_pending as pending_tests

SHA = "a" * 40
NOW = datetime(2026, 1, 2, 12, tzinfo=timezone.utc)


def receipt(merged_at="2026-01-02T04:00:00Z", revision=SHA,
            branch="main", repo="example/repo"):
    return {"merge_commit_sha": revision, "merged_at": merged_at,
            "base": {"ref": branch, "repo": {"full_name": repo}}}


class History:
    repo = "example/repo"

    def __init__(self, rows, error=None):
        self.rows = rows
        self.error = error
        self.paths = []

    def pages(self, path):
        self.paths.append(path)
        yield from self.rows
        if self.error:
            raise incident_issue.IncidentError(self.error)


class TimingTest(unittest.TestCase):
    def test_verified_interval_and_strict_six_hour_boundary(self):
        for completed, expected, overdue in (
                (NOW - timedelta(hours=10), NOW - timedelta(hours=8), True),
                (NOW - timedelta(hours=7), NOW - timedelta(hours=7), True),
                (NOW - timedelta(hours=6), NOW - timedelta(hours=6), False),
                (NOW - timedelta(hours=5), NOW - timedelta(hours=5), False)):
            with self.subTest(completed=completed):
                history = History([receipt()])
                held = timing.held_since(history, SHA, completed, NOW)
                self.assertEqual(held, expected)
                self.assertEqual(held < NOW - timedelta(hours=6), overdue)
                self.assertEqual(history.paths, [f"commits/{SHA}/pulls"])

    def test_no_pending_still_validates_production_clock(self):
        history = History([])
        self.assertIsNone(timing.held_since(history, None, NOW, NOW))
        self.assertEqual(history.paths, [])
        with self.assertRaises(incident_issue.IncidentError):
            timing.held_since(history, None, NOW + timedelta(seconds=1), NOW)

    def test_unmatched_ambiguous_and_malformed_receipts_fail_closed(self):
        invalid = [[], [receipt(revision="b" * 40)],
                   [receipt(branch="other")], [receipt(repo="other/repo")],
                   [receipt(), receipt()], [None], [{}],
                   [receipt(), receipt(revision=None)], [receipt(), receipt(revision="bad")],
                   [receipt(merged_at=None)], [receipt(merged_at="2026-01-02T04:00:00")],
                   [receipt(merged_at="0001-01-01T00:00:00+23:59")],
                   [receipt(merged_at="2026-01-03T00:00:00Z")]]
        for rows in invalid:
            for now in (NOW, NOW + timedelta(minutes=1)):
                with self.subTest(rows=rows, now=now):
                    with self.assertRaises(incident_issue.IncidentError):
                        timing.held_since(History(rows), SHA, NOW - timedelta(days=1), now)

    def test_incomplete_history_cannot_accept_earlier_match(self):
        for rows in ([], [receipt()]):
            with self.subTest(rows=rows):
                with self.assertRaises(incident_issue.IncidentError):
                    timing.held_since(History(rows, "history unavailable"), SHA,
                                      NOW - timedelta(days=1), NOW)

    def test_invalid_clocks_and_revision_fail_closed(self):
        for pending, completed, now in ((SHA, NOW.replace(tzinfo=None), NOW),
                                        (SHA, NOW, NOW.replace(tzinfo=None)),
                                        (SHA, NOW + timedelta(seconds=1), NOW),
                                        (SHA, datetime.min.replace(tzinfo=timezone(timedelta(hours=1))), NOW),
                                        ("bad", NOW, NOW)):
            with self.subTest(pending=pending, completed=completed, now=now):
                with self.assertRaises(incident_issue.IncidentError):
                    timing.held_since(History([receipt()]), pending, completed, now)

    def test_repeated_missing_and_ambiguous_evidence_never_ages_into_validity(self):
        for rows in ([], [receipt(), receipt()]):
            for now in (NOW, NOW + timedelta(days=30)):
                with self.subTest(rows=rows, now=now):
                    with self.assertRaises(incident_issue.IncidentError):
                        timing.held_since(History(rows), SHA, NOW, now)

    def test_late_page_error_and_late_match_are_observed(self):
        class PagedHistory:
            repo = "example/repo"

            def __init__(self, pages):
                self.pages_data = pages
                self.visited = []

            def pages(self, path, key=None, **query):
                for page in self.pages_data:
                    self.visited.append((path, key, query))
                    if isinstance(page, incident_issue.IncidentError):
                        raise page
                    yield from page

        path = f"commits/{SHA}/pulls"
        history = PagedHistory([[receipt(revision="b" * 40)], [receipt()]])
        self.assertEqual(timing.held_since(history, SHA, NOW - timedelta(days=1), NOW),
                         NOW - timedelta(hours=8))
        self.assertEqual(history.visited, [(path, None, {}), (path, None, {})])
        for error in ("history unavailable", "history is malformed",
                      "history exceeds the request cap", "history exceeds the page cap"):
            history = PagedHistory([[receipt()], incident_issue.IncidentError(error)])
            with self.subTest(error=error), self.assertRaises(incident_issue.IncidentError):
                timing.held_since(history, SHA, NOW, NOW)
            self.assertEqual(len(history.visited), 2)

    def test_future_server_receipt_becomes_usable_without_reset(self):
        future = NOW + timedelta(hours=1)
        row = receipt(merged_at=future.strftime("%Y-%m-%dT%H:%M:%SZ"))
        with self.assertRaises(incident_issue.IncidentError):
            timing.held_since(History([row]), SHA, NOW, NOW)
        for now in (future, future + timedelta(hours=7)):
            self.assertEqual(timing.held_since(History([row]), SHA, NOW, now), future)

    def test_exhausts_all_results_even_after_a_match(self):
        rows = [receipt(), receipt(revision="b" * 40), receipt()]
        with self.assertRaises(incident_issue.IncidentError):
            timing.held_since(History(rows), SHA, NOW - timedelta(days=1), NOW)
        for message in ("request unavailable", "request cap", "page cap", "malformed page"):
            with self.subTest(message=message):
                with self.assertRaises(incident_issue.IncidentError):
                    timing.held_since(History([receipt()], message), SHA, NOW, NOW)


class SelectorTimingTest(unittest.TestCase):
    def test_git_dates_and_advancing_main_do_not_change_verified_basis(self):
        for git_date, merged_at, expected in (
                (NOW + timedelta(days=30), NOW - timedelta(hours=8), NOW - timedelta(hours=8)),
                (NOW - timedelta(days=30), NOW - timedelta(hours=1), NOW - timedelta(hours=1))):
            with self.subTest(git_date=git_date), tempfile.TemporaryDirectory() as directory:
                with mock.patch.object(pending_tests, "BASE_TS", int(git_date.timestamp()) - 200):
                    repo = pending_tests.Repo(directory)
                    base = repo.commit({"src/main.rs": "base"})
                    repo.commit({"docs/a.md": "docs"})
                    first = repo.commit({"src/main.rs": "runtime"})
                    stamp = datetime.fromtimestamp(repo.times[3], timezone.utc)
                    row = receipt(merged_at=merged_at.strftime("%Y-%m-%dT%H:%M:%SZ"), revision=first)
                    completion = NOW - timedelta(days=40)
                    for now in (NOW, stamp - timedelta(seconds=1), stamp,
                                stamp + timedelta(seconds=1)):
                        if now < merged_at:
                            continue
                        for extra in ({"docs/later.md": "docs"}, {"src/later.rs": "runtime"}):
                            main = repo.git("rev-parse", "HEAD")
                            selected = pending.first_pending_sha(base, main, repo.path)
                            self.assertEqual(selected, first)
                            history = History([row])
                            held = timing.held_since(history, selected, completion, now)
                            self.assertEqual(held, expected)
                            self.assertEqual(history.paths, [f"commits/{first}/pulls"])
                            if now == NOW:
                                self.assertEqual(held < now - timedelta(hours=6),
                                                 git_date > NOW)
                            repo.commit(extra)

    def test_selector_history_errors_never_become_empty_ranges(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = pending_tests.Repo(directory)
            base = repo.commit({"src/main.rs": "base"})
            main = repo.commit({"src/main.rs": "runtime"})
            for helper in ("_first_parent_chain", "_changed_paths"):
                with self.subTest(helper=helper), mock.patch.object(
                        pending, helper, side_effect=incident_issue.IncidentError("unreadable")):
                    with self.assertRaises(incident_issue.IncidentError):
                        pending.first_pending_sha(base, main, repo.path)

    def test_empty_docs_and_rollback_ranges(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = pending_tests.Repo(directory)
            base = repo.commit({"src/main.rs": "base"})
            self.assertIsNone(pending.first_pending_sha(base, base, repo.path))
            docs = repo.commit({"docs/a.md": "docs"})
            self.assertIsNone(pending.first_pending_sha(base, docs, repo.path))
            runtime = repo.commit({"src/main.rs": "changed"})
            rollback = repo.commit({"src/main.rs": "base"})
            self.assertEqual(pending.first_pending_sha(base, rollback, repo.path), runtime)
            self.assertEqual(pending.first_pending_sha(runtime, rollback, repo.path), rollback)
            self.assertEqual(pending.pending_since(base, rollback, repo.path),
                             datetime.fromtimestamp(repo.times[3], timezone.utc))
            with self.assertRaises(incident_issue.IncidentError):
                pending.first_pending_sha(rollback, base, repo.path)
