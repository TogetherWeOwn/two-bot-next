"""Exercise the workflow's restart step with retained attempt-one evidence."""

from contextlib import redirect_stderr
from copy import deepcopy
from hashlib import sha256
from io import BytesIO, StringIO
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import Mock, call, patch
from zipfile import ZipFile

import yaml

import test_production_recovery_artifact as artifact_fixture
import test_production_release_forward as forward_fixture
import production_release_interrupted as interrupted
import production_release_preflight as preflight
from production_recovery_artifact import STOP_INTENT_ARTIFACT
from staging_migrate_approval import digest_json
from staging_rollout import GateError


class InterruptedAttemptEvidenceTests(unittest.TestCase):
    def setUp(self):
        forward = forward_fixture.ReleaseForwardTests()
        forward.setUp()
        self.pin, plan = forward.preflight_fixture()
        plan_artifact = {"archive_digest": "sha256:" + "9" * 64}
        self.epoch = 21

        fixture = artifact_fixture.RecoveryArtifactTests()
        fixture.setUp()
        env, _, routes = fixture.artifact_fixture()
        self.current_route = f"actions/runs/{env['GITHUB_RUN_ID']}"
        current = routes[self.current_route]
        origin = {**current, "id": 456, "status": "completed", "conclusion": "cancelled"}
        self.routes = {self.current_route: current, "actions/runs/456": origin}
        prepare = fixture.producer(preflight.PREPARE_JOB, preflight.STOP_INTENT_STEPS)
        prepare["run_id"] = origin["id"]
        self.routes["actions/runs/456/attempts/1/jobs?per_page=30"] = {"total_count": 1, "jobs": [prepare]}

        intent = preflight.stop_intent(self.pin, origin, "789", plan, plan_artifact, self.epoch - 1)
        self.publish(origin, 654, STOP_INTENT_ARTIFACT, intent)
        admission = {"binding": {"run_id": env["GITHUB_RUN_ID"], "workflow_sha": current["head_sha"],
                                 "stop_intent": intent}, "phase": "stopped"}
        decision = {"admission": admission, "classification": {"state": "pre"}}
        self.decision_digest = digest_json(decision)
        self.routes[f"{self.current_route}/attempts/1/jobs?per_page=30"] = {"total_count": 2, "jobs": [
            fixture.producer(interrupted.ADMISSION_JOB, interrupted.ADMISSION_STEPS),
            fixture.producer(interrupted.DATABASE_JOB, (interrupted.CLASSIFY_STEP, interrupted.DECISION_STEP)),
        ]}
        admission_digest = self.publish(current, 701, interrupted.ADMISSION, admission)
        decision_digest = self.publish(current, 702, interrupted.DECISION, decision)

        temp = self.enterContext(TemporaryDirectory())
        self.receipt = Path(temp) / "interrupted-production-release" / "production-release-interrupted-restored.json"
        self.env = {**env, "RELEASE_RECOVERY_RUN_ID": "456", "RUNNER_TEMP": temp,
                    "ADMISSION_ARTIFACT_ID": "701", "ADMISSION_ARCHIVE_SHA256": admission_digest,
                    "DECISION_ARTIFACT_ID": "702", "DECISION_ARCHIVE_SHA256": decision_digest,
                    "INVERSE_ARTIFACT_ID": "", "INVERSE_ARCHIVE_SHA256": "",
                    "PRODUCTION_WORKER_URL": "https://bot.fixture.invalid", "OWNERSHIP_CONTROL_TOKEN": "synthetic",
                    "CLOUDFLARE_ACCOUNT_ID": "fixture", "CLOUDFLARE_API_TOKEN": "synthetic"}
        self.state = {
            "worker": {"version_id": self.pin.rollback_worker, "sha": self.pin.rollback_sha, "traffic_percent": 100},
            "container": {"application_id": self.pin.application_id, "namespace_id": self.pin.namespace_id,
                          "image_digest": self.pin.rollback_image, "rollout_status": "completed"},
            "ownership": {"running": False, "phase": "fenced", "epoch": self.epoch,
                          "owner_actor": f"production-schema-release:456:{self.pin.candidate_sha}"},
        }
        self.api = Mock(side_effect=lambda route, **_: deepcopy(self.routes[route]))
        self.observe = self.enterContext(patch.object(interrupted, "read_status", side_effect=lambda _: deepcopy(self.state)))
        self.control = self.enterContext(patch.object(interrupted, "control_client"))
        self.takeover = self.control.return_value
        self.takeover.side_effect = self.commit_takeover
        self.provider = self.enterContext(patch.object(interrupted, "RollbackClient"))
        self.rollback = self.enterContext(patch.object(interrupted, "Rollback")).return_value
        self.rollback.evidence = {"stable_polls": 2}
        self.database = self.enterContext(patch.object(interrupted.subprocess, "run"))
        self.enterContext(patch.object(interrupted, "environment_policy"))
        self.enterContext(patch.object(preflight, "verify_environment"))
        self.enterContext(patch.object(preflight, "authenticated_plan", return_value=(plan, plan_artifact)))

    def commit_takeover(self, *_args, **_kwargs):
        self.state["ownership"].update(running=True, phase="active", epoch=self.epoch + 1)

    def publish(self, run, artifact_id, name, document):
        archive = BytesIO()
        with ZipFile(archive, "w") as bundle:
            bundle.writestr(f"{name}.json", json.dumps(document))
        raw = archive.getvalue()
        digest = "sha256:" + sha256(raw).hexdigest()
        entry = {"id": artifact_id, "name": f"{name}-{run['id']}-1", "expired": False,
                 "digest": digest, "size_in_bytes": len(raw)}
        lineage = {"id": run["id"], "head_sha": run["head_sha"], "head_branch": "main",
                   "repository_id": run["repository"]["id"], "head_repository_id": run["head_repository"]["id"]}
        listing = self.routes.setdefault(f"actions/runs/{run['id']}/artifacts?per_page=30", {"artifacts": []})
        listing["artifacts"].append(entry)
        listing["total_count"] = len(listing["artifacts"])
        self.routes[f"actions/artifacts/{artifact_id}"] = {**entry, "workflow_run": lineage}
        self.routes[f"actions/artifacts/{artifact_id}/zip"] = raw
        return digest

    def test_restart_refuses_retained_evidence_after_same_run_advances(self):
        self.routes[self.current_route]["run_attempt"] = 2
        for runner_attempt, expected_reads in (("2", []), ("1", [call(self.current_route)])):
            with self.subTest(runner_attempt=runner_attempt), redirect_stderr(StringIO()):
                self.api.reset_mock()
                result = interrupted.main(["restart"], {**self.env, "GITHUB_RUN_ATTEMPT": runner_attempt}, self.api)
                self.assertEqual(result, 1)
                self.assertEqual(self.api.call_args_list, expected_reads)
                self.observe.assert_not_called()
                self.control.assert_not_called()
                self.takeover.assert_not_called()
                self.provider.assert_not_called()
                self.rollback.wait_stable.assert_not_called()
                self.database.assert_not_called()
                self.assertFalse(self.receipt.exists())

    def test_original_attempt_can_restart_and_reconcile_a_lost_reply(self):
        def commit_then_lose_reply(*_args, **_kwargs):
            self.commit_takeover()
            raise GateError("fixture_takeover_reply_lost")

        self.takeover.side_effect = commit_then_lose_reply
        for _ in range(2):
            self.assertEqual(interrupted.main(["restart"], self.env, self.api), 0)
            result = json.loads(self.receipt.read_bytes())
            self.assertEqual(result["outcome"], {"state": "pre", "worker": self.pin.rollback_worker,
                                                 "epoch": self.epoch + 1})
            self.assertEqual(result["decision_sha256"], self.decision_digest)
        self.assertEqual(self.api.call_args_list.count(call("actions/artifacts/702/zip", binary=True)), 2)
        self.takeover.assert_called_once_with("takeover-fenced", self.epoch, expected_deployment=self.pin.rollback_worker)
        self.assertEqual(self.rollback.wait_stable.call_args_list, [
            call(self.pin.rollback_worker, self.rollback.now.return_value),
        ] * 2)
        self.rollback.post_worker_restore.assert_not_called()
        self.rollback.restore_image.assert_not_called()
        self.database.assert_not_called()

    def test_restart_step_receives_the_admission_and_decision_outputs(self):
        path = Path(__file__).resolve().parents[2] / ".github/workflows/deploy-production-interrupted.yml"
        steps = yaml.safe_load(path.read_text(encoding="utf-8"))["jobs"]["restart"]["steps"]
        step, = [step for step in steps if step.get("run") == "python3 scripts/production_release_interrupted.py restart"]
        for field, source in {
            "ADMISSION_ARTIFACT_ID": "admit.outputs.artifact-id",
            "ADMISSION_ARCHIVE_SHA256": "admit.outputs.archive-sha256",
            "DECISION_ARTIFACT_ID": "database.outputs.decision-id",
            "DECISION_ARCHIVE_SHA256": "database.outputs.decision-sha256",
        }.items():
            self.assertEqual(step["env"][field], "${{ needs." + source + " }}")
