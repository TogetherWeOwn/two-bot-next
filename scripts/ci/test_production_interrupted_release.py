"""Exercise recovery routing after an interrupted protected schema apply."""

from copy import deepcopy
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import production_recovery_artifact as artifacts
import production_recovery_workflow as recovery
from production_release_preflight import ReleasePreflight
from staging_rollout import GateError


class InterruptedReleaseTests(unittest.TestCase):
    def setUp(self):
        self.pin = ReleasePreflight(
            workflow_sha="a" * 40,
            candidate_sha="b" * 40,
            candidate_archive="sha256:" + "1" * 64,
            candidate_image="sha256:" + "2" * 64,
            rollback_sha="c" * 40,
            rollback_worker="33333333-3333-4333-8333-333333333333",
            rollback_image="sha256:" + "4" * 64,
            application_id="55555555-5555-4555-8555-555555555555",
            namespace_id="6" * 32,
            restore_proof_sha256="7" * 64,
        )
        self.origin = "123"
        self.epoch = 21
        self.actor = f"production-schema-release:{self.origin}:{self.pin.candidate_sha}"
        self.stopped = {
            "worker": {
                "version_id": self.pin.rollback_worker,
                "sha": self.pin.rollback_sha,
                "traffic_percent": 100,
            },
            "container": {
                "application_id": self.pin.application_id,
                "namespace_id": self.pin.namespace_id,
                "image_digest": self.pin.rollback_image,
                "rollout_status": "completed",
            },
            "ownership": {
                "epoch": self.epoch,
                "running": False,
                "phase": "fenced",
                "owner_actor": self.actor,
            },
        }
        self.live = deepcopy(self.stopped)
        self.observe = Mock(side_effect=lambda: deepcopy(self.live))
        self.control = Mock()
        self.recover_post = Mock()

    def reconcile(self, classification):
        return recovery.reconcile_interrupted(
            self.pin, self.origin, self.epoch, classification,
            self.observe, self.control, self.recover_post,
        )

    def origin_run(self):
        repo = {"id": 1396224242, "full_name": "TogetherWeOwn/two-bot-next"}
        return {
            "id": int(self.origin),
            "run_attempt": 1,
            "head_sha": self.pin.workflow_sha,
            "head_branch": "main",
            "repository": repo,
            "head_repository": repo,
            "actor": {"id": 42},
            "triggering_actor": {"id": 42},
            "path": ".github/workflows/deploy-production.yml",
            "event": "workflow_dispatch",
            "status": "completed",
            "conclusion": "cancelled",
        }

    def test_cancelled_origin_admitted_only_after_it_is_terminal(self):
        cancelled = self.origin_run()
        api = Mock(return_value=cancelled)
        self.assertEqual(artifacts.interrupted_origin(self.origin, "456", api), cancelled)
        api.assert_called_once_with(f"actions/runs/{self.origin}")

        api.return_value = {**cancelled, "status": "in_progress", "conclusion": None}
        with self.assertRaises(GateError):
            artifacts.interrupted_origin(self.origin, "456", api)

    def test_pre_lost_reply_confirms_exact_predecessor_without_a_second_takeover(self):
        def commit_then_lose_reply(*_args, **_kwargs):
            self.live["ownership"].update(epoch=self.epoch + 1, running=True, phase="active")
            raise GateError("fixture_takeover_reply_lost")

        self.control.side_effect = commit_then_lose_reply
        expected = {"state": "pre", "worker": self.pin.rollback_worker, "epoch": self.epoch + 1}
        self.assertEqual(self.reconcile({"state": "pre"}), expected)
        self.assertEqual(self.reconcile({"state": "pre"}), expected)
        self.control.assert_called_once_with(
            "takeover-fenced", self.epoch, expected_deployment=self.pin.rollback_worker,
        )
        self.recover_post.assert_not_called()

    def test_verified_post_hands_the_original_stamp_to_protected_inverse(self):
        stamp = "d" * 64
        inverse_result = {"state": "restored"}
        self.recover_post.return_value = inverse_result

        self.assertEqual(self.reconcile({"state": "post", "attendance_fence_sha256": stamp}), inverse_result)
        self.recover_post.assert_called_once_with(stamp)
        self.control.assert_not_called()
        self.assertEqual(self.live, self.stopped)

    def test_unproved_database_result_leaves_the_stop_in_place(self):
        for classification in (
            {"state": "ambiguous"},
            {"state": "post"},
            {"state": "pre", "attendance_fence_sha256": "d" * 64},
        ):
            with self.subTest(classification=classification), self.assertRaises(GateError):
                self.reconcile(classification)
        self.control.assert_not_called()
        self.recover_post.assert_not_called()
        self.assertEqual(self.live, self.stopped)

    def test_verified_database_rejects_changed_worker_image_or_fence(self):
        for section, field, value in (
            ("worker", "version_id", "88888888-8888-4888-8888-888888888888"),
            ("container", "image_digest", self.pin.candidate_image),
            ("ownership", "owner_actor", f"production-schema-release:124:{self.pin.candidate_sha}"),
            ("ownership", "epoch", self.epoch + 1),
        ):
            for classification in ({"state": "pre"}, {"state": "post", "attendance_fence_sha256": "d" * 64}):
                with self.subTest(section=section, field=field, classification=classification):
                    self.live = deepcopy(self.stopped)
                    self.live[section][field] = value
                    with self.assertRaises(GateError):
                        self.reconcile(classification)
        self.control.assert_not_called()
        self.recover_post.assert_not_called()

    def test_pre_pending_epoch_after_lost_reply_cannot_start_a_second_writer(self):
        def reserve_then_lose_reply(*_args, **_kwargs):
            self.live["ownership"]["epoch"] += 1
            raise GateError("fixture_takeover_reply_lost")

        self.control.side_effect = reserve_then_lose_reply
        with self.assertRaises(GateError):
            self.reconcile({"state": "pre"})
        with self.assertRaises(GateError):
            self.reconcile({"state": "pre"})
        self.control.assert_called_once_with(
            "takeover-fenced", self.epoch, expected_deployment=self.pin.rollback_worker,
        )
        self.recover_post.assert_not_called()
        self.assertEqual(self.live["ownership"], {**self.stopped["ownership"], "epoch": self.epoch + 1})


if __name__ == "__main__":
    unittest.main()
