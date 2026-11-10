"""Tests for scripts/production_hold_incident.py and its workflow lane.

The helper reads GitHub history through incident_issue._request. These tests
replace only that transport with an in-memory GitHub, so the real shared
incident sink (open, comment, close) runs end to end.
"""

import contextlib
import io
import json
import os
import re
import sys
import tempfile
import unittest
import urllib.parse
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import incident_issue
import production_hold_incident as hold

REPO = "TogetherWeOwn/two-bot-next"
WORKFLOW_FILE = ROOT / ".github" / "workflows" / "deploy-production.yml"
NOW = datetime(2026, 11, 10, 12, 0, 0, tzinfo=timezone.utc)
MAIN = "b" * 40
OLD = "a" * 40
OLDER = "c" * 40
RUN_URL = f"https://github.com/{REPO}/actions/runs/777"


def iso(value):
    return value.strftime("%Y-%m-%dT%H:%M:%SZ")


def deploy_id(revision, n=1):
    return f"2026.11.01.{n}+{revision[:7]}"


def paged(rows, key=None):
    def serve(query):
        size, page = int(query["per_page"]), int(query["page"])
        chunk = rows[(page - 1) * size:page * size]
        return {key: chunk} if key else chunk
    return serve


class FakeGitHub:
    """In-memory GitHub: static read routes plus a real issue lifecycle."""

    def __init__(self):
        self.routes = {}
        self.issues = {}
        self.comments = {}
        self.comment_times = {}
        self.clock = "2020-01-10T00:00:00Z"
        self.writes = []
        self.next_number = 1

    def pending_since(self, promoted, main):
        # This fake supplies ancestry/time metadata instead of a local Git repo.
        # The real patch-free Git reader has its own disposable-repo tests.
        route = self.routes[f"compare/{promoted}...{main}"]
        comparison = route({}) if callable(route) else route
        if comparison.get("status") == "identical":
            return None
        if comparison.get("status") != "ahead":
            raise incident_issue.IncidentError("production hold main ancestry is unreadable")
        commits = comparison.get("commits")
        if not isinstance(commits, list) or not commits:
            raise incident_issue.IncidentError("production hold pending commit is unreadable")
        return hold.moment(commits[0].get("commit", {}).get("committer", {}).get("date"))

    def request(self, method, path, token, body=None):
        assert token == "tok"
        parsed = urllib.parse.urlparse(path)
        query = {k: v[0] for k, v in urllib.parse.parse_qs(parsed.query).items()}
        prefix = f"/repos/{REPO}/"
        assert parsed.path.startswith(prefix), path
        rest = parsed.path[len(prefix):]
        if method != "GET":
            self.writes.append((method, rest))
        if method == "GET" and rest == "issues":
            rows = [dict(issue) for issue in self.issues.values() if issue["state"] == "open"]
            size, page = int(query["per_page"]), int(query["page"])
            return rows[(page - 1) * size:page * size]
        if method == "POST" and rest == "issues":
            number, self.next_number = self.next_number, self.next_number + 1
            self.issues[number] = {"number": number, "state": "open", "title": body["title"],
                                   "body": body["body"], "labels": body["labels"],
                                   "updated_at": self.clock}
            return {"number": number}
        match = re.fullmatch(r"issues/(\d+)/comments", rest)
        if method == "GET" and match:
            number = int(match[1])
            rows = [{"body": text, "updated_at": self.comment_times.get(
                        (number, i), self.issues[number]["updated_at"])}
                    for i, text in enumerate(self.comments.get(number, []))]
            return paged(rows)(query)
        if method == "POST" and match:
            number = int(match[1])
            self.comments.setdefault(number, []).append(body["body"])
            self.comment_times[(number, len(self.comments[number]) - 1)] = self.clock
            self.issues[number]["updated_at"] = self.clock
            return {}
        match = re.fullmatch(r"issues/(\d+)", rest)
        if method == "PATCH" and match:
            self.issues[int(match[1])]["state"] = body["state"]
            return {}
        assert method == "GET", (method, rest)
        route = self.routes[rest]
        return route(query) if callable(route) else route


def run_row(head_sha=OLD, **overrides):
    row = {"head_branch": "main", "head_sha": head_sha,
           "path": ".github/workflows/deploy-production.yml@refs/heads/main",
           "repository": {"full_name": REPO}, "head_repository": {"full_name": REPO},
           "run_attempt": 1}
    row.update(overrides)
    return row


FALLBACK_START = "2026-11-10T00:00:00Z"
FALLBACK_CREATED = "2026-11-10T00:55:00Z"


def shifted(raw, minutes, fallback):
    """raw shifted by minutes when readable, else the fallback."""
    try:
        return iso(hold.moment(raw) + timedelta(minutes=minutes))
    except incident_issue.IncidentError:
        return fallback


def job(name=hold.PRODUCTION_JOB, status="completed", conclusion="success",
        completed_at="2026-11-10T01:00:00Z", started_at="auto"):
    if started_at == "auto":
        started_at = shifted(completed_at, -30, FALLBACK_START)
    return {"name": name, "status": status, "conclusion": conclusion,
            "started_at": started_at, "completed_at": completed_at}


def install_promotion(gh, deployment_id=11, revision=OLD, run_id=501,
                      completed_at="2026-11-10T01:00:00Z", jobs=None, run=None,
                      statuses=None, deployment=None):
    """Register one deployment with its statuses, run and jobs on the fake."""
    row = {"id": deployment_id, "sha": revision, "environment": "production",
           "payload": {"deploy_id": deploy_id(revision)}}
    row.update(deployment or {})
    gh.routes.setdefault("deployments", [])
    gh.routes["deployments"].append(row)
    created = shifted(completed_at, -5, FALLBACK_CREATED)
    gh.routes[f"deployments/{deployment_id}/statuses"] = statuses if statuses is not None else [
        {"state": "inactive", "log_url": ""},
        {"state": "success", "created_at": created,
         "log_url": f"https://github.com/{REPO}/actions/runs/{run_id}"},
        {"state": "in_progress", "log_url": ""}]
    gh.routes[f"actions/runs/{run_id}"] = run or run_row()
    gh.routes[f"actions/runs/{run_id}/attempts/1/jobs"] = {
        "jobs": jobs if jobs is not None else [job(completed_at=completed_at)]}
    return row


