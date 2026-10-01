"""Offline contracts for bounded, non-deploying runtime image inspection."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("evidence", ROOT / "scripts/runtime-image-evidence.py")
evidence = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(evidence)
IMAGE_ID = "sha256:" + "a" * 64
SOURCE_SHA = "b" * 40
METADATA = [{"Id": IMAGE_ID, "Architecture": "amd64", "Os": "linux", "Config": {
    "User": "two-bot", "Entrypoint": ["/home/two-bot/two-bot"],
    "Env": ["SECRET=must-not-be-recorded"], "Cmd": None,
}}]


class EvidenceTests(unittest.TestCase):
    def fixture(self, directory):
        (directory / "image-id.txt").write_text(IMAGE_ID + "\n")
        (directory / "source-sha.txt").write_text(SOURCE_SHA + "\n")

    def test_identity_mismatch_stops_before_any_container_runs(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")) as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            metadata = [{**METADATA[0], "Id": "sha256:" + "c" * 64}]
            with patch.object(evidence, "docker", return_value=subprocess.CompletedProcess([], 0, json.dumps(metadata), "")) as docker:
                with self.assertRaisesRegex(ValueError, "identity"):
                    evidence.collect("two-bot:fixture", directory)
                self.assertEqual(docker.call_count, 1)

    def test_invalid_provenance_stops_before_docker(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")) as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            (directory / "source-sha.txt").write_text("unknown\n")
            with patch.object(evidence, "docker") as docker:
                with self.assertRaises(ValueError):
                    evidence.collect("two-bot:fixture", directory)
                docker.assert_not_called()

    def test_report_is_bound_to_image_and_does_not_include_environment(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")) as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            with patch.object(evidence, "docker", return_value=subprocess.CompletedProcess([], 0, json.dumps(METADATA), "")), \
                    patch.object(evidence, "probe", return_value={"returncode": 2, "stdout": "", "stderr": "module missing", "timed_out": False, "cleanup": {"status": "removed"}}):
                report = evidence.collect("two-bot:fixture", directory)
            self.assertEqual(report["image_id"], IMAGE_ID)
            self.assertEqual(report["source_sha"], SOURCE_SHA)
            self.assertEqual(report["architecture"], "amd64")
            self.assertEqual(report["configured_user"], "two-bot")
            self.assertNotIn("SECRET", json.dumps(report))
            self.assertNotIn("accepted", report)
            self.assertEqual(report["probes"]["perl_archive_tar"]["returncode"], 2)
            self.assertEqual(report["probes"]["perl_archive_tar"]["stderr"], "module missing")

    def test_fixed_probe_commands_parse_without_executing(self):
        self.assertEqual(len(evidence.PROBES), 16)
        for label, command in evidence.PROBES.items():
            with self.subTest(probe=label):
                result = subprocess.run(["sh", "-n", "-c", command], capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_probe_is_isolated_and_always_cleans_its_named_container(self):
        calls = []

        def docker(*args, **kwargs):
            calls.append((args, kwargs))
            return subprocess.CompletedProcess(args, 0, "raw observation\n", "")

        with patch.object(evidence, "docker", side_effect=docker):
            result = evidence.probe(IMAGE_ID, "dpkg-query -W")
        run = calls[0][0]
        for flag in ["--read-only", "--no-healthcheck"]:
            self.assertIn(flag, run)
        for flag, value in [("--network", "none"), ("--cap-drop", "ALL"),
                            ("--security-opt", "no-new-privileges"), ("--pids-limit", "32"),
                            ("--memory", "128m"), ("--memory-swap", "128m"), ("--cpus", "0.5"),
                            ("--entrypoint", "/bin/sh")]:
            self.assertEqual(run[run.index(flag) + 1], value)
        self.assertEqual(run[-3:], (IMAGE_ID, "-c", "dpkg-query -W"))
        for forbidden in ["--publish", "--volume", "--mount", "--env", "--privileged"]:
            self.assertNotIn(forbidden, run)
        name = run[run.index("--name") + 1]
        self.assertEqual(calls[-1][0], ("rm", "--force", name))
        self.assertEqual(calls[0][1]["timeout"], 20)
        self.assertEqual(result["stdout"], "raw observation\n")

    def test_timeout_is_explicit_and_still_cleans_container(self):
        with patch.object(evidence, "docker", side_effect=[subprocess.TimeoutExpired("docker", 20), subprocess.CompletedProcess([], 0, "", "")]) as docker:
            result = evidence.probe(IMAGE_ID, "sleep 1000")
        self.assertTrue(result["timed_out"])
        self.assertIsNone(result["returncode"])
        self.assertEqual(docker.call_args.args[:2], ("rm", "--force"))

    def test_cleanup_failure_is_not_hidden(self):
        with patch.object(evidence, "docker", side_effect=[subprocess.CompletedProcess([], 0, "observation", ""), subprocess.CompletedProcess([], 1, "", "cleanup refused")]):
            result = evidence.probe(IMAGE_ID, "true")
        self.assertEqual(result["stdout"], "observation")
        self.assertEqual(result["cleanup"]["status"], "failed")
        self.assertEqual(result["cleanup"]["stderr"], "cleanup refused")

    def test_precreation_failure_keeps_startup_error_and_confirms_absence(self):
        def docker(*args, **kwargs):
            if args[0] == "run":
                return subprocess.CompletedProcess(args, 125, "", "OCI runtime create failed before container creation")
            return subprocess.CompletedProcess(args, 1, "", f"Error response from daemon: No such container: {args[-1]}\n")

        with patch.object(evidence, "docker", side_effect=docker):
            result = evidence.probe(IMAGE_ID, "true")
        self.assertEqual(result["returncode"], 125)
        self.assertIn("OCI runtime", result["stderr"])
        self.assertEqual(result["cleanup"]["status"], "absent")

    def test_unrelated_missing_container_message_is_not_accepted(self):
        with patch.object(evidence, "docker", side_effect=[
            subprocess.CompletedProcess([], 125, "", "startup failed"),
            subprocess.CompletedProcess([], 1, "", "Error response from daemon: No such container: someone-elses-container"),
        ]):
            result = evidence.probe(IMAGE_ID, "true")
        self.assertEqual(result["cleanup"]["status"], "failed")

    def test_client_and_cleanup_timeouts_preserve_partial_output(self):
        with patch.object(evidence, "docker", side_effect=[
            subprocess.TimeoutExpired("docker", 20, output=b"partial observation", stderr=b"partial error"),
            subprocess.TimeoutExpired("docker rm", 30),
        ]):
            result = evidence.probe(IMAGE_ID, "true")
        self.assertEqual(result["stdout"], "partial observation")
        self.assertIn("partial error", result["stderr"])
        self.assertTrue(result["timed_out"])
        self.assertEqual(result["cleanup"]["status"], "failed")
        self.assertTrue(result["cleanup"]["timed_out"])

    def test_missing_docker_client_is_explicit_and_cleanup_is_attempted(self):
        with patch.object(evidence, "docker", side_effect=FileNotFoundError("docker unavailable")) as docker:
            result = evidence.probe(IMAGE_ID, "true")
        self.assertEqual(docker.call_count, 2)
        self.assertIsNone(result["returncode"])
        self.assertIn("docker unavailable", result["stderr"])
        self.assertEqual(result["cleanup"]["status"], "failed")

    def test_success_then_precreation_failure_retains_all_observations(self):
        calls = []

        def docker(*args, **kwargs):
            if args[:2] == ("image", "inspect"):
                return subprocess.CompletedProcess(args, 0, json.dumps(METADATA), "")
            if args[0] == "run":
                calls.append(args)
                code = 125 if len(calls) == 2 else 0
                return subprocess.CompletedProcess(args, code, "first observation" if len(calls) == 1 else "", "startup failed" if code else "")
            if len(calls) >= 2 and args[-1] == calls[1][calls[1].index("--name") + 1]:
                return subprocess.CompletedProcess(args, 1, "", f"Error response from daemon: No such container: {args[-1]}\n")
            return subprocess.CompletedProcess(args, 0, "", "")

        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")) as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            with patch.object(evidence, "docker", side_effect=docker), patch.object(sys, "argv", ["evidence", "two-bot:fixture", str(directory)]):
                evidence.main()
            report = json.loads((directory / "runtime-image-evidence.json").read_text())
        self.assertEqual(report["probes"]["installed_packages"]["stdout"], "first observation")
        self.assertEqual(report["probes"]["affected_package_files"]["returncode"], 125)
        self.assertEqual(report["probes"]["affected_package_files"]["cleanup"]["status"], "absent")
        self.assertEqual(len(report["probes"]), 16)
        self.assertTrue(report["complete"])

    def test_cleanup_refusal_saves_partial_report_then_fails_and_stops(self):
        responses = [subprocess.CompletedProcess([], 0, json.dumps(METADATA), ""),
                     subprocess.CompletedProcess([], 0, "first observation", ""),
                     subprocess.CompletedProcess([], 0, "", ""),
                     subprocess.CompletedProcess([], 125, "", "startup failed"),
                     subprocess.CompletedProcess([], 1, "", "cleanup refused")]
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")) as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            with patch.object(evidence, "docker", side_effect=responses) as docker, patch.object(sys, "argv", ["evidence", "two-bot:fixture", str(directory)]):
                with self.assertRaisesRegex(SystemExit, "cleanup"):
                    evidence.main()
            report = json.loads((directory / "runtime-image-evidence.json").read_text())
        self.assertEqual(docker.call_count, 5)
        self.assertFalse(report["complete"])
        self.assertEqual(len(report["probes"]), 2)
        self.assertEqual(report["probes"]["installed_packages"]["stdout"], "first observation")
        self.assertEqual(report["probes"]["affected_package_files"]["stderr"], "startup failed")
        self.assertEqual(report["probes"]["affected_package_files"]["cleanup"]["stderr"], "cleanup refused")

    def test_ci_retains_evidence_after_failed_gates_without_changing_gates(self):
        workflow = (ROOT / ".github/workflows/supply-chain.yml").read_text()
        self.assertIn("python3 scripts/test-runtime-image-evidence.py", workflow)
        diagnostic = workflow.index("name: Collect exact-image applicability evidence")
        self.assertGreater(diagnostic, workflow.index("name: Gate runtime image"))
        self.assertLess(diagnostic, workflow.index("name: Retain SBOMs"))
        step = workflow[diagnostic:workflow.index("name: Retain SBOMs")]
        self.assertIn("!cancelled() && steps.inventory.outcome == 'success'", step)
        self.assertIn('python3 scripts/runtime-image-evidence.py "$IMAGE" sbom', step)
        self.assertIn('python3 scripts/runtime-image-evidence.py "$IMAGE" sbom || status=$?', step)
        self.assertIn("sha256sum runtime-image-evidence.json", step)
        self.assertLess(step.index("sha256sum runtime-image-evidence.json"), step.index('exit "$status"'))
        self.assertEqual(workflow.count("ignore-unfixed: false"), 2)
        self.assertEqual(workflow.count("exit-code: '1'"), 2)


if __name__ == "__main__":
    unittest.main()
