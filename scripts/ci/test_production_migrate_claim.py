"""Offline production-claim contract tests; no Actions dispatch, database or approval."""
from copy import deepcopy
from datetime import datetime, timezone
from hashlib import sha256
from io import BytesIO
import json
from pathlib import Path
import sys
import unittest
from zipfile import ZipFile

from production_migrate_claim import build_claim, projection_hash, Refused
from test_workflows import load_workflows, production_migrate_errors

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import production_migrate_approval as approval
from rollback_proof import ProofError
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
        rerunnable = deepcopy(workflow)
        rerunnable["jobs"]["apply"]["if"] = "github.ref == 'refs/heads/main' && inputs.mode == 'apply'"
        self.assertTrue(production_migrate_errors(rerunnable))
        claim["steps"][2]["run"] = claim["steps"][2]["run"].rsplit("python3", 1)[0]
        self.assertTrue(production_migrate_errors(workflow))


class ProductionApprovalTests(unittest.TestCase):
    def setUp(self):
        self.plan = production_plan()
        self.plan.update(role_verified_connections=1, audit={"verify_findings": [], "connection_usage": {"used": 10}})
        self.env = {**ENV, "PLAN_MANIFEST_SHA256": self.plan["plan_manifest_sha256"],
                    "STORE_PLAN_MANIFEST_SHA256": self.plan["store_plan_manifest_sha256"],
                    "EXPECTED_STORE_PENDING": "400,9007199254740992",
                    "GITHUB_WORKFLOW_SHA": ENV["GITHUB_SHA"], "GITHUB_RUN_ATTEMPT": "1",
                    "GITHUB_REF_PROTECTED": "true", "GITHUB_ACTOR_ID": "42"}
        self.claim = build_claim(self.plan, self.env)
        archive = BytesIO()
        with ZipFile(archive, "w") as writer:
            writer.writestr(f"{approval.ARTIFACT}.json", json.dumps(self.plan))
        raw = archive.getvalue()
        digest = sha256(raw).hexdigest()
        producing = f"actions/runs/{ENV['PLAN_RUN_ID']}"
        applying = f"actions/runs/{ENV['GITHUB_RUN_ID']}"
        run = {"path": approval.WORKFLOW_PATH, "event": "workflow_dispatch",
               "head_branch": "main", "head_sha": ENV["GITHUB_SHA"], "run_attempt": 1,
               "repository": {"full_name": ENV["GITHUB_REPOSITORY"]},
               "head_repository": {"full_name": ENV["GITHUB_REPOSITORY"]}}
        self.check = {
            "name": approval.CHECK_NAME, "head_sha": ENV["GITHUB_SHA"], "app": {"id": approval.APP_ID},
            "status": "completed", "conclusion": "success", "completed_at": "2026-11-24T10:00:00Z",
            "external_id": f"production-apply:{ENV['PLAN_RUN_ID']}:{digest}",
        }
        self.checks = {"total_count": 1, "check_runs": [self.check]}
        self.responses = {
            applying: {**run, "id": int(ENV["GITHUB_RUN_ID"]), "actor": {"id": 42},
                       "triggering_actor": {"id": 42}, "created_at": "2026-11-24T10:01:00Z"},
            producing: {**run, "id": int(ENV["PLAN_RUN_ID"]), "status": "completed", "conclusion": "success"},
            f"{producing}/attempts/1/jobs?per_page=4": {"total_count": 3, "jobs": [
                {"name": f"production-migrate ({job})", "conclusion": conclusion}
                for job, conclusion in (("plan", "success"), ("claim", "skipped"), ("apply", "skipped"))
            ]},
            f"{producing}/artifacts?per_page=2": {"total_count": 1, "artifacts": [
                {"id": 99, "expired": False, "name": approval.ARTIFACT,
                 "size_in_bytes": len(raw), "digest": f"sha256:{digest}"},
            ]},
            "actions/artifacts/99/zip": raw,
            f"commits/{ENV['GITHUB_SHA']}/check-runs?check_name={approval.CHECK_NAME}&filter=all&per_page=100": self.checks,
        }
        self.now = datetime(2026, 11, 24, 11, tzinfo=timezone.utc)

    def authorize(self, fresh=None, env=None):
        def api(path, *, binary=False):
            value = self.responses[path]
            self.assertEqual(isinstance(value, bytes), binary)
            return value

        return approval.authorize(fresh or self.plan, self.claim, env or self.env, self.now, api)

    def test_exact_app_approval_admits_unchanged_plan(self):
        fresh = deepcopy(self.plan)
        fresh["audit"]["connection_usage"]["used"] = 11
        fresh["expected_pending"] = []
        fresh["role_verified_connections"] = 2
        self.assertEqual(self.authorize(fresh), 99)

    def test_refuses_fresh_ledger_or_role_audit_drift(self):
        for field in ("ledger_before", "ledger_store_before", "audit"):
            with self.subTest(field=field):
                fresh = deepcopy(self.plan)
                if field == "audit":
                    fresh["audit"]["verify_findings"] = ["drift"]
                else:
                    fresh[field] = fresh[field.replace("before", "after")] = []
                with self.assertRaisesRegex(GateError, "production_migration_plan_changed"):
                    self.authorize(fresh)

    def test_refuses_wrong_stale_or_duplicate_approval(self):
        good = deepcopy(self.check)
        for changed in ({"app": {"id": 15368}}, {"external_id": "staging-apply:8"},
                        {"conclusion": "failure"}, {"completed_at": "2026-11-24T10:02:00Z"},
                        {"completed_at": "2026-11-23T10:00:00Z"}):
            with self.subTest(changed=changed):
                self.check.update({**good, **changed})
                with self.assertRaises(GateError):
                    self.authorize()
        self.check.update(good)
        self.checks.update(total_count=2, check_runs=[self.check, self.check])
        with self.assertRaises(GateError):
            self.authorize()

    def test_refuses_replayed_dispatch_and_unverified_artifact(self):
        with self.assertRaisesRegex(GateError, "production_approval_dispatch_untrusted"):
            self.authorize(env={**self.env, "GITHUB_RUN_ATTEMPT": "2"})
        self.responses["actions/artifacts/99/zip"] += b"x"
        with self.assertRaises((GateError, ProofError)):
            self.authorize()


if __name__ == "__main__":
    unittest.main()
