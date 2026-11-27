"""Synthetic CAS and provider evidence for the dormant PRE authority."""

import asyncio
from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch
from uuid import UUID

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_admission import AdmissionRecord, SettlementEvidence, SettlementSource, StartupAdmission
    from pre_start_stop import (Deployment, EffectWindow, ExternalStop, Operation, PrivateRuntime,
                                ProtectedPre, WorkerUpload)
    from pre_target import Inventory, Target, TargetError, verify_cleanup


TARGET = Target("pre-fixture-test", 123, 1_800_000_000, "0473/0465")


class Store:
    def __init__(self):
        self.now = TARGET.started_at
        self.records = {}
        self.before_swap = None
        self.lost_reply = None

    def read(self, target):
        return self.records.get(target)

    def compare_and_swap(self, target, expected, replacement, *, start_window):
        if self.before_swap is not None:
            hook, self.before_swap = self.before_swap, None
            hook()
        if self.read(target) != expected:
            return False
        if start_window is not None and not start_window[0] <= self.now < start_window[1]:
            return False
        self.records[target] = replacement
        if self.lost_reply is not None:
            error, self.lost_reply = self.lost_reply, None
            raise error
        return True


class StartupAdmissionTests(unittest.TestCase):
    def setUp(self):
        self.store = Store()
        self.observe = Mock(return_value=None)
        self.authority = StartupAdmission(self.store, self.observe)

    def test_racing_admissions_only_return_one_start_per_full_target(self):
        competitor = StartupAdmission(self.store, self.observe)
        winning = []
        self.store.before_swap = lambda: winning.append(competitor.admit_start(TARGET))
        self.assertIsNone(self.authority.admit_start(TARGET))
        self.assertIsInstance(winning[0], UUID)
        self.assertEqual(self.store.read(TARGET), AdmissionRecord(TARGET, winning[0], False))
        for neighbor in (replace(TARGET, scope="pre-fixture-other"), replace(TARGET, run_id=124),
                         replace(TARGET, started_at=TARGET.started_at - 1),
                         replace(TARGET, shape="0475/0465")):
            with self.subTest(neighbor=neighbor):
                self.assertIsInstance(competitor.admit_start(neighbor), UUID)
                self.assertIsNone(self.authority.admit_start(TARGET))

    def test_commit_clock_and_racing_close_fence_new_starts(self):
        self.store.now -= 1
        self.assertIsNone(self.authority.admit_start(TARGET))
        self.store.now = TARGET.expires_at - 1
        self.store.before_swap = lambda: setattr(self.store, "now", TARGET.expires_at)
        self.assertIsNone(self.authority.admit_start(TARGET))
        self.assertIsNone(self.store.read(TARGET))
        self.assertFalse(self.authority.settle(TARGET))

        other = replace(TARGET, run_id=124, started_at=TARGET.started_at + 1)
        self.store.before_swap = lambda: self.assertTrue(self.authority.close(other))
        self.assertIsNone(self.authority.admit_start(other))
        self.assertEqual(self.store.read(other), AdmissionRecord(other, None, True))
        self.assertIsNone(self.authority.admit_start(other))
        self.assertFalse(self.authority.settle(other))

    def test_stale_close_preserves_start_and_requires_fresh_cas(self):
        winning = []
        self.store.before_swap = lambda: winning.append(self.authority.admit_start(TARGET))
        self.assertFalse(self.authority.close(TARGET))
        self.assertFalse(self.authority.settle(TARGET))
        self.observe.assert_not_called()
        self.assertTrue(self.authority.close(TARGET))
        self.assertEqual(self.store.read(TARGET), AdmissionRecord(TARGET, winning[0], True))
        self.assertFalse(self.authority.startup_settled(TARGET))

    def test_lost_cas_reply_survives_restart_without_start_credit(self):
        self.store.lost_reply = TimeoutError("synthetic remote detail")
        with self.assertRaisesRegex(TargetError, "^pre_admission_write_unconfirmed$"):
            self.authority.admit_start(TARGET)
        restarted = StartupAdmission(self.store, self.observe)
        self.assertIsNone(restarted.admit_start(TARGET))
        self.assertTrue(restarted.close(TARGET))
        self.assertIsInstance(self.store.read(TARGET).start_id, UUID)
        self.assertFalse(restarted.settle(TARGET))

    def test_cancellation_after_commit_leaves_the_original_start_unresolved(self):
        self.store.lost_reply = asyncio.CancelledError()
        with self.assertRaises(asyncio.CancelledError):
            self.authority.admit_start(TARGET)
        pending = self.store.read(TARGET)
        self.assertIsInstance(pending.start_id, UUID)
        self.assertTrue(self.authority.close(TARGET))
        self.observe.side_effect = asyncio.CancelledError()
        with self.assertRaises(asyncio.CancelledError):
            self.authority.settle(TARGET)
        self.assertEqual(self.store.read(TARGET), replace(pending, closed=True))
        self.assertIsNone(self.authority.admit_start(TARGET))
        self.assertFalse(self.authority.startup_settled(TARGET))

    def test_cancelled_or_lost_provider_start_stays_unsettled(self):
        for offset, error in enumerate((asyncio.CancelledError(), TimeoutError())):
            with self.subTest(error=type(error)):
                target = replace(TARGET, run_id=TARGET.run_id + offset)
                start_id = self.authority.admit_start(target)
                self.assertIsInstance(start_id, UUID)
                dispatch = Mock(side_effect=error)
                with self.assertRaises(type(error)):
                    dispatch(target, start_id)
                self.assertIsNone(self.authority.admit_start(target))
                self.assertTrue(self.authority.close(target))
                self.assertEqual(self.store.read(target).start_id, start_id)
                for reply in (None, {"absent": True}, {"workflow": "completed"}, {"accepted": True}):
                    self.observe.return_value = reply
                    self.assertFalse(self.authority.settle(target))
                dispatch.assert_called_once_with(target, start_id)
        self.store.now = TARGET.expires_at
        inventory = Inventory(TARGET, None, None, 0, False, True,
                              startup_settled=self.authority.startup_settled(TARGET), assigned_count=0)
        with self.assertRaisesRegex(TargetError, "^pre_startup_unsettled$"):
            verify_cleanup(TARGET, inventory)

    def test_late_start_reply_preserves_close_and_cannot_settle_startup(self):
        runtime = Mock(spec_set=PrivateRuntime)
        external_stop = Mock(spec_set=ExternalStop)
        external_stop.arm.return_value = True
        external_stop.fence.return_value = True
        window = EffectWindow(TARGET, TARGET.expires_at)
        start = ProtectedPre(TARGET, authorize=lambda t, op: (t, op) == (TARGET, Operation.START),
                             admission=self.authority, runtime=runtime, external_stop=external_stop,
                             effect_window=window)
        stop = ProtectedPre(TARGET, authorize=lambda t, op: (t, op) == (TARGET, Operation.STOP),
                            admission=StartupAdmission(self.store, self.observe),
                            runtime=runtime, external_stop=external_stop)

        def reply_after_close(effect_window, start_id, _upload):
            self.assertEqual(effect_window, window)
            deployment = Deployment(effect_window.target, start_id, UUID(int=2), UUID(int=3))
            self.assertFalse(stop.stop())
            self.assertEqual(self.store.read(TARGET), AdmissionRecord(TARGET, start_id, closed=True))
            return deployment

        upload = Mock(spec_set=WorkerUpload, target=TARGET, supports_discord_fixture_overrides=True)
        runtime.create_private.side_effect = reply_after_close
        reply = start.start(upload)
        closed = AdmissionRecord(TARGET, reply.start_id, closed=True)
        self.observe.return_value = reply
        self.assertFalse(stop.stop())
        self.assertEqual(self.store.read(TARGET), closed)
        with self.assertRaisesRegex(TargetError, "^pre_start_not_admitted$"):
            start.start(upload)
        inventory = Inventory(TARGET, None, None, 0, False, True,
                              startup_settled=self.authority.startup_settled(TARGET), assigned_count=0)
        with self.assertRaisesRegex(TargetError, "^pre_startup_unsettled$"):
            verify_cleanup(TARGET, inventory)

        self.observe.return_value = SettlementEvidence(closed, SettlementSource.RUNTIME, True, True)
        self.assertTrue(stop.stop())
        self.assertEqual(self.store.read(TARGET), replace(closed, settled_by=SettlementSource.RUNTIME))
        with self.assertRaisesRegex(TargetError, "^pre_start_not_admitted$"):
            start.start(upload)
        verify_cleanup(TARGET, replace(inventory, startup_settled=self.authority.startup_settled(TARGET)))
        runtime.create_private.assert_called_once_with(window, reply.start_id, upload)

    def test_only_affirmative_exact_post_close_evidence_settles(self):
        self.authority.admit_start(TARGET)
        opened = self.store.read(TARGET)
        self.assertTrue(self.authority.close(TARGET))
        closed = self.store.read(TARGET)
        proof = SettlementEvidence(closed, SettlementSource.PROVIDER, True, True)
        wrong_target = replace(TARGET, shape="0475/0465")
        for reply in (replace(proof, closed=opened), replace(proof, closed=replace(closed, start_id=None)),
                      replace(proof, closed=replace(closed, start_id=UUID(int=1))),
                      replace(proof, closed=replace(closed, target=wrong_target)),
                      replace(proof, startup_drained=False), replace(proof, restart_fenced=1),
                      replace(proof, source="provider")):
            with self.subTest(reply=reply):
                self.observe.return_value = reply
                self.assertFalse(self.authority.settle(TARGET))
                self.assertFalse(self.authority.startup_settled(TARGET))

        self.observe.return_value = proof
        self.assertTrue(self.authority.settle(TARGET))
        self.observe.assert_called_with(closed)
        self.assertEqual(self.store.read(TARGET).settled_by, SettlementSource.PROVIDER)
        self.assertIsNone(self.authority.admit_start(TARGET))
        self.assertTrue(self.authority.close(TARGET))
        inventory = Inventory(TARGET, None, None, 0, False, True,
                              startup_settled=self.authority.startup_settled(TARGET), assigned_count=0)
        verify_cleanup(TARGET, inventory)

    def test_settlement_retry_uses_saved_proof_without_reobserving(self):
        self.assertTrue(self.authority.close(TARGET))
        closed = self.store.read(TARGET)
        self.observe.return_value = SettlementEvidence(closed, SettlementSource.PROVIDER, True, True)
        self.assertTrue(self.authority.settle(TARGET))
        restarted_observer = Mock(side_effect=TimeoutError("synthetic provider unavailable"))
        restarted = StartupAdmission(self.store, restarted_observer)
        with patch.object(self.store, "compare_and_swap", wraps=self.store.compare_and_swap) as swap:
            self.assertTrue(restarted.settle(TARGET))
            swap.assert_not_called()
        restarted_observer.assert_not_called()

    def test_settlement_race_cannot_overwrite_durable_terminal_proof(self):
        self.assertTrue(self.authority.close(TARGET))
        closed = self.store.read(TARGET)
        self.observe.return_value = SettlementEvidence(closed, SettlementSource.PROVIDER, True, True)
        runtime = SettlementEvidence(closed, SettlementSource.RUNTIME, True, True)
        competitor = StartupAdmission(self.store, Mock(return_value=runtime))
        self.store.before_swap = lambda: self.assertTrue(competitor.settle(TARGET))
        self.assertFalse(self.authority.settle(TARGET))
        self.assertEqual(self.store.read(TARGET).settled_by, SettlementSource.RUNTIME)
        self.assertTrue(self.authority.startup_settled(TARGET))

    def test_unconfirmed_store_or_observation_does_not_return_credit(self):
        with patch.object(self.store, "read", side_effect=OSError("synthetic remote detail")):
            with self.assertRaisesRegex(TargetError, "^pre_admission_read_unconfirmed$"):
                self.authority.startup_settled(TARGET)
        self.store.records[TARGET] = AdmissionRecord(replace(TARGET, run_id=124), None, True)
        with self.assertRaisesRegex(TargetError, "^invalid_pre_admission_record$"):
            self.authority.startup_settled(TARGET)
        with patch.object(self.store, "read", return_value=None), patch.object(
                self.store, "compare_and_swap", return_value=1):
            with self.assertRaisesRegex(TargetError, "^pre_admission_write_unconfirmed$"):
                self.authority.admit_start(TARGET)
        self.store.records[TARGET] = AdmissionRecord(TARGET, None, True)
        self.observe.side_effect = TimeoutError("synthetic provider detail")
        with self.assertRaisesRegex(TargetError, "^pre_startup_evidence_unconfirmed$"):
            self.authority.settle(TARGET)
        self.assertFalse(self.authority.startup_settled(TARGET))
