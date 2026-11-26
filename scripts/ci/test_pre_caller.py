"""Exercise the separate protected PRE callers against one synthetic authority."""

from dataclasses import dataclass, replace
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch
from uuid import UUID

from scripts.ci.test_staging_pre_reaper_fixture import SyntheticStaging

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_admission import SettlementEvidence, SettlementSource, StartupAdmission
    from pre_caller import ProtectedPreCaller, finalize_pre
    from pre_registry_cleanup import PublicationSettlement, RegistryDisposition, RegistryReadback, RegistryReservation
    from pre_start_stop import Deployment, EffectWindow, Endpoint, Operation, ProbeReply
    from pre_sweep import SweepError
    from pre_target import Target, TargetError


ORIGINAL = Target("pre-fixture-caller", 71, 1_800_000_000, "0473/0465")
RETAINED = replace(ORIGINAL, run_id=72, started_at=ORIGINAL.expires_at + 1, shape="0475/0465")
REVISION = "a" * 40
RECEIPT = {
    "source_sha": REVISION, "worker_message": REVISION, "BOT_BUILD_REVISION": REVISION,
    "BOT_BUILD_ID": "123-1", "BOT_DEPLOY_ID": f"2026.11.25.1+{REVISION[:7]}",
}


@dataclass(frozen=True)
class Upload:
    target: Target
    content_type: str = "multipart/form-data; boundary=fixture"
    body: bytes = b"synthetic Worker upload"
    supports_discord_fixture_overrides: bool = True


class Fixture(SyntheticStaging):
    def __init__(self, target=ORIGINAL):
        super().__init__(target)
        self.target = target
        self.window = EffectWindow(target, target.expires_at)
        self.before_create = Mock()
        self.provisioned = self.snapshot
        self.snapshot = replace(self.snapshot, worker=None, container=None, write_access=False,
                                startup_settled=False, instance_count=0, assigned_count=0)
        self.record = self.arm_pin = self.created = self.verified = None
        self.now = target.started_at + 1
        self.status = "active"
        self.fenced = self.lose_create_reply = self.registry_present = False
        self.drained = self.publisher_settled = True
        self.gateway = [["gateway", "ready"]]
        self.reservation = RegistryReservation(
            target, f"registry.invalid/run-{target.run_id}/two-bot-next-attendance-predecessor-staging@sha256:"
            + "b" * 64,
        )
        self.admission_reader = StartupAdmission(self, self.observe_settlement)

    def caller(self, *operations):
        return ProtectedPreCaller(
            self.target, authorize=lambda target, op: target == self.target and op in operations,
            store=self, observe_settlement=self.observe_settlement, runtime=self, external_stop=self,
            effect_window=self.window if Operation.START in operations else None,
        )

    def read(self, target):
        if target != self.target:
            raise ValueError("unexpected admission target")
        return self.record

    def compare_and_swap(self, target, expected, replacement, *, start_window):
        if self.read(target) != expected:
            return False
        if start_window is not None:
            start, end = start_window
            if not start <= self.now < end:
                return False
        self.record = replacement
        step = "close" if replacement.closed else "admit"
        if replacement.settled_by is not None:
            step = "persist_settlement"
        self.events.append((step, target))
        return True

    def observe_settlement(self, record):
        self.events.append(("settlement", record.target))
        return SettlementEvidence(record, SettlementSource.PROVIDER, self.drained, self.fenced)

    def arm(self, window, start_id):
        target = window.target
        self.events.append(("arm", target))
        if self.fenced or self.arm_pin is not None or self.read(target).start_id != start_id:
            return False
        try:
            window.require_open(self.now)
        except TargetError:
            return False
        self.arm_pin = (window, start_id)
        return True

    def fence(self, target):
        self.events.append(("fence", target))
        self.fenced = target == self.target
        return self.fenced

    def confirm_probe_active(self, target, start_id):
        if self.arm_pin is None or self.fenced:
            return False
        window, armed_start_id = self.arm_pin
        if window.target != target or armed_start_id != start_id:
            return False
        try:
            window.require_open(self.now)
        except TargetError:
            return False
        return True

    def create_private(self, window, start_id, upload):
        self.before_create()
        if self.fenced or self.arm_pin != (window, start_id):
            raise ValueError("unexpected create")
        window.require_open(self.now)
        target = window.target
        self.events.append(("create", upload.target))
        self.snapshot = replace(self.provisioned, startup_settled=False)
        self.created = Deployment(target, start_id, UUID(int=target.run_id), UUID(self.snapshot.container.id))
        if self.lose_create_reply:
            raise TimeoutError("synthetic lost reply")
        self.verified = self.created
        return self.verified

    def probe(self, deployment, endpoint):
        if self.fenced or self.arm_pin is None or deployment != self.verified:
            raise ValueError("unexpected probe")
        window, _ = self.arm_pin
        window.require_open(self.now)
        self.events.append((endpoint.value, deployment.target))
        ready = dict(build_revision=REVISION, build_id=RECEIPT["BOT_BUILD_ID"],
                     deploy_id=RECEIPT["BOT_DEPLOY_ID"], components=self.gateway)
        body = json.dumps(ready).encode() if endpoint is Endpoint.READINESS else b'{"status":"ok"}'
        return ProbeReply(200, deployment.worker_version, body)

    def inventory(self, target):
        return replace(super().inventory(target), startup_settled=self.admission_reader.startup_settled(target))

    def stop_container(self, target, action):
        return replace(super().stop_container(target, action),
                       startup_settled=self.admission_reader.startup_settled(target))

    def registry_reservation(self, target):
        self.events.append(("registry_reservation", target))
        return self.reservation

    def publication_settlement(self, reservation):
        self.events.append(("publication_settled", reservation.target))
        if self.publisher_settled:
            return PublicationSettlement(self.reservation)
        return None

    def read_registry(self, reservation):
        self.events.append(("registry", reservation.target))
        return RegistryReadback(reservation.target, reservation.reference, self.registry_present)

    def finalize(self, target=None):
        return finalize_pre(target or self.target, authorize=lambda t: t == self.target,
                            trusted_now=lambda: self.now, adapter=self)

    def steps(self):
        return [event[0] for event in self.events]


