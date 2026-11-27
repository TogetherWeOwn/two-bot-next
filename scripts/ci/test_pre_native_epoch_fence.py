from dataclasses import FrozenInstanceError, replace
from pathlib import Path
import sys
import unittest
from uuid import UUID

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from pre_native_epoch_fence import (
    EpochFenceVerdict, FenceTransition, StaleWriteProbe, verify_native_pre_epoch_fence,
)
from pre_start_stop import Deployment, EffectWindow
from pre_target import Target, TargetError


class TextAlias(str):
    def __eq__(self, other):
        raise AssertionError("comparison with a string subclass")


class FenceFixture:
    def __init__(self, shape="0473/0465"):
        self.target = Target("pre-fixture-epoch", 42, 100, shape)
        self.window = EffectWindow(self.target, 200)
        self.start_id = UUID(int=1)
        self.original = Deployment(self.target, self.start_id, UUID(int=2), UUID(int=3))
        self.replacement = Deployment(self.target, self.start_id, UUID(int=4), UUID(int=5))
        self.create = FenceTransition("original_create", self.window, self.original, 7)
        self.takeover = FenceTransition("replacement_takeover", self.window, self.replacement, 8)
        self.stop = FenceTransition("original_stop", self.window, self.original, 9)
        self.probe = StaleWriteProbe(self.window, self.original, 7, False, "epoch_conflict", 0)

    def verify(self, *observations, **overrides):
        inputs = dict(enabled=True, target=self.target, window=self.window, start_id=self.start_id,
                      original_deployment=self.original, replacement_deployment=self.replacement,
                      observations=observations)
        return verify_native_pre_epoch_fence(**(inputs | overrides))


