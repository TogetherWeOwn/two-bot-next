"""Offline trust-anchor regression and CI wiring; no TLS traffic or installs."""

import hashlib
import json
import os
from pathlib import Path
import re
import ssl
import subprocess
import sys
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "scripts/ci/fixtures"
CHECK = ROOT / "scripts/ci/check-semgrep-trust-store.py"
BAD_SHA256 = "86a1ecba089c4a8d3bbe2734c612ba341d813e043cf9e8a862cd5c57a36bbe6b"


class SemgrepTrustStoreTests(unittest.TestCase):
    def setUp(self):
        scratch = os.environ.get("PAPERCLIP_SCRATCH_DIR") or os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
        self.directory = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.good = (FIXTURES / "isrg-x1.pem").read_text()
        self.bad = (FIXTURES / "emsign-g3.pem").read_text()

    def run_gate(self, contents, expected_status, expected_code, installed=False):
        bundle = self.root / "bundle.pem"
        bundle.write_text(contents)
        env = {"PATH": os.environ["PATH"], "PYTHONDONTWRITEBYTECODE": "1"}
        command = [sys.executable, str(CHECK)]
        if installed:
            # Only certifi.where() is substituted. The real gate and OpenSSL
            # load the fixture; this proves the default installed-bundle path.
            (self.root / "certifi.py").write_text(
                "from pathlib import Path\n"
                "def where():\n"
                "    return str(Path(__file__).with_name('bundle.pem'))\n")
            env["PYTHONPATH"] = str(self.root)
        else:
            command += ["--bundle", str(bundle)]
        result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, expected_status, result.stderr + result.stdout)
        report = json.loads(result.stdout)
        self.assertEqual(report["schema"], "semgrep-trust-store.v1")
        self.assertEqual(report["code"], expected_code)
        self.assertFalse(report["retryable"])
        self.assertIn("next_action", report)
        return report

    def test_public_certificate_fixture_identities(self):
        self.assertEqual(hashlib.sha256(ssl.PEM_cert_to_DER_cert(self.bad)).hexdigest(), BAD_SHA256)
        self.assertEqual(hashlib.sha256(ssl.PEM_cert_to_DER_cert(self.good)).hexdigest(),
                         "96bcec06264976f37460779acf28c5a7cfe8a3c0aae11a8ffcee05c0bddf08c6")

    def test_safe_root_passes(self):
        report = self.run_gate(self.good, 0, "trust_store_clean")
        self.assertEqual(report["certificate_count"], 1)
        self.assertEqual(report["blocked_sha256"], [])

    def test_compromised_root_alone_fails(self):
        report = self.run_gate(self.bad, 1, "compromised_trust_anchor")
        self.assertEqual(report["blocked_sha256"], [BAD_SHA256])

    def test_old_bundle_negative_case_rejects_compromised_root_in_either_order(self):
        for contents in (self.good + self.bad, self.bad + self.good):
            with self.subTest(compromised_first=contents.startswith(self.bad)):
                report = self.run_gate(contents, 1, "compromised_trust_anchor")
                self.assertEqual(report["certificate_count"], 2)
                self.assertEqual(report["blocked_sha256"], [BAD_SHA256])

    def test_identity_check_does_not_depend_on_comment_or_pem_wrapping(self):
        body = "".join(self.bad.splitlines()[1:-1])
        rewrapped = "\n".join(body[i:i + 40] for i in range(0, len(body), 40))
        self.run_gate("# harmless-looking label\n-----BEGIN CERTIFICATE-----\n" +
                      rewrapped + "\n-----END CERTIFICATE-----\n", 1, "compromised_trust_anchor")

    def test_empty_and_malformed_bundles_fail_closed(self):
        for contents in ("", "not a PEM bundle", "-----BEGIN CERTIFICATE-----\ninvalid\n-----END CERTIFICATE-----\n"):
            with self.subTest(contents=contents):
                self.run_gate(contents, 2, "trust_store_unavailable")

    def test_default_checks_installed_certifi_bundle(self):
        self.run_gate(self.good, 0, "trust_store_clean", installed=True)
        self.run_gate(self.good + self.bad, 1, "compromised_trust_anchor", installed=True)

    def test_missing_certifi_fails_closed_without_os_store_fallback(self):
        result = subprocess.run([sys.executable, "-S", str(CHECK)],
                                env={"PATH": os.environ["PATH"]},
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual(json.loads(result.stdout)["code"], "trust_store_unavailable")

    def test_missing_bundle_fails_closed(self):
        result = subprocess.run([sys.executable, str(CHECK), "--bundle", str(self.root / "missing.pem")],
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual(json.loads(result.stdout)["code"], "trust_store_unavailable")

    def test_ci_checks_installed_bundle_before_scanning_without_waivers(self):
        workflow = yaml.load((ROOT / ".github/workflows/check.yml").read_text(), Loader=yaml.BaseLoader)
        lane = workflow["jobs"]["check"]
        step = next(step for step in lane["steps"] if step.get("name") == "Semgrep Rust rules (HARD)")
        command = "python3 scripts/ci/check-semgrep-trust-store.py"
        run = step["run"]
        self.assertIn(command, run.splitlines())
        self.assertLess(run.index('--target "$RUNNER_TEMP/semgrep"'), run.index(command))
        self.assertLess(run.index('export PYTHONPATH="$RUNNER_TEMP/semgrep'), run.index(command))
        self.assertLess(run.index(command), run.index("python3 scripts/ci/check-semgrep-warn.py"))
        self.assertTrue(run.startswith("set -euo pipefail\n"))
        self.assertNotIn("continue-on-error", step)
        self.assertNotIn("continue-on-error", lane)
        self.assertEqual(step["if"], "needs.job-inputs.outputs.rust != 'false'")
        self.assertIn("check", workflow["jobs"]["ci-ok"]["needs"])
        self.assertTrue(any("unittest discover -s scripts/ci -p 'test_*.py'" in s.get("run", "")
                            for s in lane["steps"]))

    def test_requirements_lockfile_keeps_every_package_hash_pinned(self):
        # The HARD lane installs with --require-hashes: a regenerated lockfile
        # that drops a hash, duplicates a package or regresses the certifi
        # GHSA-6q48-6c7w-9gw2 floor must fail here, before CI installs it.
        text = (ROOT / "scripts/ci/semgrep-requirements.txt").read_text()
        entries = re.findall(
            r"^([A-Za-z0-9_.-]+)==(\S+) \\\n(?: {4}--hash=sha256:[0-9a-f]{64} ?\\\n)*"
            r" {4}--hash=sha256:([0-9a-f]{64})$",
            text, re.M)
        self.assertGreaterEqual(len(entries), 60)
        body = [line for line in text.splitlines()
                if line.strip() and not line.startswith("#")]
        # Two lines per entry (pin + final hash line); extra hash lines belong
        # to their entry, so count entry headers instead of raw lines.
        headers = [line for line in body if not line.startswith(" ")]
        self.assertEqual(len(headers), len(entries))
        names = [re.sub(r"[-_.]+", "-", name).lower() for name, _, _ in entries]
        self.assertEqual(len(names), len(set(names)), "duplicate package in lockfile")
        versions = dict(zip(names, (version for _, version, _ in entries)))
        self.assertIn("semgrep", versions)
        certifi = tuple(int(part) for part in versions["certifi"].split("."))
        self.assertGreaterEqual(certifi, (2026, 10, 19))


if __name__ == "__main__":
    unittest.main()
