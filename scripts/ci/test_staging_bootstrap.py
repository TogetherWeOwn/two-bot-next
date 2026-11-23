"""Synthetic protected staging recovery evidence."""

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from test_staging_recovery_status import (
    ACCOUNT, DEPLOYMENTS, FINGERPRINT, SOURCE, VERSION, plan_fixture, provider_fixture,
)
import staging_bootstrap as bootstrap
from staging_recovery_status import projection_hash, store_projection_hash


class StagingBootstrapTests(unittest.TestCase):
    def setUp(self):
        self.routes = provider_fixture()
        self.state = {"deploymentId": VERSION, "running": False, "runtimeDatabaseFingerprint": FINGERPRINT,
                      "runtimeDatabaseStartupGuard": True, "owner": {"phase": "active", "deploymentId": VERSION}}

    def test_exact_recovery_plan_checks_0475_membership_beside_0481_and_0478(self):
        plan = plan_fixture()
        with self.assertRaisesRegex(bootstrap.GateError, "recovery_plan_missing_0475"):
            bootstrap.verified_plan(plan, SOURCE)
        source = {"version": 475, "description": "attendance recovery boundary", "sha384": "4" * 96}
        plan["source_migrations"].insert(0, source)
        for key in ("ledger_before", "ledger_after"):
            plan[key].insert(0, dict(source, success=True))
        plan["plan_manifest_sha256"] = projection_hash(plan)
        plan["store_plan_manifest_sha256"] = store_projection_hash(plan)
        proof = bootstrap.verified_plan(plan, SOURCE)
        self.assertEqual((proof["target_fingerprint"], proof["cutover_version"], proof["store_version"]),
                         (FINGERPRINT, 481, 478))

    def test_verifier_refuses_unproved_or_different_effective_binding(self):
        bootstrap.attest(self, lambda _: self.state, VERSION, SOURCE, FINGERPRINT)
        for update in ({"runtimeDatabaseFingerprint": None}, {"runtimeDatabaseFingerprint": "f" * 64},
                       {"runtimeDatabaseStartupGuard": None}, {"running": True}, {"deploymentId": "stale"}):
            with self.subTest(update=update), self.assertRaises(bootstrap.GateError):
                bootstrap.attest(self, lambda _: {**self.state, **update}, VERSION, SOURCE, FINGERPRINT)

        def split_traffic(_):
            self.routes[DEPLOYMENTS]["deployments"][0]["versions"][0]["percentage"] = 50
            return self.state

        with self.assertRaisesRegex(bootstrap.GateError, "recovery_serving_version_mismatch"):
            bootstrap.attest(self, split_traffic, VERSION, SOURCE, FINGERPRINT)

    def api(self, path):
        return self.routes[path]

    def test_bootstrap_accepts_stopped_legacy_for_replacement_only(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        config = {"env": {"production": {"vars": {}}, "staging": {"vars": {
            "TWO_DEPLOY_ENV": "staging", "STAGING_RUNTIME_POLICY": "bounded"}}}}
        snapshot = {"has_previous": True, "serving_version": VERSION, "serving_sha": SOURCE}
        path = root / "staging-deploy.json"
        path.write_text(json.dumps(config))
        (root / "staging-rollback-snapshot.json").write_text(json.dumps(snapshot))
        legacy = {"deploymentId": VERSION, "running": False}
        with patch.object(bootstrap, "verify_migration_sources") as check:
            bootstrap.prepare("bootstrap", root, "fixture", self, ACCOUNT, lambda _: legacy, SOURCE, FINGERPRINT)
            check.assert_not_called()
            config["env"]["staging"]["vars"][bootstrap.PIN] = FINGERPRINT
            self.assertEqual(json.loads(path.read_text()), config)
            check.side_effect = bootstrap.IncompatibleSource("recovery_migration_source_mismatch")
            with self.assertRaisesRegex(bootstrap.IncompatibleSource, "recovery_migration_source_mismatch"):
                bootstrap.prepare("verified", root, "fixture", self, ACCOUNT, lambda _: legacy, SOURCE, FINGERPRINT)
            check.assert_called_once_with("fixture", SOURCE, SOURCE, SOURCE)


if __name__ == "__main__":
    unittest.main()
