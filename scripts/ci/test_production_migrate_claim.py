"""Offline production-claim contract tests; no Actions dispatch, database or approval."""
from copy import deepcopy
import json
from pathlib import Path
import unittest

from production_migrate_claim import build_claim, projection_hash, Refused
from test_workflows import load_workflows, production_migrate_errors

ROOT = Path(__file__).resolve().parents[2]
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
        self.env = {**ENV, "PLAN_MANIFEST_SHA256": self.plan["plan_manifest_sha256"]}

    def test_builds_production_claim(self):
        claim = build_claim(self.plan, self.env)
        self.assertEqual(claim["kind"], "production-migrate-apply-claim")
        self.assertEqual(claim["workflow_path"], ".github/workflows/production-migrate.yml")
        self.assertEqual(claim["environment_name"], "production-migrate-apply")
        self.assertEqual(claim["target"], self.plan["target"])
        self.assertEqual(claim["expected_pending"], ["1", "9007199254740993"])
        self.assertEqual(claim["plan_manifest_sha256"], self.plan["plan_manifest_sha256"])

    def test_refuses_staging_manifest(self):
        staging = json.loads(PLAN.read_text())
        staging["plan_manifest_sha256"] = projection_hash(
            {**staging, "migration_target": "staging"})
        staging["migration_target"] = "staging"
        with self.assertRaises(Refused):
            build_claim(staging, {**self.env,
                                  "PLAN_MANIFEST_SHA256": staging["plan_manifest_sha256"]})
        # A manifest with no target marker at all is not a production plan.
        unmarked = json.loads(PLAN.read_text())
        with self.assertRaises(Refused):
            build_claim(unmarked, self.env)

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

    def test_refuses_branched_production_target(self):
        # Production takes no branch pin yet: a manifest carrying one fails
        # closed until a production branch pin lands.
        for branch_id in ("cnfixture01", "main"):
            with self.subTest(branch_id=branch_id):
                plan = deepcopy(self.plan)
                plan["target"]["branch_id"] = branch_id
                plan["plan_manifest_sha256"] = projection_hash(plan)
                with self.assertRaises(Refused):
                    build_claim(plan, {**self.env,
                                       "PLAN_MANIFEST_SHA256": plan["plan_manifest_sha256"]})

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


if __name__ == "__main__":
    unittest.main()
