"""Exercise the offline handoff against projected recovery receipts and status."""

from copy import deepcopy
from hashlib import sha256
import json
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "production_recovery_report.py"
FIXTURE = Path(__file__).with_name("fixtures") / "production_inverse_admission.json"


class ProductionRecoveryReportTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(TemporaryDirectory()))
        self.case = json.loads(FIXTURE.read_text(encoding="utf-8"))
        self.admission = self.case["admission"]
        self.old = self.receipt(19, 1)
        self.current = self.receipt(21, 3)
        self.latest = deepcopy(self.current["provider"])
        self.latest["ownership_observed_at"] = "2026-11-25T02:04:00.123456+00:00"

    def receipt(self, fence_epoch, minute):
        pin = dict(self.admission["pin"], fence_epoch=fence_epoch)
        return {
            "approval": {"plan_run_id": self.admission["plan_run_id"],
                         "plan_archive_sha256": self.case["artifact"]["archive_digest"],
                         "workflow_sha": self.case["run"]["head_sha"],
                         "recovery_ref": self.case["plan"]["recovery_evidence_ref"],
                         "candidate_worker": self.admission["candidate_worker"],
                         "candidate_image": self.admission["candidate_image"],
                         "forward_artifact_id": "789", "forward_archive_sha256": "8" * 64},
            "inverse": {"state": "restored", **pin},
            "provider": {
                "ownership_observed_at": f"2026-11-25T02:0{minute}:00.123456+00:00",
                "worker": {"deployment_id": "44444444-4444-4444-8444-444444444444",
                           "version_id": pin["worker_version"], "sha": pin["rollback_sha"], "traffic_percent": 100},
                "container": {"application_id": pin["application_id"], "namespace_id": pin["namespace_id"],
                              "image_digest": pin["image_digest"], "rollout_status": "completed",
                              "rollout_id": "55555555-5555-4555-8555-555555555555"},
                "ownership": {"epoch": fence_epoch + 1, "phase": "active", "running": True,
                              "owner_actor": f"github-actions:{pin['run_id']}:{pin['rollback_sha']}"},
            },
            "stable_polls": 20,
        }

    def invoke(self, readback, *receipts):
        observation = self.root / "status.json"
        observation.write_text(json.dumps(readback), encoding="utf-8")
        args = [sys.executable, "-B", str(SCRIPT), "--status", str(observation)]
        digests = []
        for index, receipt in enumerate(receipts):
            artifact = self.root / f"receipt-{index}.json"
            digest, raw = receipt if isinstance(receipt, tuple) else (None, json.dumps(receipt).encode())
            artifact.write_bytes(raw)
            digests.append(digest or sha256(raw).hexdigest())
            args.extend(("--receipt", digests[-1], str(artifact)))
        return subprocess.run(args, env={}, capture_output=True, timeout=10), digests

    def assert_rows(self, readback, receipts, statuses):
        result, digests = self.invoke(readback, *receipts)
        self.assertEqual((result.returncode, result.stderr), (0, b""))
        self.assertEqual(json.loads(result.stdout), {
            "ownership_observed_at": readback["ownership_observed_at"],
            "receipts": [{"sha256": digest, "status": status} for digest, status in zip(digests, statuses, strict=True)],
        })

    def test_second_fence_supersedes_old_receipt_and_preserves_current_sibling(self):
        prior = deepcopy(self.old["provider"])
        prior["ownership_observed_at"] = "2026-11-25T02:01:30.123456+00:00"
        self.assert_rows(prior, [self.old], ["current_as_of_observation"])

        fenced = deepcopy(prior)
        pinned = self.admission["pin"]
        fenced["ownership_observed_at"] = "2026-11-25T02:02:00.123456+00:00"
        fenced["worker"].update(version_id=self.admission["candidate_worker"], sha=pinned["candidate_sha"],
                                deployment_id="77777777-7777-4777-8777-777777777777")
        fenced["container"].update(image_digest=self.admission["candidate_image"],
                                   rollout_id="88888888-8888-4888-8888-888888888888")
        fenced["ownership"].update(epoch=21, phase="fenced", running=False,
                                   owner_actor=f"production-schema-release:{pinned['run_id']}:{pinned['candidate_sha']}")
        self.assert_rows(fenced, [self.old, self.current], ["superseded", "unverified"])
        for receipts, statuses in (
            ([self.old, self.current], ["superseded", "current_as_of_observation"]),
            ([self.current, self.old], ["current_as_of_observation", "superseded"]),
        ):
            self.assert_rows(self.latest, receipts, statuses)

    def test_requires_later_independent_status_and_current_exact_deployment(self):
        self.assert_rows(self.current["provider"], [self.current], ["unverified"])
        for group, field, changed in (
            ("ownership", "epoch", 20),
            ("ownership", "owner_actor", f"github-actions:999:{self.admission['pin']['rollback_sha']}"),
            ("ownership", "running", False),
            ("container", "namespace_id", "0" * 32),
            ("container", "rollout_id", "66666666-6666-4666-8666-666666666666"),
            ("worker", "deployment_id", "66666666-6666-4666-8666-666666666666"),
        ):
            with self.subTest(group=group, field=field):
                changed_status = deepcopy(self.latest)
                changed_status[group][field] = changed
                self.assert_rows(changed_status, [self.current], ["unverified"])
        unrelated = deepcopy(self.latest)
        unrelated["container"]["application_id"] = "99999999-9999-4999-8999-999999999999"
        self.assert_rows(unrelated, [self.old, self.current], ["unverified", "unverified"])

    def test_refuses_incomplete_shared_status_before_classifying_any_receipt(self):
        # The old receipt would otherwise be superseded by this later epoch.
        for group in ("worker", "container", "ownership"):
            for field in self.latest[group]:
                with self.subTest(group=group, missing=field):
                    incomplete = deepcopy(self.latest)
                    del incomplete[group][field]
                    result, _ = self.invoke(incomplete, self.old, self.current)
                    self.assertEqual((result.returncode, result.stdout), (2, b""))

        for group, field, value in (
            ("worker", "sha", "bad-sha"),
            ("worker", "traffic_percent", 100.0),
            ("container", "image_digest", "sha256:short"),
            ("container", "rollout_status", "progressing"),
            ("ownership", "owner_actor", []),
        ):
            with self.subTest(group=group, invalid=field):
                invalid = deepcopy(self.latest)
                invalid[group][field] = value
                result, _ = self.invoke(invalid, self.old, self.current)
                self.assertEqual((result.returncode, result.stdout), (2, b""))

    def test_invalid_inputs_cannot_suppress_current_sibling_or_leak_payload(self):
        wrong_digest = "0" * 64, json.dumps(self.current).encode()
        duplicate = b'{"stable_polls":null,' + json.dumps(self.current).encode()[1:]
        oversized = b" " * 65537
        for malformed in (wrong_digest, (None, duplicate), (None, oversized), dict(self.current, stable_polls=True)):
            self.assert_rows(self.latest, [malformed, self.current], ["unverified", "current_as_of_observation"])

        result, _ = self.invoke({"ownership_observed_at": "synthetic private value"}, self.current)
        self.assertEqual((result.returncode, result.stdout), (2, b""))
        self.assertEqual(result.stderr, b"recovery report refused: missing or invalid ownership observation\n")
