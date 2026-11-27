"""Verify native PRE registry closure with synthetic, offline evidence."""

from dataclasses import FrozenInstanceError, replace
from hashlib import sha256
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
from uuid import UUID

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_admission import AdmissionRecord, SettlementSource
    from pre_native_registry_verifier import NativeRegistryReadback, verify_native_pre_registry_closure
    from pre_registry_cleanup import RegistryDisposition, RegistryReservation
    from pre_start_stop import Deployment, EffectWindow
    from pre_target import Target, TargetError


TARGET = Target("pre-fixture-test", 123, 1_800_000_000, "0473/0465")
WINDOW = EffectWindow(TARGET, TARGET.started_at + 600)
DEPLOYMENT = Deployment(TARGET, UUID(int=1), UUID(int=2), UUID(int=3))
RECORD = AdmissionRecord(TARGET, DEPLOYMENT.start_id, True, SettlementSource.PROVIDER)
REPOSITORY = "registry.example.invalid/staging/two-bot-next-attendance-predecessor-staging"
REFERENCE = f"{REPOSITORY}@sha256:{'a' * 64}"
RESERVATION = RegistryReservation(TARGET, REFERENCE)
NOW = TARGET.started_at + 20
READBACK = NativeRegistryReadback(TARGET, REFERENCE, RegistryDisposition.ALREADY_ABSENT, True, NOW - 1)


def verify(deployment=DEPLOYMENT, **overrides):
    inputs = dict(window=WINDOW, record=RECORD, reservation=RESERVATION, readback=READBACK,
                  promotion_disabled=True, now=NOW)
    inputs.update(overrides)
    return verify_native_pre_registry_closure(deployment, **inputs)


class NativeRegistryClosureTests(unittest.TestCase):
    def test_closure_returns_frozen_canonical_receipt(self):
        canonical = (
            b'{"container_id":"00000000-0000-0000-0000-000000000003","cutoff":1800000600,'
            b'"disposition":"already_absent","kind":"native_pre_registry_closure","observed_at":1800000019,'
            b'"reference":"' + REFERENCE.encode("ascii") + b'","settled_by":"provider",'
            b'"start_id":"00000000-0000-0000-0000-000000000001",'
            b'"target":{"run_id":123,"scope":"pre-fixture-test","shape":"0473/0465","started_at":1800000000},'
            b'"worker_version":"00000000-0000-0000-0000-000000000002"}'
        )
        receipt = verify()
        self.assertEqual(receipt.closure_digest, sha256(canonical).hexdigest())
        self.assertEqual(receipt, verify(now=NOW + 1))
        self.assertNotIn(REFERENCE, repr(receipt))
        with self.assertRaises(FrozenInstanceError):
            receipt.closure_digest = "modified"

    def test_runtime_settlement_and_same_second_readback_are_accepted(self):
        runtime = replace(RECORD, settled_by=SettlementSource.RUNTIME)
        current_readback = replace(READBACK, observed_at=NOW)
        self.assertNotEqual(verify(record=runtime).closure_digest, verify().closure_digest)
        self.assertNotEqual(verify(readback=current_readback).closure_digest, verify().closure_digest)

    def test_refuses_without_explicit_promotion_hold(self):
        for value in (False, None, 1, "true"):
            with self.subTest(value=value), self.assertRaises(TargetError):
                verify(promotion_disabled=value)

    def test_requires_open_effect_window_and_exact_integer_clock(self):
        for now in (True, False, float(NOW), TARGET.started_at - 1, WINDOW.cutoff, WINDOW.cutoff + 1):
            with self.subTest(now=now), self.assertRaises(TargetError):
                verify(now=now)
        self.assertEqual(verify(now=WINDOW.cutoff - 1), verify())

    def test_refuses_open_unsettled_or_unbound_admission(self):
        for changes in (
            {"closed": False}, {"closed": 1}, {"settled_by": None}, {"settled_by": "provider"},
            {"start_id": None}, {"start_id": str(RECORD.start_id)}, {"start_id": UUID(int=4)},
        ):
            with self.subTest(changes=changes), self.assertRaises(TargetError):
                verify(record=replace(RECORD, **changes))

    def test_requires_full_target_even_when_provider_names_match(self):
        other_target = replace(TARGET, shape="0475/0465")
        self.assertEqual(other_target.provider_names, TARGET.provider_names)
        for field, value in (
            ("deployment", replace(DEPLOYMENT, target=other_target)),
            ("window", replace(WINDOW, target=other_target)),
            ("record", replace(RECORD, target=other_target)),
            ("reservation", replace(RESERVATION, target=other_target)),
            ("readback", replace(READBACK, target=other_target)),
        ):
            with self.subTest(field=field), self.assertRaises(TargetError):
                verify(**{field: value})

    def test_refuses_different_or_mutable_reference_without_echoing_it(self):
        for reference in (f"{REPOSITORY}@sha256:{'b' * 64}", f"{REPOSITORY}:latest"):
            with self.subTest(reference=reference):
                with self.assertRaises(TargetError) as error:
                    verify(readback=replace(READBACK, reference=reference))
                self.assertNotIn(reference, str(error.exception))

    def test_digest_binds_the_reserved_reference(self):
        reference = f"{REPOSITORY}@sha256:{'b' * 64}"
        receipt = verify(reservation=replace(RESERVATION, reference=reference),
                         readback=replace(READBACK, reference=reference))
        self.assertNotEqual(receipt, verify())

    def test_requires_affirmative_complete_absence(self):
        for changes in (
            {"inventory_complete": False}, {"inventory_complete": 1},
            {"disposition": RegistryDisposition.UNRESOLVED_DIGEST_REFERENCE},
            {"disposition": RegistryDisposition.ALREADY_ABSENT.value},
        ):
            with self.subTest(changes=changes), self.assertRaises(TargetError):
                verify(readback=replace(READBACK, **changes))

    def test_refuses_stale_future_and_non_integer_readback(self):
        for observed_at in (NOW - 3601, TARGET.started_at - 1, NOW + 1, False, True, float(NOW)):
            with self.subTest(observed_at=observed_at), self.assertRaises(TargetError):
                verify(readback=replace(READBACK, observed_at=observed_at))

    def test_refuses_wrong_evidence_types_and_deployment_ids(self):
        for field in ("deployment", "window", "record", "reservation", "readback"):
            with self.subTest(field=field), self.assertRaises(TargetError):
                verify(**{field: None})
        for field in ("start_id", "worker_version", "container_id"):
            for identity in ("00000000-0000-0000-0000-000000000001", UUID(int=0)):
                with self.subTest(field=field, identity=identity), self.assertRaises(TargetError):
                    verify(replace(DEPLOYMENT, **{field: identity}))

    def test_refuses_non_string_target_fields_that_compare_equal(self):
        class StringSubclass(str):
            pass

        for field in ("scope", "shape"):
            substitute = replace(TARGET, **{field: StringSubclass(getattr(TARGET, field))})
            self.assertEqual(substitute, TARGET)
            with self.subTest(field=field), self.assertRaises(TargetError):
                verify(record=replace(RECORD, target=substitute))
