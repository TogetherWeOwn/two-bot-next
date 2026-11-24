"""Offline regression tests for recipient validation and inventory encryption."""

import base64
import hashlib
import json
from pathlib import Path
import ssl
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import staging_recovery_capture as capture


class StagingRecoveryCaptureTests(unittest.TestCase):
    def test_shipped_pin_matches_staging_recipient_certificate(self):
        cert = Path(__file__).with_name("fixtures") / "staging_recovery_recipient.pem"
        der = ssl.PEM_cert_to_DER_cert(cert.read_text(encoding="ascii"))
        self.assertEqual(hashlib.sha256(der).hexdigest(), capture.RECIPIENT_SHA256)

    def test_unreviewed_certificate_refuses_before_database_access(self):
        with patch.dict(capture.os.environ, {"RECOVERY_RECIPIENT": "YQ=="}, clear=True), \
                patch.object(capture, "database_target") as database_target:
            self.assertEqual(capture.main(), 1)
            database_target.assert_not_called()

    def test_pinned_recipient_capture_round_trip(self):
        with tempfile.TemporaryDirectory() as directory:
            key = Path(directory) / "key.pem"
            cert = Path(directory) / "cert.pem"
            subprocess.run(
                ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-noenc", "-days", "1",
                 "-subj", "/CN=test-recipient", "-keyout", key, "-out", cert],
                capture_output=True, check=True,
            )
            der = ssl.PEM_cert_to_DER_cert(cert.read_text())
            pin = hashlib.sha256(der).hexdigest()
            target = {"host": "staging.example", "database": "two_bot", "branch_id": "fixture"}
            source = "a" * 40
            inventory = {
                "database": {"name": target["database"]}, "role": capture.DB_ROLE,
                "missing_expected": [], "unsupported": False,
                "cutover_ledger": [{"version": 481}], "store_ledger": [{"version": 478}],
                "relations": ["x" * 140_000],
            }
            csv_record = '"' + json.dumps(inventory).replace('"', '""') + '"\n'
            env = {"RECOVERY_RECIPIENT": base64.b64encode(der).decode(),
                   capture.BRANCH_ENV: target["branch_id"], "GITHUB_SHA": source}
            artifact = Path(directory) / "staging-recovery.cms"
            with (patch.dict(capture.os.environ, env, clear=True),
                  patch.object(capture, "RECIPIENT_SHA256", pin),
                  patch.object(capture, "CAPTURE", artifact),
                  patch.object(capture, "database_target", return_value=({}, target["host"], target["database"])),
                  patch.object(capture, "psql_environment", return_value={}),
                  patch.object(capture, "run_psql", return_value=csv_record) as read_inventory,
                  patch("builtins.print") as receipt):
                self.assertEqual(capture.main(), 0)
            read_inventory.assert_called_once()

            encrypted = artifact.read_bytes()
            self.assertNotIn(b"x" * 140_000, encrypted)
            decrypted = subprocess.run(
                ["openssl", "cms", "-decrypt", "-inform", "DER", "-recip", cert, "-inkey", key],
                input=encrypted, capture_output=True, check=True,
            )
            self.assertEqual(json.loads(decrypted.stdout), {**inventory, "target": target, "workflow_sha": source})
            receipt.assert_called_once_with(f"Plaintext SHA-256: {hashlib.sha256(decrypted.stdout).hexdigest()}")
