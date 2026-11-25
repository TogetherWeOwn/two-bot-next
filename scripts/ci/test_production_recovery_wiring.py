"""Pin the protected workflow and legacy rollback entry points."""

from hashlib import sha256
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]


class RecoveryWiringTests(unittest.TestCase):
    def test_forward_runner_is_built_before_stop_and_verified_before_use(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/deploy-production.yml").read_text())
        jobs = workflow["jobs"]
        for phase in ("release-plan", "release-prepare", "release-apply"):
            self.assertIn("github.run_attempt == 1", jobs[phase]["if"])
        for phase in ("release-prepare", "release-apply"):
            self.assertIn("release-plan", jobs[phase]["needs"])
        plan = jobs["release-plan"]
        self.assertEqual(plan["outputs"]["runner-artifact-id"], "${{ steps.release-runner.outputs.artifact-id }}")
        self.assertEqual(plan["outputs"]["runner-sha256"], "${{ steps.build.outputs.sha256 }}")
        self.assertIn("cargo build", next(step["run"] for step in plan["steps"] if step.get("id") == "build"))
        upload = next(step for step in plan["steps"] if step.get("id") == "release-runner")
        self.assertEqual(upload["with"]["retention-days"], 14)
        steps = jobs["release-apply"]["steps"]
        self.assertFalse(any("cargo build" in step.get("run", "") for step in steps))
        pins = next(step for step in steps if step.get("name") == "Validate the protected forward runner pins")
        download = next(step for step in steps if step.get("name") == "Download the prebuilt protected forward runner")
        self.assertEqual(download["with"]["artifact-ids"], "${{ needs.release-plan.outputs.runner-artifact-id }}")
        self.assertEqual(download["with"]["path"], "${{ runner.temp }}/production-release-runner")
        verify = next(step for step in steps if step.get("name") == "Verify the prebuilt forward runner checksum")
        apply = next(step for step in steps if step.get("name") == "Apply the protected forward transaction")
        self.assertLess(steps.index(pins), steps.index(download))
        self.assertLess(steps.index(download), steps.index(verify))
        self.assertLess(steps.index(verify), steps.index(apply))
        self.assertEqual(apply["env"]["RELEASE_RUNNER"], "${{ runner.temp }}/production-release-runner/staging-migrate")
        with tempfile.TemporaryDirectory() as temp:
            runner = Path(temp) / "production-release-runner" / "staging-migrate"
            runner.parent.mkdir()
            original = b"offline binary fixture"
            checksum = sha256(original).hexdigest()
            for artifact, digest, expected in (("123", checksum, 0), ("invalid", checksum, 1), ("123", "f" * 63, 1)):
                with self.subTest(artifact=artifact, digest=digest):
                    result = subprocess.run(["bash", "-c", pins["run"]], capture_output=True, timeout=3, check=False,
                                            env={"ARTIFACT_ID": artifact, "RUNNER_SHA256": digest})
                    self.assertEqual(result.returncode, expected, result.stderr)
            target = runner.with_name("other-binary")
            target.write_bytes(original)
            runner.symlink_to(target)
            result = subprocess.run(["bash", "-c", verify["run"]], capture_output=True, timeout=3, check=False,
                                    env={"RUNNER_TEMP": temp, "RUNNER_SHA256": checksum})
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertFalse(target.stat().st_mode & 0o100)
            runner.unlink()
            for body, expected in ((b"changed fixture", 1), (original, 0)):
                with self.subTest(expected=expected):
                    runner.write_bytes(body)
                    result = subprocess.run(["bash", "-c", verify["run"]], capture_output=True, timeout=3,
                                            check=False, env={**os.environ, "RUNNER_TEMP": temp,
                                                              "RUNNER_SHA256": checksum})
                    self.assertEqual(result.returncode, expected, result.stderr)
                    self.assertEqual(bool(runner.stat().st_mode & 0o100), expected == 0)

    def test_interrupted_recovery_isolates_database_and_authenticates_restart(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/deploy-production-interrupted.yml").read_text())
        self.assertEqual(set(workflow[True]), {"workflow_call"})
        jobs = workflow["jobs"]
        self.assertEqual(jobs["database"]["needs"], "admit")
        self.assertEqual(jobs["restart"]["needs"], ["admit", "database"])
        self.assertEqual(jobs["database"]["environment"], "production-migrate-apply")
        self.assertNotIn("OWNERSHIP_CONTROL_TOKEN", str(jobs["database"]))
        for phase in ("admit", "restart"):
            self.assertNotIn("TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL", str(jobs[phase]))
        phases = jobs["database"]["steps"][2:]
        self.assertEqual([step["name"] for step in phases], [
            "Classify the interrupted database state under lock", "Retain the locked database classification",
            "Run the exact protected inverse for POST", "Retain the verified interrupted inverse",
        ])
        for step in phases[2:]:
            self.assertEqual(step["if"], "steps.classify.outputs.state == 'post'")
        restart = jobs["restart"]["steps"][2]
        self.assertIn("DECISION_ARCHIVE_SHA256", restart["env"])
        self.assertIn("INVERSE_ARCHIVE_SHA256", restart["env"])

    def test_release_retains_the_stop_intent_before_stopping(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/deploy-production.yml").read_text())
        prepare = workflow["jobs"]["release-prepare"]["steps"]
        order = [step.get("name") for step in prepare]
        retain = order.index("Retain the exact recovery stop intent")
        self.assertLess(retain, order.index("Confirm the exact stopped writer"))
        upload = prepare[retain]["with"]
        self.assertEqual(upload["if-no-files-found"], "error")
        self.assertTrue(upload["path"].endswith("/production-release-stop-intent.json"))

    def test_protected_rollout_verifies_identity_before_takeover(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/deploy-production.yml").read_text())
        jobs = workflow["jobs"]
        steps = jobs["production"]["steps"]
        names = [step.get("name") for step in steps]
        self.assertLess(names.index("Verify the exact protected Worker and configured image before takeover"),
                        names.index("Take over only the Worker produced by this protected upload"))
        recovery = jobs["release-recover"]
        self.assertEqual(recovery["with"]["candidate-worker"], "${{ needs.production.outputs.release-worker }}")
        self.assertIn("needs.release-apply.result == 'success'", recovery["if"])

    def test_workflow_keeps_secrets_scoped_and_all_three_rollbacks_share_the_guard(self):
        path = ROOT / ".github/workflows"
        workflow = yaml.safe_load((path / "deploy-production-recovery.yml").read_text())
        self.assertEqual(set(workflow[True]), {"workflow_call"})
        self.assertEqual(workflow["env"]["RELEASE_TARGET_FINGERPRINT"], "${{ inputs.target-fingerprint }}")
        self.assertNotIn("TWO_BOT_PRODUCTION_BRANCH_ID", str(workflow))
        jobs = workflow["jobs"]
        self.assertEqual(jobs["inverse"]["environment"], "production-migrate-apply")
        self.assertEqual(jobs["inverse"]["needs"], "fence")
        self.assertEqual(jobs["restore"]["needs"], ["fence", "inverse"])
        self.assertNotIn("OWNERSHIP_CONTROL_TOKEN", str(jobs["inverse"]))
        for phase in ("fence", "restore"):
            self.assertNotIn("TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL", str(jobs[phase]))
        ordinary = yaml.safe_load((path / "deploy-production.yml").read_text())["jobs"]["production"]["steps"]
        for step_id in ("rollback", "auto_rollback", "watch_rollback"):
            command = next(step["run"] for step in ordinary if step.get("id") == step_id)
            self.assertIn("worker_container_rollback.py rollback", command)
            self.assertIn("--variant production", command)
