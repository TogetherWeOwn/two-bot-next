"""Pin the protected workflow and legacy rollback entry points."""

from hashlib import sha256
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from production_recovery_artifact import FENCE_JOB, FENCE_STEP, JOB, UPLOAD_STEP, WRITE_STEP


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
        steps = jobs["release-apply"]["steps"]
        self.assertFalse(any("cargo build" in step.get("run", "") for step in steps))
        download = next(step for step in steps if step.get("name") == "Download the prebuilt protected forward runner")
        self.assertEqual(download["with"]["artifact-ids"], "${{ needs.release-plan.outputs.runner-artifact-id }}")
        self.assertEqual(download["with"]["path"], "${{ runner.temp }}/production-release-runner")
        verify = next(step for step in steps if step.get("name") == "Verify the prebuilt forward runner checksum")
        apply = next(step for step in steps if step.get("name") == "Apply the protected forward transaction")
        self.assertLess(steps.index(download), steps.index(verify))
        self.assertLess(steps.index(verify), steps.index(apply))
        self.assertEqual(apply["env"]["RELEASE_RUNNER"], "${{ runner.temp }}/production-release-runner/staging-migrate")
        with tempfile.TemporaryDirectory() as temp:
            runner = Path(temp) / "production-release-runner" / "staging-migrate"
            runner.parent.mkdir()
            original = b"offline binary fixture"
            checksum = sha256(original).hexdigest()
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

    def test_workflow_keeps_secrets_scoped_and_all_three_rollbacks_share_the_guard(self):
        path = ROOT / ".github/workflows"
        workflow = yaml.safe_load((path / "deploy-production-recovery.yml").read_text())
        self.assertEqual(set(workflow[True]), {"workflow_call"})
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

    def test_callee_binds_the_caller_pins_and_exact_producer_jobs(self):
        path = ROOT / ".github/workflows/deploy-production-recovery.yml"
        callee = yaml.safe_load(path.read_text())
        call = callee[True]["workflow_call"]
        pins = {"RELEASE_SHA": "candidate-sha", "RELEASE_PINS": "pins", "RELEASE_PLAN_ID": "plan-run-id",
                "RELEASE_PREFLIGHT_EPOCH": "preflight-epoch", "RELEASE_STOPPED": "stopped",
                "FORWARD_ARTIFACT_ID": "forward-artifact-id", "FORWARD_ARCHIVE_SHA256": "forward-archive-sha256",
                "CANDIDATE_WORKER": "candidate-worker",
                "PRODUCTION_HOST": "database-host", "PRODUCTION_DATABASE": "database-name",
                "PRODUCTION_BRANCH_ID": "database-branch-id"}
        self.assertEqual(call["inputs"], {
            key: {"type": "string", "required": True} for key in pins.values()
        })
        self.assertEqual(callee["env"], {
            variable: "${{ inputs." + pin + " }}" for variable, pin in pins.items()
        })
        jobs = callee["jobs"]
        prefix = "Production release / Recover production schema / "
        self.assertEqual(prefix + jobs["fence"]["name"], FENCE_JOB)
        self.assertEqual(prefix + jobs["inverse"]["name"], JOB)
        self.assertIn(FENCE_STEP, [step.get("name") for step in jobs["fence"]["steps"]])
        steps = [step.get("name") for step in jobs["inverse"]["steps"]]
        self.assertLess(steps.index(WRITE_STEP), steps.index(UPLOAD_STEP))
        self.assertEqual(jobs["inverse"]["outputs"]["artifact_id"], "${{ steps.result.outputs.artifact-id }}")
        self.assertEqual(jobs["inverse"]["outputs"]["archive_sha256"], "${{ steps.result.outputs.artifact-digest }}")
        restore = next(step for step in jobs["restore"]["steps"]
                       if step.get("run") == "python3 scripts/production_recovery_workflow.py restore")
        self.assertEqual(restore["env"]["ARTIFACT_ID"], "${{ needs.inverse.outputs.artifact_id }}")
        self.assertEqual(restore["env"]["ARCHIVE_SHA256"], "${{ needs.inverse.outputs.archive_sha256 }}")
