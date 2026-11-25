"""Synthetic protected staging recovery evidence."""

from contextlib import redirect_stderr
from io import BytesIO, StringIO
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, call, patch
from urllib.error import HTTPError

from test_staging_recovery_status import (
    ACCOUNT, APP, DEPLOYMENTS, ENV, FINGERPRINT, IMAGE, ROLLOUT, SOURCE, VERSION, VERSION_DETAIL,
)
from test_staging_recovery_status import plan_fixture, provider_fixture
import staging_bootstrap as bootstrap
import staging_recovery_status as status
import staging_rollout as rollout
from staging_recovery_status import projection_hash, store_projection_hash


class Response(BytesIO):
    status = 200
    headers = {}


def recovery_plan_fixture():
    plan = plan_fixture()
    source = {"version": 475, "description": "attendance recovery boundary", "sha384": "4" * 96}
    plan["source_migrations"].insert(0, source)
    for key in ("ledger_before", "ledger_after"):
        plan[key].insert(0, dict(source, success=True))
    plan["plan_manifest_sha256"] = projection_hash(plan)
    plan["store_plan_manifest_sha256"] = store_projection_hash(plan)
    return plan


def reviewed_plan_output(root, plan, source):
    (root / "staging-schema-gate-manifest.json").write_text(json.dumps(plan))
    event, output = root / "event.json", root / "output"
    event.write_text(json.dumps({"inputs": {"recovery_mode": "verified"}}))
    env = {"GITHUB_EVENT_PATH": str(event), "GITHUB_OUTPUT": str(output), "GITHUB_SHA": "f" * 40,
           "RECOVERY_PLAN_SOURCE": source, "REVIEWED_PLAN": "fixture", "PYTHONDONTWRITEBYTECODE": "1"}
    subprocess.run([sys.executable, bootstrap.__file__, "plan"], cwd=root, env=env,
                   capture_output=True, text=True, timeout=5, check=True)
    return dict(line.split("=", 1) for line in output.read_text().splitlines())


