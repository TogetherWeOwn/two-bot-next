"""Exercise recovery routing after an interrupted protected schema apply."""

from copy import deepcopy
from hashlib import sha256
from io import BytesIO
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch
from zipfile import ZipFile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import production_recovery_workflow as recovery
import production_release_preflight as preflight
from staging_rollout import GateError


class InterruptedReleaseTests(unittest.TestCase):
    def setUp(self):
        self.pin = preflight.ReleasePreflight(
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

    def recovery_admission(self):
        repo = {"id": 1396224242, "full_name": "TogetherWeOwn/two-bot-next"}
        origin = {
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
        current = {**origin, "id": 456, "head_sha": "f" * 40, "status": "in_progress", "conclusion": None}
        env = {
            "RELEASE_RECOVERY_RUN_ID": self.origin,
            "GITHUB_RUN_ID": str(current["id"]),
            "GITHUB_SHA": current["head_sha"],
            "GITHUB_WORKFLOW_SHA": current["head_sha"],
            "GITHUB_REPOSITORY": repo["full_name"],
            "GITHUB_REPOSITORY_ID": str(repo["id"]),
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_RUN_ATTEMPT": "1",
            "GITHUB_ACTOR_ID": "42",
            "GITHUB_REF": "refs/heads/main",
            "GITHUB_REF_PROTECTED": "true",
            "GITHUB_WORKFLOW_REF": f"{repo['full_name']}/{origin['path']}@refs/heads/main",
        }
        steps = ("Verify forward release preflight", "Retain the exact recovery stop intent",
                 "Confirm the exact stopped writer")
        job = {"run_id": origin["id"], "run_attempt": 1, "head_sha": origin["head_sha"], "status": "completed"}
        prepare = {**job, "name": "Production release / Prepare and stop the prior pair", "conclusion": "success",
                   "steps": [{"name": name, "status": "completed", "conclusion": "success"} for name in steps]}
        apply = {**job, "name": "Production release / Apply the approved schema", "conclusion": "cancelled",
                 "steps": [{"name": "Apply the protected forward transaction", "status": "completed",
                            "conclusion": "cancelled"},
                           {"name": "Upload the verified forward result", "status": "completed",
                            "conclusion": "skipped"}]}
        plan_id = "789"
        plan = {"target": {"host": "db.invalid", "database": "fixture", "branch_id": "fixture"}}
        plan_artifact = {"archive_digest": "sha256:" + "9" * 64}
        intent = preflight.stop_intent(self.pin, origin, plan_id, plan, plan_artifact, self.epoch - 1)
        archive = BytesIO()
        with ZipFile(archive, "w") as bundle:
            bundle.writestr("production-release-stop-intent.json", json.dumps(intent))
        raw = archive.getvalue()
        entry = {"id": 654, "name": f"production-release-stop-intent-{self.origin}-1", "expired": False,
                 "digest": f"sha256:{sha256(raw).hexdigest()}", "size_in_bytes": len(raw)}
        lineage = {"id": origin["id"], "head_sha": origin["head_sha"], "head_branch": "main",
                   "repository_id": repo["id"], "head_repository_id": repo["id"]}
        root = f"actions/runs/{self.origin}"
        routes = {
            root: origin,
            f"actions/runs/{current['id']}": current,
            f"compare/{origin['head_sha']}...{current['head_sha']}": {
                "status": "ahead", "merge_base_commit": {"sha": origin["head_sha"]},
            },
            f"{root}/attempts/1/jobs?per_page=30": {"total_count": 2, "jobs": [prepare, apply]},
            f"{root}/artifacts?per_page=30": {"total_count": 1, "artifacts": [entry]},
            f"actions/artifacts/{entry['id']}": {**entry, "workflow_run": lineage},
            f"actions/artifacts/{entry['id']}/zip": raw,
        }
        return env, routes, (self.pin, origin, plan_id, plan, plan_artifact, self.epoch)

    def test_cancelled_apply_admits_exact_durable_intent_without_forward_artifact(self):
        env, routes, expected = self.recovery_admission()
        pin, origin, plan_id, plan, artifact, _ = expected
        api = Mock(side_effect=lambda route, **_kwargs: routes[route])
        with patch.object(preflight, "bound_plan", return_value=(plan, artifact)) as bound:
            self.assertEqual(preflight.interrupted_release_context(env, api), expected)
        bound.assert_called_once_with(pin, origin, plan_id, preflight.digest_json(plan["target"]), api)
        api.assert_any_call("actions/artifacts/654/zip", binary=True)

    def test_cancelled_apply_refuses_unfinished_origin_or_unproved_intent(self):
        env, routes, _ = self.recovery_admission()
        root = f"actions/runs/{self.origin}"
        jobs = f"{root}/attempts/1/jobs?per_page=30"
        cases = {
            "in progress": lambda routes: routes[root].update(status="in_progress", conclusion=None),
            "rerun": lambda routes: routes[root].update(run_attempt=2),
            "cancelled intent upload": lambda routes: routes[jobs]["jobs"][0]["steps"][1].update(
                conclusion="cancelled",
            ),
            "different producer": lambda routes: routes["actions/artifacts/654"]["workflow_run"].update(id=456),
            "wrong archive": lambda routes: routes.update({"actions/artifacts/654/zip": b"synthetic replacement"}),
        }
        for name, mutate in cases.items():
            with self.subTest(name=name):
                changed = deepcopy(routes)
                mutate(changed)
                api = Mock(side_effect=lambda route, **_kwargs: changed[route])
                with patch.object(preflight, "bound_plan") as bound, self.assertRaises(GateError):
                    preflight.interrupted_release_context(env, api)
                bound.assert_not_called()

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

    def test_pre_control_reply_needs_confirmed_ownership(self):
        self.control.return_value = {"deploymentId": self.pin.rollback_worker}

        with self.assertRaises(GateError):
            self.reconcile({"state": "pre"})
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
