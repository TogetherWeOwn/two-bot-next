"""Failing-first acceptance for Land #980 slice A (TOG-25230).

Offline claim-layer pin for the PlanetScale production branch gate:
bare-host (Neon/test-shaped) inputs keep passing with an empty pin, while a
PlanetScale (`*.psdb.cloud`) production target fails closed unless the
manifest and the request carry the same pinned branch id.

RED on main (pre-fix): the claim publisher requires ``branch_id == ""`` and
accepts an unpinned PlanetScale manifest, so the two pin tests below fail.
GREEN after the Land-card fix (PR #980): a pinned manifest binds, an
unpinned PlanetScale manifest refuses naming the production pin.

Not covered here: the `rust tests` red recorded on PR #980 attempt 1 is a
single voice_rooms 429 timing failure with zero claim-gate failures (see the
card); reproducing a timing flake is out of scope for this file.

No Actions dispatch, database, network, approval or secret.
"""
import json
from copy import deepcopy
from pathlib import Path
import unittest

from production_migrate_claim import Refused, build_claim, projection_hash

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "crates/cutover/tests/fixtures"
PLAN = FIXTURES / "staging-migrate-plan.json"
PSDB_HOST = "abc-useast1-1.horizon.psdb.cloud"
PIN = "cnfixture01"
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


def retarget(plan, host, database, branch_id):
    plan = deepcopy(plan)
    plan["target"] = {"host": host, "database": database, "branch_id": branch_id}
    plan["plan_manifest_sha256"] = projection_hash(plan)
    return plan


def env_for(plan, host, database, branch_id):
    return {**ENV, "PRODUCTION_HOST": host, "PRODUCTION_DATABASE": database,
            "PRODUCTION_BRANCH_ID": branch_id,
            "PLAN_MANIFEST_SHA256": plan["plan_manifest_sha256"]}


class Land980ClaimGateAcceptanceTests(unittest.TestCase):
    def test_pinned_planetscale_prod_claim_binds(self):
        # A PlanetScale manifest carrying the pinned branch id binds when the
        # request names the same pin. RED pre-fix (branch pins refused).
        plan = retarget(production_plan(), PSDB_HOST, "postgres", PIN)
        claim = build_claim(plan, env_for(plan, PSDB_HOST, "postgres", PIN))
        self.assertEqual(claim["target"], plan["target"])

    def test_unpinned_planetscale_prod_claim_fails_closed_naming_the_pin(self):
        # A PlanetScale manifest without the pin fails closed, and the refusal
        # names the production pin requirement, never the staging flag (which
        # the production target rejects). RED pre-fix (unpinned builds).
        plan = retarget(production_plan(), PSDB_HOST, "postgres", "")
        with self.assertRaises(Refused):
            build_claim(plan, env_for(plan, PSDB_HOST, "postgres", ""))
        try:
            build_claim(plan, env_for(plan, PSDB_HOST, "postgres", ""))
        except Refused as refused:
            message = str(refused).lower()
        else:  # pragma: no cover - the assertRaises above already failed
            self.fail("unpinned PlanetScale production claim built")
        self.assertIn("planetscale", message)
        self.assertIn("branch id", message)
        self.assertNotIn("--staging-branch-id", message)

    def test_branch_pin_mismatch_refuses(self):
        # A request pin that does not match the manifest refuses. Green guard
        # on both sides of the land: the refusal point moves, the verdict holds.
        plan = retarget(production_plan(), PSDB_HOST, "postgres", PIN)
        with self.assertRaises(Refused):
            build_claim(plan, env_for(plan, PSDB_HOST, "postgres", "otherbranch"))

    def test_bare_production_host_passes_with_empty_pin(self):
        # Bare (Neon/test-shaped) production hosts keep passing with an empty
        # pin. Green guard on both sides of the land.
        plan = production_plan()
        claim = build_claim(plan, dict(ENV, PLAN_MANIFEST_SHA256=plan["plan_manifest_sha256"]))
        self.assertEqual(claim["kind"], "production-migrate-apply-claim")
        self.assertEqual(claim["target"], plan["target"])


if __name__ == "__main__":
    unittest.main()