class StagingBootstrapTests(unittest.TestCase):
    def setUp(self):
        self.routes = provider_fixture()
        self.state = {"deploymentId": VERSION, "running": False, "runtimeDatabaseFingerprint": FINGERPRINT,
                      "runtimeDatabaseStartupGuard": True, "owner": {"phase": "active", "deploymentId": VERSION}}

    def test_exact_recovery_plan_checks_0475_membership_beside_0481_and_0478(self):
        with self.assertRaisesRegex(bootstrap.GateError, "recovery_plan_missing_0475"):
            bootstrap.verified_plan(plan_fixture(), SOURCE)
        proof = bootstrap.verified_plan(recovery_plan_fixture(), SOURCE)
        self.assertEqual((proof["target_fingerprint"], proof["cutover_version"], proof["store_version"]),
                         (FINGERPRINT, 481, 478))

    def test_plan_dispatch_emits_fingerprint_only_for_clean_recovery(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory(prefix="staging-plan-fixture-")))
        (root / "staging-schema-gate-manifest.json").write_text(json.dumps(recovery_plan_fixture()))
        event, output = root / "event.json", root / "output"
        env = {"GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_EVENT_PATH": str(event),
               "GITHUB_OUTPUT": str(output), "GITHUB_SHA": SOURCE, "PYTHONDONTWRITEBYTECODE": "1",
               "RECOVERY_PLAN_SOURCE": "f" * 40}
        drills = dict.fromkeys(("inject_gateway_failure", "inject_error_rate_spike",
                                "inject_voice_stall", "inject_unpaired_disconnect"), "false")
        for mode in ("bootstrap", "verified"):
            for armed in (None, *drills):
                with self.subTest(mode=mode, armed=armed):
                    inputs = {**drills, "recovery_mode": mode}
                    if armed:
                        inputs[armed] = "true"
                    event.write_text(json.dumps({"ref": "refs/heads/main", "inputs": inputs}))
                    output.write_text("")
                    result = subprocess.run([sys.executable, bootstrap.__file__, "plan"], cwd=root, env=env,
                                            capture_output=True, text=True, timeout=5)
                    if armed:
                        self.assertEqual(result.returncode, 1)
                        self.assertEqual(result.stderr, "staging recovery refused: recovery_drill_refused\n")
                        self.assertEqual(output.read_text(), "")
                    else:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertEqual(output.read_text(), f"fingerprint={FINGERPRINT}\n")

    def test_reviewed_plan_emits_source_projection_with_a_later_workflow_sha(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory(prefix="staging-reviewed-plan-")))
        plan = recovery_plan_fixture()
        fields = reviewed_plan_output(root, plan, SOURCE)
        self.assertEqual(fields["fingerprint"], FINGERPRINT)
        self.assertEqual(json.loads(fields["plan"]), bootstrap.verified_plan(plan, SOURCE))

    def test_authenticated_get_retries_only_deferred_deployment_mismatch(self):
        token = ENV["OWNERSHIP_CONTROL_TOKEN"]
        url = f"{status.ORIGIN}/internal/ownership?runtime_db_fingerprint={FINGERPRINT}"
        cases = ((503, "deployment_mismatch", True, True), (503, "deployment_mismatch", False, False),
                 (503, "staging_database_unverified", True, False), (401, "deployment_mismatch", True, False))
        for code, reason, deferred, retries in cases:
            with self.subTest(code=code, reason=reason, deferred=deferred):
                error = HTTPError(url, code, "fixture refusal", {}, BytesIO(json.dumps({"reason": reason}).encode()))
                opener = Mock()
                opener.open.side_effect = [error, Response(json.dumps(self.state).encode())]
                with patch.object(status, "no_redirect_opener", return_value=opener) as factory:
                    def read(fingerprint):
                        return status.read_ownership(token, fingerprint, deferred=deferred)

                    with patch.object(bootstrap, "monotonic", return_value=100):
                        wait = Mock()
                        if retries:
                            self.assertEqual(bootstrap.read_after_update(read, VERSION, FINGERPRINT, wait), self.state)
                            wait.assert_called_once_with(10)
                            self.assertEqual(opener.open.call_count, 2)
                        else:
                            with self.assertRaisesRegex(bootstrap.GateError, "^ownership_get_failed$"):
                                bootstrap.read_after_update(read, VERSION, FINGERPRINT, wait)
                            wait.assert_not_called()
                            opener.open.assert_called_once()
                self.assertEqual(factory.call_args_list, [call(use_proxy=False)] * opener.open.call_count)
                for request_call in opener.open.call_args_list:
                    request = request_call.args[0]
                    self.assertEqual(request.get_method(), "GET")
                    self.assertEqual(request.full_url, url)
                    self.assertEqual(request.get_header("Authorization"), f"Bearer {token}")
                    self.assertEqual(request.get_header("Cache-control"), "no-cache")
                    self.assertEqual(request_call.kwargs, {"timeout": 15})

    def test_propagation_stops_scheduling_before_deadline(self):
        now = 0
        starts = []

        def advance(seconds):
            nonlocal now
            now += seconds

        def stale(_):
            starts.append(now)
            self.assertLessEqual(len(starts), 32)
            if len(starts) == 1:
                advance(10)  # A slow response also consumes the retry budget.
            raise bootstrap.GateError("ownership_deployment_mismatch")

        with patch.object(bootstrap, "monotonic", side_effect=lambda: now):
            with self.assertRaisesRegex(bootstrap.GateError, "^ownership_propagation_timeout$"):
                bootstrap.read_after_update(stale, VERSION, FINGERPRINT, advance)
        self.assertEqual(starts, [0, *range(20, 321, 10)])
        self.assertEqual(now, 320)

    def test_verifier_refuses_unproved_or_different_effective_binding(self):
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

    def forward_fixture(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory(prefix="staging-attestation-fixture-")))
        (root / "receipt.json").write_text(json.dumps({"worker_version": VERSION}))
        env = {**ENV, "DEPLOY_SHA": SOURCE, "RECOVERY_MODE": "verified", "RECOVERY_FINGERPRINT": FINGERPRINT,
               "ROLLOUT_DIR": str(root)}
        evidence = root / "staging-runtime-binding.json"
        return env, evidence

    def test_forward_verify_attests_exact_stopped_worker_with_startup_guard(self):
        env, evidence = self.forward_fixture()
        read = Mock(return_value={**self.state, "owner": None})
        with patch.object(bootstrap, "Client", return_value=self), patch.object(bootstrap, "read_ownership", read), \
                patch.object(self, "api", wraps=self.api) as api:
            timeline = Mock()
            timeline.attach_mock(api, "provider")
            timeline.attach_mock(read, "ownership")
            self.assertEqual(bootstrap.main(["verify"], env), 0)
        self.assertEqual(timeline.mock_calls, [
            call.provider(DEPLOYMENTS), call.provider(VERSION_DETAIL),
            call.ownership(env["OWNERSHIP_CONTROL_TOKEN"], FINGERPRINT, deferred=True), call.provider(DEPLOYMENTS),
        ])
        self.assertEqual(json.loads(evidence.read_text()), {
            "worker_version": VERSION, "source_sha": SOURCE, "target_fingerprint": FINGERPRINT,
            "running": False, "startup_guard": True,
        })

    def test_reviewed_forward_verify_requires_verified_mode(self):
        env, evidence = self.forward_fixture()
        env.update(GITHUB_SHA="f" * 40, REVIEWED_SOURCE=SOURCE)
        with patch.object(bootstrap, "Client", return_value=self), patch.object(bootstrap, "read_ownership", return_value=self.state):
            self.assertEqual(bootstrap.main(["verify"], env), 0)
        self.assertEqual(json.loads(evidence.read_text())["source_sha"], SOURCE)
        with patch.object(bootstrap, "Client") as client, redirect_stderr(StringIO()) as stderr:
            self.assertEqual(bootstrap.main(["verify"], dict(env, RECOVERY_MODE="bootstrap")), 1)
        client.assert_not_called()
        self.assertIn("reviewed_source_requires_verified_recovery", stderr.getvalue())

    def test_forward_verify_refuses_when_slow_provider_exhausts_deadline(self):
        env, evidence = self.forward_fixture()
        now = 0
        timeouts = []

        def advance(seconds):
            nonlocal now
            now += seconds

        def provider(request, timeout):
            timeouts.append(timeout)
            if len(timeouts) > 2:
                advance(timeout)
                raise TimeoutError("synthetic provider timeout")
            path = (DEPLOYMENTS, VERSION_DETAIL)[len(timeouts) - 1]
            self.assertTrue(request.full_url.endswith(path))
            return Response(json.dumps({"success": True, "result": self.routes[path]}).encode())

        def ownership(*args, **kwargs):
            advance(350)  # Elapsed time at the final provider bracket after slow propagation.
            return self.state

        opener = Mock()
        opener.open.side_effect = provider
        with patch.object(rollout, "no_redirect_opener", return_value=opener), \
                patch.object(bootstrap, "read_ownership", side_effect=ownership), \
                patch.object(bootstrap, "monotonic", side_effect=lambda: now), \
                patch.object(rollout.time, "monotonic", side_effect=lambda: now), \
                patch.object(rollout.time, "sleep", side_effect=advance), redirect_stderr(StringIO()) as stderr:
            self.assertEqual(bootstrap.main(["verify"], env), 1)
        self.assertEqual(timeouts, [10, 10, 10, 9.75])
        self.assertEqual(now, 370)
        self.assertEqual(stderr.getvalue(), "staging recovery refused: rollout_timeout\n")
        self.assertFalse(evidence.exists())

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
