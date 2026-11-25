"""Synthetic checks for the production recovery handoffs and failure ordering."""

from copy import deepcopy
from dataclasses import asdict, replace
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import production_recovery_artifact as artifacts
import production_recovery_workflow as recovery
from production_recovery_takeover import RecoveryPin
from staging_rollout import GateError
from worker_container_rollback import require_ordinary_production_rollback, source_has_attendance_schema


class RecoveryWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.pin = RecoveryPin("123", 1, "a" * 40, "b" * 40, "1" * 8 + "-1111" * 3 + "-" + "1" * 12,
                               "sha256:" + "1" * 64, "2" * 8 + "-2222" * 3 + "-" + "2" * 12, "2" * 32,
                               "3" * 64, "4" * 64, "5" * 64, "6" * 64, 21)
        self.candidate = ("3" * 8 + "-3333" * 3 + "-" + "3" * 12, self.pin.candidate_sha, "sha256:" + "3" * 64)
        self.result = {"state": "restored", **asdict(self.pin)}
        self.state = {
            "worker": {"version_id": self.candidate[0], "sha": self.candidate[1], "traffic_percent": 100},
            "container": {"application_id": self.pin.application_id, "namespace_id": self.pin.namespace_id,
                          "image_digest": self.candidate[2], "rollout_status": "completed"},
            "ownership": {"epoch": 21, "running": False, "phase": "fenced",
                          "owner_actor": recovery.fence_actor(self.pin)},
        }
        self.ack = {"deploymentId": self.candidate[0], "running": False,
                    "owner": {"deploymentId": None, "phase": "fenced", "actor": recovery.fence_actor(self.pin),
                              "epoch": 21, "oldEpoch": 20}}
        self.observe = Mock(side_effect=lambda: deepcopy(self.state))
        self.control = Mock()
        self.rollback = Mock()

    def producer(self, name, steps):
        return {"name": name, "run_id": 123, "run_attempt": 1, "head_sha": "d" * 40,
                "status": "completed", "conclusion": "success", "steps": [
                    {"name": step, "status": "completed", "conclusion": "success"} for step in steps
                ]}

    def test_inverse_requires_exact_durable_stop_and_protected_job_success(self):
        run = {"id": 123, "head_sha": "d" * 40}
        job = self.producer(artifacts.FENCE_JOB, (artifacts.FENCE_STEP,))
        api = Mock(return_value={"total_count": 1, "jobs": [job]})
        inverse = Mock(return_value=self.result)
        for ack in ({**self.ack, "running": True}, {**self.ack, "deploymentId": "other"},
                    {**self.ack, "owner": {**self.ack["owner"], "epoch": 22}},
                    {**self.ack, "owner": {**self.ack["owner"], "actor": "wrong"}}):
            with self.subTest(ack=ack), self.assertRaises(GateError):
                recovery.inverse_under_fence(self.pin, self.candidate, ack, run, api, inverse)
        job["conclusion"] = "cancelled"
        with self.assertRaises(GateError):
            recovery.inverse_under_fence(self.pin, self.candidate, self.ack, run, api, inverse)
        inverse.assert_not_called()
        job["conclusion"] = "success"
        self.assertEqual(recovery.inverse_under_fence(self.pin, self.candidate, self.ack, run, api, inverse), self.result)
        inverse.assert_called_once_with()

    def test_stop_requires_matching_provider_state_after_the_ack(self):
        # The first read is a running active writer at the previous epoch.
        before = deepcopy(self.state)
        before["ownership"].update(epoch=20, running=True, phase="active")
        after = deepcopy(self.state)
        after["worker"]["deployment_id"] = "changed"
        self.control.return_value = self.ack
        with self.assertRaisesRegex(GateError, "production_inverse_serving_pair_changed"):
            recovery.establish_fence(replace(self.pin, fence_epoch=19), self.candidate,
                                     Mock(side_effect=[before, after]), self.control)
        self.control.assert_called_once_with("fence-exact", 20, expected_deployment=self.candidate[0])

    def test_cli_requires_reviewed_plan_approval(self):
        approval = {"plan_run_id": "321", "plan_archive_sha256": "sha256:" + "8" * 64}
        with (patch.object(recovery, "current_run"), patch.object(recovery, "verify_environment"),
              patch.object(recovery, "release_admission", return_value=(self.pin, self.candidate, {}, approval)),
              patch.object(recovery, "verify_check", side_effect=GateError("unapproved")) as check):
            self.assertEqual(recovery.main(["fence"], {}), 1)
            check.assert_called_once()

    def test_inverse_refuses_incomplete_database_readback(self):
        plan = dict(target=dict(host="db", database="fixture", branch_id="branch"),
                    acl_plan_ref="acl", plan_manifest_sha256="plan", store_plan_manifest_sha256="store")
        approval = {"recovery_ref": "ref", "plan_run_id": "321"}
        env = {"TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL": "fixture"}
        with TemporaryDirectory() as scratch, patch.object(recovery.subprocess, "run") as run:
            run.return_value = Mock(returncode=0, stdout=b'{"state":"restored"}')
            with self.assertRaisesRegex(GateError, "production_inverse_database_readback_invalid"):
                recovery.run_inverse("fixture", Path(scratch), self.pin, plan, approval, env)

    def restore(self):
        recovery.restore_under_fence(self.pin, self.result, self.observe, self.control, self.rollback,
                                     "test-image", self.candidate)

    def test_post_commit_provider_failures_leave_the_writer_stopped(self):
        self.rollback.post_worker_restore.side_effect = GateError("provider_write_refused")
        with self.assertRaisesRegex(GateError, "provider_write_refused"):
            self.restore()
        self.control.assert_not_called()
        self.rollback.restore_image.assert_not_called()
        self.rollback.wait_stable.assert_not_called()

        self.rollback.reset_mock(side_effect=True)
        self.state["worker"].update(version_id=self.pin.worker_version, sha=self.pin.rollback_sha)
        self.rollback.restore_image.side_effect = GateError("provider_rollout_refused")
        with self.assertRaisesRegex(GateError, "provider_rollout_refused"):
            self.restore()
        self.rollback.post_worker_restore.assert_not_called()
        self.control.assert_not_called()
        self.rollback.wait_stable.assert_not_called()

    def test_post_commit_recovery_reuses_the_forward_stop_and_refuses_an_unknown_worker(self):
        self.assertEqual(recovery.establish_fence(self.pin, self.candidate, self.observe, self.control),
                         (self.pin, None))
        self.control.assert_not_called()
        with self.assertRaisesRegex(GateError, "production_inverse_serving_pair_changed"):
            recovery.establish_fence(self.pin, ("", *self.candidate[1:]), self.observe, self.control)
        self.control.assert_not_called()
        self.state["worker"].update(version_id=self.pin.worker_version, sha=self.pin.rollback_sha)
        self.assertEqual(recovery.establish_fence(self.pin, ("", *self.candidate[1:]), self.observe, self.control),
                         (self.pin, None))
        self.control.assert_not_called()

    def test_watch_breach_refences_only_the_exact_active_candidate_at_the_next_epoch(self):
        initial = replace(self.pin, fence_epoch=19)
        self.state["ownership"].update(epoch=20, phase="active", running=True)

        def fence(*_args, **_kwargs):
            self.state["ownership"].update(epoch=21, phase="fenced", running=False)
            return self.ack

        self.control.side_effect = fence
        self.assertEqual(recovery.establish_fence(initial, self.candidate, self.observe, self.control),
                         (self.pin, self.ack))
        self.control.assert_called_once_with("fence-exact", 20, expected_deployment=self.candidate[0])
        self.control.reset_mock()
        self.state["ownership"].update(epoch=20, phase="fenced")
        with self.assertRaisesRegex(GateError, "production_inverse_fence_epoch_changed"):
            recovery.establish_fence(initial, self.candidate, self.observe, self.control)
        self.control.assert_not_called()

    def test_one_time_takeover_reconciles_lost_reply_before_readiness(self):
        def worker(version):
            self.state["worker"].update(version_id=version, sha=self.pin.rollback_sha)

        def image(_):
            self.state["container"]["image_digest"] = self.pin.image_digest

        def lost_reply(*_args, **_kwargs):
            self.state["ownership"].update(epoch=22, phase="active", owner_actor=self.pin.actor)
            raise GateError("ownership_control_unavailable")

        self.rollback.post_worker_restore.side_effect = worker
        self.rollback.restore_image.side_effect = image
        self.control.side_effect = lost_reply
        self.restore()
        self.restore()
        self.rollback.post_worker_restore.assert_called_once_with(self.pin.worker_version)
        self.rollback.restore_image.assert_called_once_with("test-image")
        self.control.assert_called_once_with("takeover-fenced", 21, expected_deployment=self.pin.worker_version)
        self.assertEqual(self.rollback.wait_stable.call_count, 2)

    def test_normal_paths_refuse_cross_schema_and_release_owned_fences(self):
        owner = {"owner": {"phase": "active", "actor": "fixture"}}
        has_schema = lambda sha: sha == self.pin.candidate_sha
        with self.assertRaisesRegex(GateError, "production_rollback_requires_protected_inverse"):
            require_ordinary_production_rollback(self.pin.candidate_sha, self.pin.rollback_sha, owner, has_schema=has_schema)
        owner["owner"].update(phase="fenced", actor=recovery.fence_actor(self.pin))
        with self.assertRaisesRegex(GateError, "production_rollback_fence_reserved"):
            require_ordinary_production_rollback(self.pin.rollback_sha, self.pin.rollback_sha, owner, has_schema=has_schema)
        owner["owner"].update(phase="active")
        with self.assertRaisesRegex(GateError, "production_rollback_requires_protected_inverse"):
            require_ordinary_production_rollback(self.pin.candidate_sha, self.pin.rollback_sha, owner, has_schema=has_schema)
        require_ordinary_production_rollback(self.pin.candidate_sha, self.pin.candidate_sha, owner, has_schema=has_schema)
        with self.assertRaisesRegex(GateError, "production_rollback_source_unproved"):
            source_has_attendance_schema(None)
