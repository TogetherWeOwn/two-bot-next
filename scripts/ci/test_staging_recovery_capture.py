"""Regression checks for the staging recovery workflow and encrypted output."""

import base64
from contextlib import redirect_stderr, redirect_stdout
import csv
import io
import json
from pathlib import Path
import ssl
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import yaml

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import staging_recovery_capture as capture


class StagingRecoveryCaptureTests(unittest.TestCase):
    def test_workflow_has_only_the_staging_plan_binding(self):
        document = yaml.safe_load(
            (ROOT / ".github/workflows/staging-recovery-capture.yml").read_text()
        )
        self.assertEqual(set(document[True]), {"workflow_dispatch"})
        job, = document["jobs"].values()
        self.assertEqual(job["if"], "github.ref == 'refs/heads/main'")
        self.assertEqual(job["environment"], "staging-migrate-plan")
        self.assertEqual(job["permissions"], {"contents": "read"})
        db_step, = [step for step in job["steps"] if "env" in step]
        self.assertEqual(
            [value for value in db_step["env"].values() if "secrets." in value],
            ["${{ secrets.TWO_BOT_STAGING_PLAN_DATABASE_URL }}"],
        )
        self.assertEqual(job["steps"][-1]["with"]["path"], "staging-recovery.cms")

    def test_capture_encrypts_catalog_and_refuses_incomplete_inventory(self):
        with tempfile.TemporaryDirectory() as directory:
            key = Path(directory) / "key.pem"
            cert = Path(directory) / "cert.pem"
            artifact = Path(directory) / "recovery.cms"
            subprocess.run(
                ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-noenc", "-days", "1",
                 "-subj", "/CN=test-recipient", "-keyout", key, "-out", cert],
                capture_output=True, check=True,
            )
            env = {
                "GITHUB_SHA": "a" * 40,
                "RECOVERY_RECIPIENT": base64.b64encode(
                    ssl.PEM_cert_to_DER_cert(cert.read_text())
                ).decode(),
                "TWO_BOT_STAGING_HOST": "staging.example.test",
                "TWO_BOT_STAGING_DATABASE": "staging",
                "TWO_BOT_STAGING_PLAN_DATABASE_URL":
                    "postgres://plan:synthetic_value@staging.example.test/staging",
            }
            report = {
                "database": {"name": "staging"}, "role": capture.DB_ROLE,
                "missing_expected": [], "unsupported": False,
                "cutover_ledger": [{"version": 480}], "store_ledger": [{"version": 478}],
                "relations": [{"name": "synthetic_private_object"}],
            }
            output = io.StringIO()
            with patch.dict(capture.os.environ, env, clear=True), \
                    patch.object(capture, "CAPTURE", artifact), \
                    patch.object(capture.shutil, "which", return_value="psql"), \
                    patch.object(capture, "run_psql") as psql, \
                    redirect_stdout(output), redirect_stderr(output):
                result = io.StringIO()
                csv.writer(result).writerow([json.dumps(dict(report, missing_expected=["missing"]))])
                psql.return_value = result.getvalue()
                self.assertEqual(capture.main(), 1)
                self.assertFalse(artifact.exists())
                result = io.StringIO()
                csv.writer(result).writerow([json.dumps(report)])
                psql.return_value = result.getvalue()
                self.assertEqual(capture.main(), 0)
                query = psql.call_args.args[2]
                self.assertIn("REPEATABLE READ READ ONLY", query)
                self.assertNotIn("-- @matrix", query)

            self.assertNotIn("synthetic_private_object", output.getvalue())
            self.assertNotIn("synthetic_value", output.getvalue())
            decrypted = subprocess.check_output(
                ["openssl", "cms", "-decrypt", "-inform", "DER", "-in", artifact,
                 "-recip", cert, "-inkey", key]
            )
            self.assertEqual(json.loads(decrypted)["relations"], report["relations"])
