"""Offline regression tests for recipient validation and inventory encryption."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import staging_recovery_capture as capture


class StagingRecoveryCaptureTests(unittest.TestCase):
    def test_unreviewed_certificate_refuses_before_database_access(self):
        with patch.dict(capture.os.environ, {"RECOVERY_RECIPIENT": "YQ=="}, clear=True), \
                patch.object(capture, "database_target") as database_target:
            self.assertEqual(capture.main(), 1)
            database_target.assert_not_called()

    def test_encrypt_decrypt_round_trip(self):
        with tempfile.TemporaryDirectory() as directory:
            key = Path(directory) / "key.pem"
            cert = Path(directory) / "cert.pem"
            subprocess.run(
                ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-noenc", "-days", "1",
                 "-subj", "/CN=test-recipient", "-keyout", key, "-out", cert],
                capture_output=True, check=True,
            )
            payload = b'{"relations":["' + b"x" * 140_000 + b'"]}'
            csv_record = '"' + payload.decode().replace('"', '""') + '"\n'
            self.assertEqual(capture.decode_inventory(csv_record)["relations"], ["x" * 140_000])
            encrypted = capture.encrypt(payload, cert.read_text())
            self.assertNotIn(payload, encrypted)
            decrypted = subprocess.run(
                ["openssl", "cms", "-decrypt", "-inform", "DER", "-recip", cert, "-inkey", key],
                input=encrypted, capture_output=True, check=True,
            )
            self.assertEqual(decrypted.stdout, payload)
