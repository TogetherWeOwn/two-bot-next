"""Acceptance pins for #979 deploy rollback gates (TOG-25125).

Offline only: no Cloudflare, GitHub, or staging credentials. The release
gate is judged through scripts/production_deploy.py, the rollback path
through the workflow text, and the worker+image restore through
scripts/worker_container_rollback.py with an in-memory client.

Failing-first: each gate fails on main without #979 and passes with it.
"""

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

PROD_WORKFLOW = ROOT / ".github/workflows/deploy-production.yml"
STAGING_WORKFLOW = ROOT / ".github/workflows/deploy-staging.yml"
SCRIPT = ROOT / "scripts/production_deploy.py"

SHA = "0123456789abcdef0123456789abcdef01234567"
BUILD_ID = "700-1"


def report(revision=SHA, build_id=BUILD_ID, gateway="ready"):
    components = [
        ["process", "ready"],
        ["gateway", gateway],
        ["database", "ready"],
        ["token_invalid", "ready"],
    ]
    return json.dumps(
        {
            "components": components,
            "jobs": {},
            "build_revision": revision,
            "build_id": build_id,
        }
    )


def judge(status="200", body=None, mode="deploy", sha=SHA, build_id=BUILD_ID):
    scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
    with tempfile.TemporaryDirectory(dir=scratch) as tmp:
        path = Path(tmp) / "readyz.json"
        if body is not None:
            path.write_text(body)
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "readyz",
                "--status",
                status,
                "--body",
                str(path),
                "--sha",
                sha,
                "--mode",
                mode,
                "--build-id",
                build_id,
            ],
            capture_output=True,
            text=True,
            timeout=60,
        )
        return result.returncode, result.stdout.strip()


def load_rollback_module():
    spec = importlib.util.find_spec("worker_container_rollback")
    assert spec is not None, (
        "scripts/worker_container_rollback.py is missing: "
        "a Worker-only rollback leaves the new image serving"
    )
    import worker_container_rollback

    return worker_container_rollback


class ParkedGatewayGateTests(unittest.TestCase):
    def test_parked_503_fails_deploy_and_asks_for_rollback(self):
        parked = report(gateway="down")
        code, message = judge("503", parked)
        self.assertEqual(
            code,
            1,
            f"a parked gateway must never certify a deploy, got: {message}",
        )
        self.assertIn("gateway not ready", message)
        self.assertIn("roll back", message)

    def test_starting_200_fails_deploy(self):
        starting = report(gateway="starting")
        code, message = judge("200", starting)
        self.assertEqual(
            code,
            1,
            f"gateway starting is not ready, got: {message}",
        )
        self.assertIn("gateway not ready", message)

    def test_ready_200_passes_deploy(self):
        code, message = judge("200", report(gateway="ready"))
        self.assertEqual(code, 0, message)
        self.assertIn("gateway ready", message)


class RollbackPathTests(unittest.TestCase):
    def test_production_guard_never_blocks_rollback_on_staging_history(self):
        text = PROD_WORKFLOW.read_text()
        self.assertIn("waived for a rollback", text)
        self.assertIn("never blocks recovery", text)

    def test_failed_gate_auto_rolls_back_worker_and_image(self):
        text = PROD_WORKFLOW.read_text()
        self.assertIn("Auto-roll back Worker version and container image", text)
        self.assertIn("Snapshot the pre-deploy rollback state", text)
        self.assertIn("Roll back production Worker version and container image", text)
        self.assertIn("Gate on stable /health + /readyz gateway-ready", text)

    def test_staging_snapshots_before_deploy_for_auto_rollback(self):
        text = STAGING_WORKFLOW.read_text()
        self.assertIn("Snapshot the pre-deploy rollback state", text)
        self.assertIn("Auto-roll back Worker version and container image", text)


