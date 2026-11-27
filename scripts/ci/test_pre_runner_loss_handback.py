"""Reconcile a stranded PRE reservation through the dormant stop and reaper."""

from asyncio import CancelledError
from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, call, patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from ci.test_pre_admission import Store
    from ci.test_pre_registry_cleanup import RESERVATION
    from ci.test_pre_start_stop import PRE_SHAPES, START, TARGET, PreFixture
    from ci.test_staging_pre_reaper_fixture import SyntheticStaging
    from pre_admission import AdmissionRecord, SettlementEvidence, SettlementSource, StartupAdmission
    from pre_registry_cleanup import RegistryDisposition, RegistryReadback, registry_cleanup_disposition
    from pre_start_stop import Operation, ProtectedPre
    from pre_sweep import SweepError, sweep_target
    from pre_target import Resource, TargetError


class ReaperFixture(SyntheticStaging):
    def __init__(self, deployment, admission):
        super().__init__(deployment.target)
        self.admission = admission
        self.status = "active"  # The workflow status can outlive the runner.
        self.snapshot = replace(self.snapshot, startup_settled=False,
                                container=Resource(deployment.target.container_name, str(deployment.container_id)))

    def inventory(self, target):
        return replace(super().inventory(target), startup_settled=self.admission.startup_settled(target))

    def stop_container(self, target, action):
        return replace(super().stop_container(target, action),
                       startup_settled=self.admission.startup_settled(target))


class PreRunnerLossHandbackTests(unittest.TestCase):
    def test_expired_owner_requires_independent_stop_and_reaper_before_handback(self):
        for shape in PRE_SHAPES:
            with self.subTest(shape=shape):
                self.check_handback(replace(TARGET, shape=shape))

    def check_handback(self, target):
        runtime = PreFixture(target)
        store = Store()
        start_observer = Mock(return_value=None)
        starter = ProtectedPre(target, authorize=lambda t, op: (t, op) == (target, Operation.START)
                               and store.now < target.expires_at,
                               admission=StartupAdmission(store, start_observer), runtime=runtime,
                               external_stop=runtime, effect_window=runtime.window)
        runtime.after_create.side_effect = CancelledError()
        with patch("pre_admission.uuid4", return_value=START), self.assertRaises(CancelledError):
            starter.start(runtime.upload)
        opened = AdmissionRecord(target, START, closed=False)
        self.assertEqual(store.read(target), opened)
        self.assertEqual(runtime.created, runtime.deployment)
        self.assertEqual(runtime.effects.mock_calls, [call.arm(runtime.window, START),
                                                     call.create(target, START, runtime.upload)])
        start_observer.assert_not_called()

        observe = Mock(return_value=None)
        admission = StartupAdmission(store, observe)
        reaper = ReaperFixture(runtime.created, admission)
        app = reaper.snapshot.container
        stop = ProtectedPre(target, authorize=lambda t, op: (t, op) == (target, Operation.STOP)
                            and store.now >= target.expires_at, admission=admission, runtime=Mock(),
                            external_stop=runtime)
        reservation = replace(RESERVATION, target=target)
        manifests = set()
        publication_settled = False

        def handback():
            inventory = reaper.inventory(target)
            readback = RegistryReadback(target, reservation.reference, reservation.reference in manifests)
            return registry_cleanup_disposition(
                reservation, inventory, readback, run_status=reaper.run_status(target),
                now=store.now, publication_settled=publication_settled,
            )

        store.now = target.expires_at - 1
        self.assertEqual(sweep_target(target, reaper, now=store.now), "spared")
        self.assertEqual(reaper.events, [("status", target), ("inventory", target)])
        self.assertIsNone(handback())

        manifests.add(reservation.reference)
        publication_settled = True
        store.now = target.expires_at
        reaper.events.clear()
        # The runtime fixture's clock stays in-window so only the fence can deny effects.
        self.assertTrue(runtime.external_stop.allows_effect(target, START))
        self.assertFalse(stop.stop())
        closed = replace(opened, closed=True)
        self.assertEqual(store.read(target), closed)
        self.assertFalse(runtime.external_stop.allows_effect(target, START))
        observe.assert_called_once_with(closed)
        with self.assertRaisesRegex(SweepError, "^PRE container stop could not be verified$"):
            sweep_target(target, reaper, now=store.now)
        self.assertEqual(reaper.events, [
            ("status", target), ("inventory", target),
            ("revoke_write_access", target, target.service_name, target.service_name),
            ("delete_worker", target, target.worker_name, target.worker_name),
            ("stop_container", target, app.name, app.id),
        ])
        self.assertEqual(reaper.snapshot.container, app)
        with self.assertRaisesRegex(TargetError, "^pre_startup_unsettled$"):
            handback()

        observe.return_value = SettlementEvidence(closed, SettlementSource.PROVIDER, True, True)
        self.assertTrue(stop.stop())
        self.assertEqual(observe.call_args_list, [call(closed), call(closed)])
        self.assertEqual(store.read(target), replace(closed, settled_by=SettlementSource.PROVIDER))
        with self.assertRaisesRegex(TargetError, "^pre_cleanup_unconfirmed$"):
            handback()

        reaper.events.clear()
        self.assertEqual(sweep_target(target, reaper, now=store.now), "cleared")
        self.assertEqual(reaper.events, [
            ("status", target), ("inventory", target),
            ("stop_container", target, app.name, app.id), ("delete_container", target, app.name, app.id),
            ("inventory", target),
        ])
        self.assertIs(handback(), RegistryDisposition.UNRESOLVED_DIGEST_REFERENCE)
        manifests.remove(reservation.reference)
        self.assertIs(handback(), RegistryDisposition.ALREADY_ABSENT)
