"""Synthetic checks for the production recovery handoffs and failure ordering."""

from copy import deepcopy
from dataclasses import asdict, replace
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import production_recovery_artifact as artifacts
import production_recovery_workflow as recovery
from ci.production_migrate_claim import projection_hash, store_projection_hash
from production_recovery_takeover import RecoveryPin
from staging_migrate_approval import digest_json
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
        after["container"]["namespace_id"] = "9" * 32
        self.control.return_value = self.ack
        with self.assertRaisesRegex(GateError, "production_inverse_serving_pair_changed"):
            recovery.establish_fence(self.pin, self.candidate, Mock(side_effect=[before, after]), self.control)
        self.control.assert_called_once_with("fence-exact", 20, expected_deployment=self.candidate[0])

    def test_cli_requires_reviewed_plan_approval(self):
        approval = {"plan_run_id": "321", "plan_archive_sha256": "sha256:" + "8" * 64}
        with (patch.object(recovery, "current_run"), patch.object(recovery, "verify_environment"),
              patch.object(recovery, "release_admission", return_value=(self.pin, self.candidate, {}, approval)),
              patch.object(recovery, "verify_check", side_effect=GateError("unapproved")) as check):
            self.assertEqual(recovery.main(["fence"], {}), 1)
            check.assert_called_once()

    def test_admission_binds_the_rust_inverse_ledger_rows(self):
        def source_row(version):
            checksum = f"{version:096x}"
            return dict(version=version, description=f"migration {version}",
                        sha384=checksum, sha384_recomputed=checksum)

        fixture = ROOT / "crates/cutover/tests/fixtures/production_release_ledger_readback.json"
        restored_cutover, expanded_store = json.loads(fixture.read_text(encoding="utf-8"))
        cutover_before, store_before = restored_cutover[:1], expanded_store[:1]
        expanded_cutover = restored_cutover + [
            dict(version=version, description=f"migration {version}", sha384=f"{version:096x}", success=True)
            for version in (479, 480, 481, 482)
        ]
        target = dict(host="db", database="fixture", branch_id="branch")
        pin = replace(self.pin, database_fingerprint=digest_json(target),
                      expanded_ledgers_sha256=digest_json([expanded_cutover, expanded_store]),
                      restored_ledgers_sha256=digest_json([restored_cutover, store_before]))
        worker, _, image = self.candidate
        run = {"id": 123, "head_sha": "d" * 40}
        reference = digest_json(dict(operation="undo-0480", candidate_sha=pin.candidate_sha,
                                     candidate_worker=worker, candidate_image=image, rollback_sha=pin.rollback_sha,
                                     rollback_worker=pin.worker_version, rollback_image=pin.image_digest,
                                     application_id=pin.application_id, namespace_id=pin.namespace_id,
                                     workflow_sha=run["head_sha"]))
        plan = dict(source_sha=pin.candidate_sha, target=target, migration_target="production", mode="plan",
                    role="two_bot_migrator_ro", recovery_evidence_ref=f"inverse-{reference}",
                    pending_before=[475, 479, 480, 481, 482], pending_store_before=[478],
                    source_migrations=[source_row(v) for v in (474, 475, 479, 480, 481, 482)],
                    source_store_migrations=[source_row(v) for v in (477, 478)],
                    ledger_before=cutover_before, ledger_after=cutover_before,
                    ledger_store_before=store_before, ledger_store_after=store_before)
        plan.update(plan_manifest_sha256=projection_hash(plan), store_plan_manifest_sha256=store_projection_hash(plan))
        artifact = {"archive_digest": "sha256:" + "8" * 64}
        env = {f"PRODUCTION_{key.upper()}": value for key, value in target.items()}
        admission = dict(pin=asdict(pin), plan_run_id="321", candidate_worker=worker, candidate_image=image)
        env["RECOVERY_ADMISSION"] = json.dumps(admission)
        with patch.object(recovery, "authenticated_plan", return_value=(plan, artifact)):
            admitted, candidate, approved_plan, approval = recovery.release_admission(env, run)
            self.assertEqual((admitted, candidate, approved_plan), (pin, self.candidate, plan))
            self.assertEqual(approval["plan_archive_sha256"], artifact["archive_digest"])
            source_cutover = [dict(row, success=True) for row in plan["source_migrations"]]
            source_store = [dict(row, success=True) for row in plan["source_store_migrations"]]
            admission["pin"].update(expanded_ledgers_sha256=digest_json([source_cutover, source_store]),
                                    restored_ledgers_sha256=digest_json([source_cutover[:2], source_store[:1]]))
            env["RECOVERY_ADMISSION"] = json.dumps(admission)
            with self.assertRaisesRegex(GateError, "production_inverse_ledger_pin_invalid"):
                recovery.release_admission(env, run)

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
        with self.assertRaisesRegex(GateError, "production_rollback_source_unproved"):
            source_has_attendance_schema(None)
