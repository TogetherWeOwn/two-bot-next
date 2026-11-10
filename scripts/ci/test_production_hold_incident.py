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
        self.clock = "2020-01-10T00:00:00Z"
        self.writes = []
        self.next_number = 1

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
        if method == "POST" and match:
            number = int(match[1])
            self.comments.setdefault(number, []).append(body["body"])
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


def job(name=hold.PRODUCTION_JOB, status="completed", conclusion="success",
        completed_at="2026-11-10T01:00:00Z"):
    return {"name": name, "status": status, "conclusion": conclusion,
            "completed_at": completed_at}


def install_promotion(gh, deployment_id=11, revision=OLD, run_id=501,
                      completed_at="2026-11-10T01:00:00Z", jobs=None, run=None,
                      statuses=None, deployment=None):
    """Register one deployment with its statuses, run and jobs on the fake."""
    row = {"id": deployment_id, "sha": revision, "environment": "production",
           "payload": {"deploy_id": deploy_id(revision)}}
    row.update(deployment or {})
    gh.routes.setdefault("deployments", [])
    gh.routes["deployments"].append(row)
    gh.routes[f"deployments/{deployment_id}/statuses"] = statuses if statuses is not None else [
        {"state": "inactive", "log_url": ""},
        {"state": "success",
         "log_url": f"https://github.com/{REPO}/actions/runs/{run_id}"},
        {"state": "in_progress", "log_url": ""}]
    gh.routes[f"actions/runs/{run_id}"] = run or run_row()
    gh.routes[f"actions/runs/{run_id}/attempts/1/jobs"] = {
        "jobs": jobs if jobs is not None else [job(completed_at=completed_at)]}
    return row


def finish(gh, main=MAIN, promoted=OLD, status="ahead", pending_at=None):
    """Register main head and the compare result, then wrap lists for paging."""
    gh.routes["commits/main"] = {"sha": main}
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
    with mock.patch.object(incident_issue, "_request", gh.request):
        return hold.decide(history(gh), needs or {}, now, RUN_URL)


def scenario(promoted_at, pending_at, **extra):
    gh = FakeGitHub()
    install_promotion(gh, completed_at=promoted_at, **extra)
    return finish(gh, pending_at=pending_at)


class ThresholdTests(unittest.TestCase):
    def test_exactly_six_hours_is_not_an_alert(self):
        gh = scenario("2026-11-01T00:00:00Z", iso(NOW - timedelta(hours=6)))
        report = decide(gh)
        self.assertEqual(report["action"], "none")
        self.assertEqual(report["held_since"], iso(NOW - timedelta(hours=6)))

    def test_one_second_over_six_hours_alerts(self):
        gh = scenario("2026-11-01T00:00:00Z", iso(NOW - timedelta(hours=6, seconds=1)))
        self.assertEqual(decide(gh)["action"], "alert")

    def test_old_pending_commit_does_not_alert_when_promotion_is_recent(self):
        # Pending since 20h ago, but the last deploy finished 2h ago.
        gh = scenario(iso(NOW - timedelta(hours=2)), iso(NOW - timedelta(hours=20)))
        report = decide(gh)
        self.assertEqual(report["action"], "none")
        self.assertEqual(report["held_since"], iso(NOW - timedelta(hours=2)))

    def test_old_promotion_does_not_alert_when_pending_commit_is_recent(self):
        gh = scenario(iso(NOW - timedelta(hours=20)), iso(NOW - timedelta(hours=2)))
        report = decide(gh)
        self.assertEqual(report["action"], "none")
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
        self.assertEqual(report["action"], "none")
        self.assertNotIn("held_since", report)

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
            return history(gh).last_promotion()

    def assertRejected(self, gh):
        finish(gh, pending_at="2026-11-10T02:00:00Z")
        with mock.patch.object(incident_issue, "_request", gh.request), \
                self.assertRaises(incident_issue.IncidentError):
            history(gh).last_promotion()

    def test_inactive_latest_status_still_counts_via_status_history(self):
        gh = FakeGitHub()
        install_promotion(gh, statuses=[
            {"state": "inactive", "log_url": ""},
            {"state": "success", "log_url": f"https://github.com/{REPO}/actions/runs/501"}])
        self.assertEqual(self.promoted(gh), (OLD, hold.moment("2026-11-10T01:00:00Z")))

    def test_rollback_deployment_is_excluded(self):
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
            {"state": "success", "log_url": f"https://github.com/{REPO}/actions/runs/501"},
            {"state": "success", "log_url": f"https://github.com/{REPO}/actions/runs/502"}])
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
            install_promotion(gh, statuses=[{"state": "success", "log_url": url}])
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
            gh.routes["commits/main"] = {"sha": revision}
            with self.subTest(revision=revision), self.assertRaises(incident_issue.IncidentError):
                decide(gh)

    def test_moment_requires_an_aware_timestamp(self):
        self.assertEqual(hold.moment("2026-11-10T02:00:00+02:00"),
                         datetime(2026, 11, 10, 0, 0, tzinfo=timezone.utc))
        for raw in (None, "", "2026-11-10T00:00:00", "garbage", 12):
            with self.subTest(raw=raw), self.assertRaises(incident_issue.IncidentError):
                hold.moment(raw)


class MainLifecycleTests(unittest.TestCase):
    """main() against the in-memory GitHub with the real shared issue sink."""

    def run_main(self, gh, needs, args=()):
        env = {"GITHUB_TOKEN": "tok", "GITHUB_REPOSITORY": REPO, "GITHUB_RUN_ID": "777",
               "NEEDS_JSON": json.dumps(needs)}
        out = io.StringIO()
        with mock.patch.dict(os.environ, env, clear=True), \
                mock.patch.object(sys, "argv", ["production_hold_incident.py", *args]), \
                mock.patch.object(incident_issue, "_request", gh.request), \
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

    def test_recovery_holds_an_issue_with_activity_after_the_promotion(self):
        gh = self.held_history()
        self.run_main(gh, self.alert_needs())
        gh.issues[1]["updated_at"] = "2020-01-20T00:00:00Z"
        recovered = self.recovered_history("2020-01-11T00:00:00Z")
        recovered.issues, recovered.comments = gh.issues, gh.comments
        self.assertEqual(self.run_main(recovered, self.recovery_needs())[0], 0)
        self.assertEqual(gh.issues[1]["state"], "open")
        self.assertEqual(gh.comments, {})

    def test_unchanged_main_writes_nothing(self):
        gh = FakeGitHub()
        install_promotion(gh, completed_at="2020-01-01T00:00:00Z")
        finish(gh, main=OLD, status="identical")
        code, out = self.run_main(gh, self.alert_needs())
        self.assertEqual((code, json.loads(out)["action"]), (0, "none"))
        self.assertEqual(gh.writes, [])

    def test_recent_hold_writes_nothing(self):
        recent = iso(datetime.now(timezone.utc) - timedelta(hours=1))
        gh = scenario(recent, recent)
        code, out = self.run_main(gh, self.alert_needs())
        self.assertEqual((code, json.loads(out)["action"]), (0, "none"))
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


class WorkflowContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        text = WORKFLOW_FILE.read_text()
        match = re.search(r"^  promotion-hold-incident:\n(.*?)(?=^  [a-z0-9-]+:\n)", text,
                          re.S | re.M)
        assert match, "promotion-hold-incident job is missing"
        cls.job = match[1]

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
