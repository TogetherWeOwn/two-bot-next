"""Offline checks for the replacement PRE ownership transition and readback."""

from dataclasses import replace
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, call, patch
from uuid import UUID

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_native_takeover import TakeoverAuthority, verify_native_takeover
    from pre_ownership import CONTROL_PATH, CONTROL_REPLY_CAP, OwnershipReply
    from pre_start_stop import Deployment, EffectWindow
    from pre_target import Target, TargetError


SHAPES = ("0473/0465", "0475/0465")
EPOCH = 7
ACTOR = "fixture:pre-recovery"
VERSION = str(UUID(int=2))
FOREIGN = str(UUID(int=4))


def reply(*, active=False, header="1", fields=None, owner_fields=None):
    owner = {
        "phase": "active" if active else "fenced",
        "deploymentId": VERSION if active else None,
        "epoch": EPOCH + int(active), "oldEpoch": EPOCH + int(active) - 1,
        "oldDeploymentId": None if active else FOREIGN,
        "actor": ACTOR if active else "fixture:original-fence",
        "timestamp": "2026-11-25T00:00:00.000Z",
    }
    owner.update(owner_fields or {})
    state = {"deploymentId": VERSION, "owner": owner, "running": active}
    state.update(fields or {})
    return OwnershipReply(200, json.dumps(state).encode(), header)


class Fixture:
    def __init__(self, shape=SHAPES[0], *replies):
        target = Target("pre-fixture-test", 123, 1800000000, shape)
        self.deployment = Deployment(target, UUID(int=1), UUID(int=2), UUID(int=3))
        self.window = EffectWindow(target, target.started_at + 5)
        self.clock = Mock(return_value=target.started_at)
        self.exchange = Mock(side_effect=replies)
        self.authority = Mock(spec_set=TakeoverAuthority)
        self.authority.confirm_current.return_value = True
        self.authority.claim_once.return_value = True
        self.get = call(self.deployment, "GET", CONTROL_PATH, None)
        self.post = call(self.deployment, "POST", CONTROL_PATH, {
            "action": "takeover-exact", "expectedDeploymentId": VERSION,
            "expectedEpoch": EPOCH, "actor": ACTOR,
        })

    def verify(self, **overrides):
        args = dict(deployment=self.deployment, window=self.window, expected_epoch=EPOCH,
                    actor=ACTOR, clock=self.clock, authority=self.authority, exchange=self.exchange)
        args.update(overrides)
        return verify_native_takeover(**args)


