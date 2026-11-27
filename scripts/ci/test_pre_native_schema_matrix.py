"""Focused synthetic proofs for the offline native PRE schema matrix."""

from dataclasses import FrozenInstanceError, replace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_native_schema_matrix import (NativePreSchemaMatrixVerdict, NativePreSchemaProof,
                                          verify_native_pre_schema_matrix)
    from pre_target import LAST_UNIX_SECOND, Target, TargetError


SOURCE_SHA = "a" * 40
IMAGE_DIGEST = "sha256:" + "b" * 64
FIRST = Target("pre-fixture-first", 41, 1_800_000_000, "0473/0465")
SECOND = Target("pre-fixture-second", 42, FIRST.started_at + 1, "0475/0465")
OBSERVED_AT = SECOND.started_at + 100
STAGES = (
    ("private_first_create", "pre_native_first_create_unconfirmed"),
    ("exact_takeover", "pre_native_takeover_unconfirmed"),
    ("independent_stop", "pre_native_stop_unconfirmed"),
)


def proof_for(target):
    return NativePreSchemaProof(
        target=target, source_sha=SOURCE_SHA, container_image_digest=IMAGE_DIGEST,
        verified_at=target.started_at + 1,
        private_first_create_ok=True, exact_takeover_ok=True, independent_stop_ok=True,
        private_first_create_receipt_id=f"{target.run_id}-first-create",
        exact_takeover_receipt_id=f"{target.run_id}-takeover",
        independent_stop_receipt_id=f"{target.run_id}-stop",
    )


PROOFS = (proof_for(FIRST), proof_for(SECOND))


class Unreadable:
    def __getattribute__(self, name):
        raise AssertionError("disabled verifier accessed evidence")

    def __iter__(self):
        raise AssertionError("disabled verifier iterated evidence")

    def __bool__(self):
        raise AssertionError("disabled verifier tested input truthiness")


