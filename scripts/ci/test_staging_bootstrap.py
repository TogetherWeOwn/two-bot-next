"""Synthetic protected staging recovery evidence."""

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from test_staging_recovery_status import ACCOUNT, APP, DEPLOYMENTS, FINGERPRINT, IMAGE, ROLLOUT, SOURCE, VERSION
from test_staging_recovery_status import plan_fixture, provider_fixture
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
        deferred = [bootstrap.GateError("ownership_deployment_mismatch"), {**self.state, "deploymentId": "b" * 8 + VERSION[8:]}, self.state]
        with patch.object(bootstrap, "read_ownership", side_effect=deferred) as read:
            self.assertEqual(bootstrap.read_after_update(read, VERSION, FINGERPRINT, lambda _: None), self.state)
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
        snapshot = {"has_previous": True, "serving_version": VERSION, "serving_sha": SOURCE, "application_id": APP,
                    "namespace_id": "c" * 32, "serving_rollout_id": ROLLOUT, "serving_image": IMAGE}
        path = root / "staging-deploy.json"
        path.write_text(json.dumps(config))
        (root / "staging-rollback-snapshot.json").write_text(json.dumps(snapshot))
        legacy = {"deploymentId": VERSION, "running": False}
        with patch.object(bootstrap, "verify_migration_sources") as check:
            bootstrap.prepare("bootstrap", root, "fixture", self, ACCOUNT, lambda _: legacy, SOURCE, FINGERPRINT)
            check.assert_not_called()
            config["env"]["staging"]["vars"][bootstrap.PIN] = FINGERPRINT
            self.assertEqual(json.loads(path.read_text()), config)
            config["env"]["staging"]["vars"].pop(bootstrap.PIN)
            path.write_text(json.dumps(config))
            args = ("verified", root, "..", self, ACCOUNT, lambda _: self.state, SOURCE, FINGERPRINT)
            with self.assertRaisesRegex(bootstrap.GateError, "recovery_pair_not_pinned"):
                bootstrap.prepare(*args, f"{VERSION}@sha256:{'f' * 64}")
            bootstrap.prepare(*args, f"{VERSION}@{IMAGE.split('@')[1]}")
            check.assert_called_with("..", SOURCE, SOURCE, SOURCE)
            self.assertEqual(json.loads(path.read_text())["env"]["staging"]["vars"][bootstrap.PIN], FINGERPRINT)


if __name__ == "__main__":
    unittest.main()
