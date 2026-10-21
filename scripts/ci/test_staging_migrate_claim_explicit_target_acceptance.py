"""Acceptance: the staging claim requires the explicit migration_target.

Protected acceptance for the paired build card. The staging gate must refuse
a field-less or wrong-target manifest with fixed vocabulary, identically in
outcome to the production gate, and the checked-in fixture must carry the
explicit staging marker. The denylist pins lock the current mirror-image
behavior (staging refuses prod-like targets, production refuses staging-like
targets) as the explicitly-different contract.

Offline only: no Actions dispatch, database or approval.
"""
from copy import deepcopy
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from production_migrate_claim import build_claim as production_build_claim
from production_migrate_claim import Refused as ProductionRefused
from staging_migrate_claim import build_claim, projection_hash, Refused

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "crates/cutover/tests/fixtures"
PLAN = FIXTURES / "staging-migrate-plan.json"
ENV = {
    "GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next",
    "GITHUB_EVENT_NAME": "workflow_dispatch",
    "GITHUB_REF": "refs/heads/main",
    "GITHUB_WORKFLOW_REF": "TogetherWeOwn/two-bot-next/.github/workflows/staging-migrate.yml@refs/heads/main",
    "GITHUB_SHA": "b" * 40,
    "GITHUB_RUN_ID": "37110947191",
    "GITHUB_RUN_ATTEMPT": "2",
    "MODE": "apply",
    "PLAN_RUN_ID": "37110947190",
    "PLAN_MANIFEST_SHA256": "e4a3136346ca546df4f1f4f654322c278ff7df1d5902ec5685cd43036b41beea",
    "STORE_PLAN_MANIFEST_SHA256": "37b4068df78c6e975a6fca576f109260be97c4aa5defa316380be3c0f5c4f593",
    "SOURCE_SHA": "a" * 40,
    "STAGING_HOST": "staging.invalid",
    "STAGING_DATABASE": "two_bot",
    "STAGING_BRANCH_ID": "",
    "RECOVERY_REF": "recovery-review#decision",
    "ACL_REF": "acl-review#decision",
    "EXPECTED_PENDING": "1,9007199254740993",
    "EXPECTED_STORE_PENDING": "400,9007199254740992",
}
PRODUCTION_ENV = {
    "GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next",
    "GITHUB_EVENT_NAME": "workflow_dispatch",
    "GITHUB_REF": "refs/heads/main",
    "GITHUB_WORKFLOW_REF": "TogetherWeOwn/two-bot-next/.github/workflows/production-migrate.yml@refs/heads/main",
    "GITHUB_SHA": "b" * 40,
    "GITHUB_RUN_ID": "37110947191",
    "GITHUB_RUN_ATTEMPT": "2",
    "MODE": "apply",
    "PLAN_RUN_ID": "37110947190",
    "PLAN_MANIFEST_SHA256": "e4a3136346ca546df4f1f4f654322c278ff7df1d5902ec5685cd43036b41beea",
    "STORE_PLAN_MANIFEST_SHA256": "37b4068df78c6e975a6fca576f109260be97c4aa5defa316380be3c0f5c4f593",
    "SOURCE_SHA": "a" * 40,
    "PRODUCTION_HOST": "staging.invalid",
    "PRODUCTION_DATABASE": "two_bot",
    "PRODUCTION_BRANCH_ID": "",
    "RECOVERY_REF": "recovery-review#decision",
    "ACL_REF": "acl-review#decision",
    "EXPECTED_PENDING": "1,9007199254740993",
    "EXPECTED_STORE_PENDING": "400,9007199254740992",
}
NOT_STAGING = "manifest is not a staging plan"


def marked(plan):
    """The stored vector with the explicit staging marker the producer emits."""
    plan = deepcopy(plan)
    plan["migration_target"] = "staging"
    return plan


def unmarked(plan):
    """The stored vector with any marker removed: a hand-crafted manifest."""
    plan = deepcopy(plan)
    plan.pop("migration_target", None)
    return plan


