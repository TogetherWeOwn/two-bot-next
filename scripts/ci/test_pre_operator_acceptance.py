"""Exercise the PRE operator join with synthetic, independently scoped rows."""

from dataclasses import replace
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from ci.pre_operator_acceptance_fixture import (
        AcceptedRow, Build, Probe, Reservation, SchemaEvidence, SchemaState,
        ServingEvidence, SweepEvidence, WorkerEvidence, accept,
    )
    from rollback_proof import ProofError


class PreOperatorAcceptanceTests(unittest.TestCase):
    def setUp(self):
        self.target = Reservation("pre-fixture-operator", 42, 1_800_000_000, "0473/0465")
        self.build = Build(17, "a" * 40, "2026.11.25.1+aaaaaaa", "sha256:" + "b" * 64)
        self.original = SchemaState((115, 473), (20, 465), False, None)
        self.restored = SchemaState((116, 475), (20, 465), True, 1_800_000_001)
        self.version = "12345678-1234-4234-8234-123456789abc"
        self.report = {"build_revision": "a" * 40, "build_id": "17-1", "deploy_id": "2026.11.25.1+aaaaaaa",
                       "components": [["process", "ready"], ["database", "ready"], ["gateway", "ready"]]}

    def evidence(self, target, state):
        return {
            "receipt": {"source_sha": "a" * 40, "worker_message": "a" * 40, "BOT_BUILD_REVISION": "a" * 40,
                        "BOT_BUILD_ID": "17-1", "BOT_DEPLOY_ID": "2026.11.25.1+aaaaaaa",
                        "image_manifest_digest": "sha256:" + "b" * 64},
            "schema": SchemaEvidence(target, state),
            "serving": ServingEvidence(target, self.version, "a" * 40, "sha256:" + "b" * 64),
            "worker": WorkerEvidence(target, Probe(200, self.version),
                                     Probe(200, self.version, json.dumps(self.report).encode())),
            "sweep": SweepEvidence(target, "cleared"),
        }

    def refuse(self, evidence, reason):
        with self.assertRaisesRegex(ProofError, f"^{reason}$"):
            accept(self.target, self.build, **evidence)

    def foreign_reservations(self):
        for change in ({"run_id": 43}, {"scope": "pre-fixture-other"},
                       {"started_at": self.target.started_at + 1}, {"shape": "0475/0465"}):
            yield replace(self.target, **change)

    def test_both_pre_endpoints_keep_their_own_reservations_and_schema(self):
        restored_target = replace(self.target, run_id=43, shape="0475/0465")
        original = accept(self.target, self.build, **self.evidence(self.target, self.original))
        self.assertEqual(original, AcceptedRow(self.target, self.build, self.original, self.version, "cleared"))
        self.version = "12345678-1234-4234-8234-123456789def"
        restored = accept(restored_target, self.build, **self.evidence(restored_target, self.restored))
        self.assertEqual(restored, AcceptedRow(restored_target, self.build, self.restored, self.version, "cleared"))

    def test_mixed_build_and_staging_runs_are_refused(self):
        evidence = self.evidence(self.target, self.original)
        receipt = evidence["receipt"] | {"BOT_BUILD_ID": "42-1"}
        self.refuse(evidence | {"receipt": receipt}, "pre_receipt_mismatch")

        for stage in ("schema", "serving", "worker"):
            for foreign in self.foreign_reservations():
                with self.subTest(stage=stage, foreign=foreign):
                    observation = replace(evidence[stage], reservation=foreign)
                    self.refuse(evidence | {stage: observation}, "pre_endpoint_reservation_mismatch")

    def test_invalid_build_pins_and_absent_receipt_are_refused(self):
        evidence = self.evidence(self.target, self.original)
        for change in ({"run_id": True}, {"revision": None}, {"deploy_id": None}, {"image_digest": None}):
            with self.subTest(change=change), self.assertRaisesRegex(ProofError, "^pre_build_pin_invalid$"):
                accept(self.target, replace(self.build, **change), **evidence)
        self.refuse(evidence | {"receipt": None}, "pre_receipt_mismatch")

        unstamped_build = replace(self.build, revision=None, deploy_id=None, image_digest=None)
        unstamped_report = {"build_id": self.build.build_id, "components": self.report["components"]}
        worker = evidence["worker"]
        evidence.update(
            receipt={"BOT_BUILD_ID": self.build.build_id},
            serving=replace(evidence["serving"], worker_message=None, image_digest=None),
            worker=replace(worker, readyz=replace(worker.readyz, body=json.dumps(unstamped_report).encode())),
        )
        with self.assertRaisesRegex(ProofError, "^pre_build_pin_invalid$"):
            accept(self.target, unstamped_build, **evidence)

    def test_wrong_predecessor_is_refused_at_each_identity_boundary(self):
        evidence = self.evidence(self.target, self.original)
        for change in (dict.fromkeys(("source_sha", "worker_message", "BOT_BUILD_REVISION"), "c" * 40),
                       {"BOT_DEPLOY_ID": "2026.11.24.1+aaaaaaa"}, {"image_manifest_digest": "sha256:" + "c" * 64}):
            with self.subTest(change=change):
                self.refuse(evidence | {"receipt": evidence["receipt"] | change}, "pre_receipt_mismatch")
        serving = evidence["serving"]
        for change in ({"worker_message": "c" * 40}, {"image_digest": "sha256:" + "c" * 64},
                       {"worker_version": "unproved"}):
            with self.subTest(change=change):
                self.refuse(evidence | {"serving": replace(serving, **change)}, "pre_serving_identity_mismatch")

        worker = evidence["worker"]
        for change in ({"build_revision": "c" * 40}, {"build_id": "18-1"}, {"deploy_id": "2026.11.24.1+aaaaaaa"},
                       {"components": [["database", "ready"]]}):
            with self.subTest(change=change):
                probe = replace(worker.readyz, body=json.dumps(self.report | change).encode())
                self.refuse(evidence | {"worker": replace(worker, readyz=probe)}, "pre_worker_readiness_unproved")

    def test_wrong_pre_schema_or_retained_timestamp_is_refused(self):
        evidence = self.evidence(self.target, self.restored)
        self.refuse(evidence, "pre_schema_mismatch")

        restored_target = replace(self.target, run_id=43, shape="0475/0465")
        for change in ({"attendance_recovered_at": False}, {"installed_0475_at": 1_800_000_002}, {"store": (21, 478)},
                       {"attendance_recovered_at": 1}, {"cutover": (116.0, 475)}, {"installed_0475_at": 1_800_000_001.0}):
            with self.subTest(change=change), self.assertRaisesRegex(ProofError, "^pre_schema_mismatch$"):
                accept(restored_target, self.build, **self.evidence(restored_target, replace(self.restored, **change)))

    def test_missing_endpoint_observations_are_refused(self):
        evidence = self.evidence(self.target, self.original)
        for stage in ("schema", "serving", "worker"):
            with self.subTest(stage=stage):
                self.refuse(evidence | {stage: None}, "pre_endpoint_evidence_missing")
        for path in ("health", "readyz"):
            with self.subTest(path=path):
                worker = replace(evidence["worker"], **{path: None})
                self.refuse(evidence | {"worker": worker}, "pre_endpoint_evidence_missing")

    def test_failed_worker_probes_are_refused_even_with_matching_versions(self):
        evidence = self.evidence(self.target, self.original)
        worker = evidence["worker"]
        for path, reason in (("health", "pre_worker_health_unproved"), ("readyz", "pre_worker_readiness_unproved")):
            with self.subTest(path=path):
                failed = replace(getattr(worker, path), status=503)
                self.refuse(evidence | {"worker": replace(worker, **{path: failed})}, reason)
        stale_health = replace(worker.health, worker_version="12345678-1234-4234-8234-123456789def")
        self.refuse(evidence | {"worker": replace(worker, health=stale_health)}, "pre_worker_health_unproved")

    def test_direct_container_probes_do_not_establish_worker_readiness(self):
        evidence = self.evidence(self.target, self.original)
        worker = evidence["worker"]
        direct_health = replace(worker.health, worker_version=None)
        direct_readyz = replace(worker.readyz, worker_version=None)
        direct = replace(worker, health=direct_health, readyz=direct_readyz)
        self.refuse(evidence | {"worker": direct}, "pre_worker_health_unproved")
        # A Worker /health response still cannot vouch for direct /readyz.
        self.refuse(evidence | {"worker": replace(worker, readyz=direct_readyz)}, "pre_worker_readiness_unproved")

    def test_cleanup_must_be_cleared_for_the_exact_reservation(self):
        evidence = self.evidence(self.target, self.original)
        for outcome in ("spared", None):
            with self.subTest(outcome=outcome):
                self.refuse(evidence | {"sweep": SweepEvidence(self.target, outcome)}, "pre_cleanup_unproved")
        self.refuse(evidence | {"sweep": None}, "pre_cleanup_unproved")

        for foreign in self.foreign_reservations():
            with self.subTest(foreign=foreign):
                self.refuse(evidence | {"sweep": SweepEvidence(foreign, "cleared")}, "pre_cleanup_reservation_mismatch")