def finish(gh, main=MAIN, promoted=OLD, status="ahead", pending_at=None):
    """Register main head and the compare result, then wrap lists for paging."""
    gh.routes["git/ref/heads/main"] = {"object": {"type": "commit", "sha": main}}
    comparison = {"status": status}
    if pending_at is not None:
        comparison["commits"] = [{"commit": {"committer": {"date": pending_at}}}]
    gh.routes[f"compare/{promoted}...{main}"] = lambda query, value=comparison: value
    for path in list(gh.routes):
        if isinstance(gh.routes[path], list):
            gh.routes[path] = paged(gh.routes[path])
        elif path.endswith("/jobs"):
            gh.routes[path] = paged(gh.routes[path]["jobs"], "jobs")
    return gh


def history(gh):
    env = {"GITHUB_TOKEN": "tok", "GITHUB_REPOSITORY": REPO}
    return hold.History(env)


def decide(gh, needs=None, now=NOW):
    with mock.patch.object(incident_issue, "_request", gh.request), \
            mock.patch.object(hold, "pending_since", gh.pending_since):
        return hold.decide(history(gh), needs or {}, now, RUN_URL)


def scenario(promoted_at, pending_at, **extra):
    gh = FakeGitHub()
    install_promotion(gh, completed_at=promoted_at, **extra)
    return finish(gh, pending_at=pending_at)


class ThresholdTests(unittest.TestCase):
    def test_exactly_six_hours_is_not_an_alert(self):
        gh = scenario("2026-11-01T00:00:00Z", iso(NOW - timedelta(hours=6)))
        report = decide(gh)
        self.assertEqual(report["action"], "recovery")
        self.assertEqual(report["held_since"], iso(NOW - timedelta(hours=6)))

    def test_one_second_over_six_hours_alerts(self):
        gh = scenario("2026-11-01T00:00:00Z", iso(NOW - timedelta(hours=6, seconds=1)))
        self.assertEqual(decide(gh)["action"], "alert")

    def test_old_pending_commit_does_not_alert_when_promotion_is_recent(self):
        # Pending since 20h ago, but the last deploy finished 2h ago.
        gh = scenario(iso(NOW - timedelta(hours=2)), iso(NOW - timedelta(hours=20)))
        report = decide(gh)
        self.assertEqual(report["action"], "recovery")
        self.assertEqual(report["held_since"], iso(NOW - timedelta(hours=2)))

    def test_old_promotion_does_not_alert_when_pending_commit_is_recent(self):
        gh = scenario(iso(NOW - timedelta(hours=20)), iso(NOW - timedelta(hours=2)))
        report = decide(gh)
        self.assertEqual(report["action"], "recovery")
        self.assertEqual(report["held_since"], iso(NOW - timedelta(hours=2)))

    def test_clock_starts_at_pending_commit_when_both_are_old(self):
        gh = scenario(iso(NOW - timedelta(hours=30)), iso(NOW - timedelta(hours=7)))
        report = decide(gh)
        self.assertEqual(report["action"], "alert")
        self.assertEqual(report["held_since"], iso(NOW - timedelta(hours=7)))

    def test_clock_starts_at_promotion_when_it_is_the_later_time(self):
        gh = scenario(iso(NOW - timedelta(hours=8)), iso(NOW - timedelta(hours=30)))
        report = decide(gh)
        self.assertEqual(report["action"], "alert")
        self.assertEqual(report["held_since"], iso(NOW - timedelta(hours=8)))

    def test_unchanged_main_never_alerts(self):
        gh = FakeGitHub()
        install_promotion(gh, completed_at="2020-01-01T00:00:00Z")
        finish(gh, main=OLD, status="identical")
        report = decide(gh)
        self.assertEqual(report["action"], "recovery")
        self.assertNotIn("held_since", report)

    def test_successful_rollback_does_not_hide_a_pending_main_revision(self):
        gh = FakeGitHub()
        install_promotion(gh, deployment_id=12, revision=OLD, run_id=502,
                          jobs=[job(name="wrangler rollback + readyz gate")],
                          deployment={"payload": {"deploy_id": deploy_id(OLD, 2),
                                                  "rollback": True}})
        install_promotion(gh, deployment_id=11, revision=MAIN, run_id=501,
                          completed_at="2026-11-09T01:00:00Z")
        finish(gh, promoted=OLD, pending_at="2026-11-09T02:00:00Z")
        gh.routes[f"compare/{MAIN}...{MAIN}"] = {"status": "identical"}
        report = decide(gh, needs={"guard": {"outputs": {"mode": "rollback"}},
                                   "production": {"result": "success"}})
        self.assertEqual(report["action"], "alert")
        self.assertEqual(report["last_promoted_sha"], OLD)

    def test_diverged_or_unknown_comparison_fails_closed(self):
        for state in ("behind", "diverged", None, 7):
            gh = FakeGitHub()
            install_promotion(gh)
            finish(gh, status=state, pending_at="2020-01-01T00:00:00Z")
            with self.subTest(state=state), self.assertRaises(incident_issue.IncidentError):
                decide(gh)

    def test_ahead_without_commits_or_with_bad_time_fails_closed(self):
        for commits in (None, [], [{"commit": {"committer": {"date": "yesterday"}}}],
                        [{"commit": {"committer": {"date": "2026-11-10T00:00:00"}}}]):
            gh = FakeGitHub()
            install_promotion(gh)
            finish(gh)
            comparison = {"status": "ahead"}
            if commits is not None:
                comparison["commits"] = commits
            gh.routes[f"compare/{OLD}...{MAIN}"] = lambda query, value=comparison: value
            with self.subTest(commits=commits), self.assertRaises(incident_issue.IncidentError):
                decide(gh)


class ReasonTests(unittest.TestCase):
    def test_ci_not_green_skip_reason_is_reported(self):
        needs = {"guard": {"result": "success", "outputs": {"skip": "ci-not-green"}}}
        self.assertEqual(hold.reason(needs), "ci-not-green")

    def test_soak_mismatch_skip_reason_is_reported(self):
        needs = {"guard": {"outputs": {}},
                 "soak": {"result": "success", "outputs": {"skip": "soak mismatch"}}}
        self.assertEqual(hold.reason(needs), "soak mismatch")

    def test_gate_failure_names_failed_jobs(self):
        needs = {"guard": {"result": "success", "outputs": {}},
                 "schema-gate": {"result": "failure"},
                 "roles-gate": {"result": "cancelled"},
                 "soak": {"result": "success"}}
        self.assertEqual(hold.reason(needs), "schema-gate, roles-gate not green")

    def test_unsafe_skip_text_is_never_published(self):
        needs = {"recheck": {"outputs": {"skip": "token=ghs_x\nhold until 2026"}}}
        self.assertEqual(hold.reason(needs), "recheck skipped")
        long_text = {"guard": {"outputs": {"skip": "a" * 161}}}
        self.assertEqual(hold.reason(long_text), "guard skipped")

    def test_reason_order_prefers_guard_skip_over_failure(self):
        needs = {"guard": {"outputs": {"skip": "held until 2026-11-11 00:00:00"}},
                 "schema-gate": {"result": "failure"}}
        self.assertEqual(hold.reason(needs), "held until 2026-11-11 00:00:00")

    def test_no_signal_has_generic_reason(self):
        self.assertEqual(hold.reason({}), "no successful production promotion")

    def test_reason_is_carried_by_an_alert_report(self):
        gh = scenario("2026-11-01T00:00:00Z", "2026-11-02T00:00:00Z")
        needs = {"guard": {"result": "success", "outputs": {"skip": "ci-not-green"}}}
        report = decide(gh, needs)
        self.assertEqual((report["action"], report["reason"]), ("alert", "ci-not-green"))
        self.assertIn("Skip/failure reason: ci-not-green", hold.body(report))


