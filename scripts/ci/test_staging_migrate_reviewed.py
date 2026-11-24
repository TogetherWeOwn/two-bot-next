"""Offline tests for protected, one-use staging migration requests."""

from datetime import datetime, timezone
import json
import sys
import unittest

from test_workflows import ROOT, load_workflows, reviewed_migrate_errors

sys.path.insert(0, str(ROOT / "scripts"))
from staging_migrate_reviewed import GateError, authorize, compare_plans, parse_request


class ReviewedMigrationTests(unittest.TestCase):
    def setUp(self):
        sha, parent, head = (letter * 40 for letter in "abc")
        self.path = f".github/staging-apply/{'a' * 32}.json"
        repo = {"full_name": "TogetherWeOwn/two-bot-next"}
        self.env = {"GITHUB_REPOSITORY": repo["full_name"], "GITHUB_SHA": sha, "GITHUB_WORKFLOW_SHA": sha,
                    "GITHUB_REPOSITORY_ID": "1396224242", "GITHUB_ACTOR_ID": "11",
                    "GITHUB_REF_PROTECTED": "true", "GITHUB_REF": "refs/heads/main", "GITHUB_EVENT_NAME": "push",
                    "GITHUB_WORKFLOW_REF": f"{repo['full_name']}/.github/workflows/staging-migrate-reviewed.yml@refs/heads/main",
                    "GITHUB_RUN_ID": "20", "GITHUB_RUN_ATTEMPT": "1"}
        self.event = {"before": parent, "after": sha, "ref": "refs/heads/main", "forced": False,
                      "created": False, "deleted": False}
        self.merged_at = "2026-11-24T03:00:00Z"
        self.pull = {"number": 10, "state": "closed", "merged": True,
                     "merged_at": self.merged_at, "user": {"id": 11}, "merged_by": {"id": 11},
                     "base": {"ref": "main", "repo": repo}, "head": {"sha": head, "repo": repo}}
        self.merge = {"event": "merged", "commit_id": sha, "created_at": self.merged_at, "actor": {"id": 11}}
        self.plan = {"mode": "plan", "migration_target": "staging", "role": "two_bot_migrator_ro",
                     "source_sha": parent, "pending_before": [475], "pending_store_before": [],
                     "recovery_evidence_ref": "staging-recovery-capture/31/artifacts/32", "ledger_before": [481],
                     "audit": {"verify_findings": [], "connection_usage": {}}, "role_verified_connections": 2}
        self.request = {"plan_run_id": "25", "plan_archive_digest": "sha256:" + "d" * 64,
                        "recovery_archive_digest": "sha256:" + "e" * 64}
        self.runs = {"total_count": 1, "workflow_runs": [{"id": 20}]}
        self.approval = {"app": {"id": 4685085}, "name": "staging-migration-approval", "head_sha": head,
                         "external_id": "staging-apply:10", "conclusion": "success", "completed_at": self.merged_at}
        self.routes = {
            f"actions/workflows/staging-migrate-reviewed.yml/runs?event=push&head_sha={sha}&branch=main&per_page=2": self.runs,
            f"commits/{sha}/pulls?per_page=2": [{"number": 10}],
            "pulls/10": self.pull,
            "issues/10/events?per_page=100": [self.merge],
            f"commits/{head}/check-runs?check_name=staging-migration-approval&filter=all&per_page=10": {
                "total_count": 1, "check_runs": [self.approval]},
            f"commits/{head}/check-runs?check_name=ci-ok&per_page=10": {"check_runs": [
                {"app": {"id": 15368}, "name": "ci-ok", "head_sha": head, "conclusion": "success"}]},
            "actions/runs/31": {"id": 31, "run_attempt": 1, "head_branch": "main", "head_sha": parent,
                                "path": ".github/workflows/staging-recovery-capture.yml", "event": "workflow_dispatch",
                                "conclusion": "success", "repository": repo, "head_repository": repo},
            "actions/artifacts/32": {"name": "staging-recovery-capture", "expired": False, "workflow_run": {"id": 31},
                                     "digest": self.request["recovery_archive_digest"]},
        }
        self.git = {"show": json.dumps(self.request).encode(), "diff": f"A\t{self.path}".encode(), "log": b"",
                    "ls-tree": b"100644 blob id", "cat-file": b"512", "fetch": b"", "merge-base": b""}
        self.run_git = lambda op, *args: (
            parent.encode() if args[:2] == ("-s", "--format=%P") else
            head.encode() if args == ("FETCH_HEAD",) else b"same-blob" if op == "rev-parse" else self.git[op])

    def authorize(self):
        return authorize(self.env, self.event, self.routes.__getitem__, datetime(2026, 11, 24, 4, tzinfo=timezone.utc),
                         self.run_git, lambda _: (self.plan, {"archive_digest": self.request["plan_archive_digest"]}))

    def test_protected_request_and_fresh_plan_succeed(self):
        self.assertEqual(self.authorize()["plan_run_id"], "25")
        compare_plans(self.plan, {**self.plan, "audit": {"verify_findings": [], "connection_usage": {"used": 10}}})
        with self.assertRaisesRegex(GateError, "staging_plan_changed"):
            compare_plans(self.plan, {**self.plan, "ledger_before": [475, 481]})

    def test_refuses_replayed_expired_or_unmerged_requests(self):
        for mutation in (
            lambda: self.env.update(GITHUB_RUN_ATTEMPT="2"), lambda: self.runs.update(total_count=2),
            lambda: self.git.update(log=b"already-committed"), lambda: self.pull.update(state="open"),
            lambda: self.merge.update(commit_id="e" * 40),
            lambda: self.git.update(diff=self.git["diff"] + b"\nM\tDockerfile"),
            lambda: self.approval["app"].update(id=15368), lambda: self.approval.update(external_id="staging-apply:9"),
            lambda: self.pull.update(merged_at="2026-11-23T01:00:00Z"),
            lambda: self.routes["actions/artifacts/32"].update(digest="sha256:" + "f" * 64),
        ):
            with self.subTest(mutation=mutation):
                self.setUp()
                mutation()
                with self.assertRaises(GateError):
                    self.authorize()

    def test_request_fixtures_and_workflow_policy(self):
        for request in (ROOT / ".github/staging-apply").glob("*.json"):
            parse_request(request.read_bytes())
        workflow = load_workflows()["staging-migrate-reviewed.yml"]
        self.assertEqual(reviewed_migrate_errors(workflow), [])
        workflow["jobs"]["apply"].update(needs=["authorize"])
        self.assertTrue(reviewed_migrate_errors(workflow))
