"""Synthetic GitHub Environment approvals for the staging migration gate."""

from copy import deepcopy
from datetime import datetime, timezone
import json
import sys
import unittest

from test_staging_migrate_claim import ENV, PLAN, ROOT

sys.path.insert(0, str(ROOT / "scripts"))
import staging_migrate_approval as approval


class StagingMigrationApprovalTests(unittest.TestCase):
    def setUp(self):
        self.plan = json.loads(PLAN.read_text())
        self.plan["audit"] = {"verify_findings": []}
        self.env = {**ENV, "GITHUB_RUN_ATTEMPT": "1", "GITHUB_ACTOR_ID": "11",
                    "GITHUB_WORKFLOW_SHA": ENV["GITHUB_SHA"], "GITHUB_REF_PROTECTED": "true"}
        self.now = datetime(2026, 11, 24, 12, tzinfo=timezone.utc)
        self.route = f"actions/runs/{self.env['GITHUB_RUN_ID']}"
        self.actor = {"type": "User", "id": 11}
        self.reviewer = {"type": "User", "id": 22}
        self.settings = {"name": approval.ENVIRONMENT, "id": 33, "can_admins_bypass": False,
                         "deployment_branch_policy": {"custom_branch_policies": True, "protected_branches": False},
                         "protection_rules": [{"type": "branch_policy"}, {
                             "type": "required_reviewers", "prevent_self_review": True,
                             "reviewers": [{"type": "User", "reviewer": self.reviewer}],
                         }]}
        self.review = {"state": "approved", "user": self.reviewer,
                       "environments": [{"name": approval.ENVIRONMENT, "id": 33}]}
        self.responses = {
            approval.POLICY: self.settings,
            f"{approval.POLICY}/deployment-branch-policies?per_page=2": {
                "total_count": 1, "branch_policies": [{"type": "branch", "name": "main"}]},
            self.route: {"id": int(self.env["GITHUB_RUN_ID"]), "path": approval.PLAN_WORKFLOW,
                         "event": "workflow_dispatch", "head_branch": "main", "head_sha": self.env["GITHUB_SHA"],
                         "actor": self.actor, "triggering_actor": self.actor, "run_attempt": 1,
                         "created_at": "2026-11-24T11:00:00Z", "repository": {"full_name": approval.REPO},
                         "head_repository": {"full_name": approval.REPO}},
            f"{self.route}/approvals": [self.review],
        }

    def prepare(self, *, env=None, producing=None):
        env = self.env if env is None else env
        producing = self.plan if producing is None else producing
        artifact = {"run_id": int(env["PLAN_RUN_ID"]), "artifact_id": 44, "archive_digest": "sha256:" + "a" * 64}
        return approval.prepare(self.plan, approval.build_claim(self.plan, env), env,
                                self.responses.__getitem__, lambda _: (producing, artifact))

    def authorize(self):
        digest, plan_digest, _ = self.prepare()
        self.env.update(APPROVAL_DIGEST=digest, APPROVAL_PLAN_SHA256=plan_digest)
        self.review["comment"] = approval.APPROVE + digest

    def verify(self):
        return approval.verify(self.env, self.plan, self.responses.__getitem__, self.now)

    def test_independent_exact_native_approval_succeeds(self):
        self.plan["audit"]["verify_findings"] = ["missing object: public.pending_fixture_table"]
        self.authorize()
        self.assertEqual(self.verify(), self.reviewer["id"])
        later_request = {**self.env, "GITHUB_RUN_ID": "37110947192"}
        self.assertNotEqual(self.prepare(env=later_request)[0], self.env["APPROVAL_DIGEST"])

    def test_request_refuses_altered_plan_and_missing_designation(self):
        for change in (lambda p: p["target"].update(branch_id="other"),
                       lambda p: p.update(recovery_evidence_ref="other"),
                       lambda p: p["audit"].update(verify_findings=["drift"]),
                       lambda p: p.update(ledger_before=[{"success": True}], ledger_after=[{"success": True}])):
            with self.subTest(change=change):
                altered = deepcopy(self.plan)
                change(altered)
                with self.assertRaises((approval.GateError, approval.Refused)):
                    self.prepare(producing=altered)
        self.settings["protection_rules"].pop()
        with self.assertRaises(approval.GateError):
            self.prepare()

    def test_refuses_missing_replayed_stale_or_unauthorized_approval(self):
        for mutation in (
            lambda: self.responses[f"{self.route}/approvals"].clear(),
            lambda: self.review.update(comment=approval.APPROVE + "0" * 64),
            lambda: self.review.update(state="rejected"),
            lambda: self.review.update(user={"type": "User", "id": 99}),
            lambda: self.actor.update(id=self.reviewer["id"]) or self.env.update(GITHUB_ACTOR_ID="22"),
            lambda: self.settings.update(can_admins_bypass=True),
            lambda: self.settings["protection_rules"][1].update(prevent_self_review=False),
            lambda: self.env.update(GITHUB_RUN_ATTEMPT="2"),
            lambda: self.responses[self.route].update(created_at="2026-11-23T12:20:00Z"),
            lambda: self.review["environments"][0].update(id=34),
            lambda: self.plan.update(acl_plan_ref="changed"),
        ):
            with self.subTest(mutation=mutation):
                self.setUp()
                self.authorize()
                mutation()
                with self.assertRaises(approval.GateError):
                    self.verify()