class TestNativePreEpochFence(unittest.TestCase):
    def setUp(self):
        self.f = FenceFixture()

    def test_disabled_does_not_inspect_inputs_and_returns_frozen_verdict(self):
        class Unreadable:
            def __getattribute__(self, name):
                raise AssertionError(name)

            def __bool__(self):
                raise AssertionError("truthiness")

            def __iter__(self):
                raise AssertionError("iteration")

        unreadable = Unreadable()
        expected = EpochFenceVerdict(False)
        self.assertEqual(verify_native_pre_epoch_fence(), expected)
        for enabled in (False, None, 1, "true", unreadable):
            verdict = verify_native_pre_epoch_fence(
                unreadable, enabled=enabled, window=unreadable, start_id=unreadable,
                original_deployment=unreadable, replacement_deployment=unreadable,
                observations=unreadable,
            )
            self.assertEqual(verdict, expected)
            with self.assertRaises(FrozenInstanceError):
                verdict.enabled = True

    def test_accepts_monotonic_transitions_and_refusals_for_both_shapes(self):
        for shape in ("0473/0465", "0475/0465"):
            with self.subTest(shape=shape):
                f = FenceFixture(shape)
                probe_after_stop = replace(f.probe, deployment=f.replacement, epoch=8)
                self.assertEqual(f.verify(f.create, f.takeover, f.probe, f.stop, probe_after_stop),
                                 EpochFenceVerdict(True, 7, 8, 9, 2))
                self.assertEqual(f.verify(f.create, f.takeover, replace(f.stop, epoch=8)),
                                 EpochFenceVerdict(True, 7, 8, 8, 0))

    def test_rejects_rollback_duplicate_takeover_and_invalid_epochs(self):
        f = self.f
        for epoch in (6, 7):
            with self.assertRaisesRegex(TargetError, "takeover_not_advanced"):
                f.verify(f.create, replace(f.takeover, epoch=epoch), f.stop)
        with self.assertRaisesRegex(TargetError, "stop_rollback"):
            f.verify(f.create, f.takeover, replace(f.stop, epoch=7))

        observations = (f.create, f.takeover, f.stop, f.probe)
        for index, observation in enumerate(observations):
            for epoch in (True, False, 0, -1, 1 << 53, 8.0, "8"):
                with self.subTest(index=index, epoch=epoch):
                    invalid = replace(observation, epoch=epoch)
                    with self.assertRaisesRegex(TargetError, "invalid_pre_epoch_fence_epoch"):
                        f.verify(*observations[:index], invalid, *observations[index + 1:])

    def test_accepts_inclusive_epoch_bounds(self):
        f = self.f
        top_epoch = (1 << 53) - 1
        self.assertEqual(f.verify(replace(f.create, epoch=1), replace(f.takeover, epoch=top_epoch),
                                  replace(f.stop, epoch=top_epoch)),
                         EpochFenceVerdict(True, 1, top_epoch, top_epoch, 0))

    def test_requires_exact_transition_order_roles_and_count(self):
        f = self.f
        for observations in (
            (), (f.create, f.takeover), (f.create, f.stop, f.takeover),
            (f.create, f.takeover, f.takeover, f.stop), (f.create, f.takeover, f.stop, f.stop),
            (f.create, replace(f.takeover, deployment=f.original), f.stop),
            (f.create, f.takeover, replace(f.stop, deployment=f.replacement)),
            (replace(f.create, kind=TextAlias("original_create")), f.takeover, f.stop),
        ):
            with self.assertRaisesRegex(TargetError, "pre_epoch_fence_sequence"):
                f.verify(*observations)

    def test_refuses_stale_writes_without_exact_zero_mutation_refusal(self):
        f = self.f
        for changes in ({"accepted": True}, {"accepted": 0}, {"refusal_code": ""},
                        {"refusal_code": "stale_epoch"}, {"refusal_code": TextAlias("epoch_conflict")},
                        {"mutations": 1}, {"mutations": False}, {"mutations": -1}):
            with self.subTest(field=next(iter(changes))):
                with self.assertRaisesRegex(TargetError, "stale_write_unconfirmed"):
                    f.verify(f.create, f.takeover, replace(f.probe, **changes), f.stop)

    def test_probe_uses_the_active_fence_at_its_position(self):
        f = self.f
        for observations in ((f.probe, f.create, f.takeover, f.stop),
                             (f.create, f.probe, f.takeover, f.stop),
                             (f.create, f.takeover, replace(f.probe, epoch=9), f.stop)):
            with self.assertRaisesRegex(TargetError, "probe_not_stale"):
                f.verify(*observations)
        self.assertEqual(f.verify(f.create, replace(f.probe, epoch=6), f.takeover, f.stop),
                         EpochFenceVerdict(True, 7, 8, 9, 1))

    def test_rejects_str_subclasses_and_target_subclasses_in_every_pin(self):
        class ShapeAlias(str):
            pass

        class TargetAlias(Target):
            pass

        f = self.f
        for target in (replace(f.target, scope=TextAlias(f.target.scope)),
                       replace(f.target, shape=ShapeAlias(f.target.shape)),
                       TargetAlias(f.target.scope, f.target.run_id, f.target.started_at, f.target.shape)):
            deployment = replace(f.original, target=target)
            with self.assertRaisesRegex(TargetError, "invalid_pre_epoch_fence_target"):
                f.verify(f.create, f.takeover, f.stop, target=target)
            with self.assertRaisesRegex(TargetError, "wrong_pre_epoch_fence_deployment"):
                f.verify(f.create, f.takeover, f.stop, original_deployment=deployment)
            with self.assertRaisesRegex(TargetError, "wrong_pre_epoch_fence_deployment"):
                f.verify(f.create, f.takeover, replace(f.probe, deployment=deployment), f.stop)
            if type(target) is Target:
                window = EffectWindow(target, f.window.cutoff)
                with self.assertRaisesRegex(TargetError, "wrong_pre_epoch_fence_window"):
                    f.verify(f.create, f.takeover, f.stop, window=window)
                with self.assertRaisesRegex(TargetError, "wrong_pre_epoch_fence_window"):
                    f.verify(f.create, f.takeover, replace(f.probe, window=window), f.stop)

    def test_rejects_cross_shape_cross_scope_and_other_reservations(self):
        f = self.f
        for target in (replace(f.target, shape="0475/0465"), replace(f.target, scope="pre-fixture-other"),
                       replace(f.target, run_id=43), replace(f.target, started_at=101)):
            with self.assertRaisesRegex(TargetError, "wrong_pre_epoch_fence_window"):
                f.verify(f.create, f.takeover, f.stop, replace(f.probe, window=EffectWindow(target, 200)))
            with self.assertRaisesRegex(TargetError, "wrong_pre_epoch_fence_deployment"):
                f.verify(f.create, f.takeover, replace(f.stop, deployment=replace(f.original, target=target)))
        with self.assertRaisesRegex(TargetError, "wrong_pre_epoch_fence_window"):
            f.verify(f.create, f.takeover, f.stop, replace(f.probe, window=EffectWindow(f.target, 201)))
        for deployment in (replace(f.original, start_id=UUID(int=6)),
                           replace(f.original, worker_version=UUID(int=6))):
            with self.assertRaisesRegex(TargetError, "wrong_pre_epoch_fence_deployment"):
                f.verify(f.create, f.takeover, f.stop, replace(f.probe, deployment=deployment))

    def test_refuses_shared_worker_version_or_container_id(self):
        f = self.f
        for deployment in (f.original, replace(f.replacement, worker_version=f.original.worker_version),
                           replace(f.replacement, container_id=f.original.container_id)):
            with self.assertRaisesRegex(TargetError, "duplicate_pre_epoch_fence_deployment"):
                f.verify(f.create, f.takeover, f.stop, replacement_deployment=deployment)
