"""Offline production-claim contract tests; no Actions dispatch, database or approval."""
from copy import deepcopy
from datetime import datetime, timedelta, timezone
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

from production_migrate_claim import build_claim, projection_hash, Refused
from test_workflows import load_workflows, production_migrate_errors

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import production_migrate_approval as approval
from staging_rollout import GateError

FIXTURES = ROOT / "crates/cutover/tests/fixtures"
PLAN = FIXTURES / "staging-migrate-plan.json"
ENV = {
    "GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next",
    "GITHUB_EVENT_NAME": "workflow_dispatch",
    "GITHUB_REF": "refs/heads/main",
    "GITHUB_WORKFLOW_REF": "TogetherWeOwn/two-bot-next/.github/workflows/production-migrate.yml@refs/heads/main",
    "GITHUB_SHA": "b" * 40,
    "GITHUB_RUN_ID": "37110947191",
    "GITHUB_RUN_ATTEMPT": "2",
    "MODE": "apply",
    "PLAN_RUN_ID": "37110947190",
    "SOURCE_SHA": "a" * 40,
    "PRODUCTION_HOST": "prod-host.invalid",
    "PRODUCTION_DATABASE": "two_bot",
    "PRODUCTION_BRANCH_ID": "",
    "RECOVERY_REF": "recovery-review#decision",
    "ACL_REF": "acl-review#decision",
    "EXPECTED_PENDING": "1,9007199254740993",
}


def production_plan():
    """The shared synthetic vector, re-targeted at production and re-hashed."""
    plan = json.loads(PLAN.read_text())
    plan["migration_target"] = "production"
    plan["target"] = {"host": ENV["PRODUCTION_HOST"], "database": ENV["PRODUCTION_DATABASE"],
                      "branch_id": ""}
    plan["plan_manifest_sha256"] = projection_hash(plan)
    return plan


