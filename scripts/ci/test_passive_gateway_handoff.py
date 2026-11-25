"""Exercise the offline gateway summary handoff with synthetic release evidence."""

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from test_t0_acceptance_report import PRODUCTION, SCRIPTS


class PassiveGatewayHandoffTests(unittest.TestCase):
    def test_production_gateway_attribution_requires_exact_window(self):
        checkpoint = {key: PRODUCTION[key] for key in ("build_revision", "build_id", "deploy_id")}
        execution_card = {key: PRODUCTION[key] for key in (
            "environment", "image_digest", "worker_version", "application_id", "start", "end",
        )}
        expected = checkpoint | execution_card
        # The execution card records hosted policy coverage separately from passive rejection.
        hosted_policy = {"disposition": "verified in hosted tests", "build_id": checkpoint["build_id"]}
        ready = {"ts": "2026-11-25T02:01:00.000Z", "msg": "ready",
                 "target": "two_bot::gateway", "build_sha": checkpoint["build_revision"]}
        packet = {"schema_version": 1, "service": execution_card["application_id"], "contains": False,
                  "window": {key: execution_card[key].replace("Z", ".000Z") for key in ("start", "end")},
                  "event_count": 1, "limit": 100, "possibly_truncated": False, "events": [ready]}
        mixed = dict(packet, event_count=2, events=[ready, dict(ready, build_sha="d" * 40)])
        other_app = "33333333-3333-4333-8333-333333333333"
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        summary_path, handoff_path = root / "gateway.json", root / "handoff.json"

        for capture, binding_overrides, verdict, differing in (
            (packet, {}, "observed", []),
            (mixed, {}, "mixed_build", []),
            (dict(packet, service=other_app), {}, "service_mismatch", []),
            (packet, {"image_digest": "sha256:" + "c" * 64}, "observed", ["image_digest"]),
            (packet, {"application_id": other_app}, "observed", ["application_id"]),
        ):
            with self.subTest(verdict=verdict, differing=differing):
                producer = subprocess.run(
                    [sys.executable, "-B", str(SCRIPTS / "passive_gateway_evidence.py"),
                     "--service", expected["application_id"], "--sha", expected["build_revision"],
                     "--start", expected["start"], "--end", expected["end"]],
                    input=json.dumps(capture).encode(), capture_output=True, env={}, timeout=10,
                )
                self.assertEqual(producer.returncode, 0 if verdict == "observed" else 1, producer.stderr)
                self.assertEqual(json.loads(producer.stdout)["verdict"], verdict)
                summary_path.write_bytes(producer.stdout)
                binding = {"window": expected | binding_overrides,
                           "sha256": hashlib.sha256(producer.stdout).hexdigest()}
                handoff_path.write_text(json.dumps({
                    "expected": expected, "observations": {"gateway": binding},
                    "endpoint_policy_coverage": hosted_policy,
                }), encoding="utf-8")

                consumer = subprocess.run(
                    [sys.executable, "-B", str(SCRIPTS / "t0_acceptance_report.py"),
                     "--handoff", str(handoff_path), "--gateway-report", str(summary_path)],
                    capture_output=True, check=True, env={}, timeout=10,
                )
                row = json.loads(consumer.stdout)["observations"]["gateway"]
                self.assertEqual(row["endpoint_denial"], "unverified")
                if verdict == "observed" and not differing:
                    self.assertEqual(row["status"], "attributed")
                    self.assertEqual(row["window"], PRODUCTION)
                    self.assertEqual(row["readiness"], "observed")
                    self.assertEqual(row["signals"]["ready"], 1)
                else:
                    self.assertEqual(row["status"], "mismatched")
                    if differing:
                        self.assertEqual(row["fields"], differing)
                    else:
                        self.assertEqual(row["source_verdict"], verdict)
                    self.assertNotIn("readiness", row)
                    self.assertNotIn("signals", row)
