"""Offline probe-runner regressions; mocked conversion is not native-selector proof."""

from copy import deepcopy
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


def load(name):
    spec = importlib.util.spec_from_file_location(name.replace("-", "_"), Path(__file__).with_name(name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


probe = load("probe-trivy-selectors")
fixtures = load("test-vulnerability-preflight")


class SelectorProbeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP"))
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        fixture = fixtures.PreflightTests()
        fixture.setUp()
        self.scan, self.evidence = fixture.scan, fixture.evidence
        self.executable = self.directory / "trivy-fixture"
        self.executable.write_bytes(b"not an executable: subprocess is mocked")
        self.output = self.directory / "receipts.json"
        self.write_evidence()
        self.invocations = []

    def write_evidence(self):
        (self.directory / "image-vulnerabilities.json").write_text(json.dumps(self.scan))
        (self.directory / "runtime-image-evidence.json").write_text(json.dumps(self.evidence))
        (self.directory / "source-sha.txt").write_text(fixtures.SHA)
        (self.directory / "image-id.txt").write_text(fixtures.IMAGE)

    def convert_fixture(self, command, **kwargs):
        # This stub tests orchestration and receipt validation only. Actual
        # scanner behavior must be measured with the pinned executable in CI.
        self.invocations.append((command, kwargs))
        if "--version" in command:
            return subprocess.CompletedProcess(command, 0, stdout="Version: 0.69.3\n", stderr="")
        data = json.loads(Path(command[-1]).read_text())
        ignore = json.loads(Path(command[command.index("--ignorefile") + 1]).read_text())["vulnerabilities"][0]
        row = data["Results"][0]["Vulnerabilities"][0]
        purl = (row.get("PkgIdentifier") or {}).get("PURL")
        remaining = int(row["VulnerabilityID"] != ignore["id"] or
                        (purl is not None and purl not in ignore["purls"]) or
                        ignore["expired_at"].startswith("2000"))
        if not remaining:
            data["Results"][0]["Vulnerabilities"] = []
        Path(command[command.index("--output") + 1]).write_text(json.dumps(data))
        return subprocess.CompletedProcess(command, remaining, stdout="", stderr="")

    def run_probe(self, side_effect=None):
        with patch.object(probe.subprocess, "run", side_effect=side_effect or self.convert_fixture):
            return probe.run_probe(self.executable, self.directory, self.output)

    def test_receipts_preserve_original_evidence_and_distinguish_guards(self):
        before = {name: (self.directory / name).read_bytes() for name in (
            "image-vulnerabilities.json", "runtime-image-evidence.json", "source-sha.txt", "image-id.txt")}
        report = self.run_probe()
        self.assertEqual(report["candidate_count"], 1)
        self.assertEqual(report["case_count"], len(probe.mutations()))
        self.assertEqual(report["repository_suppressed_count"], 0)
        cases = {row["case"]: row for row in report["receipts"]}
        self.assertEqual(cases["exact"]["remaining"], 0)
        self.assertTrue(cases["missing-purl"]["preflight"].startswith("rejected:"))
        self.assertEqual(cases["consistent-version"]["preflight"], "not-conditionally-accepted")
        self.assertEqual(cases["consistent-architecture"]["preflight"], "not-conditionally-accepted")
        self.assertEqual(cases["expired"]["preflight"], "expired")
        self.assertEqual(cases["expired"]["preflight_evaluated_at"], probe.preflight.EXPIRES.isoformat())
        self.assertTrue(all(len(row["input_sha256"]) == len(row["output_sha256"]) == len(row["ignore_sha256"]) == 64
                            for row in report["receipts"]))
        self.assertEqual(before, {name: (self.directory / name).read_bytes() for name in before})
        self.assertEqual(json.loads(self.output.read_text()), report)

    def test_consistent_mutations_bind_but_never_inherit_acceptance(self):
        for label in ("consistent-package", "consistent-version", "consistent-architecture"):
            with self.subTest(label=label):
                scan, evidence = deepcopy((self.scan, self.evidence))
                probe.mutations()[label](scan, evidence, scan["Results"][0]["Vulnerabilities"][0])
                report = probe.preflight.preflight(scan, evidence, fixtures.SHA, fixtures.IMAGE, fixtures.NOW)
                self.assertEqual(report["findings"][0]["status"], "not-conditionally-accepted")

    def test_subprocess_has_explicit_policy_cache_and_no_inherited_secrets(self):
        self.run_probe()
        self.assertTrue(self.invocations)
        for command, kwargs in self.invocations:
            self.assertEqual(set(kwargs["env"]), {"PATH", "HOME"})
            self.assertLessEqual(kwargs["timeout"], 20)
            self.assertIn("--config", command)
            self.assertIn("--cache-dir", command)
            if "convert" in command:
                self.assertEqual(command[command.index("--exit-code") + 1], "1")
                self.assertEqual(command[command.index("--severity") + 1], "HIGH,CRITICAL")
                self.assertEqual(command[command.index("--format") + 1], "json")
                self.assertEqual(Path(command[command.index("--ignorefile") + 1]).name, "ignore.yaml")
            self.assertNotIn("image", command)
            self.assertNotIn("--ignore-unfixed", command)
        self.assertFalse(Path(self.invocations[0][1]["env"]["HOME"]).exists())

    def test_private_binary_snapshot_survives_shared_installation_changes(self):
        original = self.executable.read_bytes()
        def change_installation(command, **kwargs):
            self.assertEqual(Path(command[0]).read_bytes(), original)
            self.assertEqual(Path(command[0]).parent, Path(kwargs["env"]["HOME"]))
            self.executable.write_bytes(b"changed shared installation")
            return self.convert_fixture(command, **kwargs)
        report = self.run_probe(change_installation)
        self.assertEqual(report["trivy_binary_sha256"], probe.digest(original))

    def test_invalid_source_evidence_prevents_any_conversion(self):
        self.evidence["image_id"] = "sha256:" + "c" * 64
        self.write_evidence()
        with patch.object(probe.subprocess, "run") as run:
            with self.assertRaisesRegex(ValueError, "binding"):
                probe.run_probe(self.executable, self.directory, self.output)
            run.assert_not_called()
        self.assertFalse(self.output.exists())

    def test_output_cannot_replace_source_evidence(self):
        original = (self.directory / "image-vulnerabilities.json").read_bytes()
        with self.assertRaisesRegex(ValueError, "must not replace"):
            probe.run_probe(self.executable, self.directory, self.directory / "image-vulnerabilities.json")
        self.assertEqual((self.directory / "image-vulnerabilities.json").read_bytes(), original)

    def test_provenance_changed_during_probes_refuses_receipt(self):
        def change_provenance(command, **kwargs):
            result = self.convert_fixture(command, **kwargs)
            if "convert" in command:
                (self.directory / "source-sha.txt").write_text("c" * 40)
            return result
        with self.assertRaisesRegex(ValueError, "provenance was modified"):
            self.run_probe(change_provenance)
        self.assertFalse(self.output.exists())

    def test_wrong_binary_version_refuses_selector_proof(self):
        def wrong_version(command, **kwargs):
            return subprocess.CompletedProcess(command, 0, stdout="Version: 0.69.2\n", stderr="")
        with self.assertRaisesRegex(ValueError, "v0.69.3"):
            self.run_probe(wrong_version)
        self.assertFalse(self.output.exists())

    def test_conversion_failure_cannot_reuse_previous_output(self):
        def fail_after_exact(command, **kwargs):
            if "--version" in command or not any("convert" in args for args, _ in self.invocations):
                return self.convert_fixture(command, **kwargs)
            self.assertFalse(Path(command[command.index("--output") + 1]).exists())
            return subprocess.CompletedProcess(command, 1, stdout="", stderr="decode failed")
        with self.assertRaisesRegex(RuntimeError, "decode failed"):
            self.run_probe(fail_after_exact)
        self.assertFalse(self.output.exists())

    def test_incorrect_exit_status_and_failed_control_refuse_proof(self):
        def wrong_status(command, **kwargs):
            result = self.convert_fixture(command, **kwargs)
            if "convert" in command:
                result.returncode = 1 - result.returncode
            return result
        with self.assertRaisesRegex(ValueError, "exit code"):
            self.run_probe(wrong_status)
        self.assertFalse(self.output.exists())

    def test_null_identifier_decoder_panic_is_not_a_filtered_finding(self):
        def decoder_panic(command, **kwargs):
            if "convert" in command:
                data = json.loads(Path(command[-1]).read_text())
                if data["Results"][0]["Vulnerabilities"][0].get("PkgIdentifier", {}) is None:
                    return subprocess.CompletedProcess(command, 2, stdout="", stderr="panic: (*PkgIdentifier).UnmarshalJSON")
            return self.convert_fixture(command, **kwargs)
        report = self.run_probe(decoder_panic)
        row = next(row for row in report["receipts"] if row["case"] == "null-identifier")
        self.assertEqual(row["trivy_exit"], 2)
        self.assertIsNone(row["remaining"])
        self.assertIsNone(row["output_sha256"])
        self.assertTrue(row["preflight"].startswith("rejected:"))
        self.assertIn("decoder panic", row["decoder_error"])
        self.assertNotIn(row, report["yaml_boundary_not_enforced"])

    def test_unexpected_conversion_exception_is_not_retried(self):
        with patch.object(probe.subprocess, "run", side_effect=subprocess.TimeoutExpired("trivy", 20)) as run:
            with self.assertRaises(subprocess.TimeoutExpired):
                probe.run_probe(self.executable, self.directory, self.output)
            self.assertEqual(run.call_count, 1)
        self.assertFalse(self.output.exists())

    def test_zero_candidates_is_explicitly_zero_coverage(self):
        self.scan["Results"][0]["Vulnerabilities"][0]["VulnerabilityID"] = "CVE-2026-9999999"
        self.write_evidence()
        report = self.run_probe()
        self.assertEqual(report["candidate_count"], 0)
        self.assertEqual(report["receipts"], [])
        self.assertEqual(report["case_count"], 0)
        self.assertEqual(len(self.invocations), 1)
        self.assertIn("Zero candidates means zero selector coverage, not proof that exceptions would be safe.", report["limitations"])


if __name__ == "__main__":
    unittest.main()