class ExplicitTargetTests(unittest.TestCase):
    def setUp(self):
        self.plan = json.loads(PLAN.read_text())

    def test_field_less_manifest_refuses_with_fixed_vocabulary(self):
        with self.assertRaises(Refused) as refused:
            build_claim(unmarked(self.plan), ENV)
        self.assertEqual(str(refused.exception), NOT_STAGING)

    def test_field_less_manifest_refused_through_the_actual_cli(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as directory:
            manifest, output = Path(directory) / "plan.json", Path(directory) / "claim.json"
            manifest.write_text(json.dumps(unmarked(self.plan)))
            result = subprocess.run(["python3", str(ROOT / "scripts/ci/staging_migrate_claim.py"),
                                     "--manifest", str(manifest), "--output", str(output)],
                                    env={**os.environ, **ENV}, capture_output=True, text=True)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertFalse(output.exists())
            self.assertIn(NOT_STAGING, result.stderr)

    def test_stored_fixture_carries_the_explicit_staging_marker(self):
        self.assertEqual(self.plan.get("migration_target"), "staging")

    def test_wrong_target_refuses_with_fixed_vocabulary(self):
        for bad in ("production", "STAGING", "Staging", "", " staging", "staging ",
                    None, True, 1, ["staging"], {"target": "staging"}):
            with self.subTest(bad=bad):
                plan = marked(self.plan)
                plan["migration_target"] = bad
                with self.assertRaises(Refused) as refused:
                    build_claim(plan, ENV)
                self.assertEqual(str(refused.exception), NOT_STAGING)

    def test_explicit_staging_plan_is_accepted(self):
        claim = build_claim(marked(self.plan), ENV)
        self.assertEqual(claim["target"], self.plan["target"])
        self.assertEqual(claim["expected_pending"], ["1", "9007199254740993"])
        self.assertEqual(claim["plan_manifest_sha256"], ENV["PLAN_MANIFEST_SHA256"])
        self.assertEqual(claim["store_plan_manifest_sha256"], ENV["STORE_PLAN_MANIFEST_SHA256"])

    def test_explicit_marker_leaves_the_projection_unchanged(self):
        self.assertEqual(projection_hash(marked(self.plan)), ENV["PLAN_MANIFEST_SHA256"])
        self.assertEqual(projection_hash(unmarked(self.plan)), ENV["PLAN_MANIFEST_SHA256"])

    def test_refreshed_fixture_vector_builds_end_to_end(self):
        claim = build_claim(json.loads(PLAN.read_text()), ENV)
        self.assertEqual(claim["target"], self.plan["target"])

    def test_staging_denylist_refuses_prod_like_targets(self):
        for host in ("prod-host.invalid", "production.invalid", "db-prod-01.invalid"):
            with self.subTest(host=host):
                plan = marked(self.plan)
                plan["target"]["host"] = host
                with self.assertRaises(Refused) as refused:
                    build_claim(plan, {**ENV, "STAGING_HOST": host})
                self.assertEqual(str(refused.exception), "target is not a bare staging identity")
        for database in ("two-bot-prod", "production"):
            with self.subTest(database=database):
                plan = marked(self.plan)
                plan["target"]["database"] = database
                with self.assertRaises(Refused):
                    build_claim(plan, {**ENV, "STAGING_DATABASE": database})

    def test_mirror_gates_refuse_each_others_canonical_host(self):
        # Production's own valid host is prod-like, so the staging gate refuses
        # it even with the explicit staging marker and matching pins.
        staging_plan = marked(self.plan)
        staging_plan["target"]["host"] = "prod-host.invalid"
        with self.assertRaises(Refused):
            build_claim(staging_plan, {**ENV, "STAGING_HOST": "prod-host.invalid"})
        # Staging's own valid host carries the staging label, so the
        # production gate refuses it even with matching production pins.
        production_plan = unmarked(self.plan)
        production_plan["migration_target"] = "production"
        with self.assertRaises(ProductionRefused):
            production_build_claim(production_plan, PRODUCTION_ENV)


if __name__ == "__main__":
    unittest.main()
