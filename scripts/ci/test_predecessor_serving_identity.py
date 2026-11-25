"""Exercise a split Worker and retained image revision without provider access."""

import json
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import production_ownership_status as status
from worker_container_rollback import GateError, ProvenanceReceipt, Rollback, validate_receipt_against_live


class PredecessorServingIdentityTests(unittest.TestCase):
    def test_no_single_rollback_sha_admits_a_split_stamped_predecessor(self):
        fixture = Path(__file__).with_name("fixtures") / "production_ownership_status_provider.json"
        routes = json.loads(fixture.read_text())["provider"]
        account = "c" * 32
        client = Mock(spec=["api", "request"])
        client.api.side_effect = routes.__getitem__
        observed = status.provider_sample(client, account)
        worker = observed["worker"]
        container = observed["container"]
        image = f"registry.cloudflare.com/{account}/{status.APPLICATION}@{container['image_digest']}"
        retained_revision = "e" * 40
        report = {"build_revision": retained_revision, "build_id": "1-1", "components": [["gateway", "ready"]]}

        def probe(url):
            body = json.dumps(report).encode() if url.endswith("/readyz") else b"{}"
            return 200, {"x-two-worker-version": worker["version_id"]}, body

        client.request.side_effect = probe

        def receipt(rollback_sha):
            return ProvenanceReceipt(status.WORKER, status.APPLICATION, worker["version_id"], rollback_sha, image)

        def readiness(rollback_sha):
            rollback = Rollback(client, None, status.PRODUCTION_URL, "production", status.WORKER,
                                status.APPLICATION, container["application_id"], rollback_sha, "1")
            return rollback.gate_pass(worker["version_id"])

        self.assertTrue(readiness(retained_revision)[0])
        with self.assertRaisesRegex(GateError, "^provenance_mismatch$"):
            validate_receipt_against_live(client, receipt(retained_revision))

        validate_receipt_against_live(client, receipt(worker["sha"]))
        self.assertEqual(readiness(worker["sha"]), (False, "readyz=200 identity=mismatch"))

        report["build_revision"] = worker["sha"]
        self.assertTrue(readiness(worker["sha"])[0])
