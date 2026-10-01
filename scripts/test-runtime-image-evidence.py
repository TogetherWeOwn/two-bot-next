"""Offline contracts for bounded, non-deploying runtime image inspection."""

import importlib.util
import json
from pathlib import Path
import subprocess
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
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            metadata = [{**METADATA[0], "Id": "sha256:" + "c" * 64}]
            with patch.object(evidence, "docker", return_value=subprocess.CompletedProcess([], 0, json.dumps(metadata), "")) as docker:
                with self.assertRaisesRegex(ValueError, "identity"):
                    evidence.collect("two-bot:fixture", directory)
                self.assertEqual(docker.call_count, 1)

    def test_invalid_provenance_stops_before_docker(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            (directory / "source-sha.txt").write_text("unknown\n")
            with patch.object(evidence, "docker") as docker:
                with self.assertRaises(ValueError):
                    evidence.collect("two-bot:fixture", directory)
                docker.assert_not_called()

    def test_report_is_bound_to_image_and_does_not_include_environment(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            with patch.object(evidence, "docker", return_value=subprocess.CompletedProcess([], 0, json.dumps(METADATA), "")), \
                    patch.object(evidence, "probe", return_value={"returncode": 2, "stdout": "", "stderr": "module missing", "timed_out": False}):
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
        with patch.object(evidence, "docker", side_effect=[subprocess.CompletedProcess([], 0, "", ""), subprocess.CompletedProcess([], 1, "", "cleanup refused")]):
            with self.assertRaisesRegex(RuntimeError, "cleanup refused"):
                evidence.probe(IMAGE_ID, "true")

    def test_ci_retains_evidence_after_failed_gates_without_changing_gates(self):
        workflow = (ROOT / ".github/workflows/supply-chain.yml").read_text()
        self.assertIn("python3 scripts/test-runtime-image-evidence.py", workflow)
        diagnostic = workflow.index("name: Collect exact-image applicability evidence")
        self.assertGreater(diagnostic, workflow.index("name: Gate runtime image"))
        self.assertLess(diagnostic, workflow.index("name: Retain SBOMs"))
        step = workflow[diagnostic:workflow.index("name: Retain SBOMs")]
        self.assertIn("!cancelled() && steps.inventory.outcome == 'success'", step)
        self.assertIn('python3 scripts/runtime-image-evidence.py "$IMAGE" sbom', step)
        self.assertIn("sha256sum runtime-image-evidence.json", step)
        self.assertEqual(workflow.count("ignore-unfixed: false"), 2)
        self.assertEqual(workflow.count("exit-code: '1'"), 2)


if __name__ == "__main__":
    unittest.main()