class RecoveryTests(unittest.TestCase):
    def deploy_needs(self, production="success", mode="deploy"):
        return {"guard": {"result": "success", "outputs": {"mode": mode}},
                "production": {"result": production}}

    def test_successful_production_job_is_recovery(self):
        gh = scenario("2026-11-10T11:00:00Z", None, )
        gh.routes[f"compare/{OLD}...{MAIN}"] = lambda query: {"status": "identical"}
        report = decide(gh, self.deploy_needs())
        self.assertEqual(report["action"], "recovery")
        self.assertEqual(report["last_promoted_at"], "2026-11-10T11:00:00Z")

    def test_delayed_success_needs_cannot_recover_an_identical_rollback_baseline(self):
        gh = FakeGitHub()
        install_promotion(gh, revision=OLD, run_id=888,
                          jobs=[job(name="wrangler rollback + readyz gate")],
                          deployment={"payload": {"deploy_id": deploy_id(OLD), "rollback": True}})
        finish(gh, main=OLD, status="identical")
        report = decide(gh, self.deploy_needs())
        self.assertEqual(report["action"], "none")
        self.assertEqual((report["promotion_mode"], report["promotion_run_id"]), ("rollback", "888"))

    def test_history_recovery_retains_the_successful_producing_attempt(self):
        gh = FakeGitHub()
        install_promotion(gh, run=run_row(run_attempt=2))
        gh.routes["actions/runs/501/attempts/2/jobs"] = {"jobs": [job(conclusion="skipped")]}
        finish(gh, main=OLD, status="identical")
        report = decide(gh)
        self.assertEqual(report["action"], "recovery")
        self.assertEqual((report["promotion_run_id"], report["promotion_attempt"]), ("501", 1))
        self.assertEqual(report["last_promoted_at"], "2026-11-10T01:00:00Z")

    def test_skipped_production_job_is_not_recovery(self):
        for result in ("skipped", "failure", "cancelled", None):
            gh = scenario("2020-01-01T00:00:00Z", "2020-01-02T00:00:00Z")
            with self.subTest(result=result):
                self.assertNotEqual(decide(gh, self.deploy_needs(result))["action"], "recovery")

    def test_green_workflow_without_deploy_mode_is_not_recovery(self):
        gh = scenario("2020-01-01T00:00:00Z", "2020-01-02T00:00:00Z")
        self.assertNotEqual(decide(gh, self.deploy_needs(mode="skip"))["action"], "recovery")
        self.assertNotEqual(decide(gh, {"production": {"result": "success"}})["action"], "recovery")