class ProductionClaimTests(unittest.TestCase):
    def setUp(self):
        self.plan = production_plan()
        self.env = {**ENV, "PLAN_MANIFEST_SHA256": self.plan["plan_manifest_sha256"],
                    "STORE_PLAN_MANIFEST_SHA256": self.plan["store_plan_manifest_sha256"],
                    "EXPECTED_STORE_PENDING": "400,9007199254740992"}

    def test_builds_production_claim(self):
        claim = build_claim(self.plan, self.env)
        self.assertEqual(claim["kind"], "production-migrate-apply-claim")
        self.assertEqual(claim["workflow_path"], ".github/workflows/production-migrate.yml")
        self.assertEqual(claim["environment_name"], "production-migrate-apply")
        self.assertEqual(claim["target"], self.plan["target"])
        self.assertEqual(claim["expected_pending"], ["1", "9007199254740993"])
        self.assertEqual(claim["plan_manifest_sha256"], self.plan["plan_manifest_sha256"])
        self.assertEqual(claim["expected_store_pending"], ["400", "9007199254740992"])
        self.assertEqual(claim["store_plan_manifest_sha256"],
                         self.plan["store_plan_manifest_sha256"])
        # A wrong store list or hash refuses like the cutover binding.
        with self.assertRaises(Refused):
            build_claim(self.plan, {**self.env, "EXPECTED_STORE_PENDING": "400"})
        with self.assertRaises(Refused):
            build_claim(self.plan, {**self.env, "STORE_PLAN_MANIFEST_SHA256": "c" * 64})

    def test_refuses_staging_manifest(self):
        staging = json.loads(PLAN.read_text())
        staging["plan_manifest_sha256"] = projection_hash(
            {**staging, "migration_target": "staging"})
        staging["migration_target"] = "staging"
        with self.assertRaises(Refused):
            build_claim(staging, {**self.env,
                                  "PLAN_MANIFEST_SHA256": staging["plan_manifest_sha256"]})
        # A manifest with no target marker at all is not a production plan,
        # even when its target is production-like and the pins match, so the
        # refusal can only come from the marker check.
        unmarked = json.loads(PLAN.read_text())
        unmarked.pop("migration_target", None)
        unmarked["target"] = {"host": ENV["PRODUCTION_HOST"],
                              "database": ENV["PRODUCTION_DATABASE"],
                              "branch_id": ""}
        with self.assertRaises(Refused) as refused:
            build_claim(unmarked, self.env)
        self.assertEqual(str(refused.exception), "manifest is not a production plan")

    def test_refuses_staging_hosts(self):
        for host in ("ep-staging-example.us-east-2.aws.neon.tech",
                     "ep-test-pooler.us-east-2.aws.neon.tech",
                     "staging-host.invalid", "agent-testdb"):
            with self.subTest(host=host):
                plan = deepcopy(self.plan)
                plan["target"]["host"] = host
                plan["plan_manifest_sha256"] = projection_hash(plan)
                env = {**self.env, "PRODUCTION_HOST": host,
                       "PLAN_MANIFEST_SHA256": plan["plan_manifest_sha256"]}
                with self.assertRaises(Refused):
                    build_claim(plan, env)

    def test_accepts_production_branch_pin(self):
        # Production mirrors the staging pin: a PlanetScale manifest carrying
        # the pinned branch id binds when the request names the same pin.
        host = "abc-useast1-1.horizon.psdb.cloud"
        plan = deepcopy(self.plan)
        plan["target"] = {"host": host, "database": "postgres", "branch_id": "cnfixture01"}
        plan["plan_manifest_sha256"] = projection_hash(plan)
        env = {**self.env, "PRODUCTION_HOST": host, "PRODUCTION_DATABASE": "postgres",
               "PRODUCTION_BRANCH_ID": "cnfixture01",
               "PLAN_MANIFEST_SHA256": plan["plan_manifest_sha256"]}
        claim = build_claim(plan, env)
        self.assertEqual(claim["target"], plan["target"])

    def test_refuses_branch_pin_mismatch(self):
        # A PlanetScale manifest without the pin, or a request pin that does
        # not match the manifest, refuses.
        host = "abc-useast1-1.horizon.psdb.cloud"
        plan = deepcopy(self.plan)
        plan["target"] = {"host": host, "database": "postgres", "branch_id": "cnfixture01"}
        plan["plan_manifest_sha256"] = projection_hash(plan)
        base = {**self.env, "PRODUCTION_HOST": host, "PRODUCTION_DATABASE": "postgres",
                "PLAN_MANIFEST_SHA256": plan["plan_manifest_sha256"]}
        with self.assertRaises(Refused):
            build_claim(plan, {**base, "PRODUCTION_BRANCH_ID": "otherbranch"})
        unpinned = deepcopy(self.plan)
        unpinned["target"] = {"host": host, "database": "postgres", "branch_id": ""}
        unpinned["plan_manifest_sha256"] = projection_hash(unpinned)
        with self.assertRaises(Refused):
            build_claim(unpinned, {**self.env, "PRODUCTION_HOST": host,
                                   "PRODUCTION_DATABASE": "postgres",
                                   "PRODUCTION_BRANCH_ID": "",
                                   "PLAN_MANIFEST_SHA256": unpinned["plan_manifest_sha256"]})

    def test_refuses_wrong_workflow_identity(self):
        with self.assertRaises(Refused):
            build_claim(self.plan, {**self.env, "GITHUB_WORKFLOW_REF":
                "TogetherWeOwn/two-bot-next/.github/workflows/staging-migrate.yml@refs/heads/main"})
        with self.assertRaises(Refused):
            build_claim(self.plan, {**self.env, "MODE": "plan"})

    def test_production_workflow_pins_claim_transport(self):
        workflow = load_workflows()["production-migrate.yml"]
        self.assertEqual(production_migrate_errors(workflow), [])
        claim = workflow["jobs"]["claim"]
        self.assertNotIn("environment", claim)
        self.assertEqual(workflow["jobs"]["apply"]["needs"], ["plan", "claim"])


