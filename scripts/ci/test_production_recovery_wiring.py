"""Pin the protected workflow and legacy rollback entry points."""

from contextlib import redirect_stdout
from hashlib import sha256
from io import StringIO
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

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
        build_run = next(step["run"] for step in plan["steps"] if step.get("id") == "build")
        self.assertIn("cargo build", build_run)
        self.assertIn("sha256sum target/debug/staging-migrate", build_run)
        self.assertIn('printf \'sha256=%s\\n\' "${checksum%% *}" >> "$GITHUB_OUTPUT"', build_run)
        upload = next(step for step in plan["steps"] if step.get("id") == "release-runner")
        self.assertEqual(upload["with"]["retention-days"], 14)
        steps = jobs["release-apply"]["steps"]
        self.assertFalse(any("cargo build" in step.get("run", "") for step in steps))
        pins = next(step for step in steps if step.get("name") == "Validate the protected forward runner pins")
        self.assertEqual(pins["env"], {
            "ARTIFACT_ID": "${{ needs.release-plan.outputs.runner-artifact-id }}",
            "RUNNER_SHA256": "${{ needs.release-plan.outputs.runner-sha256 }}",
        })
        download = next(step for step in steps if step.get("name") == "Download the prebuilt protected forward runner")
        self.assertEqual(download["with"]["artifact-ids"], "${{ needs.release-plan.outputs.runner-artifact-id }}")
        self.assertEqual(download["with"]["path"], "${{ runner.temp }}/production-release-runner")
        verify = next(step for step in steps if step.get("name") == "Verify the prebuilt forward runner checksum")
        self.assertEqual(verify["env"], {
            "RUNNER_SHA256": "${{ needs.release-plan.outputs.runner-sha256 }}",
        })
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

    def test_interrupted_dispatch_never_enters_ordinary_or_forward_deploy(self):
        jobs = yaml.safe_load((ROOT / ".github/workflows/deploy-production.yml").read_text())["jobs"]
        guard = jobs["guard"]["steps"][0]
        self.assertEqual(guard["env"].get("RELEASE_RECOVERY_RUN_ID"), "${{ inputs.release_recovery_run_id }}")
        env = {"GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next", "EVENT_NAME": "workflow_dispatch",
               "GITHUB_REF": "refs/heads/main", "GITHUB_RUN_ATTEMPT": "1", "AUTO_DISABLED": "true",
               "TAKEOVER": "true", "ALLOW_UNSTAGED": "false", "CUTOVER_HOLD": "false", "RELEASE_RECOVERY_RUN_ID": "41"}
        with (patch.dict(os.environ, env, clear=True), redirect_stdout(StringIO()) as stdout,
              patch("subprocess.check_output", side_effect=AssertionError("unexpected GitHub call")),
              patch("urllib.request.urlopen", side_effect=AssertionError("unexpected provider call")),
              self.assertRaises(SystemExit) as refused):
            exec(compile(guard["run"], "deploy-production guard", "exec"), {})
        self.assertEqual(refused.exception.code, 1)
        self.assertIn("interrupted recovery requires the activation follow-up", stdout.getvalue())
        self.assertIn("needs.guard.outputs.mode != 'interrupted-recovery'", jobs["production"]["if"])
        self.assertEqual(jobs["release-reconcile"]["name"], "Production release / Reconcile interrupted apply")

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

    def test_release_plan_target_fingerprint_reaches_each_consumer(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/deploy-production.yml").read_text())
        jobs = workflow["jobs"]
        plan = jobs["release-plan"]
        self.assertEqual(plan["outputs"].get("target-fingerprint"),
                         "${{ steps.release-target.outputs.target-fingerprint }}")
        producer = next(step for step in plan["steps"] if step.get("id") == "release-target")
        self.assertEqual(producer["run"], "python3 scripts/production_release.py plan")
        fingerprint = "${{ needs.release-plan.outputs.target-fingerprint }}"
        for phase in ("release-prepare", "release-apply", "production", "release-recover"):
            self.assertIn("release-plan", jobs[phase]["needs"], phase)
        for phase in ("release-prepare", "release-apply"):
            self.assertEqual(jobs[phase]["env"].get("RELEASE_TARGET_FINGERPRINT"), fingerprint, phase)
        self.assertEqual(jobs["release-prepare"]["outputs"].get("epoch"), "${{ steps.preflight.outputs.epoch }}")
        epoch = "${{ needs.release-prepare.outputs.epoch }}"
        for phase in ("production", "release-recover"):
            self.assertIn("release-prepare", jobs[phase]["needs"], phase)
        commands = {"python3 scripts/production_release.py authorize-deploy",
                    "python3 scripts/production_release.py takeover"}
        consumers = [step for step in jobs["production"]["steps"] if step.get("run") in commands]
        self.assertEqual(len(consumers), 3)
        for step in consumers:
            self.assertEqual(step["env"].get("RELEASE_TARGET_FINGERPRINT"), fingerprint, step["name"])
            self.assertEqual(step["env"].get("RELEASE_PREFLIGHT_EPOCH"), epoch, step["name"])
        self.assertEqual(jobs["release-recover"]["with"].get("target-fingerprint"), fingerprint)
        self.assertEqual(jobs["release-recover"]["with"].get("preflight-epoch"), epoch)

    def test_release_retains_the_stop_intent_before_stopping(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/deploy-production.yml").read_text())
        prepare = workflow["jobs"]["release-prepare"]["steps"]
        order = [step.get("name") for step in prepare]
        retain = order.index("Retain the exact recovery stop intent")
        self.assertLess(retain, order.index("Confirm the exact stopped writer"))
        upload = prepare[retain]["with"]
        self.assertEqual(upload["name"], "production-release-stop-intent-${{ github.run_id }}-${{ github.run_attempt }}")
        self.assertEqual(upload["if-no-files-found"], "error")
        self.assertTrue(upload["path"].endswith("/production-release-stop-intent.json"))

    def test_protected_rollout_verifies_identity_before_takeover(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/deploy-production.yml").read_text())
        jobs = workflow["jobs"]
        steps = jobs["production"]["steps"]
        names = [step.get("name") for step in steps]
        self.assertLess(names.index("Verify the exact protected Worker and configured image before takeover"),
                        names.index("Take over only the Worker produced by this protected upload"))

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
                "CANDIDATE_WORKER": "candidate-worker", "RELEASE_TARGET_FINGERPRINT": "target-fingerprint"}
        self.assertEqual(call["inputs"], {
            key: {"type": "string", "required": True} for key in pins.values()
        })
        self.assertEqual(callee["env"], {
            variable: "${{ inputs." + pin + " }}" for variable, pin in pins.items()
        })
        jobs = callee["jobs"]
        for job in jobs.values():
            self.assertEqual(job["permissions"], {"contents": "read", "actions": "read"})
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