class HistoryTests(unittest.TestCase):
    def promoted(self, gh):
        finish(gh, pending_at="2026-11-10T02:00:00Z")
        with mock.patch.object(incident_issue, "_request", gh.request):
            promotion = history(gh).last_promotion()
            return promotion.revision, promotion.completed_at

    def assertRejected(self, gh):
        finish(gh, pending_at="2026-11-10T02:00:00Z")
        with mock.patch.object(incident_issue, "_request", gh.request), \
                self.assertRaises(incident_issue.IncidentError):
            history(gh).last_promotion()

    def test_inactive_latest_status_still_counts_via_status_history(self):
        gh = FakeGitHub()
        install_promotion(gh, statuses=[
            {"state": "inactive", "log_url": ""},
            {"state": "success", "created_at": FALLBACK_CREATED,
             "log_url": f"https://github.com/{REPO}/actions/runs/501"}])
        self.assertEqual(self.promoted(gh), (OLD, hold.moment("2026-11-10T01:00:00Z")))

    def test_rollback_without_a_rollback_job_is_not_certified(self):
        # A deploy-mode job cannot certify a rollback record.
        gh = FakeGitHub()
        install_promotion(gh, deployment_id=12, revision=MAIN, run_id=502,
                          deployment={"payload": {"deploy_id": deploy_id(MAIN, 2),
                                                  "rollback": True}})
        install_promotion(gh, deployment_id=11, revision=OLD, run_id=501)
        self.assertEqual(self.promoted(gh)[0], OLD)

    def test_non_production_payload_or_environment_is_skipped(self):
        gh = FakeGitHub()
        install_promotion(gh, deployment_id=13, revision=MAIN, run_id=503,
                          deployment={"environment": "staging"})
        install_promotion(gh, deployment_id=12, revision=MAIN, run_id=502,
                          deployment={"payload": {"deploy_id": "not-an-id"}})
        install_promotion(gh, deployment_id=11, revision=OLD, run_id=501)
        self.assertEqual(self.promoted(gh)[0], OLD)

    def test_failed_watch_does_not_reset_the_clock(self):
        gh = FakeGitHub()
        install_promotion(gh, deployment_id=12, revision=MAIN, run_id=502,
                          jobs=[job(conclusion="failure", completed_at="2026-11-10T09:00:00Z")])
        install_promotion(gh, deployment_id=11, revision=OLD, run_id=501,
                          completed_at="2026-11-09T01:00:00Z")
        self.assertEqual(self.promoted(gh), (OLD, hold.moment("2026-11-09T01:00:00Z")))

    def test_skipped_or_missing_or_duplicate_production_job_is_not_a_promotion(self):
        for jobs in ([job(status="completed", conclusion="skipped")],
                     [job(status="in_progress", conclusion=None)],
                     [job(name="other")],
                     [job(), job()]):
            gh = FakeGitHub()
            install_promotion(gh, jobs=jobs)
            with self.subTest(jobs=jobs):
                self.assertRejected(gh)

    def test_only_the_first_success_status_run_is_consulted(self):
        gh = FakeGitHub()
        install_promotion(gh, jobs=[job(conclusion="failure")], statuses=[
            {"state": "success", "created_at": FALLBACK_CREATED,
             "log_url": f"https://github.com/{REPO}/actions/runs/501"},
            {"state": "success", "created_at": FALLBACK_CREATED,
             "log_url": f"https://github.com/{REPO}/actions/runs/502"}])
        self.assertRejected(gh)

    def test_wrong_provenance_is_rejected(self):
        bad_runs = [run_row(head_branch="feature"),
                    run_row(path=".github/workflows/other.yml@refs/heads/main"),
                    run_row(path=""),
                    run_row(repository={"full_name": "evil/fork"}),
                    run_row(head_repository={"full_name": "evil/fork"}),
                    run_row(head_repository=None),
                    run_row(run_attempt=0),
                    run_row(run_attempt="1"),
                    run_row(run_attempt=True)]
        for run in bad_runs:
            gh = FakeGitHub()
            install_promotion(gh, run=run)
            with self.subTest(run=run):
                self.assertRejected(gh)

    def test_foreign_or_malformed_log_url_is_ignored(self):
        for url in (f"https://github.com/evil/repo/actions/runs/501",
                    f"https://github.com/{REPO}/actions/runs/0501",
                    f"https://github.com/{REPO}/actions/runs/501/attempts/1",
                    f"http://github.com/{REPO}/actions/runs/501", None):
            gh = FakeGitHub()
            install_promotion(gh, statuses=[{"state": "success", "created_at": FALLBACK_CREATED,
                                             "log_url": url}])
            with self.subTest(url=url):
                self.assertRejected(gh)

    def test_deployment_sha_is_used_when_workflow_head_differs(self):
        # The guarded/recorded SHA can differ from the workflow head SHA.
        gh = FakeGitHub()
        install_promotion(gh, revision=OLD, run=run_row(head_sha=OLDER))
        self.assertEqual(self.promoted(gh)[0], OLD)

    def test_deploy_id_must_match_the_recorded_sha(self):
        gh = FakeGitHub()
        install_promotion(gh, deployment={"payload": {"deploy_id": deploy_id(OLDER)}})
        self.assertRejected(gh)

    def test_malformed_deployment_rows_fail_closed(self):
        for patch in ({"sha": "abc"}, {"sha": None}, {"id": 0}, {"id": "11"}, {"id": True}):
            gh = FakeGitHub()
            install_promotion(gh, deployment=patch)
            with self.subTest(patch=patch):
                self.assertRejected(gh)

    def test_non_success_only_statuses_find_nothing(self):
        gh = FakeGitHub()
        install_promotion(gh, statuses=[{"state": "failure", "log_url": ""},
                                        {"state": "inactive", "log_url": ""}])
        self.assertRejected(gh)

    def test_empty_history_fails_closed(self):
        gh = FakeGitHub()
        gh.routes["deployments"] = []
        self.assertRejected(gh)

    def test_unreadable_completion_time_fails_closed(self):
        for stamp_value in (None, "", "2026-11-10T01:00:00", "nope", 5):
            gh = FakeGitHub()
            install_promotion(gh, jobs=[job(completed_at=stamp_value)])
            with self.subTest(stamp=stamp_value):
                self.assertRejected(gh)

    def test_unreadable_start_or_status_time_fails_closed(self):
        for stamp_value in (None, "", "2026-11-10T00:30:00", "nope", 5):
            gh = FakeGitHub()
            install_promotion(gh, jobs=[job(started_at=stamp_value)])
            with self.subTest(started=stamp_value):
                self.assertRejected(gh)
            gh = FakeGitHub()
            install_promotion(gh, statuses=[{
                "state": "success", "created_at": stamp_value,
                "log_url": f"https://github.com/{REPO}/actions/runs/501"}])
            with self.subTest(created=stamp_value):
                self.assertRejected(gh)

    def test_status_outside_the_job_window_is_not_certified(self):
        for created in ("2026-11-10T00:29:59Z", "2026-11-10T01:00:01Z"):
            gh = FakeGitHub()
            install_promotion(gh, statuses=[{
                "state": "success", "created_at": created,
                "log_url": f"https://github.com/{REPO}/actions/runs/501"}])
            with self.subTest(created=created):
                self.assertRejected(gh)

    def test_window_bounds_are_inclusive(self):
        for created in ("2026-11-10T00:30:00Z", "2026-11-10T01:00:00Z"):
            gh = FakeGitHub()
            install_promotion(gh, statuses=[{
                "state": "success", "created_at": created,
                "log_url": f"https://github.com/{REPO}/actions/runs/501"}])
            with self.subTest(created=created):
                self.assertEqual(self.promoted(gh)[0], OLD)

    def test_later_skipped_attempt_keeps_the_original_promotion(self):
        gh = FakeGitHub()
        install_promotion(gh, run=run_row(run_attempt=2))
        gh.routes["actions/runs/501/attempts/2/jobs"] = {
            "jobs": [job(status="completed", conclusion="skipped",
                         completed_at="2026-11-10T03:00:00Z")]}
        self.assertEqual(self.promoted(gh), (OLD, hold.moment("2026-11-10T01:00:00Z")))

    def test_later_success_does_not_certify_an_earlier_failed_status(self):
        gh = FakeGitHub()
        install_promotion(gh, run=run_row(run_attempt=2),
                          jobs=[job(conclusion="failure")])
        gh.routes["actions/runs/501/attempts/2/jobs"] = {
            "jobs": [job(completed_at="2026-11-10T03:00:00Z")]}
        self.assertRejected(gh)

    def test_attempt_history_is_capped(self):
        gh = FakeGitHub()
        install_promotion(gh, run=run_row(run_attempt=51))
        self.assertRejected(gh)


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

    def test_malformed_main_revision_fails_closed(self):
        for revision in (None, "main", "B" * 40, "b" * 39, 5):
            gh = scenario("2026-11-01T00:00:00Z", "2026-11-02T00:00:00Z")
            gh.routes["git/ref/heads/main"] = {"object": {"type": "commit", "sha": revision}}
            with self.subTest(revision=revision), self.assertRaises(incident_issue.IncidentError):
                decide(gh)

    def test_malformed_or_non_commit_main_ref_fails_closed(self):
        for ref in (None, [], {}, {"object": None}, {"object": []},
                    {"object": {"type": "tag", "sha": MAIN}}):
            gh = scenario("2026-11-01T00:00:00Z", "2026-11-02T00:00:00Z")
            gh.routes["git/ref/heads/main"] = lambda query, value=ref: value
            with self.subTest(ref=ref), self.assertRaises(incident_issue.IncidentError):
                decide(gh)

    def test_moment_requires_an_aware_timestamp(self):
        self.assertEqual(hold.moment("2026-11-10T02:00:00+02:00"),
                         datetime(2026, 11, 10, 0, 0, tzinfo=timezone.utc))
        for raw in (None, "", "2026-11-10T00:00:00", "garbage", 12):
            with self.subTest(raw=raw), self.assertRaises(incident_issue.IncidentError):
                hold.moment(raw)