class NativePreSchemaMatrixTests(unittest.TestCase):
    def verify(self, **changes):
        inputs = {
            "enabled": True,
            "production_auto_promote_disabled": True,
            "observed_at": OBSERVED_AT,
            "source_sha": SOURCE_SHA,
            "container_image_digest": IMAGE_DIGEST,
            "proofs": PROOFS,
        }
        return verify_native_pre_schema_matrix(**(inputs | changes))

    def refuse(self, code, **changes):
        with self.assertRaisesRegex(TargetError, f"^{code}$"):
            self.verify(**changes)

    def refuse_second(self, code, **changes):
        self.refuse(code, proofs=(PROOFS[0], replace(PROOFS[1], **changes)))

    def test_default_off_skips_all_inputs(self):
        verdict = verify_native_pre_schema_matrix()
        self.assertEqual(verdict, NativePreSchemaMatrixVerdict(enabled=False))
        with self.assertRaises(FrozenInstanceError):
            verdict.enabled = True

        unreadable = Unreadable()
        for enabled in (False, None, 1, "true", unreadable):
            self.assertEqual(verdict, verify_native_pre_schema_matrix(
                enabled=enabled, production_auto_promote_disabled=unreadable,
                observed_at=unreadable, source_sha=unreadable, container_image_digest=unreadable,
                proofs=(unreadable,),
            ))
        self.assertEqual(verdict, verify_native_pre_schema_matrix(proofs=unreadable))

    def test_verifies_both_shapes_in_canonical_order_at_inclusive_bounds(self):
        expected = NativePreSchemaMatrixVerdict(enabled=True, verified_shapes=(FIRST.shape, SECOND.shape))
        self.assertEqual(self.verify(), expected)
        self.assertEqual(self.verify(proofs=PROOFS[::-1]), expected)
        self.assertEqual(self.verify(observed_at=FIRST.expires_at, proofs=(
            replace(PROOFS[0], verified_at=FIRST.started_at),
            replace(PROOFS[1], verified_at=FIRST.expires_at),
        )), expected)

    def test_refuses_missing_duplicate_and_unknown_shapes(self):
        for proofs in ((), PROOFS[:1], PROOFS[1:]):
            with self.subTest(proofs=proofs):
                self.refuse("missing_pre_native_matrix_shape", proofs=proofs)
        for proofs in ((PROOFS[0], PROOFS[0]), (*PROOFS, PROOFS[1])):
            with self.subTest(proofs=proofs):
                self.refuse("duplicate_pre_native_matrix_shape", proofs=proofs)

        unknown = replace(SECOND)
        object.__setattr__(unknown, "shape", "9999/0465")
        self.refuse_second("unsupported_pre_shape", target=unknown)

    def test_refuses_malformed_packet_or_entry(self):
        self.refuse("invalid_pre_native_matrix_packet", proofs=Unreadable())
        self.refuse("invalid_pre_native_matrix_proof", proofs=(PROOFS[0], None))
        self.refuse_second("invalid_pre_native_matrix_target", target=None)
        invalid = replace(SECOND)
        object.__setattr__(invalid, "run_id", True)
        self.refuse_second("invalid_pre_run", target=invalid)

    def test_refuses_overlap_in_scope_run_or_worker(self):
        for target in (replace(SECOND, scope=FIRST.scope), replace(SECOND, run_id=FIRST.run_id)):
            with self.subTest(target=target):
                self.refuse_second("pre_native_matrix_target_overlap", target=target)

        # The real Worker name is derived from the already-checked run ID and expiry.
        # Force a collision with distinct run IDs to exercise the explicit name guard.
        with patch.object(Target, "worker_name", "same-worker"):
            self.refuse("pre_native_matrix_target_overlap")

    def test_requires_exact_build_pins(self):
        for field, invalid, mismatch, suffix in (
            ("source_sha", SOURCE_SHA.upper(), "c" * 40, "source_sha"),
            ("container_image_digest", IMAGE_DIGEST + "\n", "sha256:" + "d" * 64, "image_digest"),
        ):
            with self.subTest(field=field):
                self.refuse(f"invalid_pre_native_matrix_{suffix}", **{field: invalid})
                self.refuse(f"invalid_pre_native_matrix_{suffix}", **{field: None})
                self.refuse_second(f"pre_native_matrix_{suffix}_mismatch", **{field: mismatch})
                self.refuse(f"pre_native_matrix_{suffix}_mismatch", **{field: mismatch})

    def test_requires_true_hold_and_strict_bounded_observation_clock(self):
        for hold in (False, None, 1, "true"):
            self.refuse("pre_native_matrix_promotion_not_disabled", production_auto_promote_disabled=hold)
        for observed_at in (None, True, False, 0, -1, LAST_UNIX_SECOND + 1, float(OBSERVED_AT)):
            self.refuse("invalid_pre_native_matrix_clock", observed_at=observed_at)

    def test_refuses_expiry_and_proof_outside_observation_window(self):
        self.refuse("pre_native_matrix_target_expired", observed_at=FIRST.expires_at + 1)
        for verified_at in (None, SECOND.started_at - 1, OBSERVED_AT + 1, float(OBSERVED_AT)):
            self.refuse_second("invalid_pre_native_matrix_proof_clock", verified_at=verified_at)

        self.refuse("invalid_pre_native_matrix_proof_clock", proofs=(
            replace(PROOFS[0], target=replace(FIRST, started_at=1), verified_at=1),
            replace(PROOFS[1], target=replace(SECOND, started_at=1), verified_at=True),
        ), observed_at=2)

    def test_requires_all_three_true_stages_on_each_shape(self):
        for index, proof in enumerate(PROOFS):
            for stage, code in STAGES:
                for value in (False, None, 1):
                    with self.subTest(shape=proof.target.shape, stage=stage, value=value):
                        proofs = list(PROOFS)
                        proofs[index] = replace(proof, **{f"{stage}_ok": value})
                        self.refuse(code, proofs=tuple(proofs))

    def test_requires_nonempty_distinct_receipts_within_and_across_shapes(self):
        for stage, _ in STAGES:
            for invalid in (None, "", " ", 1):
                self.refuse_second("invalid_pre_native_matrix_receipt", **{f"{stage}_receipt_id": invalid})
        self.refuse_second("duplicate_pre_native_matrix_receipt",
                           independent_stop_receipt_id=PROOFS[1].exact_takeover_receipt_id)
        self.refuse_second("duplicate_pre_native_matrix_receipt",
                           independent_stop_receipt_id=PROOFS[0].private_first_create_receipt_id)
