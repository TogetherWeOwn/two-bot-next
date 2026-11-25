"""Exercise the PRE operator join with synthetic, independently scoped rows."""

from dataclasses import replace
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, call, patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from ci.pre_operator_acceptance_fixture import (
        AcceptedRow, Build, IngressReadback, Probe, SchemaEvidence, SchemaState,
        ServingEvidence, SweepEvidence, WorkerEvidence, accept,
    )
    from ci.test_pre_admission import Store
    from ci.test_pre_start_stop import START, PreFixture
    from ci.test_staging_pre_reaper_fixture import SyntheticStaging
    from pre_admission import SettlementEvidence, SettlementSource, StartupAdmission
    from pre_start_stop import ProtectedPre
    from pre_sweep import SweepError, sweep_target
    from pre_target import ProtectedRouteReadback, Resource, Target
    from rollback_proof import ProofError


class PreOperatorAcceptanceTests(unittest.TestCase):
    def setUp(self):
        self.target = Target("pre-fixture-operator", 42, 1_800_000_000, "0473/0465")
        self.build = Build(17, "a" * 40, "2026.11.25.1+aaaaaaa", "sha256:" + "b" * 64)
        self.original = SchemaState((115, 473), (20, 465), False, None)
        self.restored = SchemaState((116, 475), (20, 465), True, 1_800_000_001)
        self.version = "12345678-1234-4234-8234-123456789abc"
        self.report = {"build_revision": "a" * 40, "build_id": "17-1", "deploy_id": "2026.11.25.1+aaaaaaa",
                       "components": [["process", "ready"], ["database", "ready"], ["gateway", "ready"]]}

    def evidence(self, target, state, *, app_id="12345678-1234-4234-8234-123456789abc"):
        route = ProtectedRouteReadback(target, target.worker_name, Resource(target.container_name, app_id))
        return {
            "receipt": {"source_sha": "a" * 40, "worker_message": "a" * 40, "BOT_BUILD_REVISION": "a" * 40,
                        "BOT_BUILD_ID": "17-1", "BOT_DEPLOY_ID": "2026.11.25.1+aaaaaaa",
                        "image_manifest_digest": "sha256:" + "b" * 64},
            "schema": SchemaEvidence(target, state),
            "serving": ServingEvidence(target, self.version, "a" * 40, "sha256:" + "b" * 64),
            "worker": WorkerEvidence(target, Probe(200, self.version),
                                     Probe(200, self.version, json.dumps(self.report).encode())),
            "sweep": SweepEvidence(target, "cleared"),
            "pinned_route": route,
            "pinned_app_id": app_id,
            "read_ingress": lambda _: IngressReadback(route, False),
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
        read_ingress = evidence["read_ingress"] = Mock()
        for outcome in ("spared", None):
            with self.subTest(outcome=outcome):
                self.refuse(evidence | {"sweep": SweepEvidence(self.target, outcome)}, "pre_cleanup_unproved")
        self.refuse(evidence | {"sweep": None}, "pre_cleanup_unproved")

        for foreign in self.foreign_reservations():
            with self.subTest(foreign=foreign):
                self.refuse(evidence | {"sweep": SweepEvidence(foreign, "cleared")}, "pre_cleanup_reservation_mismatch")
        read_ingress.assert_not_called()

    def test_stop_success_keeps_custody_until_fresh_ingress_worker_and_app_absence(self):
        target = self.target
        runtime = PreFixture(target)
        store = Store()
        admission = StartupAdmission(
            store, lambda closed: SettlementEvidence(closed, SettlementSource.PROVIDER, True, True),
        )
        controller = ProtectedPre(target, authorize=runtime.authorize, admission=admission,
                                  runtime=runtime, external_stop=runtime)
        with patch("pre_admission.uuid4", return_value=START):
            deployment = controller.start(runtime.upload)
        self.version = str(deployment.worker_version)
        evidence = self.evidence(target, self.original, app_id=str(deployment.container_id))
        route = evidence["pinned_route"]
        staging = SyntheticStaging(target)
        self.assertIs(controller.stop(), True)
        staging.snapshot = replace(staging.snapshot, container=route.app,
                                   startup_settled=admission.startup_settled(target))

        read_ingress = evidence["read_ingress"] = Mock(return_value=IngressReadback(route, True))

        def handoff():
            outcome = sweep_target(target, staging, now=store.now)
            return accept(target, self.build, **(evidence | {"sweep": SweepEvidence(target, outcome)}))

        worker = staging.snapshot.worker
        with self.assertRaisesRegex(ProofError, "^pre_ingress_unproved$"):
            handoff()
        read_ingress.assert_called_once_with(route)
        cleared = staging.snapshot
        self.assertIsNone(cleared.worker)
        self.assertIsNone(cleared.container)

        read_ingress.return_value = IngressReadback(route, False)
        for lingering in (replace(cleared, worker=worker), replace(cleared, container=route.app)):
            with self.subTest(lingering=lingering), patch.object(
                staging, "inventory", side_effect=[cleared, lingering]
            ) as inventory:
                with self.assertRaisesRegex(SweepError, "^PRE cleanup readback could not be verified$"):
                    handoff()
                self.assertEqual(inventory.call_args_list, [call(target), call(target)])
        read_ingress.assert_called_once_with(route)

        self.assertEqual(handoff(), AcceptedRow(target, self.build, self.original, self.version, "cleared"))
        self.assertEqual(read_ingress.call_args_list, [call(route), call(route)])

    def test_private_ingress_absence_requires_the_independently_pinned_scope(self):
        evidence = self.evidence(self.target, self.original)
        route = evidence["pinned_route"]
        read_ingress = evidence["read_ingress"] = Mock()
        wrong_app = replace(route, app=replace(route.app, id="12345678-1234-4234-8234-123456789def"))
        self.refuse(evidence | {"pinned_route": wrong_app}, "pre_ingress_unproved")
        read_ingress.assert_not_called()

        for observed in (replace(route, target=replace(self.target, shape="0475/0465")),
                         replace(route, worker_id="sibling-worker"), wrong_app):
            with self.subTest(observed=observed):
                read_ingress.return_value = IngressReadback(observed, False)
                self.refuse(evidence, "pre_ingress_unproved")
        for present in (None, 0):
            with self.subTest(present=present):
                read_ingress.return_value = IngressReadback(route, present)
                self.refuse(evidence, "pre_ingress_unproved")
        read_ingress.side_effect = TimeoutError("synthetic private detail")
        self.refuse(evidence, "pre_ingress_unproved")