class ProductionApprovalTests(unittest.TestCase):
    def setUp(self):
        self.plan = production_plan()
        self.plan.update(role_verified_connections=1, audit={"verify_findings": [], "connection_usage": {"used": 10}})
        self.env = {**ENV, "PLAN_MANIFEST_SHA256": self.plan["plan_manifest_sha256"],
                    "STORE_PLAN_MANIFEST_SHA256": self.plan["store_plan_manifest_sha256"],
                    "EXPECTED_STORE_PENDING": "400,9007199254740992",
                    "GITHUB_WORKFLOW_SHA": ENV["GITHUB_SHA"], "GITHUB_RUN_ATTEMPT": "1",
                    "GITHUB_REF_PROTECTED": "true", "GITHUB_ACTOR_ID": "42"}
        self.env["GITHUB_REPOSITORY_ID"] = "1396224242"
        self.claim = build_claim(self.plan, self.env)
        plan_id, apply_id = int(ENV["PLAN_RUN_ID"]), int(ENV["GITHUB_RUN_ID"])
        repo = {"full_name": ENV["GITHUB_REPOSITORY"]}
        self.check = {"name": approval.CHECK_NAME, "head_sha": ENV["GITHUB_SHA"], "app": {"id": approval.APP_ID},
                      "id": 1234, "status": "completed", "conclusion": "success",
                      "completed_at": "2026-11-24T10:00:00Z", "external_id": f"production-apply:{plan_id}:" + "a" * 64}
        self.checks = {"total_count": 1, "check_runs": [self.check]}
        self.settings = {"name": approval.ENVIRONMENT, "can_admins_bypass": False,
                         "deployment_branch_policy": {"custom_branch_policies": True, "protected_branches": False},
                         "protection_rules": [{"type": "branch_policy"}]}
        self.history = {"total_count": 2, "workflow_runs": [{"id": plan_id}, {"id": apply_id}]}
        self.responses = {
            approval.POLICY: self.settings,
            f"{approval.POLICY}/deployment-branch-policies?per_page=2": {
                "total_count": 1, "branch_policies": [{"type": "branch", "name": "main"}]},
            f"actions/runs/{apply_id}": {
                "path": approval.WORKFLOW_PATH, "event": "workflow_dispatch", "head_branch": "main",
                "head_sha": ENV["GITHUB_SHA"], "run_attempt": 1, "repository": repo, "head_repository": repo,
                "id": apply_id, "actor": {"id": 42}, "triggering_actor": {"id": 42}, "created_at": "2026-11-24T10:01:00Z"},
            f"actions/runs/{plan_id}": {"created_at": "2026-11-24T09:00:00Z"},
            ("actions/workflows/production-migrate.yml/runs?event=workflow_dispatch&branch=main"
             "&created=%3E%3D2026-11-24T09%3A00%3A00Z&per_page=30"): self.history,
            f"commits/{ENV['GITHUB_SHA']}/check-runs?check_name={approval.CHECK_NAME}&filter=all&per_page=30": self.checks,
        }
        self.enterContext(patch.object(approval, "authenticated_plan", return_value=(
            self.plan, {"run_id": plan_id, "archive_digest": "sha256:" + "a" * 64})))
        self.now = datetime(2026, 11, 24, 11, tzinfo=timezone.utc)

    def authorize(self, fresh=None, env=None):
        return approval.authorize(fresh or self.plan, self.claim, env or self.env, self.now, self.responses.__getitem__)

    def test_exact_app_approval_admits_unchanged_plan(self):
        fresh = deepcopy(self.plan)
        fresh["audit"]["connection_usage"]["used"] = 11
        evidence = self.authorize(fresh)
        self.assertEqual(evidence["check_id"], self.check["id"])

    def test_refuses_fresh_ledger_or_role_audit_drift(self):
        for field in ("ledger_before", "ledger_store_before", "audit"):
            with self.subTest(field=field):
                fresh = deepcopy(self.plan)
                if field == "audit":
                    fresh["audit"]["verify_findings"] = ["drift"]
                else:
                    fresh[field] = fresh[field.replace("before", "after")] = [{"version": 2, "success": True}]
                with self.assertRaisesRegex(GateError, "production_migration_plan_changed"):
                    self.authorize(fresh)

    def test_refuses_wrong_app_or_duplicate_approval(self):
        self.check["app"]["id"] = 15368
        with self.assertRaises(GateError):
            self.authorize()
        self.check["app"]["id"] = approval.APP_ID
        self.checks.update(total_count=2, check_runs=[self.check, self.check])
        with self.assertRaises(GateError):
            self.authorize()

    def test_refuses_unprotected_environment(self):
        self.settings["can_admins_bypass"] = True
        with self.assertRaisesRegex(GateError, "production_migration_environment_unprotected"):
            self.authorize()
        self.settings["can_admins_bypass"] = False
        self.responses[f"{approval.POLICY}/deployment-branch-policies?per_page=2"]["branch_policies"][0]["name"] = "*"
        with self.assertRaisesRegex(GateError, "production_migration_environment_unprotected"):
            self.authorize()

    def test_refuses_reuse_after_an_apply_attempt(self):
        self.history["workflow_runs"].append({"id": 456})
        self.history["total_count"] += 1
        attempted = {"name": "production-migrate (apply)", "started_at": "2026-11-24T10:00:55Z",
                     "conclusion": "failure"}
        self.responses["actions/runs/456/attempts/1/jobs?per_page=4"] = {"total_count": 1, "jobs": [attempted]}
        with self.assertRaisesRegex(GateError, "production_plan_superseded_by_apply_attempt"):
            self.authorize()
        attempted.update(started_at=None, conclusion="skipped")
        self.assertEqual(self.authorize()["check_id"], 1234)

    def test_recheck_refuses_a_revoked_or_expired_approval(self):
        evidence = self.authorize()
        env = {**self.env, **{f"APPROVAL_{key.upper()}": str(value) for key, value in evidence.items()}}
        approval.recheck(self.plan, env, self.now, self.responses.__getitem__)
        with self.assertRaisesRegex(GateError, "independent_production_approval_stale"):
            approval.recheck(self.plan, env, self.now + timedelta(days=1), self.responses.__getitem__)
        self.check["id"] = 2345
        with self.assertRaisesRegex(GateError, "independent_production_approval_changed"):
            approval.recheck(self.plan, env, self.now, self.responses.__getitem__)
        self.check["conclusion"] = "failure"
        with self.assertRaisesRegex(GateError, "independent_production_approval_invalid"):
            approval.recheck(self.plan, env, self.now, self.responses.__getitem__)


if __name__ == "__main__":
    unittest.main()
