"""Exercise dormant PRE orchestration with synthetic authorities and runtime."""

from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, replace
from pathlib import Path
import sys
from threading import Barrier
from types import SimpleNamespace
import unittest
from unittest.mock import MagicMock, Mock, call, patch
from uuid import UUID

from test_pre_admission import Store

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_admission import AdmissionRecord, SettlementEvidence, SettlementSource, StartupAdmission
    from pre_start_stop import (Deployment, EffectWindow, Endpoint, OfflineExternalStop, Operation,
                                ProbeReply, ProtectedPre)
    from pre_start_stop import StartupAdmission as AdmissionProtocol
    from pre_target import Inventory, Target, TargetError, verify_cleanup


PRE_SHAPES = ("0473/0465", "0475/0465")
TARGET = Target("pre-fixture-test", 123, 1_800_000_000, PRE_SHAPES[0])
START = UUID(int=1)


def cleared(target):
    return Inventory(target, None, None, 0, False, True, startup_settled=True, assigned_count=0)


@dataclass(frozen=True)
class Upload:
    target: Target
    content_type: str = "multipart/form-data; boundary=fixture"
    body: bytes = b"fixture"
    supports_discord_fixture_overrides: bool = True


class PreFixture:
    def __init__(self, target=TARGET, *, external_stop=None):
        self.target = target
        self.window = EffectWindow(target, target.expires_at)
        self.deployment = Deployment(target, START, UUID(int=2), UUID(int=3))
        self.upload = Upload(target)
        self.authorize = Mock(return_value=True)
        self.admission = Mock(spec_set=AdmissionProtocol)
        self.admission.admit_start.side_effect = [START, None]
        self.admission.close.return_value = True
        self.admission.settle.return_value = True
        self.effects = Mock(admission=self.admission)
        self.now = target.started_at
        self.enclosing_cutoff = target.expires_at
        self.external_stop = external_stop if external_stop is not None else OfflineExternalStop(lambda: self.now)
        self.armed = None
        self.fenced = False
        self.created = None
        self.before_create = Mock()
        self.after_create = Mock()
        self.before_probe_reply = Mock()
        self.reply = ProbeReply(200, self.deployment.worker_version, b"{}")

    def controller(self, authorize):
        return ProtectedPre(self.target, authorize=authorize, admission=self.admission,
                            runtime=self, external_stop=self, effect_window=self.window)

    def arm(self, window, start_id):
        self.effects.arm(window, start_id)
        armed = self.external_stop.arm(window, start_id)
        if armed:
            self.armed = (window, start_id)
        return armed

    def fence(self, target):
        self.effects.fence(target)
        confirmed = self.external_stop.fence(target)
        if confirmed and target == self.target:
            self.fenced = True
        return confirmed

    def confirm_probe_active(self, target, start_id):
        return self.external_stop.confirm_probe_active(target, start_id) and self.now < self.enclosing_cutoff

    def allow_effect(self, target, start_id):
        if not self.external_stop.allows_effect(target, start_id):
            raise OSError("effect refused")

    def create_private(self, window, start_id, upload):
        self.before_create()
        if self.armed != (window, start_id):
            raise OSError("unbound create")
        target = window.target
        self.allow_effect(target, start_id)
        self.effects.create(target, start_id, upload)
        self.created = replace(self.deployment, start_id=start_id)
        self.after_create()
        return self.created

    def probe(self, deployment, endpoint):
        self.allow_effect(deployment.target, deployment.start_id)
        if deployment != self.created:
            raise OSError("unbound deployment")
        self.effects.probe(deployment, endpoint)
        reply = self.reply
        self.before_probe_reply()
        return reply