class NativeTakeoverTests(unittest.TestCase):
    def test_direct_acknowledgement_for_both_pre_shapes(self):
        for shape in SHAPES:
            with self.subTest(shape=shape):
                fixture = Fixture(shape, reply(), reply(active=True, header=None, fields={"running": False}))
                fixture.verify()
                self.assertEqual(fixture.exchange.call_args_list, [fixture.get, fixture.post])
                pins = (fixture.window, fixture.deployment, EPOCH, ACTOR)
                self.assertEqual(fixture.authority.method_calls, [
                    call.confirm_current(*pins), call.claim_once(*pins), call.confirm_current(*pins),
                ])

    def test_lost_reply_reconciles_with_a_running_readback_for_both_pre_shapes(self):
        for shape in SHAPES:
            with self.subTest(shape=shape):
                fixture = Fixture(shape, reply(), TimeoutError("synthetic lost reply"), reply(active=True))
                fixture.verify()
                self.assertEqual(fixture.exchange.call_args_list, [fixture.get, fixture.post, fixture.get])
                fixture.authority.claim_once.assert_called_once()

    def test_ambiguous_acknowledgement_uses_one_readback(self):
        ambiguous_replies = (
            None, OwnershipReply(503, b"{}"), OwnershipReply(408, b"{}"), OwnershipReply(200, b"{"),
            OwnershipReply(200, b" " * (CONTROL_REPLY_CAP + 1)),
            OwnershipReply(200, b'{"running":false,"running":true}'),
            reply(active=True, fields={"running": "true"}),
        )
        for ambiguous in ambiguous_replies:
            with self.subTest(ambiguous=ambiguous):
                fixture = Fixture(SHAPES[0], reply(), ambiguous, reply(active=True))
                fixture.verify()
                self.assertEqual(fixture.exchange.call_args_list, [fixture.get, fixture.post, fixture.get])

    def test_unresolved_reply_cannot_trigger_a_second_takeover_even_on_reentry(self):
        # The GET still sees the old fence, so an epoch check alone would permit a second POST.
        fixture = Fixture(SHAPES[0], reply(), TimeoutError("synthetic lost reply"), reply(), reply())
        fixture.authority.claim_once.side_effect = [True, False]
        with self.assertRaisesRegex(TargetError, "^pre_native_takeover_unconfirmed$"):
            fixture.verify()
        with self.assertRaisesRegex(TargetError, "^pre_native_takeover_origin_unconfirmed$"):
            fixture.verify()
        self.assertEqual(fixture.exchange.call_args_list, [fixture.get, fixture.post, fixture.get, fixture.get])
        self.assertEqual(fixture.authority.claim_once.call_count, 2)

    def test_preflight_refuses_unpinned_or_unstopped_fence(self):
        invalid_states = (
            reply(header=None), reply(header="1, 1"), reply(fields={"deploymentId": FOREIGN}),
            reply(fields={"running": True}), reply(fields={"owner": None}),
            reply(owner_fields={"phase": "active"}), reply(owner_fields={"deploymentId": VERSION}),
            reply(owner_fields={"epoch": EPOCH - 1}), reply(owner_fields={"oldEpoch": EPOCH}),
        )
        for state in invalid_states:
            with self.subTest(state=state):
                fixture = Fixture(SHAPES[0], state)
                with self.assertRaises(TargetError):
                    fixture.verify()
                fixture.exchange.assert_called_once_with(*fixture.get.args)
                fixture.authority.claim_once.assert_not_called()

    def test_readback_refuses_wrong_identity_epoch_actor_or_running_state(self):
        invalid_states = (
            reply(active=True, fields={"deploymentId": FOREIGN}),
            reply(active=True, owner_fields={"deploymentId": FOREIGN}),
            reply(active=True, owner_fields={"epoch": EPOCH}),
            reply(active=True, owner_fields={"epoch": EPOCH + 2}),
            reply(active=True, owner_fields={"epoch": float(EPOCH + 1)}),
            reply(active=True, owner_fields={"oldEpoch": EPOCH - 1}),
            reply(active=True, owner_fields={"oldDeploymentId": FOREIGN}),
            reply(active=True, owner_fields={"actor": "fixture:foreign"}),
            reply(active=True, owner_fields={"phase": "fenced"}),
            reply(active=True, fields={"running": False}), reply(active=True, fields={"running": 1}),
            reply(active=True, header=None), OwnershipReply(200, b"\xff"),
            OwnershipReply(200, b"[" * 17 + b"]" * 17), OSError("synthetic read failure"),
        )
        for state in invalid_states:
            with self.subTest(state=state):
                fixture = Fixture(SHAPES[1], reply(), TimeoutError(), state)
                with self.assertRaisesRegex(TargetError, "^pre_native_takeover_unconfirmed$"):
                    fixture.verify()
                self.assertEqual(fixture.exchange.call_args_list, [fixture.get, fixture.post, fixture.get])

    def test_post_refuses_explicit_conflict_or_wrong_pins(self):
        explicit_refusals = (
            OwnershipReply(409, b"{}"), OwnershipReply(302, b""),
            reply(active=True, fields={"deploymentId": FOREIGN}),
            reply(active=True, owner_fields={"deploymentId": FOREIGN}),
            reply(active=True, owner_fields={"epoch": EPOCH}),
            reply(active=True, owner_fields={"actor": "fixture:foreign"}),
        )
        for refused in explicit_refusals:
            with self.subTest(refused=refused):
                fixture = Fixture(SHAPES[0], reply(), refused, reply(active=True))
                with self.assertRaisesRegex(TargetError, "^pre_native_takeover_unconfirmed$"):
                    fixture.verify()
                self.assertEqual(fixture.exchange.call_args_list, [fixture.get, fixture.post])

    def test_stale_origin_refuses_before_preflight_or_post(self):
        fixture = Fixture(SHAPES[0], reply())
        fixture.authority.confirm_current.side_effect = OSError("synthetic origin data")
        with self.assertRaisesRegex(TargetError, "^pre_native_takeover_origin_unconfirmed$"):
            fixture.verify()
        fixture.exchange.assert_not_called()
        fixture.authority.claim_once.assert_not_called()

        fixture.authority.confirm_current.side_effect = None
        fixture.authority.claim_once.return_value = False
        with self.assertRaisesRegex(TargetError, "^pre_native_takeover_origin_unconfirmed$"):
            fixture.verify()
        fixture.exchange.assert_called_once_with(*fixture.get.args)

    def test_lost_reply_preserves_independent_stop_custody(self):
        # Custody can go stale either while the POST is outstanding or during the readback.
        for confirmations, reads in (([True, False], 0), ([True, True, False], 1)):
            with self.subTest(confirmations=confirmations):
                fixture = Fixture(SHAPES[0], reply(), TimeoutError(), reply(active=True))
                fixture.authority.confirm_current.side_effect = confirmations
                with self.assertRaisesRegex(TargetError, "^pre_native_takeover_origin_unconfirmed$"):
                    fixture.verify()
                self.assertEqual(fixture.exchange.call_args_list, [fixture.get, fixture.post] + [fixture.get] * reads)

    def test_effect_window_checked_before_claim_and_post_and_on_acceptance(self):
        for open_checks, call_count in ((0, 0), (1, 1), (2, 1), (3, 2)):
            with self.subTest(open_checks=open_checks):
                fixture = Fixture(SHAPES[0], reply(), reply(active=True))
                fixture.clock.side_effect = [fixture.window.cutoff - 1] * open_checks + [fixture.window.cutoff]
                with self.assertRaisesRegex(TargetError, "^pre_effect_window_expired$"):
                    fixture.verify()
                self.assertEqual(fixture.exchange.call_args_list, [fixture.get, fixture.post][:call_count])
                self.assertEqual(fixture.authority.claim_once.call_count, int(open_checks >= 2))

    def test_invalid_inputs_refuse_before_observation(self):
        fixture = Fixture()
        target = replace(fixture.deployment.target, shape=SHAPES[1])
        invalid_inputs = (
            {"window": EffectWindow(target, fixture.window.cutoff)},
            {"deployment": replace(fixture.deployment, worker_version=UUID(int=0))},
            {"expected_epoch": True}, {"expected_epoch": 0}, {"expected_epoch": (1 << 53) - 1},
            {"actor": ""}, {"actor": "a" * 129}, {"actor": "fixture\nactor"},
        )
        for overrides in invalid_inputs:
            with self.subTest(overrides=overrides):
                with self.assertRaisesRegex(TargetError, "^pre_native_takeover_input_invalid$"):
                    fixture.verify(**overrides)
        self.assertEqual(fixture.authority.mock_calls, [])
        fixture.exchange.assert_not_called()