class MainLifecycleTests(unittest.TestCase):
    """main() against the in-memory GitHub with the real shared issue sink."""

    def run_main(self, gh, needs, args=(), run_id="777"):
        env = {"GITHUB_TOKEN": "tok", "GITHUB_REPOSITORY": REPO, "GITHUB_RUN_ID": run_id,
               "NEEDS_JSON": json.dumps(needs)}
        out = io.StringIO()
        with mock.patch.dict(os.environ, env, clear=True), \
                mock.patch.object(sys, "argv", ["production_hold_incident.py", *args]), \
                mock.patch.object(incident_issue, "_request", gh.request), \
                mock.patch.object(hold, "pending_since", gh.pending_since), \
                contextlib.redirect_stdout(out):
            code = hold.main()
        return code, out.getvalue()

    def held_history(self):
        return scenario("2020-01-01T00:00:00Z", "2020-01-02T00:00:00Z")

    def alert_needs(self):
        return {"guard": {"result": "success", "outputs": {"skip": "ci-not-green"}}}

    def recovery_needs(self):
        return {"guard": {"result": "success", "outputs": {"mode": "deploy"}},
                "production": {"result": "success"}}

    def recovered_history(self, completed_at):
        gh_new = FakeGitHub()
        install_promotion(gh_new, revision=MAIN, completed_at=completed_at)
        finish(gh_new, main=MAIN, promoted=MAIN, status="identical")
        return gh_new

    def test_open_then_comment_then_close(self):
        gh = self.held_history()
        code, out = self.run_main(gh, self.alert_needs())
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out)["action"], "alert")
        self.assertEqual([i["state"] for i in gh.issues.values()], ["open"])
        issue = gh.issues[1]
        self.assertEqual(issue["title"], hold.TITLE)
        self.assertEqual(issue["labels"], ["incident"])
        self.assertIn(incident_issue.marker(hold.KEY), issue["body"])
        self.assertIn("Skip/failure reason: ci-not-green", issue["body"])
        self.assertIn(f"Promotion run: {RUN_URL}", issue["body"])

        # A repeat alert comments on the same issue instead of opening another.
        gh.clock = "2020-01-10T06:00:00Z"
        self.assertEqual(self.run_main(gh, self.alert_needs())[0], 0)
        self.assertEqual(len(gh.issues), 1)
        self.assertEqual(len(gh.comments[1]), 1)
        self.assertTrue(gh.comments[1][0].startswith("Still failing."))

        # Recovery after the last issue activity closes it with a comment.
        recovered = self.recovered_history("2020-01-11T00:00:00Z")
        recovered.issues, recovered.comments = gh.issues, gh.comments
        recovered.comment_times = gh.comment_times
        recovered.next_number = gh.next_number
        code, out = self.run_main(recovered, self.recovery_needs())
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out)["action"], "recovery")
        self.assertEqual(gh.issues[1]["state"], "closed")
        self.assertTrue(gh.comments[1][-1].startswith("Production promotion recovered."))
        self.assertIn("Held since: recovered", gh.comments[1][-1])

        # Nothing open: a second recovery writes nothing.
        before = len(recovered.writes)
        self.assertEqual(self.run_main(recovered, self.recovery_needs())[0], 0)
        self.assertEqual(len(recovered.writes), before)

    def test_watch_recovers_when_terminal_reporter_was_lost(self):
        gh = self.held_history()
        self.assertEqual(self.run_main(gh, self.alert_needs())[0], 0)
        recovered = self.recovered_history("2020-01-11T00:00:00Z")
        recovered.issues, recovered.comments = gh.issues, gh.comments
        recovered.comment_times = gh.comment_times
        code, out = self.run_main(recovered, {})
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out)["action"], "recovery")
        self.assertEqual(gh.issues[1]["state"], "closed")

    def test_delayed_deploy_reporter_cannot_recover_a_newer_rollback(self):
        gh = self.held_history()
        self.assertEqual(self.run_main(gh, self.alert_needs())[0], 0)
        latest = FakeGitHub()
        install_promotion(latest, deployment_id=12, revision=OLD, run_id=888,
                          completed_at="2020-01-11T10:05:00Z",
                          jobs=[job(name="wrangler rollback + readyz gate",
                                    completed_at="2020-01-11T10:05:00Z")],
                          deployment={"payload": {"deploy_id": deploy_id(OLD), "rollback": True}})
        install_promotion(latest, revision=MAIN, run_id=777,
                          completed_at="2020-01-11T10:00:00Z")
        finish(latest, pending_at="2020-01-10T00:00:00Z")
        latest.issues, latest.comments = gh.issues, gh.comments
        latest.comment_times = gh.comment_times
        latest.clock = "2020-01-11T10:06:00Z"
        code, out = self.run_main(latest, self.recovery_needs())
        self.assertEqual(code, 0)
        report = json.loads(out)
        self.assertEqual((report["action"], report["promotion_mode"]), ("alert", "rollback"))
        self.assertEqual((report["promotion_run_id"], report["promotion_attempt"]), ("888", 1))
        self.assertEqual(report["last_promoted_sha"], OLD)
        self.assertEqual(gh.issues[1]["state"], "open")
        self.assertNotIn("PATCH", [method for method, _ in latest.writes])

    def test_watch_recovers_a_stale_alert_written_after_completion(self):
        gh = self.held_history()
        # The watch saw the old baseline before promotion, but its issue write
        # lands after promotion completed. updated_at is NOT failure provenance.
        gh.clock = "2020-01-11T00:05:00Z"
        self.assertEqual(self.run_main(gh, self.alert_needs())[0], 0)
        self.assertEqual(self.run_main(gh, self.alert_needs())[0], 0)
        recovered = self.recovered_history("2020-01-11T00:00:00Z")
        recovered.issues, recovered.comments = gh.issues, gh.comments
        recovered.comment_times = gh.comment_times
        code, out = self.run_main(recovered, {})
        self.assertEqual((code, json.loads(out)["action"]), (0, "recovery"))
        self.assertEqual(gh.issues[1]["state"], "closed")

    def test_recovery_preserves_a_genuinely_newer_generation(self):
        gh = self.held_history()
        self.assertEqual(self.run_main(gh, self.alert_needs())[0], 0)
        # A second hold is observed against the recovered generation itself.
        next_hold = scenario("2020-01-11T00:00:00Z", "2020-01-12T00:00:00Z")
        next_hold.issues, next_hold.comments = gh.issues, gh.comments
        next_hold.comment_times = gh.comment_times
        next_hold.clock = "2020-01-20T00:00:00Z"
        self.assertEqual(self.run_main(next_hold, self.alert_needs())[0], 0)
        recovered = self.recovered_history("2020-01-11T00:00:00Z")
        recovered.issues, recovered.comments = gh.issues, gh.comments
        recovered.comment_times = gh.comment_times
        self.assertEqual(self.run_main(recovered, {})[0], 0)
        self.assertEqual(gh.issues[1]["state"], "open")
        self.assertEqual(len(gh.comments[1]), 1)
        self.assertEqual(recovered.writes, [])

    def test_malformed_observation_cannot_override_the_timestamp_fence(self):
        gh = self.held_history()
        self.assertEqual(self.run_main(gh, self.alert_needs())[0], 0)
        gh.clock = "2020-01-20T00:00:00Z"
        self.assertEqual(self.run_main(gh, self.alert_needs())[0], 0)
        gh.comments[1][0] += '\n<!-- production-hold-observation: {"invalid":true} -->'
        recovered = self.recovered_history("2020-01-11T00:00:00Z")
        recovered.issues, recovered.comments = gh.issues, gh.comments
        recovered.comment_times = gh.comment_times
        self.assertEqual(self.run_main(recovered, {})[0], 0)
        self.assertEqual(gh.issues[1]["state"], "open")
        self.assertEqual(recovered.writes, [])

    def test_large_main_patch_alerts_through_bounded_transport_and_real_git(self):
        from test_production_hold_pending import Repo
        import production_hold_pending

        with tempfile.TemporaryDirectory() as tmp:
            repo = Repo(tmp)
            base = repo.commit({"src/main.rs": "old"})
            head = repo.commit({"src/main.rs": "x" * (incident_issue.MAX_RESPONSE_BYTES + 1)})
            gh = FakeGitHub()
            install_promotion(gh, revision=base, completed_at="2020-01-01T00:00:00Z")
            finish(gh, main=head, promoted=base)
            gh.routes["git/ref/heads/main"] = {"object": {"type": "commit", "sha": head}}
            gh.routes["commits/main"] = {
                "sha": head, "files": [{"patch": "x" * incident_issue.MAX_RESPONSE_BYTES}]}
            seen = []

            def serve(request, timeout):
                parsed = urllib.parse.urlparse(request.full_url)
                path = parsed.path + ("?" + parsed.query if parsed.query else "")
                seen.append(path)
                data = json.loads(request.data) if request.data else None
                payload = gh.request(request.method, path, "tok", data)
                return io.BytesIO(json.dumps(payload).encode())

            def read_pending(promoted, main):
                return production_hold_pending.pending_since(promoted, main, repo.path)

            env = {"GITHUB_TOKEN": "tok", "GITHUB_REPOSITORY": REPO,
                   "GITHUB_RUN_ID": "777", "NEEDS_JSON": json.dumps(self.alert_needs())}
            out = io.StringIO()
            with mock.patch.dict(os.environ, env, clear=True), \
                    mock.patch.object(sys, "argv", ["production_hold_incident.py"]), \
                    mock.patch.object(incident_issue.urllib.request, "build_opener") as opener, \
                    mock.patch.object(hold, "pending_since", side_effect=read_pending), \
                    mock.patch.object(hold, "datetime", wraps=datetime) as clock, \
                    contextlib.redirect_stdout(out):
                clock.now.return_value = (datetime.fromtimestamp(repo.times[2], timezone.utc)
                                          + timedelta(hours=7))
                opener.return_value.open.side_effect = serve
                code = hold.main()
            self.assertEqual(code, 0, out.getvalue())
            self.assertEqual(json.loads(out.getvalue())["action"], "alert")
            self.assertEqual(gh.issues[1]["state"], "open")
            self.assertTrue(any("git/ref/heads/main" in path for path in seen))
            self.assertFalse(any("commits/" in path or "compare/" in path for path in seen))
            self.assertEqual(incident_issue.MAX_RESPONSE_BYTES, 1 << 20)

    def test_newer_body_edit_retaining_markers_is_not_stale(self):
        for edit in (lambda text: text + "\nNew unresolved hold.",
                     lambda text: text.replace("ci-not-green", "new unresolved failure")):
            gh = self.held_history()
            gh.clock = "2020-01-11T00:05:00Z"
            self.assertEqual(self.run_main(gh, self.alert_needs())[0], 0)
            gh.issues[1]["body"] = edit(gh.issues[1]["body"])
            recovered = self.recovered_history("2020-01-11T00:00:00Z")
            recovered.issues, recovered.comments = gh.issues, gh.comments
            recovered.comment_times = gh.comment_times
            self.assertEqual(self.run_main(recovered, {})[0], 0)
            self.assertEqual(gh.issues[1]["state"], "open")
            self.assertEqual(recovered.writes, [])

    def test_newer_comment_edit_retaining_observation_is_not_stale(self):
        gh = self.held_history()
        gh.clock = "2020-01-11T00:05:00Z"
        self.assertEqual(self.run_main(gh, self.alert_needs())[0], 0)
        self.assertEqual(self.run_main(gh, self.alert_needs())[0], 0)
        gh.comments[1][0] += "\nNew unresolved hold."
        recovered = self.recovered_history("2020-01-11T00:00:00Z")
        recovered.issues, recovered.comments = gh.issues, gh.comments
        recovered.comment_times = gh.comment_times
        self.assertEqual(self.run_main(recovered, {})[0], 0)
        self.assertEqual(gh.issues[1]["state"], "open")
        self.assertEqual(recovered.writes, [])

    def test_recovery_retries_after_comment_succeeds_but_close_fails(self):
        gh = self.held_history()
        self.assertEqual(self.run_main(gh, self.alert_needs())[0], 0)
        recovered = self.recovered_history("2020-01-11T00:00:00Z")
        recovered.issues, recovered.comments = gh.issues, gh.comments
        recovered.comment_times = gh.comment_times
        recovered.clock = "2020-01-11T00:05:00Z"
        request = recovered.request

        def fail_close(method, path, token, body=None):
            if method == "PATCH":
                raise incident_issue.IncidentError("incident issue PATCH failed (transport)")
            return request(method, path, token, body)

        with mock.patch.object(recovered, "request", side_effect=fail_close):
            self.assertEqual(self.run_main(recovered, {})[0], 1)
        self.assertEqual(gh.issues[1]["state"], "open")
        self.assertEqual(len(gh.comments[1]), 1)
        self.assertIn(f"{REPO}/actions/runs/501/attempts/1", gh.comments[1][0])
        self.assertEqual(self.run_main(recovered, {}, run_id="999")[0], 0)
        self.assertEqual(gh.issues[1]["state"], "closed")

    def test_edited_or_wrong_generation_recovery_receipt_remains_fenced(self):
        for field, value in ((None, None), ("promotion_run_id", "999"),
                             ("promotion_attempt", 2), ("last_promoted_sha", OLDER),
                             ("last_promoted_at", "2020-01-12T00:00:00Z"),
                             ("promotion_mode", "rollback")):
            gh = self.held_history()
            self.assertEqual(self.run_main(gh, self.alert_needs())[0], 0)
            recovered = self.recovered_history("2020-01-11T00:00:00Z")
            recovered.issues, recovered.comments = gh.issues, gh.comments
            recovered.comment_times = gh.comment_times
            report = decide(recovered)
            if field:
                report[field] = value
            receipt = hold.recovery_body(report, REPO)
            if not field:
                receipt += "\nUnresolved new activity."
            gh.comments[1] = [receipt]
            gh.issues[1]["updated_at"] = "2020-01-11T00:05:00Z"
            self.assertEqual(self.run_main(recovered, {})[0], 0)
            self.assertEqual(gh.issues[1]["state"], "open")
            self.assertEqual(recovered.writes, [])

    def test_dry_run_recovery_does_not_close_an_open_issue(self):
        gh = self.held_history()
        self.assertEqual(self.run_main(gh, self.alert_needs())[0], 0)
        recovered = self.recovered_history("2020-01-11T00:00:00Z")
        recovered.issues, recovered.comments = gh.issues, gh.comments
        recovered.comment_times = gh.comment_times
        code, out = self.run_main(recovered, {}, ["--dry-run"])
        self.assertEqual((code, json.loads(out)["action"]), (0, "recovery"))
        self.assertEqual(gh.issues[1]["state"], "open")
        self.assertEqual(recovered.writes, [])

    def test_recovery_holds_an_issue_with_activity_after_the_promotion(self):
        gh = self.held_history()
        self.run_main(gh, self.alert_needs())
        gh.issues[1]["updated_at"] = "2020-01-20T00:00:00Z"
        gh.comments[1] = ["Unclassified newer incident activity; do not auto-close."]
        recovered = self.recovered_history("2020-01-11T00:00:00Z")
        recovered.issues, recovered.comments = gh.issues, gh.comments
        recovered.comment_times = gh.comment_times
        self.assertEqual(self.run_main(recovered, self.recovery_needs())[0], 0)
        self.assertEqual(gh.issues[1]["state"], "open")
        self.assertEqual(gh.comments[1], ["Unclassified newer incident activity; do not auto-close."])

    def test_unchanged_main_writes_nothing(self):
        gh = FakeGitHub()
        install_promotion(gh, completed_at="2020-01-01T00:00:00Z")
        finish(gh, main=OLD, status="identical")
        code, out = self.run_main(gh, self.alert_needs())
        self.assertEqual((code, json.loads(out)["action"]), (0, "recovery"))
        self.assertEqual(gh.writes, [])

    def test_recent_hold_writes_nothing(self):
        recent = iso(datetime.now(timezone.utc) - timedelta(hours=1))
        gh = scenario(recent, recent)
        code, out = self.run_main(gh, self.alert_needs())
        self.assertEqual((code, json.loads(out)["action"]), (0, "recovery"))
        self.assertEqual(gh.writes, [])

    def test_dry_run_reads_but_never_writes(self):
        gh = self.held_history()
        code, out = self.run_main(gh, self.alert_needs(), ["--dry-run"])
        self.assertEqual((code, json.loads(out)["action"]), (0, "alert"))
        self.assertEqual(gh.writes, [])
        self.assertEqual(gh.issues, {})

    def test_malformed_inputs_fail_closed_without_writes(self):
        base = {"GITHUB_TOKEN": "tok", "GITHUB_REPOSITORY": REPO, "GITHUB_RUN_ID": "777"}
        cases = [{**base, "NEEDS_JSON": "[]"},
                 {**base, "NEEDS_JSON": "{"},
                 {**base, "NEEDS_JSON": '{"a": 1, "a": 2}'},
                 {**base, "NEEDS_JSON": "{}", "GITHUB_RUN_ID": "0"},
                 {**base, "NEEDS_JSON": "{}", "GITHUB_RUN_ID": "abc"},
                 {k: v for k, v in base.items() if k != "GITHUB_TOKEN"} | {"NEEDS_JSON": "{}"},
                 {k: v for k, v in base.items() if k != "GITHUB_RUN_ID"} | {"NEEDS_JSON": "{}"},
                 {"GITHUB_TOKEN": "tok", "NEEDS_JSON": "{}", "GITHUB_RUN_ID": "1"}]
        for env in cases:
            gh = self.held_history()
            out = io.StringIO()
            with self.subTest(env=sorted(env)), \
                    mock.patch.dict(os.environ, env, clear=True), \
                    mock.patch.object(sys, "argv", ["production_hold_incident.py"]), \
                    mock.patch.object(incident_issue, "_request", gh.request), \
                    contextlib.redirect_stdout(out):
                self.assertEqual(hold.main(), 1)
                self.assertTrue(out.getvalue().startswith("::error::"))
                self.assertNotIn("tok", out.getvalue())
                self.assertEqual(gh.writes, [])

    def test_missing_verified_promotion_fails_the_step(self):
        gh = FakeGitHub()
        install_promotion(gh, jobs=[job(conclusion="skipped")])
        finish(gh, pending_at="2020-01-01T00:00:00Z")
        code, out = self.run_main(gh, self.alert_needs())
        self.assertEqual(code, 1)
        self.assertIn("no verified production promotion", out)
        self.assertEqual(gh.writes, [])


