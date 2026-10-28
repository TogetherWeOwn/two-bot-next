"""Offline pins and exit-status fixtures for the Worker dependency audit."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
AUDIT_COMMAND = "npm audit --audit-level=high --include=dev --json"
AUDIT_STEP = "Audit Worker dependencies (high/critical gate)"


class WorkerNpmAuditTests(unittest.TestCase):
    def setUp(self):
        self.workflow = yaml.load(
            (ROOT / ".github/workflows/check.yml").read_text(), Loader=yaml.BaseLoader)
        self.worker = self.workflow["jobs"]["worker"]
        self.audit = next(step for step in self.worker["steps"]
                          if step.get("name") == AUDIT_STEP)

    def test_gate_is_in_the_required_worker_lane_before_install(self):
        self.assertEqual(self.worker["name"], "worker check")
        self.assertEqual(self.worker["defaults"]["run"]["working-directory"], "wrangler")
        self.assertEqual(self.worker["permissions"], {"contents": "read"})
        self.assertNotIn("continue-on-error", self.worker)
        self.assertEqual(self.audit, {
            "name": AUDIT_STEP,
            "if": "needs.job-inputs.outputs.worker != 'false'",
            "run": AUDIT_COMMAND,
        })
        steps = self.worker["steps"]
        for command in ("npm ci", "npm run typecheck", "npm test"):
            step = next(step for step in steps if step.get("run") == command)
            self.assertLess(steps.index(self.audit), steps.index(step))
        self.assertIn("worker", self.workflow["jobs"]["ci-ok"]["needs"])

    def run_audit_fixture(self, report, status):
        # Execute the actual workflow run block with Actions' bash error
        # handling. Only npm is replaced: no registry, vulnerable install or
        # production service is needed to prove exit and log propagation.
        scratch = os.environ.get("PAPERCLIP_SCRATCH_DIR") or os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            root = Path(directory)
            npm = root / "npm"
            npm.write_text(
                f"#!{shutil.which('python3')}\n"
                "import json, os, sys\n"
                "if sys.argv[1:] != ['audit', '--audit-level=high', '--include=dev', '--json']:\n"
                "    sys.exit(97)\n"
                "print(os.environ['AUDIT_FIXTURE_REPORT'])\n"
                "sys.exit(int(os.environ['AUDIT_FIXTURE_STATUS']))\n")
            npm.chmod(0o755)
            # Do not inherit BASH_ENV or other shell startup hooks that can
            # replace PATH and accidentally invoke the real registry client.
            env = {"PATH": str(root) + os.pathsep + os.environ["PATH"],
                   "AUDIT_FIXTURE_REPORT": json.dumps(report),
                   "AUDIT_FIXTURE_STATUS": str(status)}
            result = subprocess.run(
                ["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c",
                 self.audit["run"] + "\nprintf 'suite-continued\\n' >&2"],
                cwd=ROOT / "wrangler", env=env, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, status, result.stderr)
        self.assertEqual(json.loads(result.stdout), report)
        self.assertEqual("suite-continued" in result.stderr, status == 0)

    def test_clean_and_below_high_reports_pass_and_keep_json(self):
        for counts in ({"high": 0, "critical": 0, "moderate": 0, "low": 0},
                       {"high": 0, "critical": 0, "moderate": 2, "low": 1}):
            with self.subTest(counts=counts):
                self.run_audit_fixture(
                    {"auditReportVersion": 2, "vulnerabilities": {},
                     "metadata": {"vulnerabilities": counts}}, 0)

    def test_high_and_critical_reports_fail_and_keep_json(self):
        for severity in ("high", "critical"):
            with self.subTest(severity=severity):
                self.run_audit_fixture(
                    {"auditReportVersion": 2,
                     "vulnerabilities": {"fixture-package": {"severity": severity}},
                     "metadata": {"vulnerabilities": {severity: 1}}}, 1)

    def test_registry_errors_fail_closed_and_keep_json(self):
        self.run_audit_fixture({"error": {"code": "E503", "summary": "fixture unavailable"}}, 1)

    def test_documented_rerun_matches_the_gate(self):
        docs = (ROOT / "CONTRIBUTING.md").read_text()
        self.assertIn(AUDIT_COMMAND, docs)
        self.assertIn("worker check", docs)
        self.assertIn("test_worker_npm_audit.py", docs)


if __name__ == "__main__":
    unittest.main()