class WorkerImageRollsTests(unittest.TestCase):
    def test_stable_gate_needs_18_consecutive_ready_polls(self):
        rollback = load_rollback_module()
        self.assertEqual(rollback.STABLE_PASSES, 18)

    def test_image_must_be_digest_pinned(self):
        rollback = load_rollback_module()
        good = (
            "registry.cloudflare.com/acct/"
            "two-bot-next-twobotcontainer-staging@sha256:" + "a" * 64
        )
        self.assertEqual(
            rollback.restore_image_shape(
                good, "two-bot-next-twobotcontainer-staging"
            ),
            good,
        )
        for bad in (
            "latest",
            good.replace("@sha256:", ":latest@sha256:"),
            good[:-1],
            good.replace("two-bot-next-twobotcontainer-staging", "other-app"),
        ):
            with self.subTest(bad=bad[:40]):
                with self.assertRaises(Exception):
                    rollback.restore_image_shape(
                        bad, "two-bot-next-twobotcontainer-staging"
                    )

    def test_rollback_restores_worker_and_image_together(self):
        rollback = load_rollback_module()
        import staging_rollout as base

        worker = "two-bot-next-staging"
        application = "two-bot-next-twobotcontainer-staging"
        app_id = "app-1"
        good_version = "11111111-1111-4111-8111-111111111111"
        bad_version = "22222222-2222-4222-8222-222222222222"
        good_image = (
            "registry.cloudflare.com/acct/"
            "two-bot-next-twobotcontainer-staging@sha256:" + "a" * 64
        )

        state = {"serving": bad_version, "created": False}

        def rollout_row(rollout_id, image, created, status="completed"):
            target_version = 9 if rollout_id == "r-new" else 7
            return {
                "id": rollout_id,
                "created_at": created,
                "status": status,
                "strategy": "rolling",
                "kind": "full_auto",
                "target_version": target_version,
                "current_version": target_version - 1,
                "target_configuration": {"image": image},
                "current_configuration": {"image": image},
                "health": {
                    "instances": {
                        "active": 1,
                        "healthy": 1,
                        "failed": 0,
                        "starting": 0,
                        "scheduling": 0,
                    }
                },
                "progress": {
                    "total_steps": 1,
                    "current_step": 1,
                    "total_instances": 1,
                    "updated_instances": 1,
                },
                "steps": [{"status": "completed"}],
            }

        class FakeClient(rollback.RollbackClient):
            def __init__(self):
                super().__init__("a" * 32, "token")
                self.posts = []

            def api(self, path):
                if path == "/containers/applications":
                    return [
                        {
                            "id": app_id,
                            "name": application,
                            "durable_objects": {"namespace_id": "ns-1"},
                            "configuration": {"image": good_image},
                        }
                    ]
                if path.endswith("/deployments"):
                    return {
                        "deployments": [
                            {
                                "versions": [
                                    {
                                        "version_id": state["serving"],
                                        "percentage": 100,
                                    }
                                ]
                            }
                        ]
                    }
                if path.endswith("/rollouts?limit=100"):
                    return [
                        rollout_row(
                            "r-prev", good_image, "2026-10-14T00:00:00Z"
                        )
                    ]
                if path.endswith("/rollouts/r-new"):
                    return rollout_row(
                        "r-new", good_image, "2026-10-15T01:00:00Z"
                    )
                raise AssertionError(f"unexpected GET {path}")

            def post(self, path, body):
                self.posts.append((path, body))
                if path.endswith("/deployments"):
                    state["serving"] = body["versions"][0]["version_id"]
                    return {"ok": True}
                if path.endswith("/rollouts"):
                    state["created"] = True
                    return {"id": "r-new", "target_version": 9}
                raise AssertionError(f"unexpected POST {path}")

        def fake_control(action, epoch=None, expected_deployment=None):
            return {"owner": {"phase": "active", "epoch": 9}}

        clock = {"t": 0.0}

        def now():
            return clock["t"]

        def sleep(seconds):
            clock["t"] += seconds

        client = FakeClient()
        rb = rollback.Rollback(
            client,
            fake_control,
            "https://two-bot-next-staging.workers.dev",
            "staging",
            worker,
            application,
            app_id,
            SHA,
            "700",
            now=now,
            sleep=sleep,
            log=lambda msg: None,
        )
        rb.restore_worker(good_version)
        rb.restore_image(good_image)

        worker_post = next(
            body for path, body in client.posts if "deployments" in path
        )
        self.assertEqual(
            worker_post["versions"],
            [{"version_id": good_version, "percentage": 100}],
        )
        self.assertEqual(
            worker_post["code_update_strategy"], {"mode": "immediate"}
        )
        self.assertNotIn("force", json.dumps(worker_post))

        image_post = next(
            body
            for path, body in client.posts
            if "rollouts" in path and "deployments" not in path
        )
        self.assertEqual(image_post["strategy"], "rolling")
        self.assertEqual(image_post["step_percentage"], 100)
        self.assertEqual(
            image_post["target_configuration"], {"image": good_image}
        )
        self.assertEqual(state["serving"], good_version)
        self.assertTrue(state["created"])
        self.assertEqual(bad_version.count("-"), 4)


if __name__ == "__main__":
    unittest.main()