ACTIVE_RUNS = "actions/workflows/deploy-production.yml/runs"


def install_active(gh, runs, jobs_by_run):
    gh.routes[ACTIVE_RUNS] = {"workflow_runs": runs}
    for run in runs:
        rows = jobs_by_run.get(run["id"], [])
        gh.routes[f"actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs"] = paged(
            rows, "jobs")


class PendingReasonTests(unittest.TestCase):
    def reason_for(self, gh, own="777"):
        with mock.patch.object(incident_issue, "_request", gh.request):
            return history(gh).pending_reason(own)

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
        for payload in bad:
            gh = FakeGitHub()
            gh.routes[ACTIVE_RUNS] = payload
            with self.subTest(payload=payload), self.assertRaises(incident_issue.IncidentError):
                self.reason_for(gh)

    def test_active_run_query_is_bounded_to_main(self):
        seen = []
        gh = FakeGitHub()
        gh.routes[ACTIVE_RUNS] = lambda query: seen.append(query) or {"workflow_runs": []}
        self.reason_for(gh)
        self.assertEqual(seen, [{"branch": "main", "per_page": "20"}])

    def test_watch_lifecycle_opens_one_incident_naming_the_waiting_run(self):
        gh = scenario("2020-01-01T00:00:00Z", "2020-01-02T00:00:00Z")
        install_active(gh, [{"id": 777, "run_attempt": 1}, {"id": 888, "run_attempt": 1}],
                       {888: [job(status="waiting", conclusion=None)]})
        runner = MainLifecycleTests()
        code, out = runner.run_main(gh, {})
        self.assertEqual(code, 0)
        report = json.loads(out)
        self.assertEqual((report["action"], report["reason"]), ("alert", "production waiting"))
        self.assertEqual(report["run_url"], f"https://github.com/{REPO}/actions/runs/888")
        issue = gh.issues[1]
        self.assertIn("Skip/failure reason: production waiting", issue["body"])
        self.assertIn("actions/runs/888", issue["body"])
        # The next scheduled watch comments on the same issue.
        self.assertEqual(runner.run_main(gh, {})[0], 0)
        self.assertEqual(len(gh.issues), 1)
        self.assertEqual(len(gh.comments[1]), 1)

    def test_pending_lookup_runs_only_for_an_empty_needs_alert(self):
        runner = MainLifecycleTests()
        gh = scenario("2020-01-01T00:00:00Z", "2020-01-02T00:00:00Z")
        # No active-runs route registered: a lookup would raise KeyError.
        needs = {"guard": {"result": "success", "outputs": {"skip": "ci-not-green"}}}
        code, out = runner.run_main(gh, needs)
        self.assertEqual((code, json.loads(out)["reason"]), (0, "ci-not-green"))
        quiet = FakeGitHub()
        install_promotion(quiet, completed_at="2020-01-01T00:00:00Z")
        finish(quiet, main=OLD, status="identical")
        self.assertEqual(json.loads(runner.run_main(quiet, {})[1])["action"], "recovery")


