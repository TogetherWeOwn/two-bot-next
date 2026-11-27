"""Focused offline checks for predecessor retention and private ingress."""

from dataclasses import FrozenInstanceError, replace
from hashlib import sha256
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
from uuid import UUID

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_native_first_create import InitialDeployment, PrivateBindingPin, PrivateBindingReadback
    from pre_native_retention_guard import RetainedVersionInventory, verify_native_pre_retention_guard
    from pre_start_stop import Deployment, EffectWindow
    from pre_target import Target, TargetError


class TextSubclass(str):
    pass


class TupleSubclass(tuple):
    pass


class WindowSubclass(EffectWindow):
    def require_open(self, now):
        pass


class NativeRetentionGuardTests(unittest.TestCase):
    def setUp(self):
        target = Target("pre-fixture-retention", 123, 1800000000, "0473/0465")
        old, new, extra = UUID(int=1), UUID(int=2), UUID(int=3)
        destination = (target, target.worker_name, "pre-reserved")
        self.evidence = dict(
            target=target, reserved_environment=destination[2],
            window=EffectWindow(target, target.started_at + 50), now=target.started_at,
            promotion_disabled=True, predecessor=Deployment(target, UUID(int=4), old, UUID(int=5)),
            inventory=RetainedVersionInventory(*destination, old, new, (old, new, extra), True, True),
            initial=InitialDeployment(*destination, new, False, False, True, (), (), True, (), True),
            binding=PrivateBindingPin(*destination, UUID(int=6), new),
            serving=PrivateBindingReadback(*destination, UUID(int=6), new, True, True),
        )

    def verify(self, **overrides):
        return verify_native_pre_retention_guard(**(self.evidence | overrides))

    def test_returns_frozen_digest_of_canonical_pins(self):
        canonical = (
            b'{"binding_id":"00000000-0000-0000-0000-000000000006","cutoff":1800000050,'
            b'"environment":"pre-reserved","predecessor_container_id":"00000000-0000-0000-0000-000000000005",'
            b'"predecessor_start_id":"00000000-0000-0000-0000-000000000004",'
            b'"predecessor_version":"00000000-0000-0000-0000-000000000001",'
            b'"replacement_version":"00000000-0000-0000-0000-000000000002","schema":"pre-native-retention-v1",'
            b'"target":["pre-fixture-retention",123,1800000000,"0473/0465"],'
            b'"worker_service":"two-bot-next-pre-123-1800003600"}'
        )
        receipt = self.verify()
        self.assertEqual(receipt.retention_digest, "sha256:" + sha256(canonical).hexdigest())
        inventory = self.evidence["inventory"]
        self.assertEqual(receipt, self.verify(now=self.evidence["window"].cutoff - 1,
                         inventory=replace(inventory, retained_versions=inventory.retained_versions[::-1])))
        rebound = {field: replace(self.evidence[field], binding_id=UUID(int=7)) for field in ("binding", "serving")}
        self.assertNotEqual(receipt, self.verify(**rebound))
        with self.assertRaises(FrozenInstanceError):
            receipt.retention_digest = "tampered"

    def test_requires_promotion_hold_and_open_window(self):
        for disabled in (False, 1, "true", None):
            with self.subTest(disabled=disabled), self.assertRaisesRegex(TargetError, "promotion_not_disabled"):
                self.verify(promotion_disabled=disabled)
        cutoff = self.evidence["window"].cutoff
        for now, error in ((cutoff, "window_expired"), (cutoff + 1, "window_expired"),
                           (self.evidence["target"].started_at - 1, "invalid_pre_clock"),
                           (float(cutoff - 1), "invalid_pre_clock")):
            with self.subTest(now=now), self.assertRaisesRegex(TargetError, error):
                self.verify(now=now)
        target = self.evidence["target"]
        with self.assertRaisesRegex(TargetError, "invalid_pre_effect_window"):
            self.verify(window=WindowSubclass(target, cutoff), now=cutoff)
        window = EffectWindow(target, cutoff)
        object.__setattr__(window, "cutoff", target.expires_at + 1)
        with self.assertRaisesRegex(TargetError, "invalid_pre_effect_window"):
            self.verify(window=window)
        window = EffectWindow(target, cutoff)
        object.__setattr__(window, "require_open", lambda now: None)
        with self.assertRaisesRegex(TargetError, "window_expired"):
            self.verify(window=window, now=cutoff)

    def test_requires_distinct_predecessor_with_complete_untouched_history(self):
        inventory = self.evidence["inventory"]
        old, new, extra = inventory.retained_versions
        cases = (
            ({"retained_versions": (new, extra)}, "predecessor_missing"),
            ({"retained_versions": ()}, "predecessor_missing"),
            ({"inventory_complete": False}, "retention_unconfirmed"),
            ({"inventory_complete": 1}, "retention_unconfirmed"),
            ({"predecessor_untouched": False}, "retention_unconfirmed"),
            ({"predecessor_untouched": 1}, "retention_unconfirmed"),
            ({"predecessor_version": new}, "retention_unconfirmed"),
            ({"after_serving_version": old}, "retention_unconfirmed"),
            ({"after_serving_version": None}, "retention_unconfirmed"),
            ({"after_serving_version": str(new)}, "retention_unconfirmed"),
            ({"retained_versions": (old, str(new))}, "invalid_pre_native_retained_versions"),
            ({"retained_versions": (str(old),)}, "invalid_pre_native_retained_versions"),
            ({"retained_versions": [old, new, extra]}, "invalid_pre_native_retained_versions"),
        )
        for changes, error in cases:
            with self.subTest(changes=changes), self.assertRaisesRegex(TargetError, error):
                self.verify(inventory=replace(inventory, **changes))
        with self.assertRaisesRegex(TargetError, "invalid_pre_native_replacement_version"):
            self.verify(initial=replace(self.evidence["initial"], version=old))

    def test_refuses_public_or_unconfirmed_ingress(self):
        initial = self.evidence["initial"]
        cases = (
            ("workers_dev", True), ("workers_dev", 0), ("preview_urls", True), ("preview_urls", 0),
            ("routes", ("example.invalid/*",)), ("routes", []), ("routes", TupleSubclass()),
            ("custom_domains", ("example.invalid",)), ("custom_domains", []),
            ("complete", False), ("complete", 1), ("closed_from_first_effect", False),
            ("closed_from_first_effect", 1),
        )
        for field, value in cases:
            with self.subTest(field=field, value=value), self.assertRaisesRegex(TargetError, "ingress_unconfirmed"):
                self.verify(initial=replace(initial, **{field: value}))

    def test_binds_all_observations_to_exact_reservation_and_private_dispatch(self):
        target = self.evidence["target"]
        other_shape = replace(target, shape="0475/0465")
        self.assertEqual(target.worker_name, other_shape.worker_name)
        for evidence in ("window", "predecessor", "inventory", "initial", "binding", "serving"):
            with self.subTest(evidence=evidence), self.assertRaises(TargetError):
                self.verify(**{evidence: replace(self.evidence[evidence], target=other_shape)})
        for field in ("worker_service", "environment"):
            with self.subTest(field=field), self.assertRaisesRegex(TargetError, "retention_unconfirmed"):
                self.verify(inventory=replace(self.evidence["inventory"], **{field: "other"}))
        for evidence in ("initial", "binding", "serving"):
            for field, value in (("worker_service", "other-worker"), ("environment", "other-environment"),
                                 ("version", UUID(int=7))):
                with self.subTest(evidence=evidence, field=field), self.assertRaises(TargetError):
                    self.verify(**{evidence: replace(self.evidence[evidence], **{field: value})})
        serving = self.evidence["serving"]
        for field, value in (("binding_id", UUID(int=7)), ("via_private_binding", False), ("independent", False),
                             ("via_private_binding", 1), ("independent", 1)):
            with self.subTest(field=field, value=value), self.assertRaisesRegex(TargetError, "dispatch_unconfirmed"):
                self.verify(serving=replace(serving, **{field: value}))

    def test_refuses_blank_environment_even_when_all_pins_agree(self):
        exact_blanks = {field: replace(self.evidence[field], environment="")
                        for field in ("initial", "binding", "serving", "inventory")}
        with self.assertRaisesRegex(TargetError, "invalid_pre_native_environment"):
            self.verify(reserved_environment="", **exact_blanks)

    def test_rejects_tampered_types_without_echo(self):
        target = self.evidence["target"]
        other = replace(target)
        object.__setattr__(other, "run_id", float(target.run_id))
        synthetic_marker = "synthetic-input-must-not-echo"
        malformed_uuid = UUID(int=1)
        object.__setattr__(malformed_uuid, "int", synthetic_marker)
        for argument, replacement in (
            ("target", replace(target, scope=TextSubclass(target.scope))),
            ("window", replace(self.evidence["window"], target=other)),
            ("predecessor", replace(self.evidence["predecessor"], worker_version=malformed_uuid)),
            ("predecessor", replace(self.evidence["predecessor"], worker_version=UUID(int=0))),
            ("initial", replace(self.evidence["initial"], version=synthetic_marker)),
            ("binding", replace(self.evidence["binding"], worker_service=TextSubclass(target.worker_name))),
            ("serving", replace(self.evidence["serving"], binding_id=str(self.evidence["binding"].binding_id))),
            ("serving", {"binding_id": self.evidence["binding"].binding_id}),
            ("reserved_environment", TextSubclass("pre-reserved")),
        ):
            with self.subTest(argument=argument):
                with self.assertRaises(TargetError) as rejected:
                    self.verify(**{argument: replacement})
                self.assertNotIn(synthetic_marker, str(rejected.exception))