class PreCallerTests(unittest.TestCase):
    def test_delayed_create_honors_lease_and_preserves_independent_stop(self):
        for target in (ORIGINAL, RETAINED):
            with self.subTest(shape=target.shape):
                fixture = Fixture(target)
                cutoff = target.started_at + 60
                fixture.window = EffectWindow(target, cutoff)
                fixture.before_create.side_effect = lambda: setattr(fixture, "now", cutoff + 1)
                start = fixture.caller(Operation.START)
                with self.assertRaisesRegex(TargetError, "^pre_create_unconfirmed$"):
                    start.start(Upload(target))
                fixture.before_create.assert_called_once_with()
                self.assertLess(fixture.now, target.expires_at)
                self.assertEqual(fixture.steps(), ["admit", "arm"])
                self.assertIsNone(fixture.created)
                self.assertIsNone(fixture.snapshot.worker)
                self.assertIsNone(fixture.snapshot.container)
                self.assertFalse(fixture.snapshot.write_access)
                opened = fixture.record
                self.assertFalse(opened.closed)
                self.assertEqual(fixture.arm_pin, (fixture.window, opened.start_id))

                stop = fixture.caller(Operation.STOP)
                fixture.drained = False
                self.assertFalse(stop.stop())
                closed = replace(opened, closed=True)
                self.assertEqual(fixture.record, closed)
                fixture.drained = True
                self.assertTrue(stop.stop())
                self.assertEqual(fixture.record, replace(closed, settled_by=SettlementSource.PROVIDER))

    def test_both_shapes_cross_independent_start_read_stop_and_terminal_sweep(self):
        for target in (ORIGINAL, RETAINED):
            with self.subTest(shape=target.shape):
                fixture = Fixture(target)
                deployment = fixture.caller(Operation.START).start(Upload(target))
                self.assertEqual(deployment.target, target)
                self.assertEqual(deployment.start_id, fixture.record.start_id)
                health, readiness = fixture.caller(Operation.PROBE).read(deployment, verified_receipt=RECEIPT)
                self.assertEqual((health.status, readiness.status), (200, 200))
                self.assertEqual(fixture.steps(), ["admit", "arm", "create", "/health", "/readyz"])

                self.assertTrue(fixture.caller(Operation.STOP).stop())
                self.assertEqual(fixture.finalize(), "spared")
                self.assertEqual(fixture.steps()[-6:],
                                 ["fence", "close", "settlement", "persist_settlement", "status", "inventory"])
                fixture.status = "terminal"
                before = len(fixture.events)
                self.assertIs(fixture.finalize(), RegistryDisposition.ALREADY_ABSENT)
                self.assertEqual(fixture.steps()[before:], [
                    "status", "inventory", "revoke_write_access", "delete_worker", "stop_container",
                    "delete_container", "inventory", "inventory", "registry_reservation",
                    "publication_settled", "status", "registry",
                ])
                self.assertTrue(all(event[1] == target for event in fixture.events))

    def test_refused_admission_can_be_stopped_without_a_deployment_or_upload(self):
        fixture = Fixture()
        foreign = replace(fixture.target, shape=RETAINED.shape)
        with self.assertRaisesRegex(TargetError, "^pre_operation_denied$"):
            fixture.caller(Operation.STOP).start(Upload(fixture.target))
        with self.assertRaisesRegex(TargetError, "^wrong_pre_upload_target$"):
            fixture.caller(Operation.START).start(Upload(foreign))
        with self.assertRaisesRegex(SweepError, "^PRE finalizer authorization"):
            fixture.finalize(foreign)
        self.assertEqual(fixture.events, [])

        fixture.now = fixture.target.expires_at
        with self.assertRaisesRegex(TargetError, "^pre_start_not_admitted$"):
            fixture.caller(Operation.START).start(Upload(fixture.target))
        self.assertIsNone(fixture.record)
        self.assertTrue(fixture.caller(Operation.STOP).stop())
        self.assertIsNone(fixture.record.start_id)
        self.assertIs(fixture.finalize(), RegistryDisposition.ALREADY_ABSENT)
        self.assertNotIn("delete_container", fixture.steps())

    def test_lost_create_reply_cannot_restart_and_requires_independent_stop(self):
        fixture = Fixture()
        fixture.lose_create_reply = True
        start = fixture.caller(Operation.START)
        with self.assertRaisesRegex(TargetError, "^pre_create_unconfirmed$"):
            start.start(Upload(fixture.target))
        with self.assertRaisesRegex(TargetError, "^pre_probe_unconfirmed$"):
            fixture.caller(Operation.PROBE).read(fixture.created, verified_receipt=RECEIPT)
        with self.assertRaisesRegex(TargetError, "^pre_start_not_admitted$"):
            start.start(Upload(fixture.target))
        with self.assertRaisesRegex(TargetError, "^pre_operation_denied$"):
            start.stop()
        self.assertEqual(fixture.steps(), ["admit", "arm", "create"])
        fixture.drained = False
        self.assertFalse(fixture.caller(Operation.STOP).stop())
        self.assertEqual(fixture.finalize(), "spared")
        fixture.drained = True
        fixture.now = fixture.target.expires_at
        self.assertTrue(fixture.caller(Operation.STOP).stop())
        self.assertEqual(fixture.record.start_id, fixture.created.start_id)
        self.assertIs(fixture.finalize(), RegistryDisposition.ALREADY_ABSENT)

    def test_lost_final_readback_withholds_registry_handback_until_retry(self):
        for failed_read, error in (
            (1, "PRE cleanup readback could not be verified"),
            (2, "PRE registry handback could not be verified"),
        ):
            with self.subTest(failed_read=failed_read):
                fixture = Fixture()
                fixture.caller(Operation.START).start(Upload(fixture.target))
                self.assertTrue(fixture.caller(Operation.STOP).stop())
                fixture.status = "terminal"
                inventory = fixture.inventory
                reads_after_delete = 0

                def lose_final_readback(target):
                    nonlocal reads_after_delete
                    if fixture.snapshot.container is None:
                        reads_after_delete += 1
                        if reads_after_delete == failed_read:
                            raise TimeoutError("synthetic lost final readback")
                    return inventory(target)

                with patch.object(fixture, "inventory", side_effect=lose_final_readback):
                    with self.assertRaisesRegex(SweepError, f"^{error}$"):
                        fixture.finalize()
                self.assertEqual(reads_after_delete, failed_read)
                self.assertNotIn("registry_reservation", fixture.steps())
                self.assertNotIn("registry", fixture.steps())
                self.assertIs(fixture.finalize(), RegistryDisposition.ALREADY_ABSENT)
                self.assertEqual(fixture.steps().count("delete_container"), 1)

    def test_readiness_requires_reviewed_runtime_and_ready_gateway(self):
        fixture = Fixture()
        deployment = fixture.caller(Operation.START).start(Upload(fixture.target))
        read = fixture.caller(Operation.PROBE).read
        with self.assertRaisesRegex(TargetError, "^pre_build_stamps_unconfirmed$"):
            read(deployment, verified_receipt=None)
        with self.assertRaisesRegex(TargetError, "^pre_readiness_unconfirmed$"):
            read(deployment, verified_receipt={**RECEIPT, "BOT_BUILD_ID": "124-1"})
        fixture.gateway = [["database", "ready"]]
        with self.assertRaisesRegex(TargetError, "^pre_readiness_unconfirmed$"):
            read(deployment, verified_receipt=RECEIPT)
        self.assertTrue(fixture.caller(Operation.STOP).stop())

    def test_registry_handback_requires_settled_publisher_and_exact_fresh_absence(self):
        fixture = Fixture()
        fixture.caller(Operation.START).start(Upload(fixture.target))
        self.assertTrue(fixture.caller(Operation.STOP).stop())
        fixture.status = "terminal"
        fixture.publisher_settled = False
        with self.assertRaisesRegex(SweepError, "^PRE registry handback"):
            fixture.finalize()
        self.assertNotIn("registry", fixture.steps())

        fixture.publisher_settled = True
        fixture.registry_present = True
        self.assertIs(fixture.finalize(), RegistryDisposition.UNRESOLVED_DIGEST_REFERENCE)
        fixture.registry_present = False
        with patch.object(fixture, "read_registry", return_value=RegistryReadback(
            fixture.target, fixture.reservation.reference.replace("registry.invalid", "foreign.invalid"), False,
        )), self.assertRaisesRegex(SweepError, "^PRE registry handback"):
            fixture.finalize()
        self.assertIs(fixture.finalize(), RegistryDisposition.ALREADY_ABSENT)
        self.assertEqual(fixture.steps().count("delete_container"), 1)


    def test_registry_handback_requires_current_cleanup_and_publication_receipts(self):
        fixture = Fixture()
        fixture.now = fixture.target.expires_at
        self.assertTrue(fixture.caller(Operation.STOP).stop())
        prior_readback = fixture.cleanup_inventory(fixture.target, request=object())
        observe = fixture.cleanup_inventory

        def only_sweep_readback_is_current(target, *, request):
            if fixture.cleanup_inventory.call_count == 1:
                return observe(target, request=request)
            return prior_readback

        with patch.object(fixture, "cleanup_inventory", return_value=prior_readback):
            with self.assertRaisesRegex(SweepError, "^PRE cleanup readback could not be verified$"):
                fixture.finalize()
        with patch.object(fixture, "cleanup_inventory", side_effect=only_sweep_readback_is_current):
            with self.assertRaisesRegex(SweepError, "^PRE registry handback could not be verified$"):
                fixture.finalize()
        self.assertNotIn("registry_reservation", fixture.steps())

        prior_reservation = replace(fixture.reservation, target=replace(fixture.target, run_id=70))
        for settlement in (True, PublicationSettlement(prior_reservation)):
            with self.subTest(settlement=settlement):
                with patch.object(fixture, "publication_settlement", return_value=settlement):
                    with self.assertRaisesRegex(SweepError, "^PRE registry handback could not be verified$"):
                        fixture.finalize()
                self.assertNotIn("registry", fixture.steps())
        self.assertIs(fixture.finalize(), RegistryDisposition.ALREADY_ABSENT)
        self.assertEqual(fixture.steps().count("registry"), 1)

    def test_handback_requires_a_new_no_writer_read_and_independent_settlement(self):
        for target in (ORIGINAL, RETAINED):
            for case in ("exact", "unresolved digest", "replayed cleanup", "delegated writer",
                         "foreign settlement", "legacy true", "missing settlement"):
                with self.subTest(shape=target.shape, case=case):
                    fixture = Fixture(target)
                    fixture.caller(Operation.START).start(Upload(target))
                    self.assertTrue(fixture.caller(Operation.STOP).stop())
                    fixture.status = "terminal"
                    fixture.registry_present = case == "unresolved digest"
                    read_inventory = fixture.cleanup_inventory
                    requests = []
                    observations = []
                    sibling = replace(target, run_id=target.run_id + 100)
                    writers = {(sibling, sibling.service_name)}

                    def observe(requested, *, request):
                        self.assertEqual(requested, target)
                        requests.append(request)
                        current = read_inventory(requested, request=request)
                        if len(requests) == 2 and case == "replayed cleanup":
                            return observations[0]
                        if len(requests) == 2 and case == "delegated writer":
                            writers.add((target, "fixture-delegated-role"))
                        current = replace(current, inventory=replace(
                            current.inventory, write_access=any(database == requested for database, _ in writers),
                        ))
                        observations.append(current)
                        return current

                    settlement = PublicationSettlement(fixture.reservation)
                    if case == "foreign settlement":
                        settlement = PublicationSettlement(replace(fixture.reservation, target=sibling))
                    elif case == "legacy true":
                        settlement = True
                    elif case == "missing settlement":
                        settlement = None
                    registry = fixture.read_registry
                    with (patch.object(fixture, "cleanup_inventory", side_effect=observe),
                          patch.object(fixture, "publication_settlement", return_value=settlement) as verify_settlement,
                          patch.object(fixture, "read_registry", wraps=registry) as read_registry):
                        if case in ("exact", "unresolved digest"):
                            expected = (RegistryDisposition.ALREADY_ABSENT if case == "exact"
                                        else RegistryDisposition.UNRESOLVED_DIGEST_REFERENCE)
                            self.assertIs(fixture.finalize(), expected)
                            verify_settlement.assert_called_once_with(fixture.reservation)
                            read_registry.assert_called_once_with(fixture.reservation)
                        else:
                            with self.assertRaisesRegex(SweepError, "^PRE registry handback could not be verified$"):
                                fixture.finalize()
                            read_registry.assert_not_called()
                            if case in ("foreign settlement", "legacy true", "missing settlement"):
                                verify_settlement.assert_called_once_with(fixture.reservation)
                            else:
                                verify_settlement.assert_not_called()
                    self.assertEqual(len(requests), 2)
                    self.assertIsNot(requests[0], requests[1])
                    self.assertIn((sibling, sibling.service_name), writers)