class WorkflowContractTests(unittest.TestCase):
    @staticmethod
    def extract(name):
        text = WORKFLOW_FILE.read_text()
        match = re.search(rf"^  {name}:\n(.*?)(?=^  [a-z0-9-]+:\n)", text, re.S | re.M)
        assert match, f"{name} job is missing"
        return match[1]

    @classmethod
    def setUpClass(cls):
        cls.job = cls.extract("promotion-hold-incident")

    def test_runs_always_on_main_after_every_promotion_stage(self):
        self.assertRegex(self.job, r"(?m)^    if: always\(\) && github\.ref == 'refs/heads/main'$")
        needs = re.search(r"(?m)^    needs: \[(.*)\]$", self.job)
        self.assertIsNotNone(needs)
        self.assertEqual(
            sorted(n.strip() for n in needs[1].split(",")),
            sorted(["guard", "schema-gate", "roles-gate", "soak", "staging-soak-evidence",
                    "recheck", "production"]))

    def test_has_no_deployment_environment_or_lock(self):
        self.assertNotRegex(self.job, r"(?m)^    environment:")
        self.assertNotIn("deployment-lock", self.job)

    def test_permissions_are_least_privilege(self):
        block = re.search(r"(?m)^    permissions:\n((?:      .*\n)+)", self.job)
        self.assertIsNotNone(block)
        granted = dict(re.findall(r"(?m)^      ([a-z-]+): (\w+)", block[1]))
        self.assertEqual(granted, {"contents": "read", "actions": "read",
                                   "deployments": "read", "issues": "write"})

    def test_runs_the_helper_with_needs_and_token_only(self):
        self.assertIn("python3 scripts/production_hold_incident.py", self.job)
        self.assertIn("NEEDS_JSON: ${{ toJSON(needs) }}", self.job)
        self.assertIn("GITHUB_TOKEN: ${{ github.token }}", self.job)
        self.assertIn("persist-credentials: false", self.job)
        self.assertNotIn("secrets.", self.job)

    def test_independent_watch_job_does_not_wait_on_production(self):
        watch = self.extract("promotion-hold-watch")
        self.assertNotRegex(watch, r"(?m)^    needs:")
        self.assertRegex(watch, r"(?m)^    if: .*github\.event_name == 'schedule'")
        self.assertRegex(watch, r"(?m)^    if: .*github\.ref == 'refs/heads/main'")
        self.assertNotRegex(watch, r"(?m)^    environment:")
        self.assertNotIn("deployment-lock", watch)
        self.assertIn("NEEDS_JSON: '{}'", watch)
        self.assertIn("python3 scripts/production_hold_incident.py", watch)
        block = re.search(r"(?m)^    permissions:\n((?:      .*\n)+)", watch)
        self.assertIsNotNone(block)
        self.assertEqual(dict(re.findall(r"(?m)^      ([a-z-]+): (\w+)", block[1])),
                         {"contents": "read", "actions": "read",
                          "deployments": "read", "issues": "write"})
        # Both reporters share one group so issue writes and recovery serialize.
        group = re.search(r"(?m)^      group: (\S+)$", watch)
        self.assertIsNotNone(group)
        self.assertEqual(group[1], "production-hold-incident")
        self.assertEqual(group[1], re.search(r"(?m)^      group: (\S+)$", self.job)[1])
        self.assertNotIn(group[1], ("deploy-production", "staging-lifecycle"))
        self.assertIn("cancel-in-progress: false", watch)

    def test_reporters_have_full_history_for_patch_free_pending_metadata(self):
        for name in ("promotion-hold-watch", "promotion-hold-incident"):
            text = self.extract(name)
            self.assertIn("fetch-depth: 0", text)
            self.assertIn("persist-credentials: false", text)

    def test_helper_constants_match_the_workflow(self):
        text = WORKFLOW_FILE.read_text()
        # The job name renders the guard mode; deploy mode yields the helper's name.
        template = "name: wrangler ${{ needs.guard.outputs.mode }} + readyz gate"
        self.assertIn(template, text)
        self.assertEqual(hold.PRODUCTION_JOB,
                         "wrangler deploy + readyz gate")
        self.assertTrue(hold.WORKFLOW.endswith("deploy-production.yml"))


if __name__ == "__main__":
    unittest.main()
