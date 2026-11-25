"""Pin the protected workflow and legacy rollback entry points."""

from hashlib import sha256
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import yaml

ROOT = Path(__file__).resolve().parents[2]


class RecoveryWiringTests(unittest.TestCase):
    def test_protected_manual_entry_selects_the_exact_green_staging_attempt(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/deploy-production.yml").read_text())
        script = workflow["jobs"]["guard"]["steps"][0]["run"]
        repo, sha = "TogetherWeOwn/two-bot-next", "a" * 40
        result = {"head_sha": sha, "status": "completed", "conclusion": "success"}
        ci = {**result, "id": 51, "run_number": 26, "run_attempt": 1, "check_suite_id": 52}
        staging = {**result, "id": 61, "run_number": 15, "run_attempt": 2}
        checks = {name: {**result, "name": name, "url": f"fixture/{index}",
                         "check_suite": {"id": 52}, "app": {"id": 15368}}
                  for index, name in enumerate(("ci-ok", "worker check"))}
        responses = {
            f"compare/{sha}...main": {"status": "identical"},
            "actions/workflows/check.yml/runs": {"total_count": 1, "workflow_runs": [ci]},
            "actions/runs/51": ci,
            "actions/runs/51/attempts/1/jobs": {"total_count": 2, "jobs": [
                {**result, "name": name, "run_id": 51, "run_attempt": 1, "check_run_url": check["url"]}
                for name, check in checks.items()]},
            "actions/workflows/deploy-staging.yml/runs": {"total_count": 1, "workflow_runs": [staging]},
            "actions/runs/61/attempts/2/jobs": {"total_count": 1, "jobs": [
                {**result, "name": "staging user-flow synthetic (exact SHA)", "run_id": 61, "run_attempt": 2}]},
            "environments/production": {"deployment_branch_policy": {"custom_branch_policies": True},
                                        "protection_rules": [{"type": "required_reviewers", "reviewers": [1]}]},
            "environments/production/deployment-branch-policies": {"branch_policies": [{"name": "main"}]},
        }

        def github(command, **_kwargs):
            self.assertEqual(command[:4], ["gh", "api", "--method", "GET"])
            route = command[4].removeprefix(f"repos/{repo}/")
            if route.endswith("/check-runs"):
                self.assertEqual(route, f"commits/{sha}/check-runs")
                return json.dumps({"check_runs": list(checks.values())})
            return json.dumps(responses[route])

        with tempfile.TemporaryDirectory() as temp:
            out = Path(temp) / "outputs"
            env = {"GITHUB_REPOSITORY": repo, "EVENT_NAME": "workflow_dispatch",
                   "GITHUB_RUN_ATTEMPT": "1", "GITHUB_REF": "refs/heads/main", "SHA": sha,
                   "ALLOW_UNSTAGED": "false", "TAKEOVER": "true", "CUTOVER_HOLD": "false",
                   "AUTO_DISABLED": "true", "RELEASE_PLAN_ID": "42", "RELEASE_PINS": "{}",
                   "GITHUB_OUTPUT": str(out), "GITHUB_STEP_SUMMARY": str(Path(temp) / "summary")}
            with (patch.dict(os.environ, env, clear=True), patch("subprocess.check_output", side_effect=github),
                  patch("urllib.request.urlopen", side_effect=AssertionError("unexpected provider call"))):
                exec(compile(script, "deploy-production guard", "exec"), {})
            outputs = dict(line.split("=", 1) for line in out.read_text().splitlines())
            self.assertEqual({key: outputs[key] for key in ("protected_release", "sha", "staging_run_id", "staging_attempt")},
                             {"protected_release": "true", "sha": sha, "staging_run_id": "61", "staging_attempt": "2"})

    def test_protected_manual_entry_rejects_unsafe_dispatch_before_provider_access(self):
        path = ROOT / ".github/workflows/deploy-production.yml"
        workflow = yaml.safe_load(path.read_text())
        script = workflow["jobs"]["guard"]["steps"][0]["run"]
        env = {"GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next", "EVENT_NAME": "workflow_dispatch",
               "GITHUB_RUN_ATTEMPT": "1", "GITHUB_REF": "refs/heads/main", "SHA": "a" * 40,
               "ALLOW_UNSTAGED": "false", "TAKEOVER": "true", "CUTOVER_HOLD": "false",
               "AUTO_DISABLED": "true", "RELEASE_PLAN_ID": "42", "RELEASE_PINS": "{}"}
        for mutation in ({"GITHUB_RUN_ATTEMPT": "2"}, {"EVENT_NAME": "schedule"}, {"RELEASE_PLAN_ID": ""},
                         {"RELEASE_PINS": ""}, {"SHA": "a" * 7}, {"ALLOW_UNSTAGED": "true"},
                         {"TAKEOVER": "false"}, {"CUTOVER_HOLD": "true"}, {"AUTO_DISABLED": ""}):
            with self.subTest(mutation=mutation):
                result = subprocess.run([sys.executable, "-c", script], env={**env, **mutation},
                                        capture_output=True, text=True, timeout=3, check=False)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("protected release requires", result.stdout)
                self.assertEqual(result.stderr, "")

    def test_protected_manual_entry_carries_the_lane_and_exact_evidence_through_watch(self):
        path = ROOT / ".github/workflows"
        workflow = yaml.safe_load((path / "deploy-production.yml").read_text())
        jobs = workflow["jobs"]
        migrate = yaml.safe_load((path / "production-migrate.yml").read_text())
        self.assertIn("(inputs.release_plan_run_id != '' || inputs.release_recovery_run_id != '') && 'deploy-production'",
                      workflow["concurrency"]["group"])
        self.assertIn("inputs.mode == 'apply' && 'deploy-production'", migrate["concurrency"]["group"])
        self.assertFalse(workflow["concurrency"]["cancel-in-progress"])
        self.assertEqual(workflow["concurrency"]["queue"], "max")
        self.assertIn("deploy-production-release-job-", jobs["production"]["concurrency"]["group"])
        self.assertEqual(jobs["release-plan"]["environment"], "production-migrate-plan")
        plan = jobs["release-plan"]
        self.assertEqual(plan["outputs"]["target-fingerprint"], "${{ steps.release-target.outputs.target-fingerprint }}")
        target_read = next(step for step in plan["steps"] if step.get("id") == "release-target")
        self.assertIn("TWO_BOT_PRODUCTION_BRANCH_ID", str(target_read["env"]))
        for phase in ("release-prepare", "release-apply", "production", "release-recover"):
            self.assertIn("release-plan", jobs[phase]["needs"])
            self.assertNotIn("TWO_BOT_PRODUCTION_BRANCH_ID", str(jobs[phase]))
        self.assertEqual(jobs["release-prepare"]["environment"], {"name": "production", "deployment": False})
        self.assertEqual(jobs["release-apply"]["environment"], "production-migrate-apply")
        for phase in ("release-plan", "release-prepare", "production", "release-recover"):
            self.assertNotIn("TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL", str(jobs[phase]))
        self.assertNotIn("OWNERSHIP_CONTROL_TOKEN", str(jobs["release-apply"]))
        self.assertIn("staging-soak-evidence", jobs["release-plan"]["needs"])
        prepare = [step.get("name") for step in jobs["release-prepare"]["steps"]]
        self.assertLess(prepare.index("Verify the approved staging archive without copying the image"),
                        prepare.index("Confirm the exact stopped writer"))
        production = [step.get("name") for step in jobs["production"]["steps"]]
        phases = ("Authorize the exact forward result and stopped pair", "Admit the immutable staging artifact before production mutation",
                  "Recheck the protected pair immediately before upload", "Deploy to production",
                  "Record the exact Worker version produced by this upload", "Take over only the Worker produced by this protected upload",
                  "Watch production for 30 minutes after the promotion", "Require a clean completed watch for the protected release")
        self.assertEqual([name for name in production if name in phases], list(phases))
        recover = jobs["release-recover"]
        self.assertIn("needs.release-apply.result == 'success' && needs.production.result != 'success'", recover["if"])
        self.assertEqual(recover["with"]["forward-artifact-id"], "${{ needs.release-apply.outputs.artifact-id }}")
        self.assertEqual(recover["with"]["forward-archive-sha256"], "${{ needs.release-apply.outputs.archive-sha256 }}")
        self.assertEqual(recover["with"]["candidate-worker"], "${{ needs.production.outputs.release-worker }}")
        self.assertEqual(recover["with"]["target-fingerprint"], "${{ needs.release-plan.outputs.target-fingerprint }}")
        for step in jobs["production"]["steps"]:
            if step.get("id") in ("auto_rollback", "watch_rollback"):
                self.assertIn("inputs.release_plan_run_id == ''", step["if"])
        watch = next(step for step in jobs["production"]["steps"] if step.get("name") == phases[-1])
        for verdict, status in (("clean", 0), ("breach", 1), ("superseded", 1), ("error", 1), ("", 1)):
            with self.subTest(verdict=verdict):
                result = subprocess.run(["bash", "-e", "-c", watch["run"]],
                                        env={"WATCH_VERDICT": verdict}, timeout=3, check=False)
                self.assertEqual(result.returncode, status)

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

    def test_interrupted_dispatch_never_enters_ordinary_or_forward_deploy(self):
        path = ROOT / ".github/workflows"
        workflow = yaml.safe_load((path / "deploy-production.yml").read_text())
        jobs = workflow["jobs"]
        script = jobs["guard"]["steps"][0]["run"]
        repo = "TogetherWeOwn/two-bot-next"
        responses = {
            "environments/production": {
                "name": "production",
                "deployment_branch_policy": {"custom_branch_policies": True, "protected_branches": False},
                "protection_rules": [{"type": "branch_policy"}, {"type": "required_reviewers", "reviewers": [1]}],
            },
            "environments/production/deployment-branch-policies": {
                "total_count": 1, "branch_policies": [{"type": "branch", "name": "main"}],
            },
        }
        base = {"GITHUB_REPOSITORY": repo, "GITHUB_REPOSITORY_ID": "1396224242",
                "GITHUB_REF": "refs/heads/main", "GITHUB_REF_PROTECTED": "true", "GITHUB_RUN_ID": "73",
                "GITHUB_RUN_ATTEMPT": "1", "EVENT_NAME": "workflow_dispatch", "AUTO_DISABLED": "true",
                "ALLOW_UNSTAGED": "false", "TAKEOVER": "true", "CUTOVER_HOLD": "false",
                "RELEASE_RECOVERY_RUN_ID": "41"}

        def github(command, **_kwargs):
            self.assertEqual(command[:4], ["gh", "api", "--method", "GET"])
            return json.dumps(responses[command[4].removeprefix(f"repos/{repo}/")])

        with tempfile.TemporaryDirectory() as temp:
            out = Path(temp) / "outputs"
            with (patch.dict(os.environ, {**base, "GITHUB_OUTPUT": str(out)}, clear=True),
                  patch("subprocess.check_output", side_effect=github),
                  patch("urllib.request.urlopen", side_effect=AssertionError("unexpected provider call")),
                  self.assertRaises(SystemExit) as result):
                exec(compile(script, "deploy-production guard", "exec"), {})
            self.assertEqual(result.exception.code, 0)
            self.assertEqual(out.read_text(), "mode=interrupted-recovery\n")

        for mutation in ({"RELEASE_RECOVERY_RUN_ID": "73"}, {"RELEASE_RECOVERY_RUN_ID": "7/3"},
                         {"GITHUB_RUN_ATTEMPT": "2"}, {"EVENT_NAME": "schedule"},
                         {"GITHUB_REF": "refs/heads/topic"}, {"GITHUB_REF_PROTECTED": "false"},
                         {"RELEASE_PLAN_ID": "42"}, {"RELEASE_PINS": "{}"}, {"SHA": "a" * 40},
                         {"ROLLBACK": "rollback"}, {"IMAGE": "image"}, {"AUTO_DISABLED": "false"},
                         {"TAKEOVER": "false"}, {"ALLOW_UNSTAGED": "true"}, {"CUTOVER_HOLD": "true"}):
            with self.subTest(mutation=mutation):
                result = subprocess.run([sys.executable, "-c", script], env={**base, **mutation},
                                        capture_output=True, text=True, timeout=3, check=False)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("interrupted recovery requires", result.stdout)

        self.assertIn("needs.guard.outputs.mode != 'interrupted-recovery'", jobs["production"]["if"])
        self.assertIn("inputs.release_plan_run_id != ''", jobs["release-plan"]["if"])
        self.assertEqual(jobs["release-reconcile"]["needs"], "guard")
        self.assertEqual(jobs["release-reconcile"]["if"], "needs.guard.outputs.mode == 'interrupted-recovery'")

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
        commands = {"python3 scripts/production_release.py authorize-deploy",
                    "python3 scripts/production_release.py takeover"}
        consumers = [step for step in jobs["production"]["steps"] if step.get("run") in commands]
        self.assertEqual(len(consumers), 3)
        for step in consumers:
            self.assertEqual(step["env"].get("RELEASE_TARGET_FINGERPRINT"), fingerprint, step["name"])
        self.assertEqual(jobs["release-recover"]["with"].get("target-fingerprint"), fingerprint)

    def test_release_retains_the_stop_intent_before_stopping(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/deploy-production.yml").read_text())
        prepare = workflow["jobs"]["release-prepare"]["steps"]
        order = [step.get("name") for step in prepare]
        retain = order.index("Retain the exact recovery stop intent")
        self.assertLess(retain, order.index("Confirm the exact stopped writer"))
        upload = prepare[retain]["with"]
        self.assertEqual(upload.get("name"), "production-release-stop-intent-${{ github.run_id }}-${{ github.run_attempt }}")
        self.assertEqual(upload["if-no-files-found"], "error")
        self.assertTrue(upload["path"].endswith("/production-release-stop-intent.json"))

    def test_interrupted_restart_waits_for_successful_inverse_receipt_upload(self):
        path = ROOT / ".github/workflows/deploy-production-interrupted.yml"
        jobs = yaml.safe_load(path.read_text())["jobs"]
        database, restart = jobs["database"], jobs["restart"]
        by_name = {step.get("name"): step for step in database["steps"]}
        classify = by_name["Classify the interrupted database state under lock"]
        decision = by_name["Retain the locked database classification"]
        inverse = by_name["Run the exact protected inverse for POST"]
        receipt = by_name["Retain the verified interrupted inverse"]
        self.assertNotIn("continue-on-error", database)
        for step in (classify, decision, inverse, receipt):
            self.assertNotIn("continue-on-error", step)
        self.assertEqual(receipt["with"]["if-no-files-found"], "error")
        self.assertEqual(restart["needs"], ["admit", "database"])
        self.assertNotIn("if", restart)

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
