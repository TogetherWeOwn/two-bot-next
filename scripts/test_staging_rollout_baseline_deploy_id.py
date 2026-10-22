"""Offline regression pins for deploy IDs in local rollout baselines."""

import argparse
import contextlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_rollout as rollout
from test_staging_rollout import baseline


class BaselineDeployIdTests(unittest.TestCase):
    def assert_refuses(self, value):
        with self.assertRaises(rollout.GateError) as caught:
            rollout.validate_baseline(value)
        self.assertEqual(str(caught.exception), "invalid_baseline_receipt")

    def test_missing_deploy_id_refuses(self):
        value = baseline()
        del value["deploy_id"]
        self.assert_refuses(value)

    def test_mistyped_deploy_id_refuses(self):
        for deploy_id in (None, True, 123, [], {}, [baseline()["deploy_id"]]):
            with self.subTest(deploy_id=deploy_id):
                self.assert_refuses(dict(baseline(), deploy_id=deploy_id))

    def test_malformed_deploy_id_refuses(self):
        for deploy_id in ("", "unknown", "2026.02.30.1+aaaaaaa",
                          "2026.10.21.0+aaaaaaa", "2026.10.21.01+aaaaaaa",
                          "2026.10.21.1+AAAAAAA", "2026.10.21.1+aaaaaaa\n",
                          "2026.10.21.1234567+aaaaaaa"):
            with self.subTest(deploy_id=deploy_id):
                self.assert_refuses(dict(baseline(), deploy_id=deploy_id))

    def test_valid_receipt_passes_through_untouched(self):
        value = baseline()
        before = json.dumps(value)
        self.assertIs(rollout.validate_baseline(value), value)
        self.assertEqual(json.dumps(value), before)

    def test_file_modes_refuse_missing_deploy_id_before_external_reads(self):
        with tempfile.TemporaryDirectory() as root:
            args = argparse.Namespace(
                receipt=str(Path(root) / "baseline.json"),
                output=str(Path(root) / "deploy.ndjson"),
                evidence=str(Path(root) / "evidence.json"),
            )
            value = baseline()
            del value["deploy_id"]
            Path(args.receipt).write_text(json.dumps(value))
            for mode in ("receipt", "verify"):
                with self.subTest(mode=mode), patch.object(
                    rollout, "read_records", side_effect=AssertionError("deploy read before validation")
                ), patch.object(
                    rollout, "docker_image", side_effect=AssertionError("Docker before validation")
                ), contextlib.redirect_stdout(io.StringIO()):
                    with self.assertRaises(rollout.GateError) as caught:
                        getattr(rollout, mode)(args, Mock())
                    self.assertEqual(str(caught.exception), "invalid_baseline_receipt")
                    self.assertFalse(Path(args.evidence).exists())


if __name__ == "__main__":
    unittest.main()
