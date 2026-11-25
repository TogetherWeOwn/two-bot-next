"""Synthetic production recovery without GitHub, Cloudflare or a database."""

from dataclasses import asdict
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from production_recovery_takeover import GateError, RecoveryPin, complete_legacy_takeover
import worker_container_rollback as rollback


class ProductionRecoveryTakeoverTests(unittest.TestCase):
    def setUp(self):
        self.pin = RecoveryPin(
            run_id="123", run_attempt=1, candidate_sha="a" * 40, rollback_sha="b" * 40,
            worker_version="12345678-1234-4567-8123-123456789abc",
            image_digest="sha256:" + "1" * 64,
            application_id="12345678-1234-4567-8123-123456789def", namespace_id="2" * 32,
            database_fingerprint="3" * 64, expanded_ledgers_sha256="4" * 64,
            restored_ledgers_sha256="5" * 64, attendance_fence_sha256="6" * 64, fence_epoch=21,
        )
        self.readback = {"state": "restored", **asdict(self.pin)}
        self.state = {
            "worker": {"version_id": self.pin.worker_version, "sha": self.pin.rollback_sha, "traffic_percent": 100},
            "container": {"application_id": self.pin.application_id, "namespace_id": self.pin.namespace_id,
                          "image_digest": self.pin.image_digest, "rollout_status": "completed"},
            "ownership": {"epoch": 21, "running": False, "phase": "fenced", "owner_actor": "fixture-fence"},
        }
        self.observe = Mock(side_effect=lambda: self.state)
        self.control = Mock()

    def complete(self):
        return complete_legacy_takeover(self.pin, self.readback, self.observe, self.control)

    def commit_takeover(self, *args, **kwargs):
        self.state["ownership"].update(epoch=22, phase="active", owner_actor=self.pin.actor)

    def test_refuses_missing_or_mismatched_inverse_before_any_provider_call(self):
        original = self.readback
        mutations = (None, {**original, "run_attempt": True}, {**original, "run_id": "124"},
                     {**original, "fence_epoch": 20}, {**original, "image_digest": "sha256:" + "2" * 64},
                     {**original, "restored_ledgers_sha256": "4" * 64}, {**original, "state": "expanded"})
        for readback in mutations:
            with self.subTest(readback=readback), self.assertRaisesRegex(
                    GateError, "^production_inverse_readback_mismatch$"):
                self.readback = readback
                self.complete()
        self.observe.assert_not_called()
        self.control.assert_not_called()

    def test_refuses_drift_from_current_fence_or_restored_pair(self):
        self.state["container"]["image_digest"] = "sha256:" + "2" * 64
        with self.assertRaisesRegex(GateError, "^production_recovery_pair_changed$"):
            self.complete()
        self.state["container"]["image_digest"] = self.pin.image_digest
        self.state["ownership"].update(epoch=22, phase="fenced")
        with self.assertRaisesRegex(GateError, "^production_recovery_fence_changed$"):
            self.complete()
        self.state["ownership"].update(epoch=22, phase="active", owner_actor="different-actor")
        with self.assertRaisesRegex(GateError, "^production_recovery_fence_changed$"):
            self.complete()
        self.control.assert_not_called()

    def test_interrupted_takeover_resumes_committed_epoch_without_another_post(self):
        def commit_then_lose_acknowledgement(*args, **kwargs):
            self.commit_takeover()
            raise GateError("ownership_control_unavailable")

        self.control.side_effect = commit_then_lose_acknowledgement
        with self.assertRaisesRegex(GateError, "^ownership_control_unavailable$"):
            self.complete()
        self.state["ownership"]["running"] = True
        self.assertEqual(self.complete(), "resumed")
        self.control.assert_called_once_with("takeover-fenced", 21, expected_deployment=self.pin.worker_version)

    def test_interrupted_takeover_can_resume_only_at_the_same_fenced_epoch(self):
        self.control.side_effect = GateError("ownership_control_unavailable")
        with self.assertRaisesRegex(GateError, "^ownership_control_unavailable$"):
            self.complete()
        self.control.reset_mock(side_effect=True)
        self.control.side_effect = self.commit_takeover
        self.assertEqual(self.complete(), "taken-over")
        self.control.assert_called_once_with("takeover-fenced", 21, expected_deployment=self.pin.worker_version)

    def test_acknowledgement_without_owned_epoch_is_not_success(self):
        with self.assertRaisesRegex(GateError, "^production_recovery_takeover_unconfirmed$"):
            self.complete()
        self.control.assert_called_once_with("takeover-fenced", 21, expected_deployment=self.pin.worker_version)

    def test_persisted_pending_takeover_refuses_another_post(self):
        self.state["ownership"].update(epoch=22, phase="fenced", owner_actor=self.pin.actor)
        with self.assertRaisesRegex(GateError, "^production_recovery_takeover_pending_reconcile$"):
            self.complete()
        self.control.assert_not_called()

    def test_recovery_adapter_pins_release_flag_epoch_and_deployment(self):
        control = rollback.control_client("production", "synthetic-token", "https://fixture.invalid", "", self.pin.actor)
        with patch.object(rollback.subprocess, "run", return_value=Mock(returncode=0, stdout="{}")) as run:
            control("takeover-fenced", 21, expected_deployment=self.pin.worker_version)
        args, options = run.call_args
        self.assertEqual(args[0][-2:], ["takeover-fenced", "21"])
        self.assertEqual(options["env"]["OWNERSHIP_RELEASE_FENCE"], "true")
        self.assertEqual(options["env"]["OWNERSHIP_EXPECTED_DEPLOYMENT"], self.pin.worker_version)
        self.assertEqual(options["env"]["OWNERSHIP_ACTOR"], self.pin.actor)