class ProtectedPreTests(unittest.TestCase):
    def setUp(self):
        self.fixture = PreFixture()
        self.controller = self.fixture.controller(self.fixture.authorize)

    def assert_refuses(self, code, operation, *args):
        with self.assertRaisesRegex(TargetError, rf"^{code}$"):
            operation(*args)

    def test_refuses_other_reservations_and_unauthorized_operations_before_effects(self):
        fixture = self.fixture
        fixture.authorize.return_value = False
        self.assert_refuses("pre_operation_denied", self.controller.start, fixture.upload)
        self.assert_refuses("pre_operation_denied", self.controller.probe, fixture.deployment, Endpoint.HEALTH)
        self.assert_refuses("pre_operation_denied", self.controller.stop)
        fixture.authorize.return_value = True
        foreign = replace(TARGET, shape=PRE_SHAPES[1])  # The Worker name and deadline are the same.
        self.assert_refuses("wrong_pre_upload_target", self.controller.start, Upload(foreign))
        self.assert_refuses("pre_deployment_unconfirmed", self.controller.probe,
                            replace(fixture.deployment, target=foreign), Endpoint.HEALTH)
        self.assert_refuses("invalid_pre_probe", self.controller.probe, fixture.deployment, "/health")
        self.assertEqual(fixture.effects.mock_calls, [])

    def test_unarmed_deadline_refuses_create(self):
        self.fixture.now = TARGET.expires_at
        self.assert_refuses("pre_external_stop_unarmed", self.controller.start, self.fixture.upload)
        self.assertEqual(self.fixture.effects.mock_calls,
                         [call.admission.admit_start(TARGET), call.arm(self.fixture.window, START)])

    def test_arm_at_enclosing_lease_cutoff_refuses_create(self):
        fixture = self.fixture
        fixture.now = TARGET.started_at + 60
        fixture.window = EffectWindow(TARGET, fixture.now)
        controller = fixture.controller(fixture.authorize)
        self.assert_refuses("pre_external_stop_unarmed", controller.start, fixture.upload)
        self.assertLess(fixture.now, TARGET.expires_at)
        self.assertIsNone(fixture.armed)
        fixture.before_create.assert_not_called()
        self.assertEqual(fixture.effects.mock_calls,
                         [call.admission.admit_start(TARGET), call.arm(fixture.window, START)])

    def test_claim_lookup_and_insert_share_one_critical_section(self):
        authority = self.fixture.external_stop
        events = Mock(lock=MagicMock(), claims=MagicMock(spec_set=dict), clock=Mock(return_value=TARGET.started_at))
        window = self.fixture.window
        events.claims.get.return_value = None
        with (patch.object(authority, "_lock", events.lock), patch.object(authority, "_claims", events.claims),
              patch.object(authority, "_clock", events.clock)):
            self.assertTrue(authority.arm(window, START))
        self.assertEqual(events.mock_calls, [call.lock.__enter__(), call.clock(),
                         call.claims.get(TARGET.provider_names),
                         call.claims.__setitem__(TARGET.provider_names, (window, START)),
                         call.lock.__exit__(None, None, None)])

    def test_colliding_scope_or_shape_cannot_create_fence_or_release_the_winner(self):
        neighbors = (replace(TARGET, scope="pre-fixture-neighbor"), replace(TARGET, shape=PRE_SHAPES[1]))
        for neighbor in neighbors:
            with self.subTest(neighbor=neighbor):
                self.assertEqual(neighbor.provider_names, TARGET.provider_names)
                authority = OfflineExternalStop(lambda: TARGET.started_at)
                rendezvous = Barrier(2, timeout=5)
                fixtures = [PreFixture(target, external_stop=authority) for target in (TARGET, neighbor)]
                with ThreadPoolExecutor(max_workers=2) as runners:
                    starts = []
                    for fixture in fixtures:
                        fixture.effects.arm.side_effect = lambda *_: rendezvous.wait()
                        controller = fixture.controller(fixture.authorize)
                        starts.append(runners.submit(controller.start, fixture.upload))
                    outcomes = [(fixture, start.exception()) for fixture, start in zip(fixtures, starts, strict=True)]
                winners = [fixture for fixture, error in outcomes if error is None]
                self.assertEqual(len(winners), 1)
                winner = winners[0]
                loser, refusal = next((fixture, error) for fixture, error in outcomes if fixture is not winner)
                self.assertIsInstance(refusal, TargetError)
                self.assertEqual(str(refusal), "pre_external_stop_unarmed")
                loser.effects.create.assert_not_called()
                loser_stop = loser.controller(lambda t, op: (t, op) == (loser.target, Operation.STOP))
                self.assertFalse(loser_stop.stop())
                loser.admission.close.assert_called_once_with(loser.target)
                loser.admission.settle.assert_not_called()
                self.assertFalse(authority.release(loser.target, START, inventory=cleared(loser.target)))
                self.assertFalse(authority.allows_effect(loser.target, START))
                winner_probe = winner.controller(winner.authorize)
                self.assertEqual(winner_probe.probe(winner.deployment, Endpoint.HEALTH), winner.reply)

    def test_release_requires_exact_fenced_claim_and_target_cleanup(self):
        authority = self.fixture.external_stop
        self.controller.start(self.fixture.upload)
        self.assertFalse(authority.arm(self.fixture.window, START))
        self.assertFalse(authority.arm(self.fixture.window, UUID(int=4)))
        shortened = replace(self.fixture.window, cutoff=TARGET.started_at + 1)
        self.assertFalse(authority.arm(shortened, START))
        self.fixture.now = shortened.cutoff
        self.assertTrue(authority.allows_effect(TARGET, START))
        self.assertFalse(authority.allows_effect(TARGET, UUID(int=4)))
        self.assertFalse(authority.release(TARGET, START, inventory=cleared(TARGET)))
        self.assertTrue(self.controller.stop())
        self.assertTrue(self.controller.stop())
        self.assertFalse(authority.allows_effect(TARGET, START))
        foreign = replace(TARGET, shape=PRE_SHAPES[1])
        self.assertFalse(authority.arm(EffectWindow(foreign, foreign.expires_at), START))
        self.assertFalse(authority.fence(foreign))
        self.assertFalse(authority.release(foreign, START, inventory=cleared(foreign)))
        self.assertFalse(authority.release(TARGET, UUID(int=4), inventory=cleared(TARGET)))
        self.assert_refuses("pre_startup_unsettled", lambda: authority.release(
            TARGET, START, inventory=replace(cleared(TARGET), startup_settled=False)))
        successor = replace(TARGET, scope="pre-fixture-successor")
        self.assert_refuses("wrong_pre_inventory_target", lambda: authority.release(
            TARGET, START, inventory=cleared(successor)))
        self.assertTrue(authority.release(TARGET, START, inventory=cleared(TARGET)))
        self.assertFalse(authority.arm(self.fixture.window, START))
        self.assertFalse(authority.arm(EffectWindow(foreign, foreign.expires_at), START))
        self.assertTrue(authority.arm(EffectWindow(successor, successor.expires_at), START))
        self.assertFalse(authority.fence(TARGET))
        self.assertFalse(authority.release(TARGET, START, inventory=cleared(TARGET)))
        self.assertTrue(authority.allows_effect(successor, START))

    def test_pre_arm_fence_is_exact_and_distinct_names_can_be_claimed(self):
        authority = self.fixture.external_stop
        foreign = replace(TARGET, shape=PRE_SHAPES[1])
        distinct = replace(TARGET, run_id=TARGET.run_id + 1)
        self.assertFalse(authority.fence(foreign))
        self.assertFalse(authority.arm(EffectWindow(foreign, foreign.expires_at), START))
        self.assertEqual(self.controller.start(self.fixture.upload), self.fixture.deployment)
        self.assertTrue(authority.arm(EffectWindow(distinct, distinct.expires_at), START))
        self.assertTrue(authority.allows_effect(TARGET, START))
        self.assertTrue(authority.allows_effect(distinct, START))

    def test_missing_or_unconfirmed_discord_overrides_refuse_before_admission(self):
        for capability in ({}, {"supports_discord_fixture_overrides": False}, {"supports_discord_fixture_overrides": 1}):
            with self.subTest(capability=capability):
                fixture = PreFixture()
                upload = SimpleNamespace(target=TARGET, **capability)
                self.assert_refuses("pre_discord_fixture_overrides_unavailable",
                                    fixture.controller(fixture.authorize).start, upload)
                self.assertEqual(fixture.effects.mock_calls, [])

    def test_delayed_create_refused_when_stop_races_after_arm(self):
        fixture = self.fixture
        fixture.before_create.side_effect = self.controller.stop
        self.assert_refuses("pre_create_unconfirmed", self.controller.start, fixture.upload)
        self.assertEqual(fixture.effects.mock_calls, [call.admission.admit_start(TARGET), call.arm(fixture.window, START),
                         call.fence(TARGET), call.admission.close(TARGET), call.admission.settle(TARGET)])
        self.assertIsNone(fixture.created)

    def test_refuses_missing_foreign_or_invalid_effect_window(self):
        fixture = self.fixture
        foreign = replace(TARGET, shape=PRE_SHAPES[1])
        for window in (None, EffectWindow(foreign, fixture.window.cutoff)):
            with self.subTest(window=window):
                fixture.window = window
                self.assert_refuses("wrong_pre_effect_window", fixture.controller(fixture.authorize).start,
                                    fixture.upload)
        self.assertEqual(fixture.effects.mock_calls, [])
        for cutoff in (None, True, float(TARGET.expires_at), TARGET.started_at, TARGET.expires_at + 1):
            with self.subTest(cutoff=cutoff):
                self.assert_refuses("invalid_pre_effect_window", EffectWindow, TARGET, cutoff)

    def test_delayed_create_respects_lease_cutoff_and_original_stop_settles(self):
        cutoff = TARGET.started_at + 60
        for delay in (0, 1):
            with self.subTest(delay=delay):
                fixture = PreFixture()
                fixture.window = EffectWindow(TARGET, cutoff)
                store = Store()
                pending_observation = Mock(return_value=None)
                fixture.admission = StartupAdmission(store, pending_observation)
                fixture.before_create.side_effect = lambda: setattr(fixture, "now", cutoff + delay)
                start = fixture.controller(lambda t, op: (t, op) == (TARGET, Operation.START))
                self.assert_refuses("pre_create_unconfirmed", start.start, fixture.upload)
                fixture.before_create.assert_called_once_with()
                self.assertLess(fixture.now, TARGET.expires_at)
                self.assertFalse(fixture.fenced)
                fixture.effects.create.assert_not_called()
                self.assertIsNone(fixture.created)

                window, start_id = fixture.armed
                self.assertEqual(window, EffectWindow(TARGET, cutoff))
                self.assertEqual(store.read(TARGET), AdmissionRecord(TARGET, start_id, False))
                self.assert_refuses("pre_start_not_admitted", start.start, fixture.upload)
                self.assert_refuses("pre_operation_denied", start.stop)
                pending_observation.assert_not_called()

                original_closed = AdmissionRecord(TARGET, start_id, True)
                proof = Mock(return_value=None)
                admission = StartupAdmission(store, proof)
                stop = ProtectedPre(TARGET, authorize=lambda t, op: (t, op) == (TARGET, Operation.STOP),
                                    admission=admission, runtime=fixture, external_stop=fixture)
                self.assertFalse(stop.stop())
                fixture.effects.fence.assert_called_once_with(TARGET)
                self.assertEqual(store.read(TARGET), original_closed)
                proof.assert_called_once_with(original_closed)
                self.assertFalse(admission.startup_settled(TARGET))
                proof.return_value = SettlementEvidence(original_closed, SettlementSource.PROVIDER, True, True)
                self.assertTrue(stop.stop())
                self.assertEqual(store.read(TARGET), replace(original_closed, settled_by=SettlementSource.PROVIDER))

    def test_create_before_lease_cutoff_succeeds_and_probe_at_cutoff_is_refused(self):
        fixture = self.fixture
        cutoff = TARGET.started_at + 60
        fixture.window = EffectWindow(TARGET, cutoff)
        fixture.before_create.side_effect = lambda: setattr(fixture, "now", cutoff - 1)
        controller = fixture.controller(fixture.authorize)
        deployment = controller.start(fixture.upload)
        fixture.effects.create.assert_called_once_with(TARGET, START, fixture.upload)
        self.assertEqual(controller.probe(deployment, Endpoint.HEALTH), fixture.reply)
        fixture.now = cutoff
        self.assert_refuses("pre_probe_unconfirmed", controller.probe, deployment, Endpoint.HEALTH)
        fixture.effects.probe.assert_called_once_with(deployment, Endpoint.HEALTH)

    def test_stop_refuses_settlement_when_fence_or_close_returns_false(self):
        for fenced, closed in ((False, True), (True, False)):
            with self.subTest(fenced=fenced, closed=closed):
                fixture = PreFixture()
                fixture.fence = Mock(return_value=fenced)
                fixture.admission.close.return_value = closed
                self.assertFalse(fixture.controller(fixture.authorize).stop())
                fixture.fence.assert_called_once_with(TARGET)
                fixture.admission.close.assert_called_once_with(TARGET)
                fixture.admission.settle.assert_not_called()

    def test_stop_closes_admission_and_redacts_uncertain_errors(self):
        fixture = self.fixture
        fixture.fence = Mock(side_effect=OSError("provider detail must stay private"))
        self.assert_refuses("pre_external_stop_unconfirmed", self.controller.stop)
        fixture.admission.close.assert_called_once_with(TARGET)
        fixture.admission.close.side_effect = OSError("store detail must stay private")
        with self.assertRaisesRegex(TargetError, "^pre_external_stop_and_close_unconfirmed$") as caught:
            self.controller.stop()
        self.assertIsInstance(caught.exception.__context__, TargetError)
        self.assertIsNone(caught.exception.__context__.__context__)
        fixture.admission.settle.assert_not_called()

    def test_rejects_wrong_create_operation(self):
        wrong_start = replace(self.fixture.deployment, start_id=UUID(int=4))
        with patch.object(self.fixture, "create_private", return_value=wrong_start):
            self.assert_refuses("pre_deployment_unconfirmed", self.controller.start, self.fixture.upload)

    def test_rejects_forged_deployment_or_wrong_probe_version(self):
        fixture = self.fixture
        deployment = self.controller.start(fixture.upload)
        forged = replace(deployment, container_id=UUID(int=5))
        self.assert_refuses("pre_probe_unconfirmed", self.controller.probe, forged, Endpoint.READINESS)
        fixture.effects.probe.assert_not_called()
        fixture.reply = replace(fixture.reply, worker_version=UUID(int=5))
        self.assert_refuses("pre_probe_unconfirmed", self.controller.probe, deployment, Endpoint.READINESS)

    def test_probe_acceptance_requires_active_reservation_at_reply(self):
        for outcome in ("timely", "stopped", "expired", "enclosing_cutoff"):
            with self.subTest(outcome=outcome):
                fixture = PreFixture()
                controller = fixture.controller(lambda t, op: t == TARGET and op in (Operation.START, Operation.PROBE))
                stop = fixture.controller(lambda t, op: (t, op) == (TARGET, Operation.STOP))
                deployment = controller.start(fixture.upload)
                if outcome == "enclosing_cutoff":
                    fixture.enclosing_cutoff = TARGET.expires_at - 5
                    fixture.window = EffectWindow(TARGET, fixture.enclosing_cutoff)
                fixture.now = fixture.enclosing_cutoff - 1
                fixture.reply = ProbeReply(200, deployment.worker_version, b'{"components":[["gateway","ready"]]}')
                stop_results = []

                def before_reply():
                    if outcome == "stopped":
                        stop_results.append(stop.stop())
                    elif outcome in ("expired", "enclosing_cutoff"):
                        fixture.now = fixture.enclosing_cutoff

                fixture.before_probe_reply.side_effect = before_reply
                if outcome == "timely":
                    self.assertIs(controller.probe(deployment, Endpoint.READINESS), fixture.reply)
                else:
                    self.assert_refuses("pre_probe_unconfirmed", controller.probe, deployment, Endpoint.READINESS)
                fixture.effects.probe.assert_called_once_with(deployment, Endpoint.READINESS)
                self.assertEqual(stop_results, [True] if outcome == "stopped" else [])

    def test_probe_acceptance_refuses_unconfirmed_authority(self):
        fixture = self.fixture
        deployment = self.controller.start(fixture.upload)
        responses = (None, 1, OSError("private authority detail"))
        with patch.object(fixture, "confirm_probe_active", side_effect=responses) as confirm:
            for response in responses:
                with self.subTest(response=response):
                    self.assert_refuses("pre_probe_unconfirmed", self.controller.probe, deployment, Endpoint.HEALTH)
        self.assertEqual(confirm.call_args_list, [call(TARGET, deployment.start_id)] * len(responses))

    def test_rehearsal_probe_sequence_for_both_pre_snapshots(self):
        probes = ((Endpoint.HEALTH, 200, b'{"status":"ok"}'),
                  (Endpoint.READINESS, 503, b'{"components":[["gateway","starting"]]}'),
                  (Endpoint.READINESS, 200, b'{"components":[["gateway","ready"]]}'))
        for shape in PRE_SHAPES:
            with self.subTest(shape=shape):
                target = replace(TARGET, shape=shape)
                fixture = PreFixture(target)
                controller = fixture.controller(fixture.authorize)
                deployment = controller.start(fixture.upload)
                self.assertEqual(deployment, fixture.deployment)
                for endpoint, status, body in probes:
                    fixture.reply = ProbeReply(status, deployment.worker_version, body)
                    self.assertEqual(controller.probe(deployment, endpoint), fixture.reply)
                fixture.now = target.expires_at
                self.assert_refuses("pre_probe_unconfirmed", controller.probe, deployment, Endpoint.READINESS)
                self.assertEqual(fixture.effects.mock_calls, [call.admission.admit_start(target),
                                 call.arm(fixture.window, START), call.create(target, START, fixture.upload),
                                 *(call.probe(deployment, endpoint) for endpoint, _, _ in probes)])
                probe_permissions = [call(target, Operation.PROBE)] * (len(probes) + 1)
                self.assertEqual(fixture.authorize.call_args_list, [call(target, Operation.START), *probe_permissions])

    def test_resumed_stop_preserves_admission_and_requires_its_exact_start_id(self):
        for proof_start_id, settled in ((START, True), (UUID(int=4), False)):
            with self.subTest(proof_start_id=proof_start_id):
                fixture = PreFixture()
                store = Store()
                start_observer = Mock(return_value=None)
                start = ProtectedPre(TARGET, authorize=lambda t, op: (t, op) == (TARGET, Operation.START),
                                     admission=StartupAdmission(store, start_observer),
                                     runtime=fixture, external_stop=fixture, effect_window=fixture.window)
                with patch("pre_admission.uuid4", return_value=START):
                    deployment = start.start(fixture.upload)
                self.assertEqual(deployment, fixture.deployment)
                self.assertEqual(store.read(TARGET), AdmissionRecord(TARGET, deployment.start_id, closed=False))
                self.assertFalse(fixture.fenced)
                start_observer.assert_not_called()

                proof_record = AdmissionRecord(deployment.target, proof_start_id, closed=True)
                observe = Mock(side_effect=lambda _: SettlementEvidence(
                    proof_record, SettlementSource.RUNTIME, startup_drained=True, restart_fenced=fixture.fenced))
                fixture.effects.attach_mock(observe, "observe")
                resumed_admission = StartupAdmission(store, observe)
                stop = ProtectedPre(TARGET, authorize=lambda t, op: (t, op) == (TARGET, Operation.STOP),
                                    admission=resumed_admission, runtime=fixture, external_stop=fixture)
                self.assertIs(stop.stop(), settled)

                closed = AdmissionRecord(TARGET, deployment.start_id, closed=True)
                self.assertEqual(fixture.effects.mock_calls, [call.arm(fixture.window, START),
                                 call.create(TARGET, START, fixture.upload), call.fence(TARGET), call.observe(closed)])
                self.assertEqual(store.records, {
                    TARGET: replace(closed, settled_by=SettlementSource.RUNTIME if settled else None),
                })
                self.assertIs(resumed_admission.startup_settled(TARGET), settled)

    def test_lost_start_reply_requires_affirmative_exact_stop_evidence(self):
        cases = (
            ("active", 1, 0, True, False), ("assigned", 0, 1, True, False),
            ("app-absent", None, None, True, False), ("restart-unfenced", 0, 0, False, False),
            ("foreign-target", 0, 0, True, False), ("drained", 0, 0, True, True),
        )
        for shape in PRE_SHAPES:
            for name, active, assigned, restart_fenced, expected in cases:
                with self.subTest(shape=shape, observation=name):
                    target = replace(TARGET, shape=shape)
                    observed = replace(target, scope="pre-fixture-foreign") if name == "foreign-target" else target
                    self.assert_uncertain_start(target, observed, active, assigned, restart_fenced, expected)

    def assert_uncertain_start(self, target, observed, active, assigned, restart_fenced, expected):
        fixture = PreFixture(target)
        start = fixture.controller(lambda t, op: (t, op) == (target, Operation.START))
        stop = fixture.controller(lambda t, op: (t, op) == (target, Operation.STOP))
        fixture.after_create.side_effect = TimeoutError("synthetic create reply lost")
        fixture.admission.settle.side_effect = lambda closed: (
            restart_fenced and observed == closed and active == assigned == 0)
        self.assert_refuses("pre_create_unconfirmed", start.start, fixture.upload)
        self.assertEqual(fixture.created, fixture.deployment)
        self.assert_refuses("pre_start_not_admitted", start.start, fixture.upload)
        self.assert_refuses("pre_operation_denied", start.stop)
        self.assertIs(stop.stop(), expected)
        self.assertEqual(fixture.effects.mock_calls, [call.admission.admit_start(target), call.arm(fixture.window, START),
                         call.create(target, START, fixture.upload), call.admission.admit_start(target),
                         call.fence(target), call.admission.close(target), call.admission.settle(target)])
        probe = fixture.controller(lambda t, op: (t, op) == (target, Operation.PROBE))
        self.assert_refuses("pre_probe_unconfirmed", probe.probe, fixture.deployment, Endpoint.HEALTH)
        if expected:
            absent = Inventory(target, None, None, active, write_access=False, complete=True,
                               startup_settled=True, assigned_count=assigned)
            self.assert_refuses("pre_cleanup_unconfirmed", verify_cleanup, target,
                                replace(absent, instance_count=None, assigned_count=None))
            verify_cleanup(target, absent)
        else:
            fixture.admission.settle.side_effect = None
            self.assertTrue(stop.stop())
            self.assertEqual(fixture.admission.settle.call_args_list, [call(target), call(target)])
